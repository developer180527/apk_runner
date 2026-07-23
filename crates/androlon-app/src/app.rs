//! Integrated multi-window shell: one SDL event loop drives the ImGui management
//! window (SDLRenderer3) and any number of SDL_GPU "app surface" windows. Events
//! are polled once at the low level, fed to ImGui, then converted to typed
//! events and routed by `window_id`.
//!
//! Today each app-surface window is fed by a self-contained decode demo
//! (encode→decode→present). Swapping `FrameProducer` for a scrcpy `FrameStream`
//! is the only change needed to show a live Android display.

use crate::input;
use crate::ui::AppState;
use crate::video::{self, VideoWindow};
use androlon_core::SdkConfig;
use androlon_stream::control::{self, keycodes, ControlChannel, Position};
use androlon_stream::{
    spawn_decode, DecodedFrame, FrameStream, Openh264Decoder, ScrcpyClient, ScrcpyOptions,
    TestEncoder, VideoDecoder,
};
use dear_imgui_rs::Context;
use dear_imgui_sdl3::{sdl3_poll_event_ll, Sdl3RendererBackend};
use sdl3::event::{Event, WindowEvent};
use sdl3::mouse::MouseButton;
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
/// True when the zero-copy AVSampleBufferDisplayLayer presenter should be
/// used for live panes (macOS only; opt out with ANDROLON_PRESENT=gpu).
fn use_avlayer() -> bool {
    cfg!(target_os = "macos")
        && std::env::var("ANDROLON_PRESENT").map(|v| v != "gpu").unwrap_or(true)
}

/// Mirror the device's main display (the "phone in a window" pane).
fn open_live_surface(video: &VideoSubsystem, cfg: &SdkConfig) -> Result<VideoPane, String> {
    let opts = ScrcpyOptions { max_size: 1024, ..ScrcpyOptions::default() };
    open_stream_pane(video, cfg, opts, None)
}

