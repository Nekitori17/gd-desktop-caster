#[path = "linux_pipewire.rs"]
mod pipewire;
#[path = "linux_x11.rs"]
mod x11;

use super::{CaptureBackend, CaptureError};

pub enum LinuxCaptureBackend {
    PipeWire(pipewire::PipeWireCaptureBackend),
    X11(x11::X11CaptureBackend),
}

impl CaptureBackend for LinuxCaptureBackend {
    fn init(width: u32, height: u32) -> Result<Self, String> {
        // Try PipeWire / XDG Portal first (Wayland & modern X11)
        let pipewire_error = match pipewire::PipeWireCaptureBackend::init(width, height) {
            Ok(pw) => {
                godot::global::godot_print!("Initialized Linux capture via PipeWire/XDG Portal");
                return Ok(Self::PipeWire(pw));
            }
            Err(error) => error,
        };

        // Fallback to X11 XGetImage
        let x11_error = match x11::X11CaptureBackend::init(width, height) {
            Ok(x11) => {
                godot::global::godot_print!("Initialized Linux capture via X11 fallback");
                return Ok(Self::X11(x11));
            }
            Err(error) => error,
        };

        Err(format!(
            "Both PipeWire and X11 backends failed to initialize.\n\
             PipeWire/XDG Portal error: {pipewire_error}\n\
             X11 error: {x11_error}"
        ))
    }

    fn capture_frame(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<bool, CaptureError> {
        match self {
            Self::PipeWire(pw) => pw.capture_frame(buffer, timeout_ms),
            Self::X11(x11) => x11.capture_frame(buffer, timeout_ms),
        }
    }

    fn destroy(&mut self) {
        match self {
            Self::PipeWire(pw) => pw.destroy(),
            Self::X11(x11) => x11.destroy(),
        }
    }
}

pub type PlatformBackend = LinuxCaptureBackend;