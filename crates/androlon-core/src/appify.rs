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

/// What an APK says about itself — the details an installer shows before
/// anything is written or installed.
#[derive(Debug, Clone)]
pub struct ApkInfo {
    pub label: String,
    pub package: String,
    pub version: String,
    pub min_sdk: String,
    pub size_bytes: u64,
}

/// Read identity + details from an APK without touching anything (fast: one
/// `aapt2 dump badging` + a stat).
pub fn inspect(cfg: &SdkConfig, apk: &Path) -> Result<ApkInfo> {
    if !apk.exists() {
        return Err(EngineError::ApkNotFound(apk.display().to_string()));
    }
    let apk_str = apk.display().to_string();
    let badging = run_capture(&cfg.aapt2(), &["dump", "badging", &apk_str])?;
    let package = extract(&badging, "package: name='")
        .ok_or_else(|| EngineError::PackageUnresolved(apk.display().to_string()))?;
    let label = extract(&badging, "application-label:'")
        .or_else(|| extract(&badging, "application-label-en:'"))
        .unwrap_or_else(|| package.clone());
    let version = extract(&badging, "versionName='").unwrap_or_else(|| "?".into());
    let min_sdk = extract(&badging, "sdkVersion:'").unwrap_or_else(|| "?".into());
    let size_bytes = std::fs::metadata(apk).map(|m| m.len()).unwrap_or(0);
    Ok(ApkInfo { label, package, version, min_sdk, size_bytes })
}

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
    // Store names are long and punctuated; ':' in particular is rendered as
    // '/' by Finder and upsets codesign, so build the bundle from a
    // filesystem-safe form while keeping `label` for display.
    let file_name = safe_name(&label);

    // ---- 2. Install into the runtime (best-effort; needs a booted device) ----
    let installed = AdbService::new(cfg).install(apk).is_ok();

    // ---- 3. Bundle skeleton + executable ----
    let bundle = out_dir.join(format!("{file_name}.app"));
    let macos_dir = bundle.join("Contents/MacOS");
    let res_dir = bundle.join("Contents/Resources");
    std::fs::create_dir_all(&macos_dir).map_err(io_err("create bundle"))?;
    std::fs::create_dir_all(&res_dir).map_err(io_err("create bundle"))?;

    // Named after the app so the process — and the menu bar — carries its
    // name. A SYMLINK to the shared player: bundles never go stale on
    // rebuild, and the exec path stays inside the bundle so macOS resolves
    // the right bundle identity (icon, name).
    let exe_name = file_name.clone();
    let exe_dest = macos_dir.join(&exe_name);
    let target = std::fs::canonicalize(host_binary).unwrap_or_else(|_| host_binary.to_path_buf());
    let _ = std::fs::remove_file(&exe_dest);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &exe_dest).map_err(io_err("link player"))?;
    #[cfg(not(unix))]
    std::fs::copy(&target, &exe_dest).map_err(io_err("copy player"))?;

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
        <key>ANDROLON_RUNTIMED</key>
        <string>{runtimed}</string>
    </dict>
</dict>
</plist>
"#,
        sdk = sdk_root.display(),
        server = server_jar.display(),
        // The bundled binary can't find the daemon as a sibling — point at it.
        runtimed = host_binary.with_file_name("androlon-runtimed").display(),
    );
    std::fs::write(bundle.join("Contents/Info.plist"), plist).map_err(io_err("write plist"))?;

    // Ad-hoc sign so macOS treats the copied binary + bundle as one identity.
    let _ = Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(&bundle)
        .output();

    // Register with Launch Services. Without this the bundle is unknown to
    // the system, so Finder falls back to the generic application icon even
    // though CFBundleIconFile and the .icns are both correct — and bumping
    // the mtime invalidates any icon Finder cached for this path before.
    let _ = filetime_touch(&bundle);
    let _ = Command::new(LSREGISTER).arg("-f").arg(&bundle).output();

    Ok(AppifyOutcome { label, package, bundle, installed, icon })
}

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// Bump the bundle's modification time so Finder re-reads its icon.
fn filetime_touch(bundle: &Path) -> std::io::Result<()> {
    Command::new("touch").arg(bundle).output().map(|_| ())
}

/// Pull the value after `key` up to the closing quote.
fn extract(badging: &str, key: &str) -> Option<String> {
    let start = badging.find(key)? + key.len();
    let end = badging[start..].find('\'')?;
    Some(badging[start..start + end].to_string())
}

