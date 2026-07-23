//! scrcpy client: deploy + launch the server on the device, tunnel a TCP
//! socket, and read the video stream. Protocol per scrcpy's develop.md
//! (forward-tunnel variant). The server .jar version MUST match `server_version`.
//!
//! End-to-end operation needs (a) the bundled `scrcpy-server` binary and (b) a
//! booted device; this module implements the client side faithfully so those
//! are the only remaining pieces.

use crate::control::ControlChannel;
use crate::error::{Result, StreamError};
use crate::model::{Codec, EncodedPacket, StreamMeta};
use androlon_core::subprocess::spawn_detached;
use androlon_core::{AdbService, SdkConfig};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

const DEVICE_NAME_LEN: usize = 64;
// scrcpy v4.x packet-header flags (byte 0 of the 12-byte header):
//   bit 63 = session packet marker (resolution change, no payload)
//   bit 62 = config packet, bit 61 = key frame, low 61 bits = PTS
const SESSION_FLAG: u8 = 0x80; // header[0] MSB
const CONFIG_FLAG: u64 = 1 << 62;
const KEYFRAME_FLAG: u64 = 1 << 61;
const PTS_MASK: u64 = (1 << 61) - 1;
const DEVICE_SOCKET: &str = "/data/local/tmp/scrcpy-server.jar";

pub struct ScrcpyOptions {
    /// Local path to the `scrcpy-server` binary to push to the device.
    pub server_jar: PathBuf,
    /// Must exactly match the server jar (e.g. "2.7").
    pub server_version: String,
    /// Random 31-bit id (hex) so multiple instances don't collide.
    pub scid: String,
    /// Local TCP port for the forward tunnel.
    pub port: u16,
    /// 0 = device resolution; else clamp the longer side to this many px.
    pub max_size: u32,
    /// Target frame rate (0 = server default). Games want an explicit 60+.
    pub max_fps: u32,
    /// Encoder bitrate in bits/s (0 = server default 8 Mbps). Higher = less
    /// compression artifacting in fast motion; localhost bandwidth is free.
    pub video_bit_rate: u32,
    /// Android display id to capture (0 = default; others = per-app in Coherence).
    pub display_id: u32,
    /// Open the control channel (input injection). The server then expects a
    /// second connection on the same tunnel; `start()` makes it.
    pub control: bool,
    /// Coherence: create a fresh virtual display of this size and capture it
    /// instead of `display_id`. Each app pane gets its own display, sized by
    /// us — so its window is always pixel-exact, never letterboxed.
    pub new_display: Option<(u32, u32)>,
    /// Android density for the new display (None = device default). Pair the
    /// pixel size with a matching density or the UI renders the wrong scale:
    /// e.g. a Retina-sized display (2× window points) wants dpi 320 (2× mdpi).
    pub new_display_dpi: Option<u32>,
    /// Launch this package on the captured display once the stream starts.
    /// Sent as a START_APP *control message* after connecting (it is not a
    /// server option) — so it requires `control: true`. With `new_display`,
    /// the app opens on that display.
    pub start_app: Option<String>,
    /// Show Android system decorations (nav/status/taskbar) on a new virtual
    /// display. `false` = pure-app surface for native-feeling windows.
    pub vd_system_decorations: bool,
}

/// Per-process counter so concurrent clients (multiple Coherence panes) get
/// distinct forward-tunnel ports and server ids.
static NEXT_CLIENT: AtomicU16 = AtomicU16::new(0);

impl Default for ScrcpyOptions {
    fn default() -> Self {
        let n = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        ScrcpyOptions {
            server_jar: PathBuf::from("vendor/scrcpy-server"),
            server_version: "4.1".into(),
            scid: format!("{:08x}", ((std::process::id() & 0x7fff) << 16) | n as u32),
            port: 27183 + n,
            max_size: 0,
            max_fps: 0,
            video_bit_rate: 0,
            display_id: 0,
            control: true,
            new_display: None,
            new_display_dpi: None,
            start_app: None,
            vd_system_decorations: true,
        }
    }
}

pub struct ScrcpyClient {
    cfg: SdkConfig,
    opts: ScrcpyOptions,
    server: Option<Child>,
}

impl ScrcpyClient {
    pub fn new(cfg: SdkConfig, opts: ScrcpyOptions) -> Self {
        ScrcpyClient { cfg, opts, server: None }
    }

