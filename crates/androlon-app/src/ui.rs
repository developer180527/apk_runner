//! Management UI (Dear ImGui): engine health, actions, and a live log. Long
//! engine actions (setup, boot) run on a worker thread and stream progress back
//! over an mpsc channel so the UI never blocks.

use androlon_core::backend::AndroidBackend;
use androlon_core::{AdbService, Avd, BootProfile, DoctorReport, EmulatorService, SdkConfig};
use dear_imgui_rs::{Condition, Ui};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

/// A long-running engine action requested from the UI.
#[derive(Clone, Copy)]
enum Task {
    Refresh,
    Setup,
    CreateAvds,
    BootDesktop,
    RootAdbd,
    Stop,
}

impl Task {
    fn verb(self) -> &'static str {
        match self {
            Task::Refresh => "refresh status",
            Task::Setup => "install SDK",
            Task::CreateAvds => "create AVDs",
            Task::BootDesktop => "boot desktop AVD",
            Task::RootAdbd => "enable adbd root",
            Task::Stop => "stop emulator",
        }
    }
}

/// Messages from the worker thread back to the UI.
enum Msg {
    Log(String),
    Report(DoctorReport),
    Done,
}

/// An APK waiting for the user's go-ahead in the installer dialog.
pub struct PendingInstall {
    pub apk: String,
    pub info: androlon_core::appify::ApkInfo,
    /// Destination folder for the generated .app (editable in the dialog).
    pub dest: String,
}

pub struct AppState {
    cfg: SdkConfig,
    report: DoctorReport,
    log: Vec<String>,
    busy: bool,
    profile: BootProfile,
    open_video: bool,
    open_live: bool,
    rx: Option<Receiver<Msg>>,
    pending_install: Option<PendingInstall>,
    confirmed_install: Option<PendingInstall>,
}

impl AppState {
    pub fn new(cfg: SdkConfig) -> Self {
        let report = EmulatorService::new(cfg.clone()).doctor();
        let mut log = Vec::new();
        log.push(format!("Ready · Android {} · {}", report.api, report.system_image));
        log.push("Tip: run `Setup` once (~1–2 GB) before booting.".into());
        AppState {
            cfg,
            report,
            log,
            busy: false,
            profile: BootProfile::Developer,
            open_video: false,
            open_live: false,
            rx: None,
            pending_install: None,
            confirmed_install: None,
        }
    }

    /// Queue the installer dialog for a dropped/double-clicked APK.
    pub fn request_install(&mut self, apk: String, info: androlon_core::appify::ApkInfo) {
        let dest = std::env::var("HOME")
            .map(|h| format!("{h}/Applications"))
            .unwrap_or_else(|_| ".".into());
        self.push_log(format!("› install requested: {} ({})", info.label, info.package));
        self.pending_install = Some(PendingInstall { apk, info, dest });
    }

    /// Consume a confirmed install (the app loop runs it).
    pub fn take_install(&mut self) -> Option<PendingInstall> {
        self.confirmed_install.take()
    }

    /// Consume a pending "open app surface window" (demo) request.
    pub fn take_open_video(&mut self) -> bool {
        std::mem::take(&mut self.open_video)
    }

    /// Consume a pending "open LIVE surface" (scrcpy) request.
    pub fn take_open_live(&mut self) -> bool {
        std::mem::take(&mut self.open_live)
    }

    /// Append a line to the log (used by the app loop to report window events).
    pub fn log_line(&mut self, line: impl Into<String>) {
        self.push_log(line.into());
    }

