use crate::{
    http::{send_json_error, send_response},
    routes::desktop::InputDesktopGuard,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    net::TcpStream,
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
            VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT,
            VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE,
            VK_TAB, VK_UP,
        },
        WindowsAndMessaging::{
            GetForegroundWindow, GetSystemMetrics, IsIconic, IsWindow, SM_CXSCREEN, SM_CYSCREEN,
            SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
    },
};

static CONTROL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const MAX_ACTIONS: usize = 256;
const MAX_TEXT_CODE_UNITS: usize = 4096;
const DEFAULT_ACTION_DELAY_MS: u64 = 50;
const MAX_ACTION_DELAY_MS: u64 = 5_000;
const MAX_TOTAL_ACTION_DELAY_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
struct ControlRequest {
    actions: Vec<Action>,
    #[serde(default = "default_action_delay_ms", alias = "delay_ms")]
    delay: u64,
}

fn default_action_delay_ms() -> u64 {
    DEFAULT_ACTION_DELAY_MS
}

fn total_action_delay_ms(action_count: usize, delay: u64) -> u64 {
    (action_count.saturating_sub(1) as u64).saturating_mul(delay)
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KeyState {
    Down,
    Up,
    #[default]
    Press,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    FocusWindow {
        hwnd: Value,
    },
    Keyboard {
        key: String,
        #[serde(default)]
        state: KeyState,
    },
    Text {
        text: String,
    },
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: MouseButton,
        #[serde(default)]
        state: KeyState,
    },
    MouseClick {
        #[serde(default = "default_mouse_button")]
        button: MouseButton,
    },
    MouseWheel {
        delta: i32,
    },
}

fn default_mouse_button() -> MouseButton {
    MouseButton::Left
}

fn parse_hwnd(value: &Value) -> Result<HWND, String> {
    let raw = match value {
        Value::Number(number) => usize::try_from(
            number
                .as_u64()
                .ok_or("hwnd number must be a non-negative integer")?,
        )
        .map_err(|_| "hwnd is too large for this Windows architecture")?,
        Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                usize::from_str_radix(hex, 16).map_err(|_| "invalid hexadecimal hwnd")?
            } else {
                text.parse::<usize>().map_err(|_| "invalid decimal hwnd")?
            }
        }
        _ => return Err("hwnd must be an integer or string".to_string()),
    };
    if raw == 0 {
        return Err("hwnd must not be zero".to_string());
    }
    Ok(raw as HWND)
}

fn format_hwnd(hwnd: HWND) -> Option<String> {
    (!hwnd.is_null()).then(|| format!("0x{:X}", hwnd as usize))
}

fn key_code(key: &str) -> Result<u16, String> {
    let normalized = key.trim().to_ascii_uppercase();
    if normalized.len() == 1 {
        let value = normalized.as_bytes()[0];
        if value.is_ascii_alphanumeric() {
            return Ok(value as u16);
        }
    }
    if let Some(hex) = normalized.strip_prefix("0X") {
        return u16::from_str_radix(hex, 16).map_err(|_| "invalid hexadecimal key code".into());
    }
    if let Some(function) = normalized.strip_prefix('F')
        && let Ok(number) = function.parse::<u16>()
        && (1..=24).contains(&number)
    {
        return Ok(0x70 + number - 1);
    }
    match normalized.as_str() {
        "BACKSPACE" | "BACK" => Ok(VK_BACK),
        "TAB" => Ok(VK_TAB),
        "ENTER" | "RETURN" => Ok(VK_RETURN),
        "SHIFT" => Ok(VK_SHIFT),
        "CTRL" | "CONTROL" => Ok(VK_CONTROL),
        "ALT" | "MENU" => Ok(VK_MENU),
        "ESC" | "ESCAPE" => Ok(VK_ESCAPE),
        "SPACE" => Ok(VK_SPACE),
        "PAGEUP" | "PAGE_UP" => Ok(VK_PRIOR),
        "PAGEDOWN" | "PAGE_DOWN" => Ok(VK_NEXT),
        "END" => Ok(VK_END),
        "HOME" => Ok(VK_HOME),
        "LEFT" => Ok(VK_LEFT),
        "UP" => Ok(VK_UP),
        "RIGHT" => Ok(VK_RIGHT),
        "DOWN" => Ok(VK_DOWN),
        "INSERT" => Ok(VK_INSERT),
        "DELETE" | "DEL" => Ok(VK_DELETE),
        "WIN" | "WINDOWS" => Ok(VK_LWIN),
        _ => Err(format!("unsupported key: {key}")),
    }
}

fn is_extended_key(key: u16) -> bool {
    matches!(
        key,
        VK_PRIOR
            | VK_NEXT
            | VK_END
            | VK_HOME
            | VK_LEFT
            | VK_UP
            | VK_RIGHT
            | VK_DOWN
            | VK_INSERT
            | VK_DELETE
    )
}

fn keyboard_input(key: u16, key_up: bool) -> INPUT {
    let mut flags = if key_up { KEYEVENTF_KEYUP } else { 0 };
    if is_extended_key(key) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(code_unit: u16, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: code_unit,
                dwFlags: KEYEVENTF_UNICODE | if key_up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent != inputs.len() as u32 {
        Err(format!(
            "only {sent} of {} input events were accepted: {}",
            inputs.len(),
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

fn button_flags(button: MouseButton) -> (u32, u32) {
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

fn execute_action(action: &Action) -> Result<(), String> {
    match action {
        Action::FocusWindow { hwnd } => {
            let hwnd = parse_hwnd(hwnd)?;
            if unsafe { IsWindow(hwnd) } == 0 {
                return Err("hwnd does not identify a current window".to_string());
            }
            if unsafe { IsIconic(hwnd) } != 0 {
                unsafe { ShowWindow(hwnd, SW_RESTORE) };
            }
            unsafe { SetForegroundWindow(hwnd) };
            if unsafe { GetForegroundWindow() } != hwnd {
                return Err("Windows refused to focus the requested window".to_string());
            }
            Ok(())
        }
        Action::Keyboard { key, state } => {
            let key = key_code(key)?;
            match state {
                KeyState::Down => send_inputs(&[keyboard_input(key, false)]),
                KeyState::Up => send_inputs(&[keyboard_input(key, true)]),
                KeyState::Press => {
                    send_inputs(&[keyboard_input(key, false), keyboard_input(key, true)])
                }
            }
        }
        Action::Text { text } => {
            let code_units = text.encode_utf16().count();
            if code_units > MAX_TEXT_CODE_UNITS {
                return Err(format!(
                    "text must not exceed {MAX_TEXT_CODE_UNITS} UTF-16 code units"
                ));
            }
            let mut inputs = Vec::with_capacity(code_units * 2);
            for code_unit in text.encode_utf16() {
                inputs.push(unicode_input(code_unit, false));
                inputs.push(unicode_input(code_unit, true));
            }
            if inputs.is_empty() {
                Ok(())
            } else {
                send_inputs(&inputs)
            }
        }
        Action::MouseMove { x, y } => {
            let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
            let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
            if width <= 1 || height <= 1 {
                return Err("screen dimensions are unavailable".to_string());
            }
            if *x < 0 || *x >= width || *y < 0 || *y >= height {
                return Err(format!(
                    "mouse coordinates must be within 0..{} and 0..{}",
                    width - 1,
                    height - 1
                ));
            }
            let normalized_x = ((*x as i64 * 65_535) / (width - 1) as i64) as i32;
            let normalized_y = ((*y as i64 * 65_535) / (height - 1) as i64) as i32;
            send_inputs(&[mouse_input(
                normalized_x,
                normalized_y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
            )])
        }
        Action::MouseButton { button, state } => {
            let (down, up) = button_flags(*button);
            match state {
                KeyState::Down => send_inputs(&[mouse_input(0, 0, 0, down)]),
                KeyState::Up => send_inputs(&[mouse_input(0, 0, 0, up)]),
                KeyState::Press => {
                    send_inputs(&[mouse_input(0, 0, 0, down), mouse_input(0, 0, 0, up)])
                }
            }
        }
        Action::MouseClick { button } => {
            let (down, up) = button_flags(*button);
            send_inputs(&[mouse_input(0, 0, 0, down), mouse_input(0, 0, 0, up)])
        }
        Action::MouseWheel { delta } => {
            send_inputs(&[mouse_input(0, 0, *delta as u32, MOUSEEVENTF_WHEEL)])
        }
    }
}

pub fn handle(stream: &mut TcpStream, body: &[u8]) {
    let request: ControlRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(err) => {
            send_json_error(stream, "400 Bad Request", &format!("invalid json: {err}"));
            return;
        }
    };
    if request.actions.is_empty() {
        send_json_error(stream, "400 Bad Request", "actions must not be empty");
        return;
    }
    if request.actions.len() > MAX_ACTIONS {
        send_json_error(
            stream,
            "400 Bad Request",
            &format!("actions must not contain more than {MAX_ACTIONS} items"),
        );
        return;
    }
    if request.delay > MAX_ACTION_DELAY_MS {
        send_json_error(
            stream,
            "400 Bad Request",
            &format!("delay must not exceed {MAX_ACTION_DELAY_MS} ms"),
        );
        return;
    }
    if total_action_delay_ms(request.actions.len(), request.delay) > MAX_TOTAL_ACTION_DELAY_MS {
        send_json_error(
            stream,
            "400 Bad Request",
            &format!("total action delay must not exceed {MAX_TOTAL_ACTION_DELAY_MS} ms"),
        );
        return;
    }

    let lock = CONTROL_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _desktop_guard = match InputDesktopGuard::enter() {
        Ok(guard) => guard,
        Err(err) => {
            send_json_error(
                stream,
                "500 Internal Server Error",
                &format!("failed to enter input desktop: {err}"),
            );
            return;
        }
    };
    for (index, action) in request.actions.iter().enumerate() {
        if let Err(error) = execute_action(action) {
            let body = serde_json::json!({
                "ok": false,
                "completed_actions": index,
                "failed_action": index,
                "error": error,
                "foreground_hwnd": format_hwnd(unsafe { GetForegroundWindow() }),
            })
            .to_string();
            let _ = send_response(stream, "409 Conflict", &body, "application/json");
            return;
        }
        if index + 1 < request.actions.len() && request.delay > 0 {
            thread::sleep(Duration::from_millis(request.delay));
        }
    }
    let body = serde_json::json!({
        "ok": true,
        "completed_actions": request.actions.len(),
        "foreground_hwnd": format_hwnd(unsafe { GetForegroundWindow() }),
    })
    .to_string();
    let _ = send_response(stream, "200 OK", &body, "application/json");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_keys() {
        assert_eq!(key_code("G").unwrap(), b'G' as u16);
        assert_eq!(key_code("enter").unwrap(), VK_RETURN);
        assert_eq!(key_code("F12").unwrap(), 0x7b);
        assert!(key_code("unknown-key").is_err());
    }

    #[test]
    fn parses_decimal_and_hexadecimal_hwnds() {
        assert_eq!(parse_hwnd(&serde_json::json!(123)).unwrap() as usize, 123);
        assert_eq!(
            parse_hwnd(&serde_json::json!("0x7B")).unwrap() as usize,
            123
        );
        assert!(parse_hwnd(&serde_json::json!(0)).is_err());
    }

    #[test]
    fn control_delay_defaults_to_fifty_milliseconds_and_is_configurable() {
        let default: ControlRequest =
            serde_json::from_str(r#"{"actions":[{"type":"keyboard","key":"G"}]}"#).unwrap();
        assert_eq!(default.delay, DEFAULT_ACTION_DELAY_MS);

        let configured: ControlRequest =
            serde_json::from_str(r#"{"actions":[{"type":"keyboard","key":"G"}],"delay":125}"#)
                .unwrap();
        assert_eq!(configured.delay, 125);
        assert_eq!(total_action_delay_ms(1, 5_000), 0);
        assert_eq!(total_action_delay_ms(7, 5_000), 30_000);
        assert_eq!(total_action_delay_ms(8, 5_000), 35_000);
    }
}