    fn adb(&self) -> AdbService<'_> {
        AdbService::new(&self.cfg)
    }

    fn adb_ok(&self, args: &[&str]) -> Result<()> {
        self.adb()
            .adb(args)
            .map_err(|e| StreamError::Engine(e.to_string()))
            .and_then(|o| {
                if o.ok() {
                    Ok(())
                } else {
                    Err(StreamError::Engine(o.trimmed().to_string()))
                }
            })
    }

    /// Push the server jar to the device.
    pub fn deploy_server(&self) -> Result<()> {
        let jar = self.opts.server_jar.display().to_string();
        if !self.opts.server_jar.exists() {
            return Err(StreamError::ServerNotFound(jar));
        }
        self.adb_ok(&["push", &jar, DEVICE_SOCKET])
    }

    /// Set up the forward tunnel, launch the server, connect, and read metadata.
    /// With `control` on, a second connection on the same tunnel becomes the
    /// input-injection channel (the server accepts video first, then control).
    pub fn start(&mut self) -> Result<(VideoStream, Option<ControlChannel>)> {
        // Probe for a free local port: the in-process counter can't see other
        // Androlon processes (each appified .app is its own process), but a
        // port held by one — or by an active adb forward — fails the bind.
        for candidate in self.opts.port..self.opts.port + 100 {
            if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
                self.opts.port = candidate;
                break;
            }
        }
        let socket_name = format!("localabstract:scrcpy_{}", self.opts.scid);
        let tcp = format!("tcp:{}", self.opts.port);

        // Forward: computer connects to the device's abstract socket.
        self.adb_ok(&["forward", &tcp, &socket_name])?;

        // Launch the server detached (long-running).
        let ver = self.opts.server_version.clone();
        let scid = format!("scid={}", self.opts.scid);
        let max_size = format!("max_size={}", self.opts.max_size);
        // Coherence: a fresh virtual display per pane, instead of mirroring an
        // existing display id.
        let display = match self.opts.new_display {
            Some((w, h)) => match self.opts.new_display_dpi {
                Some(dpi) => format!("new_display={w}x{h}/{dpi}"),
                None => format!("new_display={w}x{h}"),
            },
            None => format!("display_id={}", self.opts.display_id),
        };
        let decorations = (!self.opts.vd_system_decorations)
            .then(|| "vd_system_decorations=false".to_string());
        let max_fps = (self.opts.max_fps > 0).then(|| format!("max_fps={}", self.opts.max_fps));
        let bit_rate = (self.opts.video_bit_rate > 0)
            .then(|| format!("video_bit_rate={}", self.opts.video_bit_rate));
        // With control=true the server waits for a *second* connection on the
        // same tunnel before proceeding; we make it below, right after the
        // video socket's dummy byte proves the server is up.
        let control = format!("control={}", self.opts.control);
        let server_args: Vec<&str> = vec![
            "shell",
            "CLASSPATH=/data/local/tmp/scrcpy-server.jar",
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            &ver,
            "tunnel_forward=true",
            "audio=false",
            &control,
            "cleanup=true",
            "video_codec=h264",
            // Request the metadata our parser expects. (send_codec_meta was
            // removed in scrcpy 4.x — codec header is always sent now.)
            "send_device_meta=true",
            "send_frame_meta=true",
            &scid,
            &max_size,
            &display,
        ];
        let mut server_args = server_args;
        if let Some(d) = decorations.as_deref() {
            server_args.push(d);
        }
        if let Some(f) = max_fps.as_deref() {
            server_args.push(f);
        }
        if let Some(b) = bit_rate.as_deref() {
            server_args.push(b);
        }
        let log = self.cfg.sdk_root.join(".scrcpy-server.log");
        let child = spawn_detached(
            &self.cfg.adb(),
            &server_args,
            &self.cfg.tool_dirs(),
            &self.cfg.tool_env(),
            &log,
        )
        .map_err(|e| StreamError::Engine(e.to_string()))?;
        self.server = Some(child);

        // The server needs a moment to create the abstract socket.
        let stream = connect_retry(self.opts.port, 50, Duration::from_millis(100))?;

        // Control socket: second connect, no dummy byte (that was video-only).
        // Must happen before read_meta — the server only proceeds to the
        // handshake once every expected socket is connected.
        let mut control = if self.opts.control {
            let ctl = TcpStream::connect(("127.0.0.1", self.opts.port))?;
            Some(ControlChannel::new(ctl))
        } else {
            None
        };

        let meta = read_meta(&stream)?;

        // Coherence: ask the server to launch the app on this connection's
        // display. After the handshake — the server processes control messages
        // once streaming is set up.
        if let Some(pkg) = self.opts.start_app.clone() {
            match control.as_mut() {
                Some(ctl) => ctl.send_start_app(&pkg)?,
                None => {
                    return Err(StreamError::Protocol(
                        "start_app requires the control channel (control=true)".into(),
                    ))
                }
            }
        }

        Ok((VideoStream { stream, meta }, control))
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
        }
        let tcp = format!("tcp:{}", self.opts.port);
        let _ = self.adb_ok(&["forward", "--remove", &tcp]);
    }
}

