use crate::http::{send_json_error, send_response};
use serde::Serialize;
use std::net::TcpStream;
use windows_sys::Win32::{
    Foundation::{BOOL, HWND, LPARAM, RECT, SetLastError},
    UI::{
        Input::KeyboardAndMouse::IsWindowEnabled,
        WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongW, GetWindowRect,
            GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
            IsWindowVisible, IsZoomed, WS_EX_TOPMOST,
        },
    },
};

#[derive(Debug, Serialize)]
struct WindowRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Serialize)]
struct WindowInfo {
    hwnd: String,
    pid: u32,
    thread_id: u32,
    title: String,
    rect: WindowRect,
    top_level: bool,
    foreground: bool,
    topmost: bool,
    visible: bool,
    enabled: bool,
    minimized: bool,
    maximized: bool,
}

struct EnumContext {
    windows: Vec<WindowInfo>,
    foreground: HWND,
}

fn format_hwnd(hwnd: HWND) -> String {
    format!("0x{:X}", hwnd as usize)
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let context = &mut *(lparam as *mut EnumContext);
        let mut pid = 0;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut pid);
        let title_length = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut title = vec![0u16; title_length + 1];
        let copied = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
        title.truncate(copied);

        let mut rect: RECT = std::mem::zeroed();
        let has_rect = GetWindowRect(hwnd, &mut rect) != 0;
        if !has_rect {
            rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
        }
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        context.windows.push(WindowInfo {
            hwnd: format_hwnd(hwnd),
            pid,
            thread_id,
            title: String::from_utf16_lossy(&title),
            rect: WindowRect {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
                width: rect.right.saturating_sub(rect.left),
                height: rect.bottom.saturating_sub(rect.top),
            },
            top_level: true,
            foreground: hwnd == context.foreground,
            topmost: ex_style & WS_EX_TOPMOST != 0,
            visible: IsWindowVisible(hwnd) != 0,
            enabled: IsWindowEnabled(hwnd) != 0,
            minimized: IsIconic(hwnd) != 0,
            maximized: IsZoomed(hwnd) != 0,
        });
        1
    }
}

pub fn handle(stream: &mut TcpStream) {
    let foreground_hwnd = unsafe { GetForegroundWindow() };
    let mut context = EnumContext {
        windows: Vec::new(),
        foreground: foreground_hwnd,
    };
    unsafe { SetLastError(0) };
    let result = unsafe {
        EnumWindows(
            Some(collect_window),
            (&mut context as *mut EnumContext) as LPARAM,
        )
    };
    let error = std::io::Error::last_os_error();
    if result == 0 && error.raw_os_error().is_some_and(|code| code != 0) {
        send_json_error(
            stream,
            "500 Internal Server Error",
            &format!("failed to enumerate windows: {error}"),
        );
        return;
    }
    let body = serde_json::json!({
        "foreground_hwnd": if foreground_hwnd.is_null() { None } else { Some(format_hwnd(foreground_hwnd)) },
        "windows": context.windows,
    })
    .to_string();
    let _ = send_response(stream, "200 OK", &body, "application/json");
}
