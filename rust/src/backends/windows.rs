use super::{CaptureBackend, CaptureError};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

pub struct DxgiCaptureBackend {
    // Keep a strong device reference for all resources created from it.
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

/// Releases an acquired duplication frame on every return path, including
/// `Map`/resource conversion errors. DXGI rejects the next AcquireNextFrame
/// call if the current frame is still owned by the client.
struct AcquiredFrame<'a> {
    duplication: &'a IDXGIOutputDuplication,
    released: bool,
}

impl<'a> AcquiredFrame<'a> {
    fn new(duplication: &'a IDXGIOutputDuplication) -> Self {
        Self {
            duplication,
            released: false,
        }
    }

    unsafe fn release(&mut self) -> Result<(), CaptureError> {
        if self.released {
            return Ok(());
        }
        let result = unsafe { self.duplication.ReleaseFrame() }
            .map_err(|error| CaptureError::Reinitialize(format!("ReleaseFrame failed: {error}")));
        self.released = true;
        result
    }
}

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        if !self.released {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}

/// Balances a successful D3D11 Map even if copying pixels panics unexpectedly.
struct MappedResource<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for MappedResource<'_> {
    fn drop(&mut self) {
        unsafe {
            self.context.Unmap(self.texture, 0);
        }
    }
}

impl CaptureBackend for DxgiCaptureBackend {
    fn init(width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            // 1. Create D3D11 Device
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None, // Default adapter
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None, // Feature levels (default)
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDevice failed: {e}"))?;

            let device = device.ok_or_else(|| {
                "D3D11CreateDevice succeeded without returning a device".to_owned()
            })?;
            let context = context.ok_or_else(|| {
                "D3D11CreateDevice succeeded without returning a device context".to_owned()
            })?;

            // 2. Get IDXGIOutput1 for primary monitor
            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|error| format!("ID3D11Device -> IDXGIDevice failed: {error}"))?;
            let adapter = dxgi_device
                .GetAdapter()
                .map_err(|error| format!("GetAdapter failed: {error}"))?;
            let output: IDXGIOutput = adapter
                .EnumOutputs(0)
                .map_err(|error| format!("No display output is available: {error}"))?;
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|error| format!("IDXGIOutput -> IDXGIOutput1 failed: {error}"))?;

            // 3. Create Desktop Duplication
            let duplication = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput failed: {e}"))?;

            // 4. Create Staging Texture (CPU-readable)
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };

            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| format!("CreateTexture2D failed: {e}"))?;

            Ok(Self {
                _device: device,
                context,
                duplication,
                staging_texture: staging.ok_or_else(|| {
                    "CreateTexture2D succeeded without returning a staging texture".to_owned()
                })?,
                width,
                height,
            })
        }
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

        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Err(CaptureError::Reinitialize(format!("AcquireNextFrame: {e}")));
                }
                Err(e) => return Err(CaptureError::Recoverable(format!("AcquireNextFrame: {e}"))),
            }

            let mut frame = AcquiredFrame::new(&self.duplication);
            let copy_result = (|| -> Result<(), CaptureError> {
                let resource = resource.ok_or_else(|| {
                    CaptureError::Recoverable("DXGI returned no desktop resource".to_owned())
                })?;
                let texture: ID3D11Texture2D = resource.cast().map_err(|error| {
                    CaptureError::Recoverable(format!("Desktop resource is not a texture: {error}"))
                })?;

                self.context.CopyResource(&self.staging_texture, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(
                        &self.staging_texture,
                        0,
                        D3D11_MAP_READ,
                        0,
                        Some(&mut mapped),
                    )
                    .map_err(|error| CaptureError::Reinitialize(format!("Map failed: {error}")))?;
                let _mapped_resource = MappedResource {
                    context: &self.context,
                    texture: &self.staging_texture,
                };

                let src = mapped.pData as *const u8;
                let width = self.width as usize;
                let dst_stride = width * 4;
                let pitch = mapped.RowPitch as usize;

                for row in 0..self.height as usize {
                    // Create safe slices for this row
                    let src_slice = std::slice::from_raw_parts(src.add(row * pitch), dst_stride);
                    let dst_slice = &mut buffer[row * dst_stride..row * dst_stride + dst_stride];
                    
                    // Safely cast [u8] to [u32] for bulk processing. 
                    // LLVM auto-vectorizes this zip loop into fast SIMD instructions.
                    let (_, src_u32, _) = src_slice.align_to::<u32>();
                    let (_, dst_u32, _) = dst_slice.align_to_mut::<u32>();

                    for (d, s) in dst_u32.iter_mut().zip(src_u32.iter()) {
                        let bgra = *s;
                        *d = (bgra & 0xff00_ff00)
                            | ((bgra & 0x00ff_0000) >> 16)
                            | ((bgra & 0x0000_00ff) << 16);
                    }
                }
                Ok(())
            })();

            let release_result = frame.release();
            copy_result?;
            release_result?;
            Ok(true)
        }
    }

    fn destroy(&mut self) {
        // COM objects auto-drop thanks to windows-rs RAII
    }
}