    /// Drain worker messages (called once per frame).
    pub fn poll(&mut self) {
        let mut done = false;
        let mut msgs = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                Msg::Log(line) => self.push_log(line),
                Msg::Report(r) => self.report = r,
                Msg::Done => done = true,
            }
        }
        if done {
            self.busy = false;
            self.rx = None;
        }
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        // Keep the log bounded.
        if self.log.len() > 200 {
            let drop = self.log.len() - 200;
            self.log.drain(0..drop);
        }
    }

    fn spawn(&mut self, task: Task) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.push_log(format!("▶ {}…", task.verb()));
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        let cfg = self.cfg.clone();
        let profile = self.profile;
        thread::spawn(move || run_task(cfg, task, profile, tx));
    }

    pub fn draw(&mut self, ui: &Ui) {
        let mut requested: Vec<Task> = Vec::new();
        // Borrow only the fields the closures need, so `self` stays free to
        // mutate afterwards when processing requested tasks.
        let report = &self.report;
        let log = &self.log;
        let busy = self.busy;
        let profile = self.profile;
        let mut toggle_profile = false;
        let mut open_video = false;
        let mut open_live = false;

        ui.window("Androlon — Control")
            .size([460.0, 640.0], Condition::FirstUseEver)
            .position([16.0, 16.0], Condition::FirstUseEver)
            .build(|| {
                ui.text("Parallels-class Android · Apple Silicon");
                ui.text_colored([0.5, 0.7, 1.0, 1.0], "Android 17 (API 37) · arm64 · gfxstream");
                ui.separator();

                ui.text("Engine status");
                for t in &report.tools {
                    if t.present {
                        ui.text_colored([0.4, 0.9, 0.5, 1.0], format!("  OK  {}", t.name));
                    } else {
                        ui.text_colored([0.95, 0.5, 0.4, 1.0], format!("  --  {} (needs setup)", t.name));
                    }
                }
                ui.spacing();
                ui.bullet_text(format!(
                    "SDK: {}",
                    if report.tools.iter().all(|t| t.present) { "provisioned" } else { "incomplete" }
                ));
                ui.bullet_text(format!(
                    "AVDs: {}",
                    if report.avds.is_empty() { "none".into() } else { report.avds.join(", ") }
                ));
                ui.bullet_text(format!("Emulator: {}", if report.emulator_running { "running" } else { "stopped" }));
                ui.bullet_text(format!("Rootable image: {}", if report.is_rootable { "yes" } else { "no" }));
                ui.bullet_text(format!("Root: {}", report.root_status.label()));

                ui.separator();
                ui.text("Boot profile");
                let (col, desc) = match profile {
                    BootProfile::Developer => ([0.6, 0.8, 1.0, 1.0], "developer · cold boot, root-safe, deterministic"),
                    BootProfile::Consumer => ([0.6, 1.0, 0.7, 1.0], "consumer · Quick Boot, fast resume"),
                };
                ui.text_colored(col, format!("  {desc}"));
                if !busy && ui.button("Toggle profile") {
                    toggle_profile = true;
                }

                ui.separator();
                ui.text("Actions");
                if busy {
                    ui.text_colored([1.0, 0.8, 0.3, 1.0], "  working…");
                } else {
                    if ui.button("Setup SDK") {
                        requested.push(Task::Setup);
                    }
                    ui.same_line();
                    if ui.button("Create AVDs") {
                        requested.push(Task::CreateAvds);
                    }
                    if ui.button("Boot desktop") {
                        requested.push(Task::BootDesktop);
                    }
                    ui.same_line();
                    if ui.button("Root (adbd)") {
                        requested.push(Task::RootAdbd);
                    }
                    if ui.button("Stop") {
                        requested.push(Task::Stop);
                    }
                    ui.same_line();
                    if ui.button("Refresh") {
                        requested.push(Task::Refresh);
                    }
                }

                ui.separator();
                ui.text("App surface");
                if ui.button("Open surface (demo)") {
                    open_video = true;
                }
                ui.same_line();
                if ui.button("Open surface (LIVE scrcpy)") {
                    open_live = true;
                }
                ui.text_colored([0.7, 0.7, 0.75, 1.0], "  LIVE needs a booted device + vendor/scrcpy-server");
            });

        ui.window("Log")
            .size([460.0, 260.0], Condition::FirstUseEver)
            .position([492.0, 396.0], Condition::FirstUseEver)
            .build(|| {
                for line in log.iter().rev().take(14).rev() {
                    ui.text(line);
                }
            });

        // Installer dialog: shown while an APK awaits confirmation. Details
        // come from `aapt2 dump badging`; nothing is written until Install.
        let mut install_clicked = false;
        let mut cancel_clicked = false;
        if let Some(pending) = &mut self.pending_install {
            let info = &pending.info;
            let title = format!("Install “{}”?###installer", info.label);
            ui.window(title)
                .size([420.0, 260.0], Condition::FirstUseEver)
                .position([260.0, 180.0], Condition::FirstUseEver)
                .build(|| {
                    ui.text_colored([0.6, 0.85, 1.0, 1.0], &info.label);
                    ui.separator();
                    ui.bullet_text(format!("Package   {}", info.package));
                    ui.bullet_text(format!("Version   {}", info.version));
                    ui.bullet_text(format!("Min SDK   API {}", info.min_sdk));
                    ui.bullet_text(format!(
                        "Size      {:.1} MB",
                        info.size_bytes as f64 / (1024.0 * 1024.0)
                    ));
                    ui.spacing();
                    ui.text("Create the Mac app in:");
                    ui.input_text("##dest", &mut pending.dest).build();
                    ui.text_colored(
                        [0.7, 0.7, 0.75, 1.0],
                        "Installs into the Android runtime and creates a native app.",
                    );
                    ui.spacing();
                    if ui.button("Install") {
                        install_clicked = true;
                    }
                    ui.same_line();
                    if ui.button("Cancel") {
                        cancel_clicked = true;
                    }
                });
        }
        if install_clicked {
            self.confirmed_install = self.pending_install.take();
        } else if cancel_clicked {
            if let Some(p) = self.pending_install.take() {
                self.push_log(format!("✗ install of {} cancelled", p.info.label));
            }
        }

        if toggle_profile {
            self.profile = match self.profile {
                BootProfile::Developer => BootProfile::Consumer,
                BootProfile::Consumer => BootProfile::Developer,
            };
            self.push_log(format!("profile → {}", self.profile.label()));
        }
        if open_video {
            self.open_video = true;
        }
        if open_live {
            self.open_live = true;
        }
        for task in requested {
            self.spawn(task);
        }
    }
}

