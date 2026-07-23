//! APK → native macOS `.app` bundle ("appify"), as a library so both the CLI
//! (`androlon-ctl appify`) and the GUI's double-click/drop install flow share
//! one implementation.
//!
//! The bundle carries its own identity (name, icon, bundle id), so the app
//! gets its own Dock icon, Cmd-Tab entry, and Launchpad/Spotlight presence.
//! Its executable is a copy of `androlon-app`; `LSEnvironment` in Info.plist
//! sets `ANDROLON_APP=<package>` (Launch Services passes no custom argv), so
//! launching it opens that app's Coherence window in single-app mode.
//!
//! Name/icon come from `aapt2 dump badging` + extracting the densest raster
//! icon from the APK zip; `sips` + `iconutil` (stock macOS) build the .icns.

use crate::error::{EngineError, Result};
use crate::{AdbService, SdkConfig};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct AppifyOutcome {
    pub label: String,
    pub package: String,
    pub bundle: PathBuf,
    /// APK installed into the runtime (false = no booted device right now).
    pub installed: bool,
    /// A real icon was extracted (false = vector-only APK, generic icon).
    pub icon: bool,
}

/// Create `<out_dir>/<Label>.app` from `apk`. `host_binary` is the
/// `androlon-app` executable to embed (callers: androlon-app passes its own
/// `current_exe`; androlon-ctl passes its sibling binary).
pub fn appify(
    cfg: &SdkConfig,
    apk: &Path,
    out_dir: &Path,
    host_binary: &Path,
) -> Result<AppifyOutcome> {
    if !apk.exists() {
        return Err(EngineError::ApkNotFound(apk.display().to_string()));
    }
    if !host_binary.exists() {
        return Err(EngineError::SdkMissing(format!(
            "androlon-app binary at {}",
            host_binary.display()
        )));
    }

    // ---- 1. Read identity from the APK ----
    let apk_str = apk.display().to_string();
    let badging = run_capture(&cfg.aapt2(), &["dump", "badging", &apk_str])?;
    let package = extract(&badging, "package: name='")
        .ok_or_else(|| EngineError::PackageUnresolved(apk.display().to_string()))?;
    let label = extract(&badging, "application-label:'")
        .or_else(|| extract(&badging, "application-label-en:'"))
        .unwrap_or_else(|| package.clone());

    // ---- 2. Install into the runtime (best-effort; needs a booted device) ----
    let installed = AdbService::new(cfg).install(apk).is_ok();

    // ---- 3. Bundle skeleton + executable ----
    let bundle = out_dir.join(format!("{label}.app"));
    let macos_dir = bundle.join("Contents/MacOS");
    let res_dir = bundle.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir).map_err(io_err("create bundle"))?;
    std::fs::create_dir_all(&res_dir).map_err(io_err("create bundle"))?;

    // Named after the app so the process — and the menu bar — carries its name.
    let exe_name = label.replace('/', "-");
    let exe_dest = macos_dir.join(&exe_name);
    std::fs::copy(host_binary, &exe_dest).map_err(io_err("copy androlon-app"))?;

    // ---- 4. Icon: densest raster icon in the APK → .icns ----
    let icon = build_icns(&badging, apk, &res_dir).unwrap_or(false);

    // ---- 5. Info.plist: identity + LSEnvironment configuration ----
    let sdk_root =
        std::fs::canonicalize(&cfg.sdk_root).unwrap_or_else(|_| cfg.sdk_root.clone());
    let server_jar = std::env::var("ANDROLON_SCRCPY_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("vendor/scrcpy-server"));
    let server_jar = std::fs::canonicalize(&server_jar).unwrap_or(server_jar);
    let icon_entry = if icon {
        "    <key>CFBundleIconFile</key>\n    <string>app</string>\n"
    } else {
        ""
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>{label}</string>
    <key>CFBundleDisplayName</key>
    <string>{label}</string>
    <key>CFBundleExecutable</key>
    <string>{exe_name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.androlon.app.{package}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
{icon_entry}    <key>LSEnvironment</key>
    <dict>
        <key>ANDROLON_APP</key>
        <string>{package}</string>
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
    std::fs::write(bundle.join("Contents/Info.plist"), plist).map_err(io_err("write plist"))?;

    // Ad-hoc sign so macOS treats the copied binary + bundle as one identity.
    let _ = Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(&bundle)
        .output();

    Ok(AppifyOutcome { label, package, bundle, installed, icon })
}

/// Pull the value after `key` up to the closing quote.
fn extract(badging: &str, key: &str) -> Option<String> {
    let start = badging.find(key)? + key.len();
    let end = badging[start..].find('\'')?;
    Some(badging[start..start + end].to_string())
}

/// Find the highest-density raster icon in the APK and convert it to
/// Resources/app.icns. Returns Ok(false) when no usable raster icon exists
/// (e.g. adaptive-XML-only icons with no fallback).
fn build_icns(badging: &str, apk: &Path, res_dir: &Path) -> std::result::Result<bool, String> {
    // aapt2 lists application-icon-<density> lines in ascending density.
    let mut icon_path: Option<String> = None;
    for line in badging.lines() {
        if line.starts_with("application-icon-") {
            if let Some(p) = extract(line, ":'") {
                if p.ends_with(".png") || p.ends_with(".webp") {
                    icon_path = Some(p); // keep the last (densest) raster icon
                }
            }
        }
    }

    let tmp = std::env::temp_dir().join(format!("androlon-icon-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let raw = tmp.join("icon-src");

    if let Some(icon_path) = icon_path {
        let extracted = Command::new("unzip")
            .args(["-p"])
            .arg(apk)
            .arg(&icon_path)
            .output()
            .map_err(|e| format!("unzip: {e}"))?;
        if extracted.stdout.is_empty() {
            return Err(format!("could not extract {icon_path}"));
        }
        std::fs::write(&raw, &extracted.stdout).map_err(|e| e.to_string())?;
    } else {
        // Adaptive-icon APKs badge only XML, and shrunk release builds
        // obfuscate resource names — so extract the APK's rasters, measure
        // them, and take the largest square one (launcher icons ship as a
        // 48..512px density ladder; the biggest square is the densest icon).
        let rasters = tmp.join("rasters");
        let _ = Command::new("unzip")
            .args(["-o", "-q"])
            .arg(apk)
            .args(["res/*.png", "res/*.webp", "-d"])
            .arg(&rasters)
            .output()
            .map_err(|e| format!("unzip: {e}"))?;
        let mut best: Option<(u32, PathBuf)> = None;
        let entries = std::fs::read_dir(rasters.join("res"))
            .map_err(|_| "no raster resources in APK".to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            let out = Command::new("sips")
                .args(["-g", "pixelWidth", "-g", "pixelHeight"])
                .arg(&path)
                .output()
                .map_err(|e| format!("sips: {e}"))?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let dim = |key: &str| -> Option<u32> {
                let line = text.lines().find(|l| l.contains(key))?;
                line.rsplit(' ').next()?.parse().ok()
            };
            if let (Some(w), Some(h)) = (dim("pixelWidth"), dim("pixelHeight")) {
                if w == h
                    && (48..=1024).contains(&w)
                    && best.as_ref().map_or(true, |(bw, _)| w > *bw)
                {
                    best = Some((w, path));
                }
            }
        }
        let Some((_, path)) = best else {
            return Ok(false); // vector-only app; no raster to use
        };
        std::fs::copy(&path, &raw).map_err(|e| e.to_string())?;
    }

    // sips reads png/webp; write the standard iconset sizes, then iconutil.
    let iconset = tmp.join("app.iconset");
    std::fs::create_dir_all(&iconset).map_err(|e| e.to_string())?;
    for (size, name) in [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
    ] {
        let out = Command::new("sips")
            .args(["-s", "format", "png", "-z", &size.to_string(), &size.to_string()])
            .arg(&raw)
            .arg("--out")
            .arg(iconset.join(name))
            .output()
            .map_err(|e| format!("sips: {e}"))?;
        if !out.status.success() {
            return Err("sips failed converting the icon".into());
        }
    }
    let out = Command::new("iconutil")
        .args(["-c", "icns", "-o"])
        .arg(res_dir.join("app.icns"))
        .arg(&iconset)
        .output()
        .map_err(|e| format!("iconutil: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(true)
}

fn run_capture(tool: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(tool).args(args).output().map_err(|e| EngineError::Launch {
        tool: tool.display().to_string(),
        source: e,
    })?;
    if !out.status.success() {
        return Err(EngineError::NonZero {
            tool: tool.display().to_string(),
            status: out.status.code().unwrap_or(-1),
            output: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn io_err(what: &'static str) -> impl Fn(std::io::Error) -> EngineError {
    move |e| EngineError::Launch { tool: what.to_string(), source: e }
}
