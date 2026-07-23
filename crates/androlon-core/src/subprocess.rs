use crate::error::{EngineError, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Result of a finished child process (stdout+stderr merged).
#[derive(Debug)]
pub struct CommandOutput {
    pub status: i32,
    pub output: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
    pub fn trimmed(&self) -> &str {
        self.output.trim()
    }
}

fn build_command(
    exe: &Path,
    args: &[&str],
    extra_path: &[PathBuf],
    env: &[(String, String)],
) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    if !extra_path.is_empty() {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let prefix = extra_path
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(&sep.to_string());
        let existing = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{prefix}{sep}{existing}"));
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

/// Run `exe args`, blocking until exit; stdout+stderr are captured together so a
/// single reader can't deadlock on a full pipe (matters for chatty sdkmanager).
pub fn run(
    exe: &Path,
    args: &[&str],
    extra_path: &[PathBuf],
    env: &[(String, String)],
) -> Result<CommandOutput> {
    let tool = exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe.display().to_string());

    let out = build_command(exe, args, extra_path, env)
        .output()
        .map_err(|source| EngineError::Launch { tool: tool.clone(), source })?;

    let mut merged = String::from_utf8_lossy(&out.stdout).into_owned();
    merged.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(CommandOutput {
        status: out.status.code().unwrap_or(-1),
        output: merged,
    })
}

/// Like `run`, but errors if the process exits non-zero.
pub fn run_checked(
    exe: &Path,
    args: &[&str],
    extra_path: &[PathBuf],
    env: &[(String, String)],
) -> Result<CommandOutput> {
    let tool = exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe.display().to_string());
    let result = run(exe, args, extra_path, env)?;
    if result.ok() {
        Ok(result)
    } else {
        Err(EngineError::NonZero {
            tool,
            status: result.status,
            output: result.output,
        })
    }
}

/// Spawn a long-running process (the emulator) detached, redirecting output to
/// `log_file`. Returns the `Child` so the caller can track/terminate it.
pub fn spawn_detached(
    exe: &Path,
    args: &[&str],
    extra_path: &[PathBuf],
    env: &[(String, String)],
    log_file: &Path,
) -> Result<std::process::Child> {
    let tool = exe
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe.display().to_string());
    let log = std::fs::File::create(log_file)
        .map_err(|source| EngineError::Launch { tool: tool.clone(), source })?;
    let log_err = log
        .try_clone()
        .map_err(|source| EngineError::Launch { tool: tool.clone(), source })?;
    build_command(exe, args, extra_path, env)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|source| EngineError::Launch { tool, source })
}
