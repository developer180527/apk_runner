//! The player's app surface: one Android virtual display streamed into a
//! native window, with input, audio, and keymaps.
//!
//! Presentation has two paths — the macOS zero-copy AVSampleBufferDisplayLayer
//! and the portable SDL_GPU upload+blit — selected by `ANDROLON_PRESENT`.

use crate::input;
use crate::keymap::{Action, JoyState, Keymap};
use crate::video::VideoWindow;
use androlon_core::SdkConfig;
use androlon_stream::control::{self, keycodes, ControlChannel, Position};
use androlon_stream::{spawn_decode, DecodedFrame, FrameStream, ScrcpyClient, ScrcpyOptions};
use sdl3::event::{Event, WindowEvent};
use sdl3::mouse::MouseButton;
use sdl3::VideoSubsystem;

/// Source of frames for one app-surface window.
enum FrameProducer {
    /// Live device stream: a background decode thread delivers frames over a
    /// channel. `_client` keeps the scrcpy server + tunnel alive for the pane's
    /// lifetime (its Drop tears them down when the window closes).
    Live { stream: FrameStream, _client: ScrcpyClient },
}

impl FrameProducer {
    fn next_frame(&mut self) -> Option<DecodedFrame> {
        match self {
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

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
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

/// Wait for the runtime daemon to have Android up, WITHOUT blocking the SDL
/// event loop — a cold boot takes minutes, and an app that stops servicing
/// its event loop is what macOS reports as "not responding". Pumps events
/// while it waits; returns the lease (None = no daemon, assume a booted
/// device, which is how a manually-started emulator still works).
fn wait_for_runtime(sdl: &sdl3::Sdl) -> Option<androlon_ipc::RuntimeLease> {
    let rx = androlon_ipc::RuntimeLease::acquire_async();
    let mut pump = sdl.event_pump().ok();
    let started = std::time::Instant::now();
    let mut announced = false;
    loop {
        match rx.try_recv() {
            Ok(Ok(lease)) => return Some(lease),
            Ok(Err(e)) => {
                eprintln!("⚠ runtime daemon unavailable ({e}); assuming a booted device");
                return None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        // Servicing events is what keeps the app alive in the OS's eyes.
        if let Some(pump) = pump.as_mut() {
            for event in pump.poll_iter() {
                if matches!(event, Event::Quit { .. }) {
                    std::process::exit(0);
                }
            }
        }
        if !announced && started.elapsed() > std::time::Duration::from_secs(3) {
            announced = true;
            println!("› starting the Android runtime (first boot can take a few minutes)…");
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// Coherence: give `package` its own Android virtual display and present it as
/// an independent native window. Sized by us → never letterboxed.
fn open_coherence_surface(
    video: &VideoSubsystem,
    cfg: &SdkConfig,
    package: &str,
    decorations: bool,
    // Some = play device audio through this host subsystem. Audio capture is
    // device-wide, so exactly one pane should have it (the single-app pane).
    sound: Option<&sdl3::AudioSubsystem>,
    lease: Option<androlon_ipc::RuntimeLease>,
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
        // Game-friendly stream: explicit 60 fps target and a generous bitrate
        // (localhost, so bandwidth is free — artifacts aren't).
        max_fps: env_u32("ANDROLON_FPS", 60),
        video_bit_rate: env_u32("ANDROLON_BITRATE", 16_000_000),
        audio: sound.is_some(),
        ..ScrcpyOptions::default()
    };
    open_stream_pane(
        video,
        cfg,
        opts,
        Some((package, (ww, wh))),
        sound,
        lease,
        Some(Respawn {
            cfg: cfg.clone(),
            package: package.to_string(),
            decorations,
            audio: sound.is_some(),
        }),
    )
}

fn open_stream_pane(
    video: &VideoSubsystem,
    cfg: &SdkConfig,
    mut opts: ScrcpyOptions,
    // Coherence: (package, window size in points). None = mirror pane.
    app: Option<(&str, (u32, u32))>,
    sound: Option<&sdl3::AudioSubsystem>,
    // Runtime lease to hand to the pane (kept alive for its lifetime).
    lease: Option<androlon_ipc::RuntimeLease>,
    respawn: Option<Respawn>,
) -> Result<VideoPane, String> {
    if let Ok(path) = std::env::var("ANDROLON_SCRCPY_SERVER") {
        opts.server_jar = path.into();
    }

    // The caller is responsible for having the runtime up (see
    // `wait_for_runtime`, which does it without blocking an event loop).

    let mut client = ScrcpyClient::new(cfg.clone(), opts);
    client.deploy_server().map_err(|e| format!("deploy server: {e}"))?;
    let (stream, audio, ctl) = client.start().map_err(|e| format!("start stream: {e}"))?;

    // Device audio → SDL playback. Raw PCM (48 kHz stereo s16le), pushed by a
    // feeder thread; the thread (and the device it owns) ends with the socket.
    if let (Some(audio), Some(sound)) = (audio, sound) {
        use sdl3::audio::{AudioFormat, AudioSpec};
        let spec = AudioSpec {
            freq: Some(androlon_stream::scrcpy::AUDIO_SAMPLE_RATE as i32),
            channels: Some(androlon_stream::scrcpy::AUDIO_CHANNELS as i32),
            format: Some(AudioFormat::S16LE),
        };
        let out = sound
            .default_playback_device()
            .open_device_stream(Some(&spec))
            .map_err(|e| format!("audio device: {e}"))?;
        let _ = out.resume();
        // SDL audio streams are internally locked; pushing from the feeder
        // thread is safe even though the handle type isn't marked Send. The
        // method (vs. field access) makes the closure capture the whole
        // wrapper, so the unsafe Send actually applies (2021 disjoint capture).
        struct SendAudio(sdl3::audio::AudioStreamOwner);
        unsafe impl Send for SendAudio {}
        impl SendAudio {
            fn put(&self, chunk: &[u8]) {
                let _ = self.0.put_data(chunk);
            }
        }
        let out = SendAudio(out);
        let _ = androlon_stream::spawn_audio(audio, move |chunk| out.put(chunk));
        println!("✓ audio: device playback → host output");
    }

    // Game keybindings, if the user wrote a profile for this package.
    let keymap = app.and_then(|(pkg, _)| {
        let map = Keymap::load(pkg);
        if map.is_some() {
            println!("✓ keymap loaded for {pkg} (~/.androlon/keymaps/{pkg}.conf)");
        }
        map
    });

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
    // decodes + presents on the GPU. No decode thread, no RGBA, no upload —
    // and the stream thread enqueues directly, so frames skip the UI loop.
    #[cfg(target_os = "macos")]
    if use_avlayer() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

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
        let present = Arc::new(crate::avlayer::AvLayerPresenter::new(&window)?);
        let id = window.id();
        // Stream size shared with the input path (updates on rotation).
        let size = Arc::new(AtomicU64::new(pack_size(mw, mh)));
        let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let feed = {
            let present = Arc::clone(&present);
            let size = Arc::clone(&size);
            let frames = Arc::clone(&frames);
            androlon_stream::spawn_samples(stream, move |sample, (w, h)| {
                size.store(pack_size(w, h), Ordering::Relaxed);
                present.enqueue(&sample);
                frames.fetch_add(1, Ordering::Relaxed);
            })
        };
        return Ok(VideoPane {
            screen: Screen::Layer {
                window,
                _present: present,
                size,
                frames,
                _feed: feed,
                _client: client,
            },
            id,
            control: ctl,
            stream_size: (mw, mh),
            touch_down: false,
            keymap,
            joy: JoyState::default(),
            aim: AimState::default(),
            title,
            hud_t: std::time::Instant::now(),
            _lease: lease,
            respawn,
            pending_resize: None,
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
        keymap,
        joy: JoyState::default(),
        aim: AimState::default(),
        title,
        hud_t: std::time::Instant::now(),
        _lease: lease,
        respawn,
        pending_resize: None,
    })
}

/// How a pane gets pixels on screen.
enum Screen {
    /// Portable: decoded RGBA frames uploaded + blitted via SDL_GPU.
    Gpu { window: VideoWindow, source: FrameProducer },
    /// macOS zero-copy: the stream thread enqueues compressed samples straight
    /// to a CoreAnimation layer; the UI loop only reads the size for input.
    #[cfg(target_os = "macos")]
    Layer {
        window: sdl3::video::Window,
        _present: std::sync::Arc<crate::avlayer::AvLayerPresenter>,
        /// Packed (w << 32 | h), written by the stream thread on rotation.
        size: std::sync::Arc<std::sync::atomic::AtomicU64>,
        /// Frames enqueued since the HUD last sampled (fps counter).
        frames: std::sync::Arc<std::sync::atomic::AtomicU32>,
        _feed: androlon_stream::SampleFeed,
        /// Keeps the scrcpy server + tunnel alive for the pane's lifetime.
        _client: ScrcpyClient,
    },
}

fn pack_size(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

impl Screen {
    fn window_size(&self) -> (u32, u32) {
        match self {
            Screen::Gpu { window, .. } => window.size(),
            #[cfg(target_os = "macos")]
            Screen::Layer { window, .. } => window.size(),
        }
    }

    fn raw_window(&self) -> *mut sdl3_sys::video::SDL_Window {
        match self {
            Screen::Gpu { window, .. } => window.raw(),
            #[cfg(target_os = "macos")]
            Screen::Layer { window, .. } => window.raw(),
        }
    }

    /// Capture/release the mouse for shooter-style aiming (relative motion,
    /// hidden cursor).
    fn set_relative_mouse(&self, on: bool) {
        unsafe { sdl3_sys::mouse::SDL_SetWindowRelativeMouseMode(self.raw_window(), on) };
    }

    fn set_title(&mut self, title: &str) {
        match self {
            Screen::Gpu { window, .. } => window.set_title(title),
            #[cfg(target_os = "macos")]
            Screen::Layer { window, .. } => {
                let _ = window.set_title(title);
            }
        }
    }
}

/// Pointer ids for shooter mode (distinct from tap bindings 1.. and stick 0).
const AIM_POINTER: u64 = 100;
const FIRE_POINTER: u64 = 101;

/// Mouse-look state: the aim finger's current drag offset in stream pixels.
#[derive(Default)]
struct AimState {
    captured: bool,
    down: bool,
    off: (f32, f32),
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
    /// Game keybindings (keys → synthetic touches); None = plain keyboard.
    keymap: Option<Keymap>,
    joy: JoyState,
    /// Mouse-look (shooter mode) state; active only with an `aim` binding.
    aim: AimState,
    /// Base window title (the fps HUD appends to it).
    title: String,
    hud_t: std::time::Instant,
    /// Holds the runtime daemon's boot lease while this pane is open.
    _lease: Option<androlon_ipc::RuntimeLease>,
    /// What this pane is showing, so its stream can be rebuilt at a new size.
    /// `None` for mirror panes, which track the device's own display.
    respawn: Option<Respawn>,
    /// Window size seen at the last resize event, and when. The stream is
    /// only rebuilt once resizing has settled — a virtual display can't be
    /// resized in place, so each rebuild is a reconnect.
    pending_resize: Option<(std::time::Instant, u32, u32)>,
}

/// Everything needed to re-create a Coherence stream at a different size.
struct Respawn {
    cfg: SdkConfig,
    package: String,
    decorations: bool,
    audio: bool,
}

/// How long the window must sit still before we reconnect. Long enough that
/// a drag doesn't spawn a stream per frame, short enough to feel immediate.
const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(450);

impl VideoPane {
    /// Note a resize; the reconnect happens once the window settles.
    fn note_resize(&mut self, w: u32, h: u32) {
        if self.respawn.is_some() {
            self.pending_resize = Some((std::time::Instant::now(), w, h));
        }
    }

    /// If resizing has settled, rebuild the stream so Android renders at the
    /// window's true resolution instead of being upscaled into it.
    ///
    /// A `new_display` virtual display cannot be resized in place, so this is
    /// a full reconnect: new server, new display, same window and same
    /// CoreAnimation layer (the layer is what makes this invisible — the last
    /// frame stays on screen while the new stream comes up).
    fn maybe_rebuild(&mut self) {
        let Some((at, w, h)) = self.pending_resize else { return };
        if at.elapsed() < RESIZE_SETTLE {
            return;
        }
        self.pending_resize = None;
        let Some(respawn) = self.respawn.as_ref() else { return };

        // Retina-exact, as at first open: display pixels = 2x window points.
        let opts = ScrcpyOptions {
            new_display: Some((w * 2, h * 2)),
            new_display_dpi: Some(env_u32("ANDROLON_COHERENCE_DPI", 320)),
            start_app: Some(respawn.package.clone()),
            vd_system_decorations: respawn.decorations,
            max_fps: env_u32("ANDROLON_FPS", 60),
            video_bit_rate: env_u32("ANDROLON_BITRATE", 16_000_000),
            // Audio belongs to the pane's original stream; re-requesting it
            // would fight the still-running one for device capture.
            audio: false,
            ..ScrcpyOptions::default()
        };
        let mut client = ScrcpyClient::new(respawn.cfg.clone(), opts);
        if client.deploy_server().is_err() {
            return; // keep the current stream rather than dropping to nothing
        }
        let Ok((stream, _audio, control)) = client.start() else { return };

        #[cfg(target_os = "macos")]
        if let Screen::Layer { _present, size, frames, _feed, _client, .. } = &mut self.screen {
            use std::sync::atomic::Ordering;
            let (mw, mh) = (stream.meta().width, stream.meta().height);
            size.store(pack_size(mw, mh), Ordering::Relaxed);
            let present = std::sync::Arc::clone(_present);
            let size = std::sync::Arc::clone(size);
            let frames = std::sync::Arc::clone(frames);
            // Replacing the feed drops the old one, which closes its socket
            // and ends the previous server — and with it the old display.
            *_feed = androlon_stream::spawn_samples(stream, move |sample, (w, h)| {
                size.store(pack_size(w, h), Ordering::Relaxed);
                present.enqueue(&sample);
                frames.fetch_add(1, Ordering::Relaxed);
            });
            *_client = client;
            self.control = control;
            self.stream_size = (mw, mh);
        }
    }

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
            // Shooter mode: the capture key (default `, configurable via a
            // `capture` line) toggles mouse-look. Needs an `aim` binding.
            Event::KeyDown { keycode: Some(key), repeat: false, .. }
                if self
                    .keymap
                    .as_ref()
                    .is_some_and(|m| m.aim.is_some() && m.capture_key() == key) =>
            {
                if self.aim.captured {
                    self.release_capture();
                } else {
                    self.aim.captured = true;
                    self.screen.set_relative_mouse(true);
                }
            }
            // Captured: relative motion drags the aim finger.
            Event::MouseMotion { xrel, yrel, .. } if self.aim.captured => {
                self.aim_move(xrel, yrel);
            }
            // Captured: left click taps the game's fire button.
            Event::MouseButtonDown { mouse_btn: MouseButton::Left, .. } if self.aim.captured => {
                if let Some((fx, fy)) = self.keymap.as_ref().and_then(|m| m.fire) {
                    let pos = self.norm_pos(fx, fy);
                    let msg = control::touch_event(
                        control::ACTION_DOWN, FIRE_POINTER, pos, 1.0, 0, 0,
                    );
                    let _ = self.control.as_mut().unwrap().send(&msg);
                }
            }
            Event::MouseButtonUp { mouse_btn: MouseButton::Left, .. } if self.aim.captured => {
                if let Some((fx, fy)) = self.keymap.as_ref().and_then(|m| m.fire) {
                    let pos = self.norm_pos(fx, fy);
                    let msg =
                        control::touch_event(control::ACTION_UP, FIRE_POINTER, pos, 0.0, 0, 0);
                    let _ = self.control.as_mut().unwrap().send(&msg);
                }
            }
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
            Event::KeyDown { keycode: Some(key), keymod, repeat, .. } => {
                // Keymap bindings swallow the key (including auto-repeats);
                // unmapped keys go to Android as keyboard input.
                if let Some(action) = self.keymap.as_ref().and_then(|m| m.get(key)) {
                    if !repeat {
                        self.apply_binding(action, true);
                    }
                } else if let Some(ak) = input::android_keycode(key) {
                    let ctl = self.control.as_mut().unwrap();
                    let _ = ctl.send_key(control::ACTION_DOWN, ak, input::meta_of(keymod));
                }
            }
            Event::KeyUp { keycode: Some(key), keymod, .. } => {
                if let Some(action) = self.keymap.as_ref().and_then(|m| m.get(key)) {
                    self.apply_binding(action, false);
                } else if let Some(ak) = input::android_keycode(key) {
                    let ctl = self.control.as_mut().unwrap();
                    let _ = ctl.send_key(control::ACTION_UP, ak, input::meta_of(keymod));
                }
            }
            // Losing focus (Cmd-Tab etc.) always releases shooter capture.
            Event::Window { win_event: WindowEvent::FocusLost, .. } => {
                self.release_capture();
            }
            _ => {}
        }
    }

    fn norm_pos(&self, nx: f32, ny: f32) -> Position {
        let (sw, sh) = self.stream_size;
        Position {
            x: (nx * sw as f32) as i32,
            y: (ny * sh as f32) as i32,
            width: sw as u16,
            height: sh as u16,
        }
    }

    /// Leave shooter mode, lifting the aim finger if it's down.
    fn release_capture(&mut self) {
        if !self.aim.captured {
            return;
        }
        if self.aim.down {
            if let Some(cfg) = self.keymap.as_ref().and_then(|m| m.aim) {
                let (ox, oy) = self.aim.off;
                let (sw, sh) = self.stream_size;
                let pos = Position {
                    x: ((cfg.cx * sw as f32 + ox) as i32).clamp(0, sw as i32 - 1),
                    y: ((cfg.cy * sh as f32 + oy) as i32).clamp(0, sh as i32 - 1),
                    width: sw as u16,
                    height: sh as u16,
                };
                let msg = control::touch_event(control::ACTION_UP, AIM_POINTER, pos, 0.0, 0, 0);
                if let Some(ctl) = self.control.as_mut() {
                    let _ = ctl.send(&msg);
                }
            }
        }
        self.aim = AimState::default();
        self.screen.set_relative_mouse(false);
    }

    /// One relative mouse step in shooter mode: move the aim finger, silently
    /// re-anchoring (lift + re-press at center) when it nears the edge.
    fn aim_move(&mut self, xrel: f32, yrel: f32) {
        let Some(cfg) = self.keymap.as_ref().and_then(|m| m.aim) else {
            return;
        };
        let (sw, sh) = (self.stream_size.0 as f32, self.stream_size.1 as f32);
        let anchor = (cfg.cx * sw, cfg.cy * sh);
        let ctl_pos = |x: f32, y: f32, sw: f32, sh: f32| Position {
            x: (x as i32).clamp(0, sw as i32 - 1),
            y: (y as i32).clamp(0, sh as i32 - 1),
            width: sw as u16,
            height: sh as u16,
        };

        if !self.aim.down {
            self.aim.down = true;
            self.aim.off = (0.0, 0.0);
            let msg = control::touch_event(
                control::ACTION_DOWN, AIM_POINTER,
                ctl_pos(anchor.0, anchor.1, sw, sh), 1.0, 0, 0,
            );
            let _ = self.control.as_mut().unwrap().send(&msg);
        }
        self.aim.off.0 += xrel * cfg.sensitivity;
        self.aim.off.1 += yrel * cfg.sensitivity;
        let (x, y) = (anchor.0 + self.aim.off.0, anchor.1 + self.aim.off.1);
        let msg = control::touch_event(
            control::ACTION_MOVE, AIM_POINTER, ctl_pos(x, y, sw, sh), 1.0, 0, 0,
        );
        let _ = self.control.as_mut().unwrap().send(&msg);

        // Near an edge → lift and re-anchor so the next motion starts fresh.
        if x < sw * 0.05 || x > sw * 0.95 || y < sh * 0.05 || y > sh * 0.95 {
            let msg = control::touch_event(
                control::ACTION_UP, AIM_POINTER, ctl_pos(x, y, sw, sh), 0.0, 0, 0,
            );
            let _ = self.control.as_mut().unwrap().send(&msg);
            self.aim.down = false;
            self.aim.off = (0.0, 0.0);
        }
    }

    /// Turn a key binding press/release into synthetic touch traffic.
    fn apply_binding(&mut self, action: Action, down: bool) {
        let (sw, sh) = self.stream_size;
        let at = |nx: f32, ny: f32| Position {
            x: (nx * sw as f32) as i32,
            y: (ny * sh as f32) as i32,
            width: sw as u16,
            height: sh as u16,
        };
        match action {
            Action::Tap { x, y, pointer } => {
                let (act, pressure) = if down {
                    (control::ACTION_DOWN, 1.0)
                } else {
                    (control::ACTION_UP, 0.0)
                };
                let msg = control::touch_event(act, pointer, at(x, y), pressure, 0, 0);
                let _ = self.control.as_mut().unwrap().send(&msg);
            }
            Action::Joy { dx, dy } => {
                let Some(cfg) = self.keymap.as_ref().and_then(|m| m.joystick) else {
                    return;
                };
                if down {
                    self.joy.press(dx, dy);
                } else {
                    self.joy.release(dx, dy);
                }
                // Sync the persistent joystick finger with the held keys.
                let msg = match (self.joy.direction(), self.joy.down) {
                    (Some((jx, jy)), false) => {
                        self.joy.down = true;
                        // Engage like a thumb: touch DOWN at the stick center,
                        // then slide outward. A DOWN landing directly at the
                        // deflected position can fall outside the stick's hit
                        // zone and be read as a camera swipe instead.
                        let center = at(cfg.cx, cfg.cy);
                        let down =
                            control::touch_event(control::ACTION_DOWN, cfg.pointer, center, 1.0, 0, 0);
                        let _ = self.control.as_mut().unwrap().send(&down);
                        let pos = at(cfg.cx + jx * cfg.radius, cfg.cy + jy * cfg.radius);
                        control::touch_event(control::ACTION_MOVE, cfg.pointer, pos, 1.0, 0, 0)
                    }
                    (Some((jx, jy)), true) => {
                        let pos = at(cfg.cx + jx * cfg.radius, cfg.cy + jy * cfg.radius);
                        control::touch_event(control::ACTION_MOVE, cfg.pointer, pos, 1.0, 0, 0)
                    }
                    (None, true) => {
                        self.joy.down = false;
                        let pos = at(cfg.cx, cfg.cy);
                        control::touch_event(control::ACTION_UP, cfg.pointer, pos, 0.0, 0, 0)
                    }
                    (None, false) => return,
                };
                let _ = self.control.as_mut().unwrap().send(&msg);
            }
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

    // Bring the runtime up first, staying responsive while it boots.
    let lease = wait_for_runtime(&sdl);
    // The single-app pane owns the device audio (capture is device-wide).
    let sound = sdl.audio().ok();
    let mut pane = match open_coherence_surface(&video, &cfg, package, false, sound.as_ref(), lease)
    {
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
                Event::Window { win_event: WindowEvent::Resized(w, h), .. } => {
                    pane.note_resize(w.max(1) as u32, h.max(1) as u32);
                }
                ref e => pane.handle_input(e),
            }
        }

        pane.maybe_rebuild();

        let VideoPane { screen, stream_size, title, hud_t, .. } = &mut pane;
        let mut hud_fps: Option<u32> = None;
        match screen {
            Screen::Gpu { window, source } => {
                if let Some(frame) = source.next_frame() {
                    *stream_size = (frame.width, frame.height);
                    let _ = window.present(&frame);
                }
            }
            #[cfg(target_os = "macos")]
            Screen::Layer { size, frames, .. } => {
                *stream_size = unpack_size(size.load(std::sync::atomic::Ordering::Relaxed));
                if hud_t.elapsed() >= std::time::Duration::from_secs(1) {
                    *hud_t = std::time::Instant::now();
                    hud_fps = Some(frames.swap(0, std::sync::atomic::Ordering::Relaxed));
                }
            }
        }
        if let Some(fps) = hud_fps {
            screen.set_title(&format!("{title} — {fps} fps"));
        }

        // Frames bypass this loop (stream thread → layer); it only services
        // input, so poll tightly for low input latency.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