/// Coherence: give `package` its own Android virtual display and present it as
/// an independent native window. Sized by us → never letterboxed.
fn open_coherence_surface(
    video: &VideoSubsystem,
    cfg: &SdkConfig,
    package: &str,
    decorations: bool,
) -> Result<VideoPane, String> {
    // Size is in *window points* (default 800x500). The virtual display is
    // created at 2x pixels with a matching 2x Android density (320 = 2x mdpi),
    // so the stream is Retina-exact: Android renders at the same physical
    // resolution the window occupies on a HiDPI screen — no upscaling blur.
    let (ww, wh) = std::env::var("ANDROLON_COHERENCE_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((800, 500));
    let dpi = std::env::var("ANDROLON_COHERENCE_DPI")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(320);
    let opts = ScrcpyOptions {
        new_display: Some((ww * 2, wh * 2)),
        new_display_dpi: Some(dpi),
        start_app: Some(package.to_string()),
        vd_system_decorations: decorations,
        ..ScrcpyOptions::default()
    };
    open_stream_pane(video, cfg, opts, Some((package, (ww, wh))))
}

fn open_stream_pane(
    video: &VideoSubsystem,
    cfg: &SdkConfig,
    mut opts: ScrcpyOptions,
    // Coherence: (package, window size in points). None = mirror pane.
    app: Option<(&str, (u32, u32))>,
) -> Result<VideoPane, String> {
    if let Ok(path) = std::env::var("ANDROLON_SCRCPY_SERVER") {
        opts.server_jar = path.into();
    }
    let mut client = ScrcpyClient::new(cfg.clone(), opts);
    client.deploy_server().map_err(|e| format!("deploy server: {e}"))?;
    let (stream, ctl) = client.start().map_err(|e| format!("start stream: {e}"))?;

    let (mw, mh) = (stream.meta().width, stream.meta().height);
    let name = stream.meta().device_name.clone();
    let codec = stream.meta().codec.label();
    // Coherence panes open at their chosen point size (the stream is exactly
    // 2x that); mirror panes fit the device's aspect under a 800px cap.
    let (ww, wh) = match app {
        Some((_, size)) => size,
        None => fit(mw, mh, 800),
    };
    // A Coherence pane is titled as the app, like any native window.
    let title = match app {
        Some((pkg, _)) => format!("{pkg} — Androlon"),
        None => format!("Androlon — {name} [{codec}]"),
    };

    // Zero-copy path: compressed samples → AVSampleBufferDisplayLayer, which
    // decodes + presents on the GPU. No decode thread, no RGBA, no upload.
    #[cfg(target_os = "macos")]
    if use_avlayer() {
        let window = video
            .window(&title, ww, wh)
            .position_centered()
            .resizable()
            .build()
            .map_err(|e| format!("layer window: {e}"))?;
        // Lock the window to the stream's aspect ratio so resizing scales the
        // video edge-to-edge instead of letterboxing it.
        let aspect = mw as f32 / mh.max(1) as f32;
        unsafe { sdl3_sys::video::SDL_SetWindowAspectRatio(window.raw(), aspect, aspect) };
        let present = crate::avlayer::AvLayerPresenter::new(&window)?;
        let id = window.id();
        let samples = androlon_stream::spawn_samples(stream);
        return Ok(VideoPane {
            screen: Screen::Layer { window, present, samples, _client: client },
            id,
            control: ctl,
            stream_size: (mw, mh),
            touch_down: false,
        });
    }

    // Portable path: decode thread → RGBA → SDL_GPU upload + blit.
    let window = VideoWindow::new(video, &title, ww, wh)
        .map_err(|e| format!("video window: {e}"))?;
    let id = window.id();
    let stream = spawn_decode(stream);
    Ok(VideoPane {
        screen: Screen::Gpu {
            window,
            source: FrameProducer::Live { stream, _client: client },
        },
        id,
        control: ctl,
        stream_size: (mw, mh),
        touch_down: false,
    })
}

/// How a pane gets pixels on screen.
enum Screen {
    /// Portable: decoded RGBA frames uploaded + blitted via SDL_GPU.
    Gpu { window: VideoWindow, source: FrameProducer },
    /// macOS zero-copy: compressed samples enqueued to a CoreAnimation layer.
    #[cfg(target_os = "macos")]
    Layer {
        window: sdl3::video::Window,
        present: crate::avlayer::AvLayerPresenter,
        samples: androlon_stream::SampleStream,
        /// Keeps the scrcpy server + tunnel alive for the pane's lifetime.
        _client: ScrcpyClient,
    },
}

impl Screen {
    fn window_size(&self) -> (u32, u32) {
        match self {
            Screen::Gpu { window, .. } => window.size(),
            #[cfg(target_os = "macos")]
            Screen::Layer { window, .. } => window.size(),
        }
    }
}

struct VideoPane {
    screen: Screen,
    id: u32,
    /// Input-injection channel (live panes only; `None` for demos).
    control: Option<ControlChannel>,
    /// Size of the *stream* (device pixels) — the coordinate space touches are
    /// sent in. Updated from each decoded frame, so it tracks rotation.
    stream_size: (u32, u32),
    /// Left button held → we're mid-touch and motion becomes ACTION_MOVE.
    touch_down: bool,
}

impl VideoPane {
    fn pos_at(&self, x: f32, y: f32) -> Position {
        let (sx, sy) = input::window_to_stream((x, y), self.screen.window_size(), self.stream_size);
        Position {
            x: sx,
            y: sy,
            width: self.stream_size.0 as u16,
            height: self.stream_size.1 as u16,
        }
    }

    /// Translate one SDL event (already routed to this window) into control
    /// messages. Send failures are ignored: the drain thread ending / a dead
    /// socket surfaces as the video stream closing anyway.
    fn handle_input(&mut self, event: &Event) {
        if self.control.is_none() {
            return;
        }
        match *event {
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                self.touch_down = true;
                let pos = self.pos_at(x, y);
                let ctl = self.control.as_mut().unwrap();
                let _ = ctl.send_touch(
                    control::ACTION_DOWN, pos, 1.0,
                    control::BUTTON_PRIMARY, control::BUTTON_PRIMARY,
                );
            }
            Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
                self.touch_down = false;
                let pos = self.pos_at(x, y);
                let ctl = self.control.as_mut().unwrap();
                let _ = ctl.send_touch(
                    control::ACTION_UP, pos, 0.0,
                    control::BUTTON_PRIMARY, 0,
                );
            }
            Event::MouseMotion { x, y, .. } if self.touch_down => {
                let pos = self.pos_at(x, y);
                let ctl = self.control.as_mut().unwrap();
                let _ = ctl.send_touch(
                    control::ACTION_MOVE, pos, 1.0,
                    0, control::BUTTON_PRIMARY,
                );
            }
            // scrcpy-style shortcuts: right click = BACK, middle click = HOME.
            Event::MouseButtonDown { mouse_btn: MouseButton::Right, .. } => {
                let _ = self.control.as_mut().unwrap().send_back();
            }
            Event::MouseButtonDown { mouse_btn: MouseButton::Middle, .. } => {
                let ctl = self.control.as_mut().unwrap();
                let _ = ctl.send_key(control::ACTION_DOWN, keycodes::AKEYCODE_HOME, 0);
                let _ = ctl.send_key(control::ACTION_UP, keycodes::AKEYCODE_HOME, 0);
            }
            Event::MouseWheel { x, y, mouse_x, mouse_y, .. } => {
                let pos = self.pos_at(mouse_x, mouse_y);
                let ctl = self.control.as_mut().unwrap();
                let _ = ctl.send_scroll(pos, x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));
            }
            Event::KeyDown { keycode: Some(key), keymod, .. } => {
                if let Some(ak) = input::android_keycode(key) {
                    let ctl = self.control.as_mut().unwrap();
                    let _ = ctl.send_key(control::ACTION_DOWN, ak, input::meta_of(keymod));
                }
            }
            Event::KeyUp { keycode: Some(key), keymod, .. } => {
                if let Some(ak) = input::android_keycode(key) {
                    let ctl = self.control.as_mut().unwrap();
                    let _ = ctl.send_key(control::ACTION_UP, ak, input::meta_of(keymod));
                }
            }
            _ => {}
        }
    }
}

