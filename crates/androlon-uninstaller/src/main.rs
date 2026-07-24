//! Androlon Uninstaller — removes an appified Android app *completely*.
//!
//! Dragging a generated `.app` to the Trash only removes the Mac-side shell:
//! the APK and everything it stored (saves, logins, caches) stay in the
//! Android runtime indefinitely, and macOS offers no uninstall hook that
//! could clean them up. This app is the counterpart to the Installer: it
//! removes both halves, and says plainly what it is about to remove first.
//!
//! Destructive by nature, so the confirm is an `NSAlert` in warning style
//! with Cancel as the default — the safe choice is the one you get by
//! pressing Return.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("androlon-uninstaller: native shell is macOS-only for now");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}

#[cfg(target_os = "macos")]
mod mac {
    use androlon_core::SdkConfig;
    use objc2::rc::Retained;
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{
        NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy, NSImage,
        NSModalResponse, NSOpenPanel,
    };
    use objc2_foundation::{NSString, NSURL};
    use std::path::{Path, PathBuf};

    const FIRST_BUTTON: NSModalResponse = 1000;
    const OK: NSModalResponse = 1;

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        let Some(bundle) = target_bundle(mtm) else { return };
        let Some((label, package)) = read_identity(&bundle) else {
            alert(
                mtm,
                NSAlertStyle::Warning,
                "Not an Androlon app",
                &format!(
                    "{} wasn't created by Androlon, so there's no Android app behind it to \
                     remove. You can move it to the Trash yourself.",
                    bundle.display()
                ),
                &["OK"],
                None,
            );
            return;
        };

        let cfg = SdkConfig::from_env();
        let icon = bundle_icon(&bundle);

        // Ask the runtime what this app is actually using, so the confirm can
        // state a real number instead of a vague warning.
        let lease = androlon_ipc::RuntimeLease::acquire().ok();
        let adb = androlon_core::AdbService::new(&cfg);
        let installed = adb
            .shell(&["pm", "list", "packages", &package])
            .map(|out| out.contains(&package))
            .unwrap_or(false);
        let data = installed.then(|| data_size(&adb, &package)).flatten();

        let detail = format!(
            "This will permanently remove:\n\n\
             •  {}\n\
             •  the Android app “{}”{}\n\n\
             Anything the app saved — progress, logins, downloads — is removed with it. \
             This cannot be undone.",
            bundle.display(),
            package,
            match (&installed, &data) {
                (false, _) => " (not currently installed in the runtime)".to_string(),
                (true, Some(size)) => format!(" and its data ({size})"),
                (true, None) => " and its data".to_string(),
            },
        );

        // Cancel first = Cancel is the Return-key default. Destructive actions
        // should not be the thing an impatient keypress does.
        let choice = alert(
            mtm,
            NSAlertStyle::Warning,
            &format!("Completely uninstall “{label}”?"),
            &detail,
            &["Cancel", "Uninstall"],
            icon.as_deref(),
        );
        if choice == FIRST_BUTTON {
            return; // Cancel
        }

        let mut problems = Vec::new();
        if installed {
            match adb.adb(&["uninstall", &package]) {
                Ok(out) if out.ok() => {}
                Ok(out) => problems.push(format!("Android app: {}", out.trimmed())),
                Err(e) => problems.push(format!("Android app: {e}")),
            }
        }
        if let Err(e) = std::fs::remove_dir_all(&bundle) {
            problems.push(format!("Mac app: {e}"));
        }
        // Drop it from Launch Services too, or Spotlight keeps offering it.
        let _ = std::process::Command::new(LSREGISTER).arg("-u").arg(&bundle).output();
        drop(lease);

        if problems.is_empty() {
            alert(
                mtm,
                NSAlertStyle::Informational,
                &format!("“{label}” was removed"),
                "The Mac app and the Android app it ran, along with its data, are gone.",
                &["Done"],
                None,
            );
        } else {
            alert(
                mtm,
                NSAlertStyle::Warning,
                &format!("“{label}” was only partly removed"),
                &format!("These parts could not be removed:\n\n{}", problems.join("\n")),
                &["OK"],
                None,
            );
        }
    }

    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

    /// The `.app` to remove: argv (CLI / Library), else a picker limited to
    /// the folder generated apps land in.
    fn target_bundle(mtm: MainThreadMarker) -> Option<PathBuf> {
        if let Some(path) = std::env::args().skip(1).find(|a| a.ends_with(".app")) {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        unsafe {
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false); // a bundle reads as a file
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&NSString::from_str("Choose the Android app to uninstall")));
            panel.setPrompt(Some(&NSString::from_str("Choose")));
            let start = std::env::var("HOME").ok()? + "/Applications";
            panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(&start))));
            NSApplication::sharedApplication(mtm).activate();
            if panel.runModal() != OK {
                return None;
            }
            let url = panel.URL()?;
            url.path().map(|p| PathBuf::from(p.to_string()))
        }
    }

    /// (display name, android package) from a bundle Androlon generated.
    /// `ANDROLON_APP` in `LSEnvironment` is what marks it as ours.
    fn read_identity(bundle: &Path) -> Option<(String, String)> {
        let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?;
        let package = plist_value_after(&plist, "ANDROLON_APP")?;
        let label = plist_value_after(&plist, "CFBundleDisplayName")
            .or_else(|| plist_value_after(&plist, "CFBundleName"))
            .unwrap_or_else(|| package.clone());
        Some((label, package))
    }

    /// The `<string>` following a given `<key>` in a plist.
    fn plist_value_after(plist: &str, key: &str) -> Option<String> {
        let at = plist.find(&format!("<key>{key}</key>"))?;
        let rest = &plist[at..];
        let start = rest.find("<string>")? + "<string>".len();
        let end = rest[start..].find("</string>")?;
        Some(rest[start..start + end].trim().to_string())
    }

    /// Human-readable size of everything the package has stored.
    fn data_size(adb: &androlon_core::AdbService, package: &str) -> Option<String> {
        let out = adb
            .shell(&["du", "-sh", &format!("/data/data/{package}")])
            .ok()?;
        out.split_whitespace().next().map(|s| s.to_string()).filter(|s| !s.is_empty())
    }

    fn bundle_icon(bundle: &Path) -> Option<Retained<NSImage>> {
        let icns = bundle.join("Contents/Resources/app.icns");
        if !icns.exists() {
            return None;
        }
        unsafe {
            let s = NSString::from_str(&icns.display().to_string());
            NSImage::initWithContentsOfFile(NSImage::alloc(), &s)
        }
    }

    fn alert(
        mtm: MainThreadMarker,
        style: NSAlertStyle,
        message: &str,
        detail: &str,
        buttons: &[&str],
        icon: Option<&NSImage>,
    ) -> NSModalResponse {
        unsafe {
            let alert = NSAlert::new(mtm);
            alert.setAlertStyle(style);
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
}
