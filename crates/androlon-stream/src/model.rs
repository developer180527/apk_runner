/// Video codec advertised by the scrcpy server (4-byte id at stream start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Unknown(u32),
}

impl Codec {
    pub fn from_id(id: u32) -> Codec {
        match id {
            0x6832_3634 => Codec::H264, // "h264"
            0x6832_3635 => Codec::H265, // "h265"
            0x0061_7631 => Codec::Av1,  // "av1\0"
            other => Codec::Unknown(other),
        }
    }
    pub fn label(self) -> String {
        match self {
            Codec::H264 => "h264".into(),
            Codec::H265 => "h265".into(),
            Codec::Av1 => "av1".into(),
            Codec::Unknown(id) => format!("unknown(0x{id:08x})"),
        }
    }
}

/// Metadata read once at the start of a video stream.
#[derive(Debug, Clone)]
pub struct StreamMeta {
    pub device_name: String,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

/// One encoded video packet from the stream (a NAL/frame, or codec config).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub pts: u64,
    pub is_config: bool,
    pub is_keyframe: bool,
    pub data: Vec<u8>,
}

/// A decoded frame ready to upload as an SDL_GPU texture.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8 (width*height*4 bytes).
    pub rgba: Vec<u8>,
}
