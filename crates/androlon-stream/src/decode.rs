//! Decoder abstraction: encoded packets → RGBA frames for the SDL_GPU texture.
//!
//! `Openh264Decoder` is the cross-platform software path (H.264). Hardware
//! decoders (VideoToolbox on macOS, ffmpeg vaapi/nvdec on Linux/Windows) slot in
//! behind the same `VideoDecoder` trait later. `NullDecoder` keeps the pipeline
//! runnable (minus pixels) with no codec.

use crate::error::{Result, StreamError};
use crate::model::{DecodedFrame, EncodedPacket};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

pub trait VideoDecoder {
    /// Feed one encoded packet. Returns a frame when one is ready (decoders may
    /// buffer, and need the config/SPS-PPS packet before the first frame).
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedFrame>>;
    fn name(&self) -> &'static str;
}

/// Placeholder: consumes packets, produces no frames. Useful for measuring
/// stream throughput without a codec.
#[derive(Default)]
pub struct NullDecoder {
    pub packets: u64,
    pub config_packets: u64,
}

impl VideoDecoder for NullDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedFrame>> {
        self.packets += 1;
        if packet.is_config {
            self.config_packets += 1;
        }
        Ok(None)
    }
    fn name(&self) -> &'static str {
        "null (no decode)"
    }
}

/// Software H.264 decoder (OpenH264). scrcpy sends Annex-B NAL units; the config
/// packet carries SPS/PPS, after which frames decode to I420, which we convert
/// straight to RGBA for upload.
pub struct Openh264Decoder {
    inner: Decoder,
    rgba: Vec<u8>, // reused scratch buffer
}

impl Openh264Decoder {
    pub fn new() -> Result<Self> {
        let inner =
            Decoder::new().map_err(|e| StreamError::Protocol(format!("openh264 init: {e}")))?;
        Ok(Openh264Decoder { inner, rgba: Vec::new() })
    }
}

impl VideoDecoder for Openh264Decoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedFrame>> {
        let maybe = self
            .inner
            .decode(&packet.data)
            .map_err(|e| StreamError::Protocol(format!("openh264 decode: {e}")))?;
        let Some(yuv) = maybe else { return Ok(None) };

        let (w, h) = yuv.dimensions();
        let needed = w * h * 4;
        self.rgba.resize(needed, 0);
        yuv.write_rgba8(&mut self.rgba);
        Ok(Some(DecodedFrame {
            width: w as u32,
            height: h as u32,
            rgba: std::mem::take(&mut self.rgba),
        }))
    }
    fn name(&self) -> &'static str {
        "openh264 (software)"
    }
}
