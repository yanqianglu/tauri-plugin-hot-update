use serde::{Serialize, Serializer};

/// Errors surfaced by the hot-update plugin.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The plugin was not initialized: either `install()` was not called on
    /// the context, or `.plugin(init(handle))` was not registered, or
    /// initialization failed at boot (in which case the app serves the
    /// embedded bundle — the fail-safe floor).
    #[error("hot-update is not active (plugin not initialized); serving embedded assets")]
    NotActive,

    /// A staging request was refused by the state machine gates.
    #[error("stage refused: {0}")]
    StageRefused(#[from] crate::machine::StageError),
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
