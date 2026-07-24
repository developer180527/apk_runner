//! Androlon Installer — the suite's `.apk` handler, with native macOS chrome.
//!
//! Double-clicking an APK lands here. We inspect it (read-only), show a
//! standard `NSAlert` carrying the app's real icon and details, and only on
//! confirmation install it into the runtime and generate its `.app` bundle.
//!
//! `NSAlert` is deliberate: a confirm-this-action panel is the macOS idiom
//! for exactly this, and using the system's own alert gives correct button
//! order, keyboard handling, dark mode, and accessibility for free — none of
//! which a hand-drawn window would get right.
//!
//! Only this shell is macOS-specific; everything it calls lives in
//! `androlon-core`, so a GTK/WinUI installer is a sibling, not a rewrite.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("androlon-installer: native shell is macOS-only for now");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}

#[cfg(target_os = "macos")]
mod opendoc;

#[cfg(target_os = "macos")]
mod wizard;

#[cfg(target_os = "macos")]
mod mac {
    use crate::wizard::{self, Step, Wizard};
    use androlon_core::{appify, SdkConfig};
    use objc2::rc::Retained;
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{
        NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSImage, NSOpenPanel,
        NSModalResponse,
    };
    use objc2_foundation::{NSString, NSURL};
    use std::path::{Path, PathBuf};

    // NSAlert returns 1000 for the first button, 1001 for the second, …
    const FIRST_BUTTON: NSModalResponse = 1000;
    const SECOND_BUTTON: NSModalResponse = 1001;
    const OK: NSModalResponse = 1;

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("installer must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        // A foreground app: our panels need to come to front and take focus.
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        // Finder sends the double-clicked APK as an Apple Event, not argv —
        // pump the loop once to collect it, then fall back to argv (CLI use)
        // and finally to a file picker.
        let apk = match crate::opendoc::opened_document(mtm)
            .or_else(apk_from_args)
            .or_else(|| choose_apk(mtm))
        {
            Some(apk) => apk,
            None => return, // nothing chosen — quit quietly
        };

        let cfg = SdkConfig::from_env();
        let info = match appify::inspect(&cfg, &apk) {
            Ok(info) => info,
            Err(e) => {
                alert(mtm, "Can't read this APK", &e.to_string(), &["OK"], None);
                return;
            }
        };

        // The app's own icon, extracted before anything is installed.
        let icon_file = std::env::temp_dir().join("androlon-installer-icon.png");
        let icon = match appify::extract_icon(&cfg, &apk, &icon_file) {
            Ok(true) => load_image(&icon_file),
            _ => None,
        };

        let mut bundle: Option<PathBuf> = None;
        let mut dest = default_destination();
        let wizard = Wizard::new(mtm, &info, icon.as_deref());
        wizard.show_step(Step::Introduction, &info, &dest);
        wizard.show();

        // Straight-line wizard: each modal turn returns the button pressed.
        let mut step = Step::Introduction;
        loop {
            match wizard.next_action() {
                wizard::TAG_CONTINUE => {
                    step = match step {
                        Step::Introduction => Step::Destination,
                        Step::Destination => Step::InstallType,
                        Step::InstallType => Step::Installing,
                        Step::Summary => break,
                        Step::Installing => Step::Installing,
                    };
                    wizard.show_step(step, &info, &dest);

                    if step == Step::Installing {
                        let (cfg2, apk2, dest2, player) =
                            (cfg.clone(), apk.clone(), dest.clone(), player_binary());
                        let outcome = wizard.run_with_progress(move || {
                            appify::appify(&cfg2, &apk2, &dest2, &player)
                                .map_err(|e| e.to_string())
                        });
                        match outcome {
                            Ok(done) => {
                                if !done.installed {
                                    alert(
                                        mtm,
                                        "The app was created, but not installed yet",
                                        "The Android runtime wasn't reachable, so the APK \
                                         couldn't be installed into it. Opening the app will \
                                         start the runtime; if it doesn't launch, install again.",
                                        &["OK"],
                                        icon.as_deref(),
                                    );
                                }
                                bundle = Some(done.bundle);
                                step = Step::Summary;
                                wizard.show_step(step, &info, &dest);
                            }
                            Err(e) => {
                                alert(mtm, "Installation failed", &e, &["OK"], None);
                                return;
                            }
                        }
                    }
                }
                wizard::TAG_BACK => {
                    step = match step {
                        Step::Destination => Step::Introduction,
                        Step::InstallType => Step::Destination,
                        other => other,
                    };
                    wizard.show_step(step, &info, &dest);
                }
                wizard::TAG_CHOOSE => {
                    if let Some(picked) = wizard::choose_folder(mtm, &dest) {
                        dest = picked;
                    }
                    wizard.show_step(step, &info, &dest);
                }
                _ => return, // Cancel / window closed
            }
        }

        wizard.close();
        if let Some(bundle) = bundle {
            let _ = std::process::Command::new("open").arg(&bundle).status();
        }
    }