/// Runs on the worker thread. Builds a fresh engine from the cloned config and
/// executes one action, reporting progress + a refreshed DoctorReport.
fn run_task(cfg: SdkConfig, task: Task, profile: BootProfile, tx: Sender<Msg>) {
    let engine = EmulatorService::new(cfg.clone());
    let send = |m: Msg| {
        let _ = tx.send(m);
    };

    match task {
        Task::Refresh => {}
        Task::Setup => match engine.install_packages() {
            Ok(_) => send(Msg::Log("✓ SDK packages installed".into())),
            Err(e) => send(Msg::Log(format!("✗ {e}"))),
        },
        Task::CreateAvds => {
            let r = engine
                .create_avd(&Avd::phone())
                .and_then(|_| engine.create_avd(&Avd::desktop()));
            match r {
                Ok(_) => send(Msg::Log("✓ phone + desktop AVDs ready".into())),
                Err(e) => send(Msg::Log(format!("✗ {e}"))),
            }
        }
        Task::BootDesktop => {
            let log = cfg
                .sdk_root
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join(".emulator.log");
            match engine.boot(&Avd::desktop(), profile, &log) {
                Ok(_) => send(Msg::Log(format!("✓ desktop AVD booted ({} profile)", profile.label()))),
                Err(e) => send(Msg::Log(format!("✗ {e}"))),
            }
        }
        Task::RootAdbd => {
            let adb = AdbService::new(&cfg);
            match adb.enable_adbd_root() {
                Ok(_) => send(Msg::Log(format!("✓ root: {}", adb.root_status().label()))),
                Err(e) => send(Msg::Log(format!("✗ {e}"))),
            }
        }
        Task::Stop => {
            engine.stop();
            send(Msg::Log("✓ stop signal sent".into()));
        }
    }

    send(Msg::Report(engine.doctor()));
    send(Msg::Done);
}
