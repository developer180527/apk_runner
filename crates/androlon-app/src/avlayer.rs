//! Zero-copy macOS presenter: an `AVSampleBufferDisplayLayer` attached to the
//! SDL window's NSView. Compressed H.264 samples are enqueued straight to the
//! layer; CoreAnimation decodes (VideoToolbox), scales, and composites them
//! entirely on the GPU — no pixels ever touch the CPU. This is the "thin
//! native-chrome shim": the one per-OS box in the presentation layer.
//!
//! Hand-rolled Objective-C runtime FFI (`objc_msgSend`), matching the crate
//! style of doing framework FFI without binding dependencies. All calls happen
//! on the main thread (SDL's window/event thread), as AppKit requires.

#![cfg(target_os = "macos")]
#![allow(non_snake_case, non_upper_case_globals)]

use androlon_stream::samplebuffer::Sample;
use sdl3::video::Window;
use std::ffi::c_void;

type Id = *mut c_void;
type Sel = *const c_void;
type CGFloat = f64;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: CGFloat,
    y: CGFloat,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: CGFloat,
    height: CGFloat,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const u8) -> Id;
    fn sel_registerName(name: *const u8) -> Sel;
    fn objc_msgSend(); // cast per call site; arm64 has no _stret variant
}

// Linking AVFoundation registers the AVSampleBufferDisplayLayer class and
// exports the gravity string; CoreGraphics provides the letterbox color.
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    static AVLayerVideoGravityResizeAspect: Id; // NSString *
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorCreateGenericRGB(r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat) -> Id;
    fn CGColorRelease(color: Id);
}

// CALayer autoresizing (kCALayerWidthSizable | kCALayerHeightSizable).
const LAYER_WIDTH_HEIGHT_SIZABLE: usize = 2 | 16;

/// `objc_msgSend` cast to the right shape for each call pattern.
macro_rules! msg {
    ($recv:expr, $sel:expr $(, $arg:expr)* => $ret:ty ; $($argty:ty),*) => {{
        let f: extern "C" fn(Id, Sel $(, $argty)*) -> $ret =
            std::mem::transmute(objc_msgSend as *const c_void);
        f($recv, sel($sel) $(, $arg)*)
    }};
}

unsafe fn sel(name: &str) -> Sel {
    debug_assert!(name.ends_with('\0'));
    sel_registerName(name.as_ptr())
}

unsafe fn class(name: &str) -> Id {
    debug_assert!(name.ends_with('\0'));
    objc_getClass(name.as_ptr())
}

pub struct AvLayerPresenter {
    layer: Id,
    enqueue_sel: Sel,
}

impl AvLayerPresenter {
    /// Attach a display layer covering the SDL window's content view.
    pub fn new(window: &Window) -> Result<Self, String> {
        unsafe {
            let props = sdl3_sys::video::SDL_GetWindowProperties(window.raw());
            let nswindow = sdl3_sys::properties::SDL_GetPointerProperty(
                props,
                sdl3_sys::video::SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
                std::ptr::null_mut(),
            ) as Id;
            if nswindow.is_null() {
                return Err("no NSWindow behind SDL window (not Cocoa?)".into());
            }
            let view = msg!(nswindow, "contentView\0" => Id ;);
            if view.is_null() {
                return Err("NSWindow has no contentView".into());
            }
            msg!(view, "setWantsLayer:\0", true => () ; bool);
            let root = msg!(view, "layer\0" => Id ;);
            if root.is_null() {
                return Err("contentView has no backing layer".into());
            }

            // Black behind the video for aspect-fit letterboxing.
            let black = CGColorCreateGenericRGB(0.0, 0.0, 0.0, 1.0);
            msg!(root, "setBackgroundColor:\0", black => () ; Id);
            CGColorRelease(black);

            let cls = class("AVSampleBufferDisplayLayer\0");
            if cls.is_null() {
                return Err("AVSampleBufferDisplayLayer class not found".into());
            }
            let layer = msg!(cls, "alloc\0" => Id ;);
            let layer = msg!(layer, "init\0" => Id ;);
            if layer.is_null() {
                return Err("AVSampleBufferDisplayLayer init failed".into());
            }

            msg!(layer, "setVideoGravity:\0", AVLayerVideoGravityResizeAspect => () ; Id);
            let bounds = msg!(root, "bounds\0" => CGRect ;);
            msg!(layer, "setFrame:\0", bounds => () ; CGRect);
            // Track the view through resizes without per-frame bookkeeping.
            msg!(layer, "setAutoresizingMask:\0", LAYER_WIDTH_HEIGHT_SIZABLE => () ; usize);
            let scale = msg!(nswindow, "backingScaleFactor\0" => CGFloat ;);
            msg!(layer, "setContentsScale:\0", scale => () ; CGFloat);
            msg!(root, "addSublayer:\0", layer => () ; Id);

            Ok(AvLayerPresenter { layer, enqueue_sel: sel("enqueueSampleBuffer:\0") })
        }
    }

    /// Hand one compressed sample to the layer (it decodes + presents).
    pub fn enqueue(&self, sample: &Sample) {
        unsafe {
            let f: extern "C" fn(Id, Sel, *const c_void) =
                std::mem::transmute(objc_msgSend as *const c_void);
            f(self.layer, self.enqueue_sel, sample.as_raw());
        }
    }

    /// 0 = unknown, 1 = rendering, 2 = failed (AVQueuedSampleBufferRenderingStatus).
    #[allow(dead_code)] // diagnostics; used when debugging enqueue failures
    pub fn status(&self) -> isize {
        unsafe { msg!(self.layer, "status\0" => isize ;) }
    }

    /// Drop queued frames after an error or a stream discontinuity.
    #[allow(dead_code)] // will be used on resolution-change handling
    pub fn flush(&self) {
        unsafe { msg!(self.layer, "flush\0" => () ;) }
    }
}

impl Drop for AvLayerPresenter {
    fn drop(&mut self) {
        unsafe {
            msg!(self.layer, "removeFromSuperlayer\0" => () ;);
            msg!(self.layer, "release\0" => () ;);
        }
    }
}
