//! Androlon Hub — the suite's management console: runtime status, engine
//! actions, and the library of installed apps.
//!
//! It deliberately does not stream, present, or run anything itself. Opening
//! an app spawns `androlon-player`; installing hands off to the Installer;
//! uninstalling hands off to the Uninstaller. Each is its own process with
//! its own window and lifecycle, so the Hub stays a small console that a
//! native shell can replace wholesale.

mod ui;

use androlon_core::SdkConfig;
use dear_imgui_rs::Context;
use dear_imgui_sdl3::{sdl3_poll_event_ll, Sdl3RendererBackend};
use sdl3::event::{Event, WindowEvent};
use sdl3::pixels::Color;
use ui::{AppState, LibraryRequest};

fn main() {
    let cfg = SdkConfig::from_env();

    // CI / no-display check: prove the engine is reachable and exit.
    if std::env::args().any(|a| a == "--headless") {
        let report = androlon_core::EmulatorService::new(cfg).doctor();
        println!("✓ headless: engine reachable — API {} · {}", report.api, report.system_image);
        println!("  SDK provisioned: {}", report.tools.iter().all(|t| t.present));
        return;
    }

    let sdl = sdl3::init().expect("SDL_Init");
    let video = sdl.video().expect("SDL video subsystem");
    let window = video
        .window("Androlon", 1000, 680)
        .position_centered()
        .resizable()
        .build()
        .expect("create management window");
    let mut canvas = window.into_canvas();

    let mut imgui = Context::create();
    let _ = imgui.set_ini_filename::<std::path::PathBuf>(None);
    let mut backend = Sdl3RendererBackend::init(&mut imgui, canvas.window(), &canvas)
        .expect("init ImGui SDLRenderer3 backend");

    let mut app = AppState::new(cfg.clone());
    let (log_tx, log_rx) = std::sync::mpsc::channel::<String>();
    let (lib_tx, lib_rx) = std::sync::mpsc::channel::<Option<Vec<String>>>();

    'main: loop {
        while let Some(raw) = sdl3_poll_event_ll() {
            backend.process_event(&mut imgui, &raw);
            match Event::from_ll(raw) {
                Event::Quit { .. }
                | Event::Window { win_event: WindowEvent::CloseRequested, .. } => break 'main,
                // An APK dropped on the console: hand it to the Installer,
                // which owns that experience natively.
                Event::DropFile { ref filename, .. }
                    if filename.to_lowercase().ends_with(".apk") =>
                {
                    match spawn_sibling("androlon-installer", &[filename]) {
                        Ok(()) => app.log_line(format!("› installing {filename}…")),
                        Err(e) => app.log_line(format!("✗ installer: {e}")),
                    }
                }
                _ => {}
            }
        }

        app.poll();
        while let Ok(line) = log_rx.try_recv() {
            app.log_line(line);
        }
        while let Ok(result) = lib_rx.try_recv() {
            match result {
                Some(list) => app.set_library(list),
                None => app.library_failed(),
            }
        }
        if let Some(req) = app.take_library_request() {
            library_action(req, cfg.clone(), log_tx.clone(), lib_tx.clone());
        }

        backend.new_frame(&mut imgui);
        let ui = imgui.frame();
        app.draw(ui);
        let draw_data = imgui.render();
        canvas.set_draw_color(Color::RGB(18, 18, 22));
        canvas.clear();
        backend.render(draw_data, &canvas);
        canvas.present();

        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

/// Run one of the suite's other binaries, which sit beside this one.
fn spawn_sibling(name: &str, args: &[&str]) -> std::io::Result<()> {
    let exe = std::env::current_exe()?.with_file_name(name);
    std::process::Command::new(exe).args(args).spawn().map(|_| ())
}

/// Library row actions, off the UI thread. The listing comes from the daemon
/// (the runtime's own view); everything else is delegated to the suite app
/// that owns it.
fn library_action(
    req: LibraryRequest,
    cfg: SdkConfig,
    log_tx: std::sync::mpsc::Sender<String>,
    lib_tx: std::sync::mpsc::Sender<Option<Vec<String>>>,
) {
    std::thread::spawn(move || {
        let log = |s: String| {
            let _ = log_tx.send(s);
        };
        let refresh = |lib_tx: &std::sync::mpsc::Sender<Option<Vec<String>>>| {
            match androlon_ipc::request("installed-apps", &[]) {
                Ok(resp) => {
                    let list = resp.list("packages").map(<[String]>::to_vec).unwrap_or_default();
                    let _ = lib_tx.send(Some(list));
                }
                Err(e) => {
                    let _ = log_tx.send(format!("✗ library: {e}"));
                    let _ = lib_tx.send(None);
                }
            }
        };

        match req {
            LibraryRequest::Refresh => refresh(&lib_tx),
            LibraryRequest::Launch(pkg) => {
                match spawn_sibling("androlon-player", &["--app", &pkg]) {
                    Ok(()) => log(format!("✓ opening {pkg}")),
                    Err(e) => log(format!("✗ open {pkg}: {e}")),
                }
                let _ = lib_tx.send(None);
            }
            LibraryRequest::Uninstall(pkg) => {
                // Hand the whole removal to the Uninstaller when the app has
                // a Mac bundle: it confirms, then removes both halves. A bare
                // package uninstall is only right when there's no bundle.
                match androlon_core::appify::find_bundle_for_package(&pkg) {
                    Some(bundle) => {
                        let bundle = bundle.display().to_string();
                        match spawn_sibling("androlon-uninstaller", &[&bundle]) {
                            Ok(()) => log(format!("› uninstalling {pkg} — confirm in the window")),
                            Err(e) => log(format!("✗ uninstaller: {e}")),
                        }
                        // Refreshes on the next Refresh: the Uninstaller is a
                        // separate process and the user may still cancel.
                        let _ = lib_tx.send(None);
                    }
                    None => {
                        let adb = androlon_core::AdbService::new(&cfg);
                        match adb.adb(&["uninstall", &pkg]) {
                            Ok(out) if out.ok() => log(format!("✓ uninstalled {pkg}")),
                            Ok(out) => log(format!("✗ uninstall {pkg}: {}", out.trimmed())),
                            Err(e) => log(format!("✗ uninstall {pkg}: {e}")),
                        }
                        refresh(&lib_tx);
                    }
                }
            }
            LibraryRequest::MakeApp(pkg) => {
                // The APK already lives on the device — pull it back so the
                // normal appify path can read its label and icon.
                let adb = androlon_core::AdbService::new(&cfg);
                let path = adb.shell(&["pm", "path", &pkg]).ok().and_then(|out| {
                    out.lines()
                        .find_map(|l| l.strip_prefix("package:"))
                        .map(|p| p.trim().to_string())
                });
                let Some(device_apk) = path else {
                    log(format!("✗ {pkg}: could not locate its APK on the device"));
                    let _ = lib_tx.send(None);
                    return;
                };
                let local = std::env::temp_dir().join(format!("{pkg}.apk"));
                let local_str = local.display().to_string();
                if let Err(e) = adb.adb(&["pull", &device_apk, &local_str]) {
                    log(format!("✗ pull {pkg}: {e}"));
                    let _ = lib_tx.send(None);
                    return;
                }
                let out_dir = std::env::var("HOME")
                    .map(|h| std::path::PathBuf::from(h).join("Applications"))
                    .unwrap_or_else(|_| ".".into());
                let _ = std::fs::create_dir_all(&out_dir);
                let player = std::env::current_exe()
                    .map(|p| p.with_file_name("androlon-player"))
                    .unwrap_or_else(|_| "androlon-player".into());
                match androlon_core::appify::appify(&cfg, &local, &out_dir, &player) {
                    Ok(done) => log(format!("✓ {} → {}", done.label, done.bundle.display())),
                    Err(e) => log(format!("✗ make app {pkg}: {e}")),
                }
                let _ = std::fs::remove_file(&local);
                let _ = lib_tx.send(None);
            }
        }
    });
}
