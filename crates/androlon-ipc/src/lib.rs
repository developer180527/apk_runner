//! Androlon suite IPC: how every mini-app (Hub, Installer, Player, ctl) talks
//! to `androlon-runtimed`, the daemon that owns the Android runtime.
//!
//! Wire format: newline-delimited JSON over a unix socket at
//! `~/.androlon/runtimed.sock`. The JSON is a deliberately tiny dialect —
//! one flat object per line, values are strings, integers, booleans, or
//! arrays of strings — hand-rolled so the engine stays std-only, and still
//! trivially parseable by a future native (Swift/etc.) shell.
//!
//! The client self-heals: if the socket is dead it spawns the daemon
//! (`$ANDROLON_RUNTIMED`, or `androlon-runtimed` next to the current
//! executable) and retries.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub mod json;

pub use json::Value;

/// Where the suite keeps its runtime state (socket, logs).
pub fn state_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".androlon")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("runtimed.sock")
}

/// One request to the daemon: `{"req":"<verb>", ...args}`.
pub fn request(verb: &str, args: &[(&str, Value)]) -> std::io::Result<Response> {
    let mut obj = BTreeMap::new();
    obj.insert("req".to_string(), Value::Str(verb.to_string()));
    for (k, v) in args {
        obj.insert((*k).to_string(), v.clone());
    }
    let line = json::encode(&obj);

    let mut stream = connect_or_spawn()?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    let obj = json::decode(reply.trim()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad reply: {reply}"))
    })?;
    Ok(Response { obj })
}

/// A persistent connection whose `ensure-booted` refcount is held until drop.
/// Players use this: the runtime stays up as long as the pane lives.
pub struct RuntimeLease {
    stream: UnixStream,
}

impl RuntimeLease {
    /// Ask the daemon to boot (if needed) and hold a lease on the runtime.
    /// Blocks until adb reports the device ready.
    pub fn acquire() -> std::io::Result<(RuntimeLease, Response)> {
        let mut stream = connect_or_spawn()?;
        stream.write_all(b"{\"req\":\"ensure-booted\"}\n")?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut reply = String::new();
        // Booting can take minutes on a cold start.
        stream.set_read_timeout(None)?;
        reader.read_line(&mut reply)?;
        let obj = json::decode(reply.trim()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad reply: {reply}"))
        })?;
        Ok((RuntimeLease { stream }, Response { obj }))
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        // Closing the connection is the release; the daemon watches EOF.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

pub struct Response {
    obj: BTreeMap<String, Value>,
}

impl Response {
    pub fn ok(&self) -> bool {
        !self.obj.contains_key("err")
    }

    pub fn err(&self) -> Option<&str> {
        match self.obj.get("err") {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.obj.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.obj.get(key) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.obj.get(key) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn list(&self, key: &str) -> Option<&[String]> {
        match self.obj.get(key) {
            Some(Value::List(l)) => Some(l),
            _ => None,
        }
    }
}

/// Connect to the daemon, spawning it if the socket is dead. The daemon
/// binary is `$ANDROLON_RUNTIMED`, or `androlon-runtimed` beside the caller.
fn connect_or_spawn() -> std::io::Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }

    // Stale socket file from a dead daemon blocks bind on the daemon side;
    // clearing it here is safe because connect() just failed.
    let _ = std::fs::remove_file(&path);

    let daemon = std::env::var("ANDROLON_RUNTIMED").map(PathBuf::from).or_else(|_| {
        std::env::current_exe().map(|p| p.with_file_name("androlon-runtimed"))
    })?;
    if !daemon.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("androlon-runtimed not found at {}", daemon.display()),
        ));
    }
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Give it a moment to bind.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "androlon-runtimed did not come up",
    ))
}
