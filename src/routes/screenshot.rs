use crate::{
    http::{send_bytes_response, send_json_error},
    logger,
};
use std::{
    net::TcpStream,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BitBlt, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
        DeleteObject, GetDC, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
};

static SCREENSHOT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct CaptureResources {
    screen_dc: windows_sys::Win32::Graphics::Gdi::HDC,
    memory_dc: windows_sys::Win32::Graphics::Gdi::HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    old_object: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
}

impl Drop for CaptureResources {
    fn drop(&mut self) {
        unsafe {
            if !self.old_object.is_null() {
                SelectObject(self.memory_dc, self.old_object);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory_dc.is_null() {
                DeleteDC(self.memory_dc);
            }
            if !self.screen_dc.is_null() {
                ReleaseDC(std::ptr::null_mut(), self.screen_dc);
            }
        }
    }
}

fn last_error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}

fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut png_data = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_data, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|err| format!("failed to start PNG encoding: {err}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|err| format!("failed to encode PNG: {err}"))?;
    }
    Ok(png_data)
}

fn bgra_to_opaque_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    rgba
}

fn capture_primary_screen_png() -> Result<Vec<u8>, String> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if width <= 0 || height <= 0 {
        return Err("screen dimensions are unavailable".to_string());
    }

    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut::<core::ffi::c_void>() as HWND);
        if screen_dc.is_null() {
            return Err(last_error("GetDC failed"));
        }
        let memory_dc = CreateCompatibleDC(screen_dc);
        if memory_dc.is_null() {
            let error = last_error("CreateCompatibleDC failed");
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(error);
        }

        let mut bitmap_info: BITMAPINFO = std::mem::zeroed();
        bitmap_info.bmiHeader.biSize = size_of_val(&bitmap_info.bmiHeader) as u32;
        bitmap_info.bmiHeader.biWidth = width;
        bitmap_info.bmiHeader.biHeight = -height;
        bitmap_info.bmiHeader.biPlanes = 1;
        bitmap_info.bmiHeader.biBitCount = 32;
        bitmap_info.bmiHeader.biCompression = BI_RGB;

        let mut pixels = std::ptr::null_mut();
        let bitmap = CreateDIBSection(
            screen_dc,
            &bitmap_info,
            DIB_RGB_COLORS,
            &mut pixels,
            std::ptr::null_mut(),
            0,
        );
        if bitmap.is_null() || pixels.is_null() {
            let error = last_error("CreateDIBSection failed");
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(memory_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return Err(error);
        }
        let old_object = SelectObject(memory_dc, bitmap);
        let resources = CaptureResources {
            screen_dc,
            memory_dc,
            bitmap,
            old_object,
        };
        if old_object.is_null() {
            return Err(last_error("SelectObject failed"));
        }
        if BitBlt(memory_dc, 0, 0, width, height, screen_dc, 0, 0, SRCCOPY) == 0 {
            return Err(last_error("BitBlt failed"));
        }

        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .ok_or("screen dimensions are too large")?;
        let byte_count = pixel_count
            .checked_mul(4)
            .ok_or("screen image is too large")?;
        let bgra = std::slice::from_raw_parts(pixels.cast::<u8>(), byte_count);
        let rgba = bgra_to_opaque_rgba(bgra);

        let png_data = encode_rgba_png(width as u32, height as u32, &rgba)?;
        drop(resources);
        Ok(png_data)
    }
}

pub fn handle(stream: &mut TcpStream) {
    let lock = SCREENSHOT_LOCK.get_or_init(|| Mutex::new(()));
    let capture = {
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        capture_primary_screen_png()
    };
    match capture {
        Ok(png) => {
            logger::info(format_args!("captured screenshot: {} bytes", png.len()));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            let _ = send_bytes_response(stream, "200 OK", &png, "image/png");
        }
        Err(err) => {
            logger::error(format_args!("screenshot error: {err}"));
            send_json_error(stream, "500 Internal Server Error", &err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn encodes_gdi_pixels_as_opaque_png() {
        let rgba = bgra_to_opaque_rgba(&[0, 0, 255, 0]);
        let png = encode_rgba_png(1, 1, &rgba).expect("PNG should encode");
        let mut reader = png::Decoder::new(Cursor::new(png))
            .read_info()
            .expect("PNG header should decode");
        let mut pixels = vec![0; reader.output_buffer_size().expect("buffer size should fit")];
        let info = reader
            .next_frame(&mut pixels)
            .expect("PNG pixels should decode");
        assert_eq!(&pixels[..info.buffer_size()], &[255, 0, 0, 255]);
    }
}
