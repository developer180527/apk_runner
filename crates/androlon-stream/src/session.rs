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
