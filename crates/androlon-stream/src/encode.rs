//! H.264 encoder — used only to self-verify the decode→present path without a
//! device: encode synthetic RGBA frames, then feed them through the decoder.
//! (The real pipeline gets its packets from the device's hardware encoder via
//! scrcpy; this is purely a test/demo source.)

use crate::error::{Result, StreamError};
use crate::model::EncodedPacket;
use openh264::encoder::Encoder;
use openh264::formats::{RgbaSliceU8, YUVBuffer};

pub struct TestEncoder {
    inner: Encoder,
    frame: u64,
}

impl TestEncoder {
    pub fn new() -> Result<Self> {
        let inner =
            Encoder::new().map_err(|e| StreamError::Protocol(format!("openh264 enc init: {e}")))?;
        Ok(TestEncoder { inner, frame: 0 })
    }

    /// Encode one RGBA frame to an Annex-B packet (the first frame is an IDR
    /// carrying SPS/PPS, so the decoder can start immediately).
    pub fn encode_rgba(&mut self, rgba: &[u8], w: u32, h: u32) -> Result<EncodedPacket> {
        let yuv = YUVBuffer::from_rgb_source(RgbaSliceU8::new(rgba, (w as usize, h as usize)));
        let bitstream = self
            .inner
            .encode(&yuv)
            .map_err(|e| StreamError::Protocol(format!("openh264 encode: {e}")))?;
        let data = bitstream.to_vec();
        let is_keyframe = self.frame == 0;
        self.frame += 1;
        Ok(EncodedPacket { pts: self.frame, is_config: false, is_keyframe, data })
    }
}
