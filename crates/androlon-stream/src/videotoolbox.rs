//! macOS hardware H.264 decode via VideoToolbox — hand-rolled framework FFI
//! (raw C signatures, no crate deps). Decodes on the GPU instead of the CPU,
//! removing the software-decode bottleneck. Phase 1 outputs BGRA→RGBA; Phase 2
//! will keep the IOSurface for a zero-copy Metal present.
//!
//! Flow: config packet → CMVideoFormatDescription from SPS/PPS; then per media
//! packet → Annex-B→AVCC → CMBlockBuffer → CMSampleBuffer → decode → callback
//! reads the CVPixelBuffer into a DecodedFrame.

#![allow(non_upper_case_globals, non_snake_case)]

use crate::decode::VideoDecoder;
use crate::error::{Result, StreamError};
use crate::model::{DecodedFrame, EncodedPacket};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;

// ---- opaque Core Foundation / Core Media / Video Toolbox handles ----
type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CMFormatDescriptionRef = *const c_void; // a.k.a. CMVideoFormatDescriptionRef
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type VTDecompressionSessionRef = *mut c_void;
type CVImageBufferRef = *mut c_void; // toll-free with CVPixelBufferRef

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type VTDecompressionOutputCallback = extern "C" fn(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVImageBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: Option<VTDecompressionOutputCallback>,
    refcon: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    /// A real allocator whose dealloc is a no-op — for referencing memory we own.
    static kCFAllocatorNull: CFAllocatorRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFNumberCreate(allocator: CFAllocatorRef, the_type: isize, value_ptr: *const c_void) -> CFNumberRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const c_void,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVImageBufferRef, lock_flags: u64) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVImageBufferRef, lock_flags: u64) -> OSStatus;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVImageBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVImageBufferRef) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

const kCVPixelFormatType_32BGRA: i32 = 0x4247_5241; // 'BGRA'
const kCFNumberSInt32Type: isize = 3;
const kCVPixelBufferLock_ReadOnly: u64 = 1;

/// Where the decode callback deposits its result (pointed to by the session's
/// refcon). Lives in a Box so its address is stable across struct moves.
struct Sink {
    frame: Option<DecodedFrame>,
    error: Option<String>,
}

extern "C" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVImageBufferRef,
    _pts: CMTime,
    _dur: CMTime,
) {
    if refcon.is_null() {
        return;
    }
    let sink = unsafe { &mut *(refcon as *mut Sink) };
    if status != 0 {
        sink.error = Some(format!("decode status {status}"));
        return;
    }
    if image_buffer.is_null() {
        return;
    }
    unsafe {
        if CVPixelBufferLockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly) != 0 {
            sink.error = Some("CVPixelBufferLockBaseAddress failed".into());
            return;
        }
        let w = CVPixelBufferGetWidth(image_buffer);
        let h = CVPixelBufferGetHeight(image_buffer);
        let stride = CVPixelBufferGetBytesPerRow(image_buffer);
        let base = CVPixelBufferGetBaseAddress(image_buffer) as *const u8;
        if !base.is_null() && w > 0 && h > 0 {
            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = base.add(y * stride);
                for x in 0..w {
                    let px = row.add(x * 4);
                    let (b, g, r, a) = (*px, *px.add(1), *px.add(2), *px.add(3));
                    let o = (y * w + x) * 4;
                    rgba[o] = r;
                    rgba[o + 1] = g;
                    rgba[o + 2] = b;
                    rgba[o + 3] = a;
                }
            }
            sink.frame = Some(DecodedFrame { width: w as u32, height: h as u32, rgba });
        }
        CVPixelBufferUnlockBaseAddress(image_buffer, kCVPixelBufferLock_ReadOnly);
    }
}

pub struct VideoToolboxDecoder {
    session: VTDecompressionSessionRef,
    format: CMFormatDescriptionRef,
    sink: Box<Sink>,
}

impl VideoToolboxDecoder {
    pub fn new() -> Result<Self> {
        Ok(VideoToolboxDecoder {
            session: ptr::null_mut(),
            format: ptr::null(),
            sink: Box::new(Sink { frame: None, error: None }),
        })
    }

