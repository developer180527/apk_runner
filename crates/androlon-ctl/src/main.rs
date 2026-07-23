//! Thin CLI over androlon-core, mirroring apkrun.sh so the Rust engine can be
//! exercised before the SDL3/ImGui app exists. The app calls the same core APIs.

use androlon_core::backend::AndroidBackend;
use androlon_core::{
    Avd, BootProfile, EmulatorService, Gfxstream, GpuBackend, SdkConfig, WindowMode,
};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = SdkConfig::from_env();
    let engine = EmulatorService::new(cfg.clone());

    let result = match args.first().map(String::as_str) {
        Some("doctor") => {
            print_doctor(&engine);
            Ok(())
        }
        None | Some("-h") | Some("--help") | Some("help") => {
            usage(&cfg);
            Ok(())
        }
        Some("setup") => {
            println!("› Installing SDK packages ({})…", cfg.system_image());
            engine.install_packages().map(|_| {
                println!("✓ SDK provisioned at {}", cfg.sdk_root.display());
            })
        }
        Some("avd") => engine
            .create_avd(&Avd::phone())
            .and_then(|_| engine.create_avd(&Avd::desktop()))
            .map(|_| println!("✓ AVDs ready: {}", engine.list_avds().join(", "))),
        Some("run") => cmd_run(&engine, &cfg, &args),
        Some("scrcpy-probe") => cmd_scrcpy_probe(&cfg),
        Some("root") => cmd_root(&engine, &args),
        Some("stop") => {
            engine.stop();
            println!("✓ stop signal sent");
            Ok(())
        }
        Some(other) => {
            fail(&format!("unknown command: {other}  (try: androlon-ctl --help)"));
        }
    };

    if let Err(e) = result {
        fail(&e.to_string());
    }
}

/// Text-only end-to-end probe of the live scrcpy path: deploy the server,
/// connect, print the handshake metadata + first packets, and decode one frame.
/// Prints everything to stdout so results can be shared for diagnosis.
fn cmd_scrcpy_probe(cfg: &SdkConfig) -> androlon_core::Result<()> {
    use androlon_stream::{Openh264Decoder, ScrcpyClient, ScrcpyOptions, VideoDecoder};

    let adb = androlon_core::AdbService::new(cfg);
    println!("› adb devices:");
    if let Ok(out) = adb.adb(&["devices"]) {
        for line in out.output.lines() {
            println!("    {line}");
        }
    }

    let mut opts = ScrcpyOptions { max_size: 1024, ..ScrcpyOptions::default() };
    if let Ok(p) = std::env::var("ANDROLON_SCRCPY_SERVER") {
        opts.server_jar = p.into();
    }
    println!("› scrcpy-server: {} (version {})", opts.server_jar.display(), opts.server_version);

    let mut client = ScrcpyClient::new(cfg.clone(), opts);
    println!("› deploying server…");
    if let Err(e) = client.deploy_server() {
        fail(&format!("deploy failed: {e}"));
    }
    println!("› starting stream (forward tunnel + app_process)…");
    let mut stream = match client.start() {
        Ok(s) => s,
        Err(e) => fail(&format!("start failed: {e}\n(check {}/.scrcpy-server.log on device side)", cfg.sdk_root.display())),
    };

    let m = stream.meta().clone();
    println!("✓ handshake OK");
    println!("    device : {}", m.device_name);
    println!("    codec  : {}", m.codec.label());
    println!("    size   : {}x{}", m.width, m.height);

    println!("› reading first 30 packets…");
    let mut decoder = Openh264Decoder::new().ok();
    let (mut n, mut config, mut key, mut decoded, mut bytes) = (0u32, 0u32, 0u32, 0u32, 0usize);
    for _ in 0..30 {
        match stream.read_packet() {
            Ok(p) => {
                n += 1;
                bytes += p.data.len();
                if p.is_config { config += 1; }
                if p.is_keyframe { key += 1; }
                if n <= 6 {
                    println!("    pkt {n}: {} bytes  config={} key={}  pts={}",
                        p.data.len(), p.is_config, p.is_keyframe, p.pts);
                }
                if let Some(d) = decoder.as_mut() {
                    if let Ok(Some(f)) = d.decode(&p) {
                        decoded += 1;
                        if decoded == 1 {
                            println!("    ✓ FIRST FRAME DECODED: {}x{} RGBA ({} bytes)",
                                f.width, f.height, f.rgba.len());
                        }
                    }
                }
            }
            Err(e) => {
                println!("    stream ended/err after {n} packets: {e}");
                break;
            }
        }
    }
    println!("✓ summary: {n} packets, {config} config, {key} keyframe, {decoded} decoded, {bytes} total bytes");
    if decoded > 0 {
        println!("🎉 LIVE PIPELINE WORKS: scrcpy → decode → RGBA. The SDL_GPU app will show this.");
    } else {
        println!("⚠ packets flowed but none decoded — likely a codec/version detail; share this output.");
    }
    client.stop();
    Ok(())
}

