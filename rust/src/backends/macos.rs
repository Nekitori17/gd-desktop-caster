use super::{CaptureBackend, CaptureError};

/// Placeholder for the ScreenCaptureKit implementation.
///
/// Returning a normal error is intentionally safer than the former `todo!()`:
/// a missing permission/backend must never panic inside the Godot process.
pub struct ScreenCaptureKitBackend;

impl CaptureBackend for ScreenCaptureKitBackend {
    fn init(_width: u32, _height: u32) -> Result<Self, String> {
        Err(
            "macOS ScreenCaptureKit backend is not implemented yet; screen-recording permission and a tested stream implementation are required."
                .to_owned(),
        )
    }

    fn capture_frame(
        &mut self,
        _buffer: &mut [u8],
        _timeout_ms: u32,
    ) -> Result<bool, CaptureError> {
        Err(CaptureError::Fatal(
            "macOS ScreenCaptureKit backend is unavailable".to_owned(),
        ))
    }

    fn destroy(&mut self) {}
}
