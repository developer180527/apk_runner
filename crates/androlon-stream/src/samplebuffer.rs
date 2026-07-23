//! scrcpy packets → owned `CMSampleBuffer`s for AVSampleBufferDisplayLayer.
//!
//! This is the zero-copy presentation feed: instead of decoding to RGBA
//! ourselves, compressed H.264 samples are handed to CoreAnimation, which
//! decodes, scales, and presents entirely on the GPU. The only CPU copy left
//! is the compressed bitstream (a few KB/frame) into a CMBlockBuffer that the
//! sample owns — required because the layer holds samples asynchronously.
//!
//! Hand-rolled framework FFI, same style as `videotoolbox`.

#![allow(non_upper_case_globals, non_snake_case)]

use crate::error::{Result, StreamError};
use crate::model::EncodedPacket;
use crate::videotoolbox::{annexb_to_avcc, nal_units};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *mut c_void; // mutable: we set the display attachment
type CFStringRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> *const c_void;
    fn CFDictionarySetValue(dict: CFDictionaryRef, key: *const c_void, value: *const c_void);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_DisplayImmediately: CFStringRef;
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
        memory_block: *mut c_void, // null → CoreMedia allocates (sample owns it)
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
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
    fn CMSampleBufferGetSampleAttachmentsArray(
        sbuf: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> CFArrayRef;
}

/// An owned CMSampleBuffer, released on drop. CF objects are internally
/// refcounted and safe to move across threads.
pub struct Sample(CMSampleBufferRef);
unsafe impl Send for Sample {}

impl Sample {
    /// The raw CMSampleBufferRef, for handing to `-[AVSampleBufferDisplayLayer
    /// enqueueSampleBuffer:]`. Remains owned by `self`.
    pub fn as_raw(&self) -> *const c_void {
        self.0
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

/// Feeds scrcpy packets in, yields display-ready samples out. Config packets
/// (SPS/PPS) refresh the format description carried by subsequent samples;
/// AVSampleBufferDisplayLayer picks up format changes per-sample.
pub struct SampleAssembler {
    format: CMFormatDescriptionRef,
}

unsafe impl Send for SampleAssembler {}

impl SampleAssembler {
    pub fn new() -> Self {
        SampleAssembler { format: ptr::null() }
    }

    fn set_format(&mut self, config: &[u8]) -> Result<()> {
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
            if !self.format.is_null() {
                CFRelease(self.format);
            }
            self.format = fmt;
        }
        Ok(())
    }

    /// Convert one packet. Config packets update state and yield `None`;
    /// media packets yield a sample (once a format description exists).
    pub fn push(&mut self, packet: &EncodedPacket) -> Result<Option<Sample>> {
        if packet.is_config {
            self.set_format(&packet.data)?;
            return Ok(None);
        }
        if self.format.is_null() {
            return Ok(None); // waiting for SPS/PPS
        }
        let avcc = annexb_to_avcc(&packet.data);
        if avcc.is_empty() {
            return Ok(None);
        }

        unsafe {
            // Block buffer with its *own* allocation (memory_block = null): the
            // layer keeps the sample past this call, so it can't reference our
            // Vec the way the synchronous VT decode path does.
            let mut block: CMBlockBufferRef = ptr::null();
            let st = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(), ptr::null_mut(), avcc.len(), ptr::null(),
                ptr::null(), 0, avcc.len(), 0, &mut block,
            );
            if st != 0 || block.is_null() {
                return Err(StreamError::Protocol(format!("block buffer create failed ({st})")));
            }
            let st = CMBlockBufferReplaceDataBytes(
                avcc.as_ptr() as *const c_void, block, 0, avcc.len(),
            );
            if st != 0 {
                CFRelease(block);
                return Err(StreamError::Protocol(format!("block buffer fill failed ({st})")));
            }

            let sizes = [avcc.len()];
            let mut sample: CMSampleBufferRef = ptr::null();
            let st = CMSampleBufferCreateReady(
                ptr::null(), block, self.format, 1, 0, ptr::null(), 1, sizes.as_ptr(), &mut sample,
            );
            CFRelease(block); // sample retains it
            if st != 0 || sample.is_null() {
                return Err(StreamError::Protocol(format!("sample buffer create failed ({st})")));
            }

            // No timing info → mark for immediate display (scrcpy-style low
            // latency: present each frame as soon as it decodes).
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 1);
            if !attachments.is_null() {
                let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
                if !dict.is_null() {
                    CFDictionarySetValue(
                        dict,
                        kCMSampleAttachmentKey_DisplayImmediately,
                        kCFBooleanTrue,
                    );
                }
            }
            Ok(Some(Sample(sample)))
        }
    }
}

impl Drop for SampleAssembler {
    fn drop(&mut self) {
        unsafe {
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
    }
}
