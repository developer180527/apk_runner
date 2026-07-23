use std::fmt;

/// Engine errors. Std-only, so we hand-roll `Display` instead of pulling `thiserror`.
#[derive(Debug)]
pub enum EngineError {
    SdkMissing(String),
    NotRootable(String),
    RamdiskMissing(String),
    ApkNotFound(String),
    PackageUnresolved(String),
    BackendUnimplemented(String),
    Launch { tool: String, source: std::io::Error },
    NonZero { tool: String, status: i32, output: String },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::SdkMissing(tool) => {
                write!(f, "{tool} not found — run `androlon-ctl setup` first.")
            }
            EngineError::NotRootable(kind) => write!(
                f,
                "image '{kind}' is a locked user build — use google_apis or aosp for root."
            ),
            EngineError::RamdiskMissing(path) => {
                write!(f, "system-image ramdisk not found at {path} — run setup.")
            }
            EngineError::ApkNotFound(path) => write!(f, "APK not found: {path}"),
            EngineError::PackageUnresolved(name) => {
                write!(f, "could not determine package name for {name}")
            }
            EngineError::BackendUnimplemented(name) => {
                write!(f, "GPU/host backend '{name}' is not implemented yet.")
            }
            EngineError::Launch { tool, source } => write!(f, "failed to launch {tool}: {source}"),
            EngineError::NonZero { tool, status, output } => {
                write!(f, "{tool} exited {status}:\n{}", output.trim())
            }
        }
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;
