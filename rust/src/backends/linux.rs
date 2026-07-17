use super::{CaptureBackend, CaptureError};

/// Placeholder for the PipeWire/XDG Portal implementation.
///
/// Portal access requires an asynchronous user-consent flow, so it must be
/// implemented and tested on Linux rather than simulated with `todo!()`.
pub struct PipeWireCaptureBackend;

impl CaptureBackend for PipeWireCaptureBackend {
    fn init(_width: u32, _height: u32) -> Result<Self, String> {
        Err(
            "Linux PipeWire/XDG Portal backend is not implemented yet; user portal consent and a tested PipeWire stream are required."
                .to_owned(),
        )
    }

    fn capture_frame(
        &mut self,
        _buffer: &mut [u8],
        _timeout_ms: u32,
    ) -> Result<bool, CaptureError> {
        Err(CaptureError::Fatal(
            "Linux PipeWire/XDG Portal backend is unavailable".to_owned(),
        ))
    }

    fn destroy(&mut self) {}
}
