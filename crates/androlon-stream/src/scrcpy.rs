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
    /// Android display id to capture (0 = default; others = per-app in Coherence).
    pub display_id: u32,
    /// Open the control channel (input injection). The server then expects a
    /// second connection on the same tunnel; `start()` makes it.
    pub control: bool,
}

impl Default for ScrcpyOptions {
    fn default() -> Self {
        ScrcpyOptions {
            server_jar: PathBuf::from("vendor/scrcpy-server"),
            server_version: "4.1".into(),
            scid: format!("{:08x}", std::process::id() & 0x7fff_ffff),
            port: 27183,
            max_size: 0,
            display_id: 0,
            control: true,
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
        let socket_name = format!("localabstract:scrcpy_{}", self.opts.scid);
        let tcp = format!("tcp:{}", self.opts.port);

        // Forward: computer connects to the device's abstract socket.
        self.adb_ok(&["forward", &tcp, &socket_name])?;

        // Launch the server detached (long-running).
        let ver = self.opts.server_version.clone();
        let scid = format!("scid={}", self.opts.scid);
        let max_size = format!("max_size={}", self.opts.max_size);
        let display = format!("display_id={}", self.opts.display_id);
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
        let control = if self.opts.control {
            let ctl = TcpStream::connect(("127.0.0.1", self.opts.port))?;
            Some(ControlChannel::new(ctl))
        } else {
            None
        };

        let meta = read_meta(&stream)?;
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
