#[derive(Debug)]
pub enum CaptureError {
    /// The backend was invalidated by an operating-system display event and
    /// can be recreated with the current capture geometry.
    Reinitialize(String),
    /// The frame can be skipped; the worker retries after a small backoff.
    Recoverable(String),
    /// Continuing would be unsafe or cannot produce a correctly sized frame.
    Fatal(String),
}

pub trait CaptureBackend: Send {
    /// Initialize a backend for a frame of the specified RGBA8 dimensions.
    fn init(width: u32, height: u32) -> Result<Self, String>
    where
        Self: Sized;

    /// Capture one frame into `buffer`.
    ///
    /// `timeout_ms = 0` polls immediately. A finite non-zero timeout waits for
    /// an OS frame while still allowing the worker to check shutdown requests.
    fn capture_frame(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<bool, CaptureError>;

    /// Release platform resources before the backend is dropped or recreated.
    fn destroy(&mut self);
}

// Compile-time platform dispatch.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub type PlatformBackend = windows::DxgiCaptureBackend;
#[cfg(target_os = "macos")]
pub type PlatformBackend = macos::ScreenCaptureKitBackend;
#[cfg(target_os = "linux")]
pub type PlatformBackend = linux::PipeWireCaptureBackend;