fn cmd_run(engine: &EmulatorService, cfg: &SdkConfig, args: &[String]) -> androlon_core::Result<()> {
    let Some(apk_arg) = args.get(1) else {
        fail("usage: androlon-ctl run <apk> [--desktop|--coherence|--phone]");
    };
    let apk = PathBuf::from(apk_arg);
    if !apk.exists() {
        return Err(androlon_core::EngineError::ApkNotFound(apk.display().to_string()));
    }
    let mode = if args.iter().any(|a| a == "--coherence") {
        WindowMode::Coherence
    } else if args.iter().any(|a| a == "--phone") {
        WindowMode::Phone
    } else {
        WindowMode::Desktop
    };
    // Developer profile by default (deterministic, root-safe); --consumer for fast resume.
    let profile = if args.iter().any(|a| a == "--consumer") {
        BootProfile::Consumer
    } else {
        BootProfile::Developer
    };
    let avd = if mode == WindowMode::Phone { Avd::phone() } else { Avd::desktop() };
    let log = cfg.sdk_root.parent().unwrap_or(Path::new(".")).join(".emulator.log");

    println!(
        "› Booting {} [{:?} · {} profile] with GPU backend '{}'…",
        avd.name,
        mode,
        profile.label(),
        Gfxstream.name()
    );
    engine.boot_and_wait(&avd, profile, &log, Duration::from_secs(180))?;

    let adb = androlon_core::AdbService::new(cfg);
    adb.install(&apk)?;
    let pkg = adb.package_name(&apk);
    if pkg.is_empty() {
        return Err(androlon_core::EngineError::PackageUnresolved(apk.display().to_string()));
    }
    let Some(component) = adb.launch_component(&pkg) else {
        return Err(androlon_core::EngineError::PackageUnresolved(pkg));
    };
    // Coherence mode would allocate a virtual display per app; v1 uses display 0.
    adb.launch(&component, None)?;
    println!("✓ Launched {component}");
    Ok(())
}

fn cmd_root(engine: &EmulatorService, args: &[String]) -> androlon_core::Result<()> {
    let adb = androlon_core::AdbService::new(&engine.config);
    match args.get(1).map(String::as_str) {
        Some("status") | None => {
            println!("root: {}", adb.root_status().label());
            Ok(())
        }
        Some("adbd") => adb.enable_adbd_root().map(|_| {
            println!("✓ adbd root: {}", adb.root_status().label());
        }),
        Some(_) => fail("usage: androlon-ctl root [status|adbd]"),
    }
}

fn print_doctor(engine: &EmulatorService) {
    let r = engine.doctor();
    println!("SDK root : {}", r.sdk_root.display());
    println!("API level: {}   image: {}", r.api, r.system_image);
    for t in &r.tools {
        if t.present {
            println!("  ✓ {} -> {}", t.name, t.path.display());
        } else {
            println!("  ✗ {} (run: androlon-ctl setup)", t.name);
        }
    }
    if r.avds.is_empty() {
        println!("AVDs: (none — run: androlon-ctl avd)");
    } else {
        println!("AVDs: {}", r.avds.join(", "));
    }
    println!("Emulator running: {}", if r.emulator_running { "yes" } else { "no" });
    println!("Rootable image  : {}", if r.is_rootable { "yes" } else { "no (locked user build)" });
    println!("Root status     : {}   (none | adbd | magisk)", r.root_status.label());
}

fn usage(cfg: &SdkConfig) {
    println!(
        "androlon-ctl — Androlon engine CLI (Android 17 / API 37, arm64)\n\n\
         \x20 androlon-ctl doctor              show what's installed / missing + root status\n\
         \x20 androlon-ctl setup               install SDK packages (cmdline-tools must exist)\n\
         \x20 androlon-ctl avd                 create phone + desktop AVDs\n\
         \x20 androlon-ctl run <apk> [--desktop|--coherence|--phone] [--consumer]\n\
         \x20 androlon-ctl root [status|adbd]  report / enable root\n\
         \x20 androlon-ctl stop                shut the emulator down\n\n\
         Env: APKRUN_API={}  APKRUN_IMG_TYPE={}  APKRUN_SDK={}",
        cfg.api,
        cfg.image_type,
        cfg.sdk_root.display()
    );
}

fn fail(msg: &str) -> ! {
    eprintln!("✗ {msg}");
    exit(1);
}
