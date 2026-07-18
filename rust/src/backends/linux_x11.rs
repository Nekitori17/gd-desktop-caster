use crate::backends::{CaptureBackend, CaptureError};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
use x11rb::rust_connection::RustConnection;

/// X11 `XGetImage`-based fallback backend.
/// Note: On compositing window managers, this may only capture the wallpaper.
/// PipeWire is the preferred backend for modern Linux.
pub struct X11CaptureBackend {
    conn: RustConnection,
    window: u32,
    width: u32,
    height: u32,
    /// Actual bytes-per-scanline for the ZPixmap format.
    /// May include padding and exceed width * 4.
    stride: usize,
}

/// Computes actual stride (bytes per line) for a 32-bit Z_PIXMAP at `depth`,
/// accounting for server-specific padding.
fn resolve_stride(conn: &RustConnection, depth: u8, width: u32) -> Result<usize, String> {
    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .ok_or_else(|| format!("Server did not advertise a pixmap format for depth {depth}"))?;

    if format.bits_per_pixel != 32 {
        return Err(format!(
            "Unsupported root window pixel format: {} bits per pixel at depth {depth} \
             (only 32-bit ZPixmap formats are supported)",
            format.bits_per_pixel
        ));
    }
    let scanline_pad = format.scanline_pad.max(8) as u32;

    // Calculate byte stride with scanline padding.
    let bits_per_line = (width * 32).div_ceil(scanline_pad) * scanline_pad;
    Ok((bits_per_line / 8) as usize)
}

impl CaptureBackend for X11CaptureBackend {
    fn init(width: u32, height: u32) -> Result<Self, String> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;
        let (window, depth) = {
            let screen = &conn.setup().roots[screen_num];
            (screen.root, screen.root_depth)
        };
        let stride = resolve_stride(&conn, depth, width)?;

        Ok(Self {
            conn,
            window,
            width,
            height,
            stride,
        })
    }

    fn capture_frame(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<bool, CaptureError> {
        // X11 XGetImage is synchronous and doesn't support blocking for damage (without XDamage extension).
        // If timeout_ms is specified (Vsync mode), we emulate the delay to prevent spinning.
        if timeout_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
        }

        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.window,
                0,
                0,
                self.width as u16,
                self.height as u16,
                u32::MAX,
            )
            .map_err(|e| CaptureError::Recoverable(format!("XGetImage error: {e}")))?
            .reply()
            .map_err(|e| CaptureError::Recoverable(format!("XGetImage reply error: {e}")))?;

        let src = reply.data;
        let dst_stride = (self.width as usize) * 4;
        let src_stride = self.stride;

        // Guard against invalid stride calculation.
        if src_stride < dst_stride {
            return Err(CaptureError::Fatal(format!(
                "Resolved X11 stride ({src_stride}) is smaller than the expected row size ({dst_stride})"
            )));
        }

        let expected_len = src_stride
            .checked_mul(self.height as usize)
            .ok_or_else(|| CaptureError::Fatal("Capture dimensions overflow usize".to_owned()))?;
        if src.len() < expected_len {
            return Err(CaptureError::Recoverable(format!(
                "X11 image data has {} bytes; expected at least {expected_len} \
                 ({src_stride} bytes/line x {} lines)",
                src.len(),
                self.height
            )));
        }
        if buffer.len() != dst_stride * self.height as usize {
            return Err(CaptureError::Fatal(format!(
                "Capture buffer has {} bytes; expected {}",
                buffer.len(),
                dst_stride * self.height as usize
            )));
        }

        // Convert BGRx/BGRA to RGBA, skipping padding bytes at end of rows.
        for row in 0..self.height as usize {
            let src_row = &src[row * src_stride..row * src_stride + dst_stride];
            let dst_row = &mut buffer[row * dst_stride..row * dst_stride + dst_stride];

            for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
                d[0] = s[2]; // R <- B
                d[1] = s[1]; // G
                d[2] = s[0]; // B <- R
                d[3] = 255; // Force opaque A
            }
        }
        Ok(true)
    }

    fn destroy(&mut self) {
        // RustConnection automatically disconnects on drop.
    }
}