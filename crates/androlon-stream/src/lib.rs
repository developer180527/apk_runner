//! Androlon streaming: a scrcpy client that turns an Android virtual display
//! into a stream of encoded packets, a decoder that yields RGBA frames for the
//! SDL_GPU compositor, and the threaded stream→decode→present plumbing.

pub mod control;
pub mod decode;
pub mod encode;
pub mod error;
pub mod model;
#[cfg(target_os = "macos")]
pub mod samplebuffer;
pub mod scrcpy;
pub mod session;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

pub use control::{ControlChannel, Position};
pub use decode::{make_decoder, DecoderKind, NullDecoder, Openh264Decoder, VideoDecoder};
pub use encode::TestEncoder;
pub use error::{Result, StreamError};
pub use model::{Codec, DecodedFrame, EncodedPacket, StreamMeta};
pub use scrcpy::{ScrcpyClient, ScrcpyOptions, VideoStream};
pub use session::{spawn_decode, FrameStream, PacketSource};
#[cfg(target_os = "macos")]
pub use session::{spawn_samples, SampleStream};