    /// Build the format description + session from the SPS/PPS in a config packet.
    fn setup(&mut self, config: &[u8]) -> Result<()> {
        let (mut sps, mut pps) = (None, None);
        for nal in nal_units(config) {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) => sps = Some(nal.to_vec()),
                Some(8) => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        let sps = sps.ok_or_else(|| StreamError::Protocol("no SPS in config packet".into()))?;
        let pps = pps.ok_or_else(|| StreamError::Protocol("no PPS in config packet".into()))?;

        unsafe {
            let ptrs = [sps.as_ptr(), pps.as_ptr()];
            let sizes = [sps.len(), pps.len()];
            let mut fmt: CMFormatDescriptionRef = ptr::null();
            let st = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(), 2, ptrs.as_ptr(), sizes.as_ptr(), 4, &mut fmt,
            );
            if st != 0 || fmt.is_null() {
                return Err(StreamError::Protocol(format!("format desc create failed ({st})")));
            }

            // Destination attributes: request 32BGRA output.
            let fmt_num = CFNumberCreate(
                ptr::null(),
                kCFNumberSInt32Type,
                &kCVPixelFormatType_32BGRA as *const i32 as *const c_void,
            );
            let keys = [kCVPixelBufferPixelFormatTypeKey];
            let values = [fmt_num];
            let dict = CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks as *const c_void,
                &kCFTypeDictionaryValueCallBacks as *const c_void,
            );

            let record = VTDecompressionOutputCallbackRecord {
                callback: Some(output_callback),
                refcon: &mut *self.sink as *mut Sink as *mut c_void,
            };
            let mut session: VTDecompressionSessionRef = ptr::null_mut();
            let st = VTDecompressionSessionCreate(
                ptr::null(), fmt, ptr::null(), dict, &record, &mut session,
            );
            CFRelease(fmt_num);
            CFRelease(dict);
            if st != 0 || session.is_null() {
                CFRelease(fmt);
                return Err(StreamError::Protocol(format!("VT session create failed ({st})")));
            }
            self.format = fmt;
            self.session = session;
        }
        Ok(())
    }

    fn decode_frame(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let avcc = annexb_to_avcc(data);
        if avcc.is_empty() {
            return Ok(None);
        }
        self.sink.frame = None;
        self.sink.error = None;

        unsafe {
            let mut block: CMBlockBufferRef = ptr::null();
            let st = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                avcc.as_ptr() as *mut c_void, // referenced, not owned…
                avcc.len(),
                kCFAllocatorNull, // …so don't let CoreMedia free it
                ptr::null(),
                0,
                avcc.len(),
                0,
                &mut block,
            );
            if st != 0 || block.is_null() {
                return Err(StreamError::Protocol(format!("block buffer create failed ({st})")));
            }

            let sizes = [avcc.len()];
            let mut sample: CMSampleBufferRef = ptr::null();
            let st = CMSampleBufferCreateReady(
                ptr::null(), block, self.format, 1, 0, ptr::null(), 1, sizes.as_ptr(), &mut sample,
            );
            if st != 0 || sample.is_null() {
                CFRelease(block);
                return Err(StreamError::Protocol(format!("sample buffer create failed ({st})")));
            }

            let mut info = 0u32;
            let st = VTDecompressionSessionDecodeFrame(self.session, sample, 0, ptr::null_mut(), &mut info);
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            CFRelease(sample);
            CFRelease(block);
            // avcc stays alive until here — safe.

            if st != 0 {
                return Err(StreamError::Protocol(format!("decode frame failed ({st})")));
            }
        }

        if let Some(err) = self.sink.error.take() {
            return Err(StreamError::Protocol(err));
        }
        Ok(self.sink.frame.take())
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
    }
}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedFrame>> {
        if packet.is_config {
            if self.session.is_null() {
                self.setup(&packet.data)?;
            }
            return Ok(None);
        }
        if self.session.is_null() {
            return Ok(None); // waiting for the config/SPS-PPS packet
        }
        self.decode_frame(&packet.data)
    }
    fn name(&self) -> &'static str {
        "videotoolbox (hardware)"
    }
}

/// Split an Annex-B buffer into NAL payloads (start codes removed).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let n = data.len();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (position, start-code length)
    let mut j = 0;
    while j + 3 <= n {
        if j + 4 <= n && data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 0 && data[j + 3] == 1 {
            starts.push((j, 4));
            j += 4;
        } else if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
            starts.push((j, 3));
            j += 3;
        } else {
            j += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for k in 0..starts.len() {
        let (pos, sc) = starts[k];
        let payload_start = pos + sc;
        let payload_end = if k + 1 < starts.len() { starts[k + 1].0 } else { n };
        if payload_end > payload_start {
            nals.push(&data[payload_start..payload_end]);
        }
    }
    nals
}

/// Convert Annex-B (start codes) to AVCC (4-byte big-endian length prefixes),
/// which is what VideoToolbox expects (nal_unit_header_length = 4).
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in nal_units(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}
