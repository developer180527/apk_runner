//! Androlon host (SDL3 + Dear ImGui). This is the management shell: an SDL3
//! window whose SDLRenderer3-backed ImGui UI drives the engine (setup, AVDs,
//! boot, root). Later milestones add SDL_GPU app-surface windows that stream
//! Android virtual displays; this control panel stays as the home surface.
//!
//! Pass `--headless` to run the init + one engine snapshot and exit (CI / no
//! display), skipping the window and event loop.

mod app;
#[cfg(target_os = "macos")]
mod avlayer;
mod input;
mod ui;
mod video;

use androlon_core::{EmulatorService, SdkConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let headless = args.iter().any(|a| a == "--headless");
    let cfg = SdkConfig::from_env();

    if headless {
        let report = EmulatorService::new(cfg.clone()).doctor();
        println!("✓ headless: engine reachable — API {} · {}", report.api, report.system_image);
        println!("  SDK provisioned: {}", report.tools.iter().all(|t| t.present));
        return;
    }

    // Prove the SDL_GPU upload+blit path with a synthetic frame source.
    if args.iter().any(|a| a == "--video-demo") {
        run_video_demo();
        return;
    }

    // Prove the full decode path: encode synthetic frames → H.264 → decode →
    // RGBA → present, the same chain scrcpy packets will take.
    if args.iter().any(|a| a == "--decode-demo") {
        run_decode_demo();
        return;
    }

    // Single-app mode (appified bundles): `--app <package>` on the CLI, or
    // ANDROLON_APP from a bundle's LSEnvironment (Launch Services passes no
    // custom argv, so generated .apps configure via environment).
    let app_pkg = args
        .iter()
        .position(|a| a == "--app")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("ANDROLON_APP").ok());
    if let Some(pkg) = app_pkg {
        app::run_single(&pkg);
        return;
    }

    // Default: the integrated multi-window shell (management + app surfaces).
    app::run();
}

/// SDL_GPU demo: open a video window and push a moving gradient through the
/// real upload→blit→present pipeline (the same path decoded frames will take).
fn run_video_demo() {
    let sdl = sdl3::init().expect("SDL_Init");
    let video = sdl.video().expect("SDL video subsystem");
    let (w, h) = (640u32, 360u32);
    let mut vw = video::VideoWindow::new(&video, "Androlon — Video (demo)", w, h)
        .expect("create SDL_GPU video window");
    let mut pump = sdl.event_pump().expect("event pump");

    println!("✓ SDL_GPU video window up ({}x{}). Close it to exit.", vw.size().0, vw.size().1);
    let mut t = 0u32;
    'main: loop {
        for event in pump.poll_iter() {
            if let sdl3::event::Event::Quit { .. } = event {
                break 'main;
            }
        }
        let frame = video::demo_frame(t, w, h);
        if let Err(e) = vw.present(&frame) {
            eprintln!("✗ present: {e}");
            break;
        }
        t = t.wrapping_add(2);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Full decode-path demo: a synthetic gradient is H.264-encoded, then decoded
/// and presented — exercising `Openh264Decoder` + `VideoWindow` exactly as the
/// scrcpy stream will (just with a local encoder standing in for the device).
fn run_decode_demo() {
    use androlon_stream::{Openh264Decoder, TestEncoder, VideoDecoder};

    let sdl = sdl3::init().expect("SDL_Init");
    let video = sdl.video().expect("SDL video subsystem");
    let (w, h) = (640u32, 360u32);
    let mut vw = video::VideoWindow::new(&video, "Androlon — Decode (demo)", w, h)
        .expect("create SDL_GPU video window");
    let mut pump = sdl.event_pump().expect("event pump");

    let mut encoder = TestEncoder::new().expect("openh264 encoder");
    let mut decoder = Openh264Decoder::new().expect("openh264 decoder");
    println!("✓ decode demo: encode→{}→present ({}x{})", decoder.name(), w, h);

    let mut t = 0u32;
    let mut decoded = 0u64;
    'main: loop {
        for event in pump.poll_iter() {
            if let sdl3::event::Event::Quit { .. } = event {
                break 'main;
            }
        }
        let src = video::demo_frame(t, w, h);
        let packet = encoder.encode_rgba(&src.rgba, w, h).expect("encode");
        match decoder.decode(&packet) {
            Ok(Some(frame)) => {
                decoded += 1;
                if decoded == 1 {
                    println!("✓ first frame decoded: {}x{} RGBA", frame.width, frame.height);
                }
                if let Err(e) = vw.present(&frame) {
                    eprintln!("✗ present: {e}");
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("✗ decode: {e}");
                break;
            }
        }
        t = t.wrapping_add(2);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    println!("decoded {decoded} frames");
}
