//! CLI wrappers around `androlon_core::appify` plus `bundle-host`, which
//! packages Androlon itself as `Androlon.app` — the bundle whose
//! CFBundleDocumentTypes make macOS route double-clicked `.apk` files to us.

use androlon_core::{EngineError, SdkConfig};
use std::path::PathBuf;
use std::process::Command;

pub fn cmd_appify(cfg: &SdkConfig, args: &[String]) -> androlon_core::Result<()> {
    let Some(apk_arg) = args.get(1) else {
        return Err(EngineError::ApkNotFound(
            "usage: androlon-ctl appify <apk> [--out <dir>]".into(),
        ));
    };
    let out_dir = out_dir_arg(args).unwrap_or_else(|| PathBuf::from("."));
    let host = host_binary()?;

    let outcome = androlon_core::appify::appify(cfg, &PathBuf::from(apk_arg), &out_dir, &host)?;
    println!("› {} ({})", outcome.label, outcome.package);
    if outcome.installed {
        println!("✓ installed into the Android runtime");
    } else {
        println!("⚠ not installed now (no booted device?) — install before launch");
    }
    if !outcome.icon {
        println!("⚠ no raster icon found in the APK — bundle has a generic icon");
    }
    println!("✓ created {}", outcome.bundle.display());
    println!(
        "  double-click it (or `open '{}'`) to launch {}",
        outcome.bundle.display(),
        outcome.label
    );
    Ok(())
}

/// Generate `Androlon.app` (the host shell) and register it with Launch
/// Services as a handler for `.apk` files. After this, double-clicking an APK
/// in Finder opens Androlon, which installs + appifies + launches it.
pub fn cmd_bundle_host(cfg: &SdkConfig, args: &[String]) -> androlon_core::Result<()> {
    let out_dir = out_dir_arg(args).unwrap_or_else(|| PathBuf::from("."));
    let host = host_binary()?;

    let bundle = out_dir.join("Androlon.app");
    let macos_dir = bundle.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)
        .map_err(|e| EngineError::Launch { tool: "create bundle".into(), source: e })?;
    // The suite travels together: hub shell + player + runtime daemon live
    // side by side in the bundle, so sibling discovery works from inside it.
    let hub = host.with_file_name("androlon-app");
    std::fs::copy(&hub, macos_dir.join("Androlon"))
        .map_err(|e| EngineError::Launch { tool: "copy androlon-app".into(), source: e })?;
    for tool in ["androlon-player", "androlon-runtimed"] {
        let src = host.with_file_name(tool);
        if src.exists() {
            std::fs::copy(&src, macos_dir.join(tool))
                .map_err(|e| EngineError::Launch { tool: format!("copy {tool}"), source: e })?;
        }
    }

    let sdk_root = std::fs::canonicalize(&cfg.sdk_root).unwrap_or_else(|_| cfg.sdk_root.clone());
    let server_jar = std::env::var("ANDROLON_SCRCPY_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("vendor/scrcpy-server"));
    let server_jar = std::fs::canonicalize(&server_jar).unwrap_or(server_jar);

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Androlon</string>
    <key>CFBundleDisplayName</key>
    <string>Androlon</string>
    <key>CFBundleExecutable</key>
    <string>Androlon</string>
    <key>CFBundleIdentifier</key>
    <string>com.androlon.host</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Android Package</string>
            <key>CFBundleTypeExtensions</key>
            <array><string>apk</string></array>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>Owner</string>
        </dict>
    </array>
    <key>LSEnvironment</key>
    <dict>
        <key>APKRUN_SDK</key>
        <string>{sdk}</string>
        <key>ANDROLON_SCRCPY_SERVER</key>
        <string>{server}</string>
    </dict>
</dict>
</plist>
"#,
        sdk = sdk_root.display(),
        server = server_jar.display(),
    );
    std::fs::write(bundle.join("Contents/Info.plist"), plist)
        .map_err(|e| EngineError::Launch { tool: "write plist".into(), source: e })?;

    let _ = Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(&bundle)
        .output();

    // Tell Launch Services about the bundle (otherwise it only learns about
    // it when the user first opens it or moves it into /Applications).
    let lsregister = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = Command::new(lsregister).arg("-f").arg(&bundle).output();

    println!("✓ created {}", bundle.display());
    println!("  registered as a handler for .apk files.");
    println!("  Double-click an APK → if Finder asks, choose Androlon (or right-click →");
    println!("  Open With → Androlon). Androlon installs it, creates the app bundle, and");
    println!("  launches it.");
    Ok(())
}

fn out_dir_arg(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

/// The shared player binary shipped next to androlon-ctl.
fn host_binary() -> androlon_core::Result<PathBuf> {
    let host = std::env::current_exe()
        .map_err(|e| EngineError::Launch { tool: "locate androlon-ctl".into(), source: e })?
        .with_file_name("androlon-player");
    if !host.exists() {
        return Err(EngineError::SdkMissing(format!(
            "androlon-player binary (expected next to androlon-ctl at {})",
            host.display()
        )));
    }
    Ok(host)
}