/// Single-app mode: this process IS one Android app, as far as macOS cares.
/// One Coherence window, no management panel, no decorations on the virtual
/// display; closing the window (or the stream dying) exits the process. This
/// is what an appified `.app` bundle launches.
pub fn run_single(package: &str) {
    let sdl = sdl3::init().expect("SDL_Init");
    let video = sdl.video().expect("SDL video subsystem");
    let cfg = SdkConfig::from_env();

    let mut pane = match open_coherence_surface(&video, &cfg, package, false) {
        Ok(pane) => pane,
        Err(e) => {
            eprintln!("✗ {package}: {e}");
            eprintln!("  (is the Android runtime booted?)");
            std::process::exit(1);
        }
    };

    let mut pump = sdl.event_pump().expect("event pump");
    'main: loop {
        for event in pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::Window { win_event: WindowEvent::CloseRequested, .. } => break 'main,
                ref e => pane.handle_input(e),
            }
        }

        let VideoPane { screen, stream_size, .. } = &mut pane;
        match screen {
            Screen::Gpu { window, source } => {
                if let Some(frame) = source.next_frame() {
                    *stream_size = (frame.width, frame.height);
                    let _ = window.present(&frame);
                }
            }
            #[cfg(target_os = "macos")]
            Screen::Layer { present, samples, .. } => {
                while let Ok((sample, size)) = samples.rx.try_recv() {
                    *stream_size = size;
                    present.enqueue(&sample);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(8));
    }
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
            panes.push(VideoPane {
                id: win.id(),
                screen: Screen::Gpu { window: win, source },
                control: None,
                stream_size: (640, 360),
                touch_down: false,
            });
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

    // Test hook: Coherence — give a package its own virtual-display window.
    // e.g. ANDROLON_COHERENCE=com.android.settings (comma-separate for several).
    if let Ok(pkgs) = std::env::var("ANDROLON_COHERENCE") {
        for pkg in pkgs.split(',').filter(|p| !p.is_empty()) {
            match open_coherence_surface(&video, &cfg, pkg, true) {
                Ok(pane) => {
                    println!("✓ coherence window for {pkg} (id {})", pane.id);
                    panes.push(pane);
                }
                Err(e) => eprintln!("✗ coherence {pkg}: {e}"),
            }
        }
    }

    'main: loop {
        // One low-level poll feeds ImGui and our own routing.
        while let Some(raw) = sdl3_poll_event_ll() {
            backend.process_event(&mut imgui, &raw);
            let event = Event::from_ll(raw);
            match event {
                Event::Quit { .. } => break 'main,
                Event::Window { window_id, win_event: WindowEvent::CloseRequested, .. } => {
                    if window_id == mgmt_id {
                        break 'main; // closing the control window quits
                    }
                    panes.retain(|p| p.id != window_id); // close just that surface
                }
                // Input on an app surface → inject into the device.
                Event::MouseButtonDown { window_id, .. }
                | Event::MouseButtonUp { window_id, .. }
                | Event::MouseMotion { window_id, .. }
                | Event::MouseWheel { window_id, .. }
                | Event::KeyDown { window_id, .. }
                | Event::KeyUp { window_id, .. }
                    if window_id != mgmt_id =>
                {
                    if let Some(pane) = panes.iter_mut().find(|p| p.id == window_id) {
                        pane.handle_input(&event);
                    }
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
                    Some(source) => panes.push(VideoPane {
                        id: win.id(),
                        screen: Screen::Gpu { window: win, source },
                        control: None,
                        stream_size: (640, 360),
                        touch_down: false,
                    }),
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
            let VideoPane { screen, stream_size, .. } = pane;
            match screen {
                Screen::Gpu { window, source } => {
                    if let Some(frame) = source.next_frame() {
                        *stream_size = (frame.width, frame.height); // tracks rotation
                        let _ = window.present(&frame);
                    }
                }
                // Enqueue *every* sample (the layer is the decoder — H.264
                // needs each frame in order; latency is bounded by
                // DisplayImmediately, not by dropping).
                #[cfg(target_os = "macos")]
                Screen::Layer { present, samples, .. } => {
                    while let Ok((sample, size)) = samples.rx.try_recv() {
                        *stream_size = size;
                        present.enqueue(&sample);
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}
