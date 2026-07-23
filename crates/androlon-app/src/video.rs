//! SDL_GPU video surface. Owns its own SDL3 window + GPU device + a streaming
//! texture, and presents decoded RGBA frames by uploading them (transfer buffer
//! + copy pass) and blitting to the swapchain — no custom shaders needed, so
//! this is a compact, cross-platform presenter. One `VideoWindow` per Android
//! virtual display becomes a per-app "Coherence" window later.
//!
//! `SDL_GPU` picks the backend per OS (Metal on macOS, Vulkan on Linux/Windows);
//! the declared shader format just has to match an available backend.

use androlon_stream::DecodedFrame;
use sdl3::gpu::{
    BlitInfo, Device, Filter, ShaderFormat, TextureCreateInfo, TextureFormat, TextureRegion,
    TextureTransferInfo, TextureType, TextureUsage, TransferBufferUsage,
};
use sdl3::gpu::Texture;
use sdl3::video::Window;
use sdl3::VideoSubsystem;

// Shader format to declare at device creation. We use no custom shaders (blit
// only), but the device still must be created against a format the host backend
// supports: Metal wants MSL/METALLIB, everyone else SPIR-V.
#[cfg(target_os = "macos")]
const SHADER_FORMAT: ShaderFormat = ShaderFormat::METALLIB;
#[cfg(not(target_os = "macos"))]
const SHADER_FORMAT: ShaderFormat = ShaderFormat::SPIRV;

pub struct VideoWindow {
    window: Window,
    device: Device,
    // Streaming texture, lazily (re)created to match the current frame size.
    texture: Option<(Texture<'static>, u32, u32)>,
}

impl VideoWindow {
    pub fn new(video: &VideoSubsystem, title: &str, w: u32, h: u32) -> Result<Self, String> {
        let window = video
            .window(title, w, h)
            .position_centered()
            .resizable()
            .build()
            .map_err(|e| e.to_string())?;
        let device = Device::new(SHADER_FORMAT, false)
            .map_err(|e| e.to_string())?
            .with_window(&window)
            .map_err(|e| e.to_string())?;
        Ok(VideoWindow { window, device, texture: None })
    }

    fn ensure_texture(&mut self, w: u32, h: u32) -> Result<(), String> {
        let stale = match &self.texture {
            Some((_, tw, th)) => *tw != w || *th != h,
            None => true,
        };
        if stale {
            let tex = self
                .device
                .create_texture(
                    TextureCreateInfo::new()
                        .with_type(TextureType::_2D)
                        .with_format(TextureFormat::R8g8b8a8Unorm)
                        .with_usage(TextureUsage::SAMPLER) // blit source needs SAMPLER
                        .with_width(w)
                        .with_height(h)
                        .with_layer_count_or_depth(1)
                        .with_num_levels(1),
                )
                .map_err(|e| e.to_string())?;
            self.texture = Some((tex, w, h));
        }
        Ok(())
    }

    /// Upload one RGBA frame and present it scaled to the window.
    pub fn present(&mut self, frame: &DecodedFrame) -> Result<(), String> {
        let (w, h) = (frame.width, frame.height);
        let expected = (w as usize) * (h as usize) * 4;
        if frame.rgba.len() != expected {
            return Err(format!("frame is {} bytes, expected {expected}", frame.rgba.len()));
        }
        self.ensure_texture(w, h)?;
        let texture = &self.texture.as_ref().unwrap().0;

        // Stage the pixels in an upload transfer buffer.
        let bytes = expected as u32;
        let transfer = self
            .device
            .create_transfer_buffer()
            .with_size(bytes)
            .with_usage(TransferBufferUsage::UPLOAD)
            .build()
            .map_err(|e| e.to_string())?;
        {
            let mut mapped = transfer.map::<u8>(&self.device, false);
            mapped.mem_mut().copy_from_slice(&frame.rgba);
            mapped.unmap();
        }

        // One command buffer: copy pass (upload) then blit to swapchain.
        let mut cmd = self.device.acquire_command_buffer().map_err(|e| e.to_string())?;
        let copy = self.device.begin_copy_pass(&cmd).map_err(|e| e.to_string())?;
        copy.upload_to_gpu_texture(
            TextureTransferInfo::new().with_transfer_buffer(&transfer).with_offset(0),
            TextureRegion::new()
                .with_texture(texture)
                .with_layer(0)
                .with_width(w)
                .with_height(h)
                .with_depth(1),
            false,
        );
        self.device.end_copy_pass(copy);

        if let Ok(swapchain) = cmd.wait_and_acquire_swapchain_texture(&self.window) {
            let (sw, sh) = self.window.size();
            let blit = BlitInfo::default()
                .with_source_texture(texture)
                .with_source_region(0, 0, 0, w, h)
                .with_destination_texture(&swapchain)
                .with_destination_region(0, 0, 0, sw, sh)
                .with_filter(Filter::Linear);
            cmd.blit_texture(blit);
            cmd.submit().map_err(|e| e.to_string())?;
        } else {
            cmd.cancel();
        }
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        self.window.size()
    }

    /// SDL window id, for routing events to the right surface.
    pub fn id(&self) -> u32 {
        self.window.id()
    }
}

/// A moving gradient, so `--video-demo` can prove the upload+blit path before a
/// real decoder feeds frames.
pub fn demo_frame(t: u32, w: u32, h: u32) -> DecodedFrame {
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            rgba[i] = ((x + t) % 256) as u8;
            rgba[i + 1] = ((y + t / 2) % 256) as u8;
            rgba[i + 2] = 128;
            rgba[i + 3] = 255;
        }
    }
    DecodedFrame { width: w, height: h, rgba }
}
