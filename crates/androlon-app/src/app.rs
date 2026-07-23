//! Integrated multi-window shell: one SDL event loop drives the ImGui management
//! window (SDLRenderer3) and any number of SDL_GPU "app surface" windows. Events
//! are polled once at the low level, fed to ImGui, then converted to typed
//! events and routed by `window_id`.
//!
//! Today each app-surface window is fed by a self-contained decode demo
//! (encode→decode→present). Swapping `FrameProducer` for a scrcpy `FrameStream`
//! is the only change needed to show a live Android display.

use crate::ui::AppState;
use crate::video::{self, VideoWindow};
use androlon_core::SdkConfig;
use androlon_stream::{
    spawn_decode, DecodedFrame, FrameStream, Openh264Decoder, ScrcpyClient, ScrcpyOptions,
    TestEncoder, VideoDecoder,
};
use dear_imgui_rs::Context;
use dear_imgui_sdl3::{sdl3_poll_event_ll, Sdl3RendererBackend};
use sdl3::event::{Event, WindowEvent};
use sdl3::pixels::Color;
use sdl3::VideoSubsystem;

/// Source of frames for one app-surface window.
enum FrameProducer {
    /// Local encode→decode round-trip (no device needed).
    DecodeDemo { enc: TestEncoder, dec: Openh264Decoder, t: u32, w: u32, h: u32 },
    /// Live device stream: a background decode thread delivers frames over a
    /// channel. `_client` keeps the scrcpy server + tunnel alive for the pane's
    /// lifetime (its Drop tears them down when the window closes).
    Live { stream: FrameStream, _client: ScrcpyClient },
}

impl FrameProducer {
    fn decode_demo(w: u32, h: u32) -> Option<Self> {
        Some(FrameProducer::DecodeDemo {
            enc: TestEncoder::new().ok()?,
            dec: Openh264Decoder::new().ok()?,
            t: 0,
            w,
            h,
        })
    }

    fn next_frame(&mut self) -> Option<DecodedFrame> {
        match self {
            FrameProducer::DecodeDemo { enc, dec, t, w, h } => {
                let src = video::demo_frame(*t, *w, *h);
                *t = t.wrapping_add(2);
                let packet = enc.encode_rgba(&src.rgba, *w, *h).ok()?;
                dec.decode(&packet).ok().flatten()
            }
            // Drain to the newest frame (drop stale ones to keep latency low).
            FrameProducer::Live { stream, .. } => {
                let mut latest = None;
                while let Ok(frame) = stream.rx.try_recv() {
                    latest = Some(frame);
                }
                latest
            }
        }
    }
}

/// Fit `(w, h)` within a `cap`-px longest side, preserving aspect ratio.
fn fit(w: u32, h: u32, cap: u32) -> (u32, u32) {
    let longest = w.max(h).max(1);
    if longest <= cap {
        return (w.max(1), h.max(1));
    }
    let scale = cap as f32 / longest as f32;
    (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1))
}

/// Launch scrcpy on the booted device and wrap it in a live app-surface pane.
/// Blocking (adb push + connect handshake), so it briefly stalls the UI — fine
/// for a one-shot action.
fn open_live_surface(video: &VideoSubsystem, cfg: &SdkConfig) -> Result<VideoPane, String> {
    let mut opts = ScrcpyOptions { max_size: 1024, ..ScrcpyOptions::default() };
    if let Ok(path) = std::env::var("ANDROLON_SCRCPY_SERVER") {
        opts.server_jar = path.into();
    }
    let mut client = ScrcpyClient::new(cfg.clone(), opts);
    client.deploy_server().map_err(|e| format!("deploy server: {e}"))?;
    let stream = client.start().map_err(|e| format!("start stream: {e}"))?;

    let (mw, mh) = (stream.meta().width, stream.meta().height);
    let name = stream.meta().device_name.clone();
    let codec = stream.meta().codec.label();
    let (ww, wh) = fit(mw, mh, 800);
    let window = VideoWindow::new(video, &format!("Androlon — {name} [{codec}]"), ww, wh)
        .map_err(|e| format!("video window: {e}"))?;
    let id = window.id();
    let stream = spawn_decode(stream);
    Ok(VideoPane { window, source: FrameProducer::Live { stream, _client: client }, id })
}

struct VideoPane {
    window: VideoWindow,
    source: FrameProducer,
    id: u32,
}

pub fn run() {
    let sdl = sdl3::init().expect("SDL_Init");
    let video = sdl.video().expect("SDL video subsystem");

    // Management window: ImGui via the SDLRenderer3 backend.
    let window = video
        .window("Androlon", 1000, 680)
        .position_centered()
        .resizable()
        .build()
        .expect("create management window");
    let mut canvas = window.into_canvas();
    let mgmt_id = canvas.window().id();

    let mut imgui = Context::create();
    let _ = imgui.set_ini_filename::<std::path::PathBuf>(None);
    let mut backend = Sdl3RendererBackend::init(&mut imgui, canvas.window(), &canvas)
        .expect("init ImGui SDLRenderer3 backend");

    let cfg = SdkConfig::from_env();
    let mut app = AppState::new(cfg.clone());
    let mut panes: Vec<VideoPane> = Vec::new();

    // Test hook: open a decode-demo app surface at startup so the multi-window
    // path (SDL_Renderer + SDL_GPU coexisting) can be verified without clicking.
    if std::env::var("ANDROLON_AUTO_SURFACE").is_ok() {
        if let (Ok(win), Some(source)) = (
            VideoWindow::new(&video, "Androlon — App surface", 640, 360),
            FrameProducer::decode_demo(640, 360),
        ) {
            println!("✓ auto-opened app surface (id {})", win.id());
            panes.push(VideoPane { id: win.id(), window: win, source });
        }
    }

    // Test hook: open a LIVE scrcpy surface at startup (needs a booted device).
    if std::env::var("ANDROLON_LIVE").is_ok() {
        match open_live_surface(&video, &cfg) {
            Ok(pane) => {
                println!("✓ live surface opened (id {})", pane.id);
                panes.push(pane);
            }
            Err(e) => eprintln!("✗ live surface: {e}"),
        }
    }

    'main: loop {
        // One low-level poll feeds ImGui and our own routing.
        while let Some(raw) = sdl3_poll_event_ll() {
            backend.process_event(&mut imgui, &raw);
            match Event::from_ll(raw) {
                Event::Quit { .. } => break 'main,
                Event::Window { window_id, win_event: WindowEvent::CloseRequested, .. } => {
                    if window_id == mgmt_id {
                        break 'main; // closing the control window quits
                    }
                    panes.retain(|p| p.id != window_id); // close just that surface
                }
                _ => {}
            }
        }

        app.poll();

        // Draw the management window.
        backend.new_frame(&mut imgui);
        let ui = imgui.frame();
        app.draw(ui);
        let draw_data = imgui.render();
        canvas.set_draw_color(Color::RGB(18, 18, 22));
        canvas.clear();
        backend.render(draw_data, &canvas);
        canvas.present();

        // Honour an "open app surface" (demo) request from the UI.
        if app.take_open_video() {
            match VideoWindow::new(&video, "Androlon — App surface", 640, 360) {
                Ok(win) => match FrameProducer::decode_demo(640, 360) {
                    Some(source) => panes.push(VideoPane { id: win.id(), window: win, source }),
                    None => app.log_line("✗ could not start decoder for app surface"),
                },
                Err(e) => app.log_line(format!("✗ open app surface: {e}")),
            }
        }

        // Honour an "open LIVE surface" (scrcpy) request from the UI.
        if app.take_open_live() {
            match open_live_surface(&video, &cfg) {
                Ok(pane) => {
                    app.log_line(format!("✓ live surface (id {})", pane.id));
                    panes.push(pane);
                }
                Err(e) => app.log_line(format!("✗ live surface: {e}")),
            }
        }

        // Present each app-surface window.
        for pane in &mut panes {
            if let Some(frame) = pane.source.next_frame() {
                let _ = pane.window.present(&frame);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}