/// Extract the APK's best raster icon to `dest` (PNG/WebP bytes as stored).
/// Returns false when the APK has no usable raster icon. Used by the native
/// installer to show the real app icon before anything is installed.
pub fn extract_icon(cfg: &SdkConfig, apk: &Path, dest: &Path) -> Result<bool> {
    let apk_str = apk.display().to_string();
    let badging = run_capture(&cfg.aapt2(), &["dump", "badging", &apk_str])?;
    match icon_source(&badging, apk) {
        Ok(Some(src)) => {
            std::fs::copy(&src, dest).map_err(io_err("copy icon"))?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(e) => Err(EngineError::Launch {
            tool: "extract icon".into(),
            source: std::io::Error::other(e),
        }),
    }
}

/// Locate the APK's densest raster icon, materialising it in a temp dir.
fn icon_source(badging: &str, apk: &Path) -> std::result::Result<Option<PathBuf>, String> {
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
        return Ok(Some(raw));
    }

    // Adaptive-icon APKs badge only XML, and shrunk release builds obfuscate
    // resource names — so extract the APK's rasters, measure them, and take
    // the largest square one (launcher icons ship as a 48..512px density
    // ladder; the biggest square is the densest icon).
    let rasters = tmp.join("rasters");
    let _ = Command::new("unzip")
        .args(["-o", "-q"])
        .arg(apk)
        .args(["res/*.png", "res/*.webp", "-d"])
        .arg(&rasters)
        .output()
        .map_err(|e| format!("unzip: {e}"))?;
    let mut best: Option<(u32, PathBuf)> = None;
    // Walk recursively: a normal APK stores icons in res/mipmap-<density>/,
    // not at the top of res/ (only name-obfuscated builds are flat).
    let mut candidates = Vec::new();
    let mut stack = vec![rasters.join("res")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                candidates.push(path);
            }
        }
    }
    if candidates.is_empty() {
        return Err("no raster resources in APK".to_string());
    }
    for path in candidates {
        // Prefer launcher icons when the names survived minification.
        let is_launcher = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("launcher") || n.contains("icon"));
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
            // Square, plausibly a launcher icon, and the biggest so far. A
            // name match wins over a merely larger non-icon image.
            let score = w + if is_launcher { 4096 } else { 0 };
            if w == h && (48..=1024).contains(&w) && best.as_ref().map_or(true, |(b, _)| score > *b) {
                best = Some((score, path));
            }
        }
    }
    Ok(best.map(|(_, path)| path))
}

/// Find the highest-density raster icon in the APK and convert it to
/// Resources/app.icns. Returns Ok(false) when no usable raster icon exists
/// (e.g. adaptive-XML-only icons with no fallback).
fn build_icns(badging: &str, apk: &Path, res_dir: &Path) -> std::result::Result<bool, String> {
    let Some(raw) = icon_source(badging, apk)? else {
        return Ok(false); // vector-only app; no raster to use
    };
    let tmp = raw.parent().unwrap_or(Path::new(".")).to_path_buf();

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

/// A label trimmed to something safe as a bundle/executable name: no path
/// separators or colons (Finder shows ':' as '/'), no leading dot, and short
/// enough to stay readable in the Dock.
fn safe_name(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| match c {
            '/' | ':' | '\\' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    // Store titles often carry a subtitle after ':' or '-'; the first clause
    // is the app's actual name and all a Dock icon can show anyway.
    let first = cleaned.split(" - ").next().unwrap_or(&cleaned);
    let first = first.split('-').next().unwrap_or(first);
    let trimmed = first.trim().trim_start_matches('.').trim();
    let name = if trimmed.is_empty() { cleaned.trim() } else { trimmed };
    name.chars().take(40).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn strips_colons_and_subtitles() {
        assert_eq!(
            safe_name("Fire Free Offline Shooting Game: Gun Games Offline"),
            "Fire Free Offline Shooting Game"
        );
        assert_eq!(safe_name("CalcYou"), "CalcYou");
        assert_eq!(safe_name("Some/App"), "Some");
    }

    #[test]
    fn never_yields_an_empty_or_hidden_name() {
        assert!(!safe_name("...").is_empty());
        assert!(!safe_name(".hidden").starts_with('.'));
    }
}
