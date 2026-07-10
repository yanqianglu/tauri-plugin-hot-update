//! Test-only local HTTP server: real sockets, no mocks of the code under
//! test. Hand-rolled over `TcpListener` (instead of a server crate) because
//! the update tests need exact control over wire behavior — most notably a
//! response that declares a Content-Length and then closes early (the
//! partial-download case), which framed servers refuse to produce.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One servable path. `declared_len` overrides the Content-Length header;
/// a value larger than `body.len()` yields a truncated response (premature
/// connection close from the client's point of view).
#[derive(Clone)]
pub struct Route {
    pub body: Vec<u8>,
    pub declared_len: Option<u64>,
}

pub struct TestServer {
    base_url: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    /// Bind 127.0.0.1 on an ephemeral port and serve on a background thread
    /// for the rest of the process (test threads are cheap and idle).
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let thread_routes = Arc::clone(&routes);
        let thread_requests = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let _ = handle(stream, &thread_routes, &thread_requests);
            }
        });
        Self {
            base_url,
            routes,
            requests,
        }
    }

    /// Absolute URL for a path (`path` must start with `/`).
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub fn set(&self, path: &str, body: Vec<u8>) {
        self.set_route(
            path,
            Route {
                body,
                declared_len: None,
            },
        );
    }

    pub fn set_route(&self, path: &str, route: Route) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), route);
    }

    /// Every request path served so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self, path: &str) -> usize {
        self.requests().iter().filter(|p| *p == path).count()
    }
}

/// Serve one connection: parse the GET line, log it, answer with the route
/// (or 404), always `Connection: close`.
fn handle(
    mut stream: TcpStream,
    routes: &Mutex<HashMap<String, Route>>,
    requests: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
    }
    requests.lock().unwrap().push(path.clone());

    let route = routes.lock().unwrap().get(&path).cloned();
    match route {
        Some(route) => {
            let declared = route.declared_len.unwrap_or(route.body.len() as u64);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            )?;
            stream.write_all(&route.body)?;
        }
        None => {
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )?;
        }
    }
    stream.flush()
}
