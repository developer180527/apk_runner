//! The stream→decode→present plumbing. A background thread reads encoded
//! packets from a `PacketSource` and decodes them; RGBA frames arrive on a
//! channel that the main (GPU) thread drains and presents. Keeping decode off
//! the main thread stops a slow frame from stalling the UI.

use crate::decode::{make_decoder, DecoderKind};
use crate::error::Result;
use crate::model::{DecodedFrame, EncodedPacket};
use crate::scrcpy::VideoStream;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

/// Anything that yields encoded packets. The real source is the scrcpy
/// `VideoStream`; tests can supply a synthetic one. Must be `Send` to move onto
/// the decode thread.
pub trait PacketSource: Send {
    fn next_packet(&mut self) -> Result<EncodedPacket>;
}

impl PacketSource for VideoStream {
    fn next_packet(&mut self) -> Result<EncodedPacket> {
        self.read_packet()
    }
}

/// Handle to a running decode thread; `rx` delivers decoded RGBA frames.
pub struct FrameStream {
    pub rx: Receiver<DecodedFrame>,
    _handle: JoinHandle<()>,
}

/// Zero-copy feed (macOS): no decode thread at all — compressed packets are
/// wrapped as CMSampleBuffers and handed to `sink` *on this thread*, straight
/// off the socket. The sink enqueues to AVSampleBufferDisplayLayer (safe from
/// any thread), so a frame reaches the display the moment it arrives — no
/// main-loop polling jitter in the frame path. Every sample is forwarded
/// (H.264 decode order matters; the *layer* is the decoder, nothing may be
/// dropped). The stream size current at read time rides along so the UI can
/// map input coordinates without decoding anything.
///
/// Returns a guard; dropping it detaches (the thread ends when the stream
/// closes, e.g. on client Drop tearing down the tunnel).
#[cfg(target_os = "macos")]
pub struct SampleFeed {
    _handle: JoinHandle<()>,
}

#[cfg(target_os = "macos")]
pub fn spawn_samples<F>(mut stream: VideoStream, mut sink: F) -> SampleFeed
where
    F: FnMut(crate::samplebuffer::Sample, (u32, u32)) + Send + 'static,
{
    let handle = thread::spawn(move || {
        let mut assembler = crate::samplebuffer::SampleAssembler::new();
        loop {
            let packet = match stream.read_packet() {
                Ok(p) => p,
                Err(_) => break, // stream closed / error → end the loop
            };
            let size = (stream.meta.width, stream.meta.height);
            match assembler.push(&packet) {
                Ok(Some(sample)) => sink(sample, size),
                Ok(None) => {} // config packet
                Err(_) => {}   // skip a malformed packet, keep going
            }
        }
    });
    SampleFeed { _handle: handle }
}

/// Spawn the decode loop. The OpenH264 decoder is created *inside* the thread
/// (it isn't `Send`), so only the `PacketSource` must cross the boundary.
pub fn spawn_decode<S: PacketSource + 'static>(mut source: S) -> FrameStream {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut decoder = match make_decoder(DecoderKind::from_env()) {
            Ok(d) => d,
            Err(_) => return,
        };
        loop {
            let packet = match source.next_packet() {
                Ok(p) => p,
                Err(_) => break, // stream closed / error → end the loop
            };
            match decoder.decode(&packet) {
                Ok(Some(frame)) => {
                    if tx.send(frame).is_err() {
                        break; // receiver dropped
                    }
                }
                Ok(None) => {}  // buffered / config packet
                Err(_) => {}    // skip an undecodable packet, keep going
            }
        }
    });
    FrameStream { rx, _handle: handle }
}
