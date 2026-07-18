use godot::classes::{DisplayServer, Image, ImageTexture, Node, image::Format};
use godot::prelude::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crate::capture_thread::{CaptureControl, CaptureThread};

/// FPS mode enum — automatically exposed to both GDScript and C#
#[derive(GodotConvert, Var, Export, Debug, Clone, Copy, PartialEq)]
#[godot(via = i32)]
pub enum FpsMode {
    /// Capture at a user-selected rate. The default is 30 FPS.
    Manual = 0,
    /// Capture when the desktop compositor delivers a new frame.
    Vsync = 1,
}

impl FpsMode {
    pub(crate) const fn as_u8(self) -> u8 {
        self as u8
    }

    pub(crate) const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Vsync,
            _ => Self::Manual,
        }
    }
}

#[derive(GodotClass)]
#[class(base = Node)]
pub struct DesktopCaster {
    base: Base<Node>,

    #[export]
    fps_mode: FpsMode,

    #[export]
    target_fps: i32,

    // Internal state
    texture: Option<Gd<ImageTexture>>,
    image: Option<Gd<Image>>,
    capture_thread: Option<CaptureThread>,
    is_running: Arc<AtomicBool>,
    control: Arc<CaptureControl>,

    // Double buffer: front (Godot reads) / back (thread writes)
    front_buffer: Arc<Mutex<Vec<u8>>>,
    back_buffer: Arc<Mutex<Vec<u8>>>,
    frame_ready: Arc<AtomicBool>,

    width: u32,
    height: u32,
}

#[godot_api]
impl INode for DesktopCaster {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            fps_mode: FpsMode::Manual,
            target_fps: 30,
            texture: None,
            image: None,
            capture_thread: None,
            is_running: Arc::new(AtomicBool::new(false)),
            control: Arc::new(CaptureControl::new(FpsMode::Manual, 30)),
            front_buffer: Arc::new(Mutex::new(Vec::new())),
            back_buffer: Arc::new(Mutex::new(Vec::new())),
            frame_ready: Arc::new(AtomicBool::new(false)),
            width: 0,
            height: 0,
        }
    }

    fn process(&mut self, _delta: f64) {
        // Godot APIs are called only from this main-thread callback. Worker
        // errors are forwarded through CaptureControl instead of logged from a
        // background thread.
        if let Some(error) = self.control.take_pending_error() {
            godot_error!("{error}");
        }

        if !self.frame_ready.load(Ordering::Acquire) {
            return;
        }

        // Swap front <-> back. The worker will only write again after it is
        // notified below, so a ready notification cannot be lost mid-swap.
        let mut front = self
            .front_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
            
        {
            let mut back = self
                .back_buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::swap(&mut *front, &mut *back);
        }
        self.frame_ready.store(false, Ordering::Release);
        self.control.notify();

        // Reuse ImageTexture object to avoid recreation, though PackedByteArray
        // still allocates per frame due to gdext lacking in-place updates.
        // TODO: Revisit if gdext adds support for writing into existing arrays.
        if let Some(ref mut image) = self.image {
            let byte_array = PackedByteArray::from(front.as_slice());

            // Update image data in-place (no new Image created!)
            image.set_data(
                self.width as i32,
                self.height as i32,
                false,
                Format::RGBA8,
                &byte_array,
            );

            // Update texture in-place (no new Texture created!)
            if let Some(ref mut tex) = self.texture {
                tex.update(&*image);
            }
        }
    }

    fn exit_tree(&mut self) {
        self.stop();
    }
}

#[godot_api]
impl DesktopCaster {
    /// Start capture. Manual mode defaults to 30 FPS.
    #[func]
    pub fn start(&mut self) -> bool {
        if self.is_running.load(Ordering::Relaxed) {
            return true;
        }

        // Explicitly join any previous worker that stopped on its own (e.g.
        // on a fatal error) to prevent unexpected blocking during Drop.
        if self.capture_thread.is_some() {
            self.stop();
        }

        let screen_size = DisplayServer::singleton()
            .screen_get_size_ex()
            .screen(0)
            .done();
        let (width, height) = match (u32::try_from(screen_size.x), u32::try_from(screen_size.y)) {
            (Ok(width), Ok(height)) if width > 0 && height > 0 => (width, height),
            _ => {
                self.control
                    .report_error("[DesktopCaster] Primary display has an invalid size.");
                return false;
            }
        };
        let Some(buf_size) = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            self.control
                .report_error("[DesktopCaster] Capture buffer size overflows this platform.");
            return false;
        };

        let image = match Image::create_empty(width as i32, height as i32, false, Format::RGBA8) {
            Some(image) => image,
            None => {
                self.control
                    .report_error("[DesktopCapture] Image allocation failed.");
                return false;
            }
        };
        let texture = match ImageTexture::create_from_image(&image) {
            Some(texture) => texture,
            None => {
                self.control
                    .report_error("[DesktopCapture] Texture allocation failed.");
                return false;
            }
        };

        self.width = width;
        self.height = height;
        *self
            .front_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![0; buf_size];
        *self
            .back_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![0; buf_size];
        self.image = Some(image);
        self.texture = Some(texture);

        // Invalid Inspector/script values fall back to Manual's documented
        // default rather than becoming a tight polling loop.
        self.target_fps = Self::normalize_target_fps(self.target_fps);
        self.control.set_fps_mode(self.fps_mode);
        self.control.set_target_fps(self.target_fps as u32);
        self.control.clear_error();
        self.frame_ready.store(false, Ordering::Release);
        self.is_running.store(true, Ordering::Release);
        match CaptureThread::spawn(
            self.width,
            self.height,
            Arc::clone(&self.back_buffer),
            Arc::clone(&self.frame_ready),
            Arc::clone(&self.is_running),
            Arc::clone(&self.control),
        ) {
            Ok(capture_thread) => {
                self.capture_thread = Some(capture_thread);
                true
            }
            Err(error) => {
                self.is_running.store(false, Ordering::Release);
                self.control
                    .report_error(format!("[DesktopCaster] {error}"));
                false
            }
        }
    }

    /// Stop capture.
    #[func]
    pub fn stop(&mut self) {
        self.is_running.store(false, Ordering::Release);
        self.control.notify();
        if let Some(mut thread) = self.capture_thread.take() {
            thread.shutdown();
        }
        self.frame_ready.store(false, Ordering::Release);
    }

    /// Get the texture to assign to a TextureRect.
    #[func]
    pub fn get_texture(&self) -> Option<Gd<ImageTexture>> {
        self.texture.clone()
    }

    /// Change FPS mode at runtime.
    #[func]
    pub fn set_fps_mode_runtime(&mut self, mode: FpsMode) {
        self.fps_mode = mode;
        self.control.set_fps_mode(mode);
    }

    /// Set Manual FPS at runtime. Values outside 1..=240 use the safe default
    /// of 30 FPS so they cannot create a busy polling loop.
    #[func]
    pub fn set_target_fps_runtime(&mut self, target_fps: i32) {
        self.target_fps = Self::normalize_target_fps(target_fps);
        self.control.set_target_fps(self.target_fps as u32);
    }

    #[func]
    pub fn is_capturing(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    #[func]
    pub fn get_last_error(&self) -> GString {
        let error = self.control.last_error();
        GString::from(error.as_str())
    }

    fn normalize_target_fps(target_fps: i32) -> i32 {
        if (1..=240).contains(&target_fps) {
            target_fps
        } else {
            30
        }
    }
}