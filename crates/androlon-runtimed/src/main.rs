//! androlon-runtimed — the suite's runtime daemon. Owns the Android emulator
//! lifecycle so no mini-app ever races another over it:
//!
//! - boots the runtime **headless** on first demand (Consumer profile: Quick
//!   Boot resume) and blocks the caller until adb reports it ready;
//! - refcounts leases: an `ensure-booted` connection holds a lease until the
//!   peer disconnects (players hold theirs for the pane's lifetime);
//! - when the last lease is released, stops the emulator after a linger
//!   (`ANDROLON_LINGER` seconds, default 120) unless someone comes back;
//! - answers queries (`status`, `installed-apps`) for the Hub and Installer.
//!
//! Protocol: see `androlon-ipc` (newline JSON over a unix socket).

use androlon_core::backend::AndroidBackend;
use androlon_core::{Avd, BootProfile, EmulatorService, SdkConfig};
use androlon_ipc::{json, socket_path, state_dir, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Daemon {
    engine: EmulatorService,
    /// Active leases (connections that ran ensure-booted).
    leases: Mutex<usize>,
    /// Bumped on every lease change; the shutdown timer only fires if its
    /// generation is still current when the linger elapses.
    generation: AtomicU64,
    /// True once WE booted the runtime. A runtime the user booted themselves
    /// (Android Studio, CLI) is never ours to stop.
    booted_by_us: std::sync::atomic::AtomicBool,
}

fn main() {
    let cfg = SdkConfig::from_env();
    let daemon = Arc::new(Daemon {
        engine: EmulatorService::new(cfg),
        leases: Mutex::new(0),
        generation: AtomicU64::new(0),
        booted_by_us: std::sync::atomic::AtomicBool::new(false),
    });

    let path = socket_path();
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::remove_file(&path); // clients only spawn us when it's dead
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("androlon-runtimed: bind {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    // Local-user only.
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    eprintln!("androlon-runtimed: listening at {}", path.display());

    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || serve(daemon, conn));
    }
}

fn serve(daemon: Arc<Daemon>, conn: UnixStream) {
    let mut leased = false;
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut conn = conn;

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // peer gone
            Ok(_) => {}
        }
        let Some(req) = json::decode(line.trim()) else {
            let _ = reply(&mut conn, &[("err", Value::Str("bad request".into()))]);
            continue;
        };
        let verb = match req.get("req") {
            Some(Value::Str(v)) => v.clone(),
            _ => {
                let _ = reply(&mut conn, &[("err", Value::Str("missing req".into()))]);
                continue;
            }
        };

        let result: Vec<(&str, Value)> = match verb.as_str() {
            "status" => {
                let booted = daemon.engine.emulator_running();
                let leases = *daemon.leases.lock().unwrap();
                vec![
                    ("ok", Value::Bool(true)),
                    ("booted", Value::Bool(booted)),
                    ("leases", Value::Int(leases as i64)),
                ]
            }
            "ensure-booted" => match ensure_booted(&daemon) {
                Ok(()) => {
                    if !leased {
                        leased = true;
                        lease_added(&daemon);
                    }
                    vec![("ok", Value::Bool(true)), ("booted", Value::Bool(true))]
                }
                Err(e) => vec![("err", Value::Str(e))],
            },
            "release" => {
                if leased {
                    leased = false;
                    lease_removed(&daemon);
                }
                vec![("ok", Value::Bool(true))]
            }
            "installed-apps" => match installed_apps(&daemon) {
                Ok(pkgs) => vec![("ok", Value::Bool(true)), ("packages", Value::List(pkgs))],
                Err(e) => vec![("err", Value::Str(e))],
            },
            "shutdown" => {
                daemon.engine.stop();
                vec![("ok", Value::Bool(true))]
            }
            other => vec![("err", Value::Str(format!("unknown req: {other}")))],
        };
        if reply(&mut conn, &result).is_err() {
            break;
        }
    }

    if leased {
        lease_removed(&daemon);
    }
}

fn reply(conn: &mut UnixStream, fields: &[(&str, Value)]) -> std::io::Result<()> {
    let mut obj = BTreeMap::new();
    for (k, v) in fields {
        obj.insert((*k).to_string(), v.clone());
    }
    conn.write_all(json::encode(&obj).as_bytes())?;
    conn.write_all(b"\n")
}

/// Boot if not booted. Serialized by a lock so concurrent clients don't both
/// launch an emulator; the loser of the race finds it booted and returns.
fn ensure_booted(daemon: &Daemon) -> Result<(), String> {
    static BOOT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = BOOT_LOCK.lock().unwrap();
    if daemon.engine.emulator_running() {
        return Ok(()); // already up (possibly user-booted — not ours to stop)
    }
    let log = state_dir().join("emulator.log");
    eprintln!("androlon-runtimed: booting runtime (headless)…");
    daemon
        .engine
        .boot_and_wait_opts(
            &Avd::desktop(),
            BootProfile::Consumer, // Quick Boot: fastest resume for app launches
            &log,
            Duration::from_secs(240),
            true, // headless — the suite shows Android only through panes
        )
        .map_err(|e| e.to_string())?;
    daemon.booted_by_us.store(true, Ordering::SeqCst);
    eprintln!("androlon-runtimed: runtime ready");
    Ok(())
}

fn installed_apps(daemon: &Daemon) -> Result<Vec<String>, String> {
    let adb = androlon_core::AdbService::new(&daemon.engine.config);
    let out = adb
        .shell(&["pm", "list", "packages", "-3"])
        .map_err(|e| e.to_string())?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("package:"))
        .map(|s| s.trim().to_string())
        .collect())
}

fn lease_added(daemon: &Arc<Daemon>) {
    let mut leases = daemon.leases.lock().unwrap();
    *leases += 1;
    daemon.generation.fetch_add(1, Ordering::SeqCst);
    eprintln!("androlon-runtimed: leases = {leases}");
}

fn lease_removed(daemon: &Arc<Daemon>) {
    let count = {
        let mut leases = daemon.leases.lock().unwrap();
        *leases = leases.saturating_sub(1);
        *leases
    };
    let expected = daemon.generation.fetch_add(1, Ordering::SeqCst) + 1;
    eprintln!("androlon-runtimed: leases = {count}");
    if count > 0 {
        return;
    }
    if !daemon.booted_by_us.load(Ordering::SeqCst) {
        return; // user-booted runtime: leave it alone
    }
    // Last client left: stop the runtime after a linger, unless anyone takes
    // a new lease (generation moves) in the meantime.
    let linger = std::env::var("ANDROLON_LINGER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    eprintln!("androlon-runtimed: idle — stopping runtime in {linger}s");
    let daemon = Arc::clone(daemon);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(linger));
        if daemon.generation.load(Ordering::SeqCst) == expected {
            eprintln!("androlon-runtimed: idle linger elapsed — stopping runtime");
            daemon.engine.stop();
        }
    });
}
