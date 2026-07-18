use crate::backends::{CaptureBackend, CaptureError};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use std::sync::mpsc;
use std::time::Duration;

// pipewire-rs 0.9+ uses *Rc variants (shared-ownership) instead of unique ones.
// `spa` is re-exported directly via `pipewire::spa`.
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
// Explicitly import macro to avoid shadowing by the `pipewire::properties` module.
use pipewire::properties::properties;
use pipewire::spa::utils::Direction;
use pipewire::stream::{StreamFlags, StreamRc};

/// Status message to sync the worker thread startup with `init()`.
enum WorkerStartup {
    Ready,
    Failed(String),
}

pub struct PipeWireCaptureBackend {
    rx: mpsc::Receiver<Vec<u8>>,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    quit_sender: Option<pipewire::channel::Sender<()>>,
    width: u32,
    height: u32,
}

impl CaptureBackend for PipeWireCaptureBackend {
    fn init(width: u32, height: u32) -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Tokio runtime error: {e}"))?;

        let (pw_fd, node_id) = rt
            .block_on(async {
                let proxy = Screencast::new().await?;
                let session = proxy.create_session(Default::default()).await?;

                proxy
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_sources(SourceType::Monitor | SourceType::Window)
                            .set_cursor_mode(CursorMode::Embedded),
                    )
                    .await?;

                let response = proxy.start(&session, None, Default::default()).await?.response()?;
                let stream = response.streams().first().ok_or(ashpd::Error::NoResponse)?;
                let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;

                Ok::<_, ashpd::Error>((fd, stream.pipe_wire_node_id()))
            })
            .map_err(|e| format!("XDG Portal error: {e}"))?;

        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (quit_sender, quit_receiver) = pipewire::channel::channel::<()>();
        let (startup_tx, startup_rx) = mpsc::channel::<WorkerStartup>();

        let pw_thread = std::thread::Builder::new()
            .name("pw-capture".into())
            .spawn(move || {
                // Report errors via `startup_tx` instead of panicking on this thread.
                macro_rules! try_setup {
                    ($expr:expr, $context:literal) => {
                        match $expr {
                            Ok(value) => value,
                            Err(error) => {
                                let _ = startup_tx.send(WorkerStartup::Failed(format!(
                                    "{}: {}",
                                    $context, error
                                )));
                                return;
                            }
                        }
                    };
                }

                pipewire::init();
                let mainloop =
                    try_setup!(MainLoopRc::new(None), "Failed to create PipeWire mainloop");
                let context = try_setup!(
                    ContextRc::new(&mainloop, None),
                    "Failed to create PipeWire context"
                );

                // `connect_fd_rc` takes ownership of the fd.
                let core = try_setup!(
                    context.connect_fd_rc(pw_fd, None),
                    "Failed to connect to PipeWire via portal FD"
                );

                let props = properties! {
                    *pipewire::keys::MEDIA_TYPE => "Video",
                    *pipewire::keys::MEDIA_CATEGORY => "Capture",
                    *pipewire::keys::MEDIA_ROLE => "Screen",
                };

                let stream = try_setup!(
                    StreamRc::new(core, "gd-capture", props),
                    "Failed to create PipeWire stream"
                );

                let loop_clone = mainloop.clone();
                let _receiver = quit_receiver.attach(&mainloop.loop_(), move |_| {
                    loop_clone.quit();
                });

                let buffer_size = (width * height * 4) as usize;

                let _listener = try_setup!(
                    stream
                        .add_local_listener_with_user_data(())
                        .process(move |stream, _| {
                            if let Some(mut buffer) = stream.dequeue_buffer() {
                                let datas = buffer.datas_mut();
                                if let Some(data) = datas.first_mut() {
                                    // Renamed to `data()` in pipewire 0.9.
                                    if let Some(map) = data.data() {
                                        // Validate buffer size before processing.
                                        if map.len() == buffer_size {
                                            let mut out = vec![0u8; buffer_size];
                                            for (d, s) in
                                                out.chunks_exact_mut(4).zip(map.chunks_exact(4))
                                            {
                                                // Convert BGRx -> RGBA.
                                                d[0] = s[2]; // R
                                                d[1] = s[1]; // G
                                                d[2] = s[0]; // B
                                                d[3] = 255;  // A
                                            }
                                            let _ = tx.try_send(out);
                                        }
                                    }
                                }
                                // pipewire 0.9+: Buffer auto-queues back on Drop.
                            }
                        })
                        .register(),
                    "Failed to register PipeWire stream listener"
                );

                let connect_result = stream.connect(
                    Direction::Input,
                    Some(node_id),
                    StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                    &mut [],
                );
                if let Err(error) = connect_result {
                    let _ = startup_tx.send(WorkerStartup::Failed(format!(
                        "Failed to connect PipeWire stream: {error}"
                    )));
                    return;
                }

                let _ = startup_tx.send(WorkerStartup::Ready);

                mainloop.run();
                unsafe { pipewire::deinit() };
            })
            .map_err(|e| format!("Failed to spawn PipeWire thread: {e}"))?;

        // Wait for worker thread initialization outcome.
        match startup_rx.recv() {
            Ok(WorkerStartup::Ready) => {}
            Ok(WorkerStartup::Failed(reason)) => {
                let _ = pw_thread.join();
                return Err(reason);
            }
            Err(_) => {
                let join_result = pw_thread.join();
                let panic_detail = match join_result {
                    Err(payload) => describe_panic_payload(&payload),
                    Ok(()) => {
                        "worker thread exited before reporting startup status".to_owned()
                    }
                };
                return Err(format!("PipeWire worker thread failed to start: {panic_detail}"));
            }
        }

        Ok(Self {
            rx,
            pw_thread: Some(pw_thread),
            quit_sender: Some(quit_sender),
            width,
            height,
        })
    }

    fn capture_frame(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<bool, CaptureError> {
        let expected_len = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| CaptureError::Fatal("Capture dimensions overflow usize".to_owned()))?;
        if buffer.len() != expected_len {
            return Err(CaptureError::Fatal(format!(
                "Capture buffer has {} bytes; expected {expected_len}",
                buffer.len()
            )));
        }

        let result = if timeout_ms == 0 {
            self.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => CaptureError::Recoverable("No frame".into()),
                mpsc::TryRecvError::Disconnected => {
                    CaptureError::Fatal("PipeWire disconnected".into())
                }
            })
        } else {
            self.rx
                .recv_timeout(Duration::from_millis(timeout_ms as u64))
                .map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => CaptureError::Recoverable("Timeout".into()),
                    mpsc::RecvTimeoutError::Disconnected => {
                        CaptureError::Fatal("PipeWire disconnected".into())
                    }
                })
        };

        match result {
            Ok(frame) => {
                if frame.len() != expected_len {
                    return Err(CaptureError::Reinitialize(format!(
                        "PipeWire frame has {} bytes; expected {expected_len}",
                        frame.len()
                    )));
                }
                buffer.copy_from_slice(&frame);
                Ok(true)
            }
            Err(CaptureError::Recoverable(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn destroy(&mut self) {
        if let Some(sender) = self.quit_sender.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.pw_thread.take() {
            if let Err(payload) = thread.join() {
                eprintln!(
                    "[DesktopCapture] PipeWire worker thread panicked during shutdown: {}",
                    describe_panic_payload(&payload)
                );
            }
        }
    }
}

/// Extracts a human-readable panic message from the thread join payload.
fn describe_panic_payload(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}