impl Drop for ScrcpyClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connect to the forward tunnel AND read scrcpy's dummy byte, retrying both
/// together. In forward mode adb accepts the TCP connection immediately — even
/// before the server has created its socket — then closes it (→ EOF on the
/// dummy read). So a working connection is only proven once the dummy byte
/// actually arrives; keep retrying until it does (or the server never comes up).
fn connect_retry(port: u16, tries: u32, delay: Duration) -> Result<TcpStream> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..tries {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(1000)));
                let mut dummy = [0u8; 1];
                match (&stream).read_exact(&mut dummy) {
                    Ok(()) => {
                        let _ = stream.set_read_timeout(None);
                        return Ok(stream); // dummy consumed; real stream follows
                    }
                    Err(e) => last = Some(e), // server socket not ready — retry
                }
            }
            Err(e) => last = Some(e),
        }
        std::thread::sleep(delay);
    }
    Err(StreamError::Io(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")
    })))
}

/// Read the handshake after the dummy byte: 64-byte device name, codec id, then
/// the session packet carrying the initial resolution.
fn read_meta(mut stream: &TcpStream) -> Result<StreamMeta> {
    let mut name_buf = [0u8; DEVICE_NAME_LEN];
    stream.read_exact(&mut name_buf)?;
    let end = name_buf.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    let device_name = String::from_utf8_lossy(&name_buf[..end]).into_owned();

    let mut codec_buf = [0u8; 4];
    stream.read_exact(&mut codec_buf)?;
    let codec = Codec::from_id(u32::from_be_bytes(codec_buf));

    // v4.x: dimensions arrive as a 12-byte "session" packet (MSB set), with
    // width at bytes 4..8 and height at bytes 8..12.
    let mut session = [0u8; 12];
    stream.read_exact(&mut session)?;
    if session[0] & SESSION_FLAG == 0 {
        return Err(StreamError::Protocol(
            "expected a session header (resolution) after codec id".into(),
        ));
    }
    let width = u32::from_be_bytes(session[4..8].try_into().unwrap());
    let height = u32::from_be_bytes(session[8..12].try_into().unwrap());

    Ok(StreamMeta { device_name, codec, width, height })
}

/// A connected video stream. Call `read_packet` in a loop.
pub struct VideoStream {
    stream: TcpStream,
    pub meta: StreamMeta,
}

impl VideoStream {
    pub fn meta(&self) -> &StreamMeta {
        &self.meta
    }

    /// Read the next encoded media packet (12-byte header + payload). Session
    /// packets (resolution changes on rotation) are consumed transparently,
    /// updating `meta`, and we continue to the next media packet.
    pub fn read_packet(&mut self) -> Result<EncodedPacket> {
        loop {
            let mut header = [0u8; 12];
            self.stream.read_exact(&mut header)?;

            if header[0] & SESSION_FLAG != 0 {
                // Session packet: new resolution, no payload. Update and loop.
                self.meta.width = u32::from_be_bytes(header[4..8].try_into().unwrap());
                self.meta.height = u32::from_be_bytes(header[8..12].try_into().unwrap());
                continue;
            }

            let pts_flags = u64::from_be_bytes(header[0..8].try_into().unwrap());
            let is_config = pts_flags & CONFIG_FLAG != 0;
            let is_keyframe = pts_flags & KEYFRAME_FLAG != 0;
            let pts = pts_flags & PTS_MASK;
            let len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;

            let mut data = vec![0u8; len];
            self.stream.read_exact(&mut data)?;
            return Ok(EncodedPacket { pts, is_config, is_keyframe, data });
        }
    }
}