    /// The APK to install: `open`-style argv, skipping the `-psn_…` argument
    /// Launch Services adds when an app is opened from Finder.
    fn apk_from_args() -> Option<PathBuf> {
        std::env::args()
            .skip(1)
            .find(|a| a.to_lowercase().ends_with(".apk"))
            .map(PathBuf::from)
            .filter(|p| p.exists())
    }

    fn default_destination() -> PathBuf {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join("Applications"))
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    /// The shared player binary bundles execute. Beside us in the suite.
    fn player_binary() -> PathBuf {
        std::env::var("ANDROLON_PLAYER").map(PathBuf::from).unwrap_or_else(|_| {
            std::env::current_exe()
                .map(|p| p.with_file_name("androlon-player"))
                .unwrap_or_else(|_| PathBuf::from("androlon-player"))
        })
    }

    /// Standard alert with optional custom icon. Returns the button response.
    fn alert(
        mtm: MainThreadMarker,
        message: &str,
        detail: &str,
        buttons: &[&str],
        icon: Option<&NSImage>,
    ) -> NSModalResponse {
        unsafe {
            let alert = NSAlert::new(mtm);
            alert.setAlertStyle(NSAlertStyle::Informational);
            alert.setMessageText(&NSString::from_str(message));
            alert.setInformativeText(&NSString::from_str(detail));
            for title in buttons {
                alert.addButtonWithTitle(&NSString::from_str(title));
            }
            if let Some(icon) = icon {
                alert.setIcon(Some(icon));
            }
            NSApplication::sharedApplication(mtm).activate();
            alert.runModal()
        }
    }

    fn load_image(path: &Path) -> Option<Retained<NSImage>> {
        unsafe {
            let s = NSString::from_str(&path.display().to_string());
            NSImage::initWithContentsOfFile(NSImage::alloc(), &s)
        }
    }

    /// System file picker for an APK (when launched without one).
    fn choose_apk(mtm: MainThreadMarker) -> Option<PathBuf> {
        unsafe {
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false);
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&NSString::from_str("Choose an APK to install")));
            panel.setPrompt(Some(&NSString::from_str("Choose")));
            NSApplication::sharedApplication(mtm).activate();
            if panel.runModal() != OK {
                return None;
            }
            url_path(&panel.URL()?)
        }
    }

    /// System folder picker for the destination.
    fn choose_folder(mtm: MainThreadMarker, start: &Path) -> Option<PathBuf> {
        unsafe {
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(false);
            panel.setCanChooseDirectories(true);
            panel.setCanCreateDirectories(true);
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&NSString::from_str("Where should the app be created?")));
            panel.setPrompt(Some(&NSString::from_str("Use Folder")));
            let start = NSString::from_str(&start.display().to_string());
            panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&start)));
            NSApplication::sharedApplication(mtm).activate();
            if panel.runModal() != OK {
                return None;
            }
            url_path(&panel.URL()?)
        }
    }

    fn url_path(url: &Retained<NSURL>) -> Option<PathBuf> {
        unsafe { url.path().map(|p| PathBuf::from(p.to_string())) }
    }
}
