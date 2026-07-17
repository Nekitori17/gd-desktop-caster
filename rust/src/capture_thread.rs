use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backends::{CaptureBackend, CaptureError, PlatformBackend};
use crate::desktop_capture::FpsMode;

const MIN_MANUAL_FPS: u32 = 1;
const MAX_MANUAL_FPS: u32 = 240;
const VSYNC_STOP_POLL: Duration = Duration::from_millis(50);
const ERROR_RETRY_DELAY: Duration = Duration::from_millis(100);

/// State shared by the Godot main thread and the capture worker.
pub struct CaptureControl {
    fps_mode: AtomicU8,
    target_fps: AtomicU32,
    wake_lock: Mutex<()>,
    wake: Condvar,
    last_error: Mutex<String>,
    pending_error: Mutex<Option<String>>,
}

impl CaptureControl {
    pub fn new(fps_mode: FpsMode, target_fps: u32) -> Self {
        Self {
            fps_mode: AtomicU8::new(fps_mode.as_u8()),
            target_fps: AtomicU32::new(Self::clamp_manual_fps(target_fps)),
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
            last_error: Mutex::new(String::new()),
            pending_error: Mutex::new(None),
        }
    }

    pub fn set_fps_mode(&self, fps_mode: FpsMode) {
        self.fps_mode.store(fps_mode.as_u8(), Ordering::Release);
        self.notify();
    }

    pub fn set_target_fps(&self, target_fps: u32) -> u32 {
        let target_fps = Self::clamp_manual_fps(target_fps);
        self.target_fps.store(target_fps, Ordering::Release);
        self.notify();
        target_fps
    }

    pub fn fps_mode(&self) -> FpsMode {
        FpsMode::from_u8(self.fps_mode.load(Ordering::Acquire))
    }

    pub fn target_fps(&self) -> u32 {
        self.target_fps.load(Ordering::Acquire)
    }

    pub fn last_error(&self) -> String {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn take_pending_error(&self) -> Option<String> {
        self.pending_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    pub fn report_error(&self, error: impl Into<String>) {
        let error = error.into();
        let mut last_error = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last_error == error {
            return;
        }
        *last_error = error.clone();
        *self
            .pending_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
    }

    pub fn clear_error(&self) {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub fn notify(&self) {
        self.wake.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let guard = self
            .wake_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = self.wake.wait_timeout(guard, timeout);
    }

    fn clamp_manual_fps(target_fps: u32) -> u32 {
        target_fps.clamp(MIN_MANUAL_FPS, MAX_MANUAL_FPS)
    }
}

pub struct CaptureThread {
    handle: Option<JoinHandle<()>>,
    is_running: Arc<AtomicBool>,
    control: Arc<CaptureControl>,
}

impl CaptureThread {
    pub fn spawn(
        width: u32,
        height: u32,
        back_buffer: Arc<Mutex<Vec<u8>>>,
        frame_ready: Arc<AtomicBool>,
        is_running: Arc<AtomicBool>,
        control: Arc<CaptureControl>,
    ) -> Result<Self, String> {
        let worker_running = Arc::clone(&is_running);
        let worker_control = Arc::clone(&control);
        let handle = thread::Builder::new()
            .name("desktop-capture".into())
            .spawn(move || {
                Self::run(
                    width,
                    height,
                    back_buffer,
                    frame_ready,
                    worker_running,
                    worker_control,
                );
            })
            .map_err(|error| format!("Failed to spawn capture thread: {error}"))?;

        Ok(Self {
            handle: Some(handle),
            is_running,
            control,
        })
    }

    fn run(
        width: u32,
        height: u32,
        back_buffer: Arc<Mutex<Vec<u8>>>,
        frame_ready: Arc<AtomicBool>,
        is_running: Arc<AtomicBool>,
        control: Arc<CaptureControl>,
    ) {
        let mut backend = match PlatformBackend::init(width, height) {
            Ok(backend) => backend,
            Err(error) => {
                control.report_error(format!("[DesktopCapture] Backend init failed: {error}"));
                is_running.store(false, Ordering::Release);
                return;
            }
        };

        let mut previous_mode = control.fps_mode();
        let mut next_manual_frame = Instant::now();

        let buf_size = (width * height * 4) as usize;
        let mut local_buffer = vec![0u8; buf_size];

        while is_running.load(Ordering::Acquire) {
            // Keep only the latest frame. Capturing while Godot still owns a
            // complete frame wastes GPU readback and increases latency.
            if frame_ready.load(Ordering::Acquire) {
                control.wait(VSYNC_STOP_POLL);
                continue;
            }

            let fps_mode = control.fps_mode();
            if fps_mode != previous_mode {
                previous_mode = fps_mode;
                next_manual_frame = Instant::now();
            }

            let timeout_ms = match fps_mode {
                FpsMode::Manual => {
                    let frame_interval =
                        Duration::from_secs_f64(1.0 / f64::from(control.target_fps()));
                    let now = Instant::now();
                    if now < next_manual_frame {
                        control.wait(next_manual_frame - now);
                        continue;
                    }

                    next_manual_frame += frame_interval;
                    if now >= next_manual_frame {
                        next_manual_frame = now + frame_interval;
                    }
                    0
                }
                FpsMode::Vsync => VSYNC_STOP_POLL.as_millis() as u32,
            };

            // Capture into local scratch buffer (no locks held during block).
            let capture_result = backend.capture_frame(local_buffer.as_mut_slice(), timeout_ms);

            match capture_result {
                Ok(true) => {
                    {
                        let mut back = back_buffer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        back.copy_from_slice(&local_buffer);
                    }
                    control.clear_error();
                    frame_ready.store(true, Ordering::Release);
                }
                Ok(false) => {}
                Err(CaptureError::Reinitialize(error)) => {
                    control.report_error(format!(
                        "[DesktopCapture] Capture device changed; restarting backend: {error}"
                    ));
                    backend.destroy();
                    match PlatformBackend::init(width, height) {
                        Ok(new_backend) => backend = new_backend,
                        Err(init_error) => {
                            control.report_error(format!(
                                "[DesktopCapture] Backend restart failed: {init_error}"
                            ));
                            is_running.store(false, Ordering::Release);
                            break;
                        }
                    }
                }
                Err(CaptureError::Recoverable(error)) => {
                    control.report_error(format!("[DesktopCapture] Capture error: {error}"));
                    control.wait(ERROR_RETRY_DELAY);
                }
                Err(CaptureError::Fatal(error)) => {
                    control.report_error(format!("[DesktopCapture] Capture stopped: {error}"));
                    is_running.store(false, Ordering::Release);
                    break;
                }
            }
        }

        backend.destroy();
        is_running.store(false, Ordering::Release);
        control.notify();
    }

    pub fn shutdown(&mut self) {
        self.is_running.store(false, Ordering::Release);
        self.control.notify();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::CaptureControl;

    #[test]
    fn manual_fps_is_always_in_the_safe_range() {
        assert_eq!(CaptureControl::clamp_manual_fps(0), 1);
        assert_eq!(CaptureControl::clamp_manual_fps(30), 30);
        assert_eq!(CaptureControl::clamp_manual_fps(1_000), 240);
    }
}
