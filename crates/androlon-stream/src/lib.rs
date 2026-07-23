//! Androlon streaming: a scrcpy client that turns an Android virtual display
//! into a stream of encoded packets, a decoder that yields RGBA frames for the
//! SDL_GPU compositor, and the threaded stream→decode→present plumbing.

pub mod decode;
pub mod encode;
pub mod error;
pub mod model;
pub mod scrcpy;
pub mod session;

pub use decode::{NullDecoder, Openh264Decoder, VideoDecoder};
pub use encode::TestEncoder;
pub use error::{Result, StreamError};
pub use model::{Codec, DecodedFrame, EncodedPacket, StreamMeta};
pub use scrcpy::{ScrcpyClient, ScrcpyOptions, VideoStream};
pub use session::{spawn_decode, FrameStream, PacketSource};
