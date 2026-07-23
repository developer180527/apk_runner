use std::fmt;

#[derive(Debug)]
pub enum StreamError {
    Io(std::io::Error),
    Engine(String),
    Protocol(String),
    ServerNotFound(String),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Io(e) => write!(f, "io: {e}"),
            StreamError::Engine(e) => write!(f, "adb/engine: {e}"),
            StreamError::Protocol(e) => write!(f, "scrcpy protocol: {e}"),
            StreamError::ServerNotFound(p) => {
                write!(f, "scrcpy-server not found at {p} (bundle it or set path)")
            }
        }
    }
}

impl std::error::Error for StreamError {}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, StreamError>;
