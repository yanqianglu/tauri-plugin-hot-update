//! Test-only helpers shared by the update and command tests: a local HTTP
//! server plus a signed-release fixture. Real sockets, real signing — no
//! mocks of the code under test. The server is hand-rolled over
//! `TcpListener` (instead of a server crate) because the update tests need
//! exact control over wire behavior — most notably a response that declares
//! a Content-Length and then closes early (the partial-download case),
//! which framed servers refuse to produce.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use minisign::KeyPair;
use semver::Version;
use tempfile::TempDir;

use crate::manifest::Manifest;
use crate::sign::{sign_release, SignOptions};
use crate::store::Store;
use crate::update::UpdateConfig;

/// A publishable signed release: a dist dir, a generated minisign keypair,
/// and a [`TestServer`] serving whatever [`Fixture::publish`] signs. `root`
/// is a fresh store root (the `hot-update/` dir) for booting against.
pub struct Fixture {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub server: TestServer,
    pub keypair: KeyPair,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("hot-update");
        let dist = tmp.path().join("dist");
        fs::create_dir_all(dist.join("assets")).unwrap();
        fs::write(dist.join("index.html"), b"<html>ota v-next</html>").unwrap();
        fs::write(dist.join("assets/app.js"), b"console.log('hot')").unwrap();
        Self {
            tmp,
            root,
            server: TestServer::start(),
            keypair: KeyPair::generate_unencrypted_keypair().unwrap(),
        }
    }

    pub fn config(&self) -> UpdateConfig {
        UpdateConfig {
            manifest_url: self.server.url("/manifest.json"),
            pubkeys: vec![self.keypair.pk.to_base64()],
        }
    }

    /// Sign the dist with the CLI's signing code and serve the three
    /// artifacts. Returns the manifest and the archive's server path.
    pub fn publish(&self, version: &str, min_shell: &str) -> (Manifest, String) {
        let out = self.tmp.path().join(format!("release-{version}"));
        let release = sign_release(
            &SignOptions {
                dist_dir: &self.tmp.path().join("dist"),
                version: Version::parse(version).unwrap(),
                min_shell_version: Version::parse(min_shell).unwrap(),
                base_url: &self.server.url(""),
                out_dir: &out,
            },
            &self.keypair.sk,
        )
        .expect("sign_release");
        let archive_path = format!("/bundle-{version}.tar.gz");
        self.server
            .set("/manifest.json", fs::read(&release.manifest_path).unwrap());
        self.server.set(
            "/manifest.json.minisig",
            fs::read(&release.signature_path).unwrap(),
        );
        self.server
            .set(&archive_path, fs::read(&release.archive_path).unwrap());
        (release.manifest, archive_path)
    }

    pub fn bundle_dir(&self, seq: u64) -> PathBuf {
        Store::new(self.root.clone()).bundle_dir(seq)
    }

    pub fn state(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.root.join("state.json")).unwrap()).unwrap()
    }

    /// Non-`seq-N` debris under bundles/ (leaked temp files/dirs).
    pub fn debris(&self) -> Vec<PathBuf> {
        Store::new(self.root.clone()).foreign_entries()
    }
}

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
        self.routes.lock().unwrap().insert(path.to_string(), route);
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
