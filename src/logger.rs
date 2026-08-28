use clap::ValueEnum;
use std::{
    fmt::{Display, Formatter},
    io::Write,
    sync::atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
#[repr(u8)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl Display for Level {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn set_level(level: Level) {
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);
}

fn enabled(level: Level) -> bool {
    level as u8 >= MIN_LEVEL.load(Ordering::Relaxed)
}

#[cfg(windows)]
fn local_time() -> (u16, u16, u16) {
    use std::mem::zeroed;
    use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

    unsafe {
        let mut time: SYSTEMTIME = zeroed();
        GetLocalTime(&mut time);
        (time.wHour, time.wMinute, time.wSecond)
    }
}

#[cfg(not(windows))]
fn local_time() -> (u16, u16, u16) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        % 86_400;
    (
        (seconds / 3_600) as u16,
        ((seconds % 3_600) / 60) as u16,
        (seconds % 60) as u16,
    )
}

fn write_lines(
    writer: &mut impl Write,
    level: Level,
    time: (u16, u16, u16),
    message: &str,
) -> std::io::Result<()> {
    let (hour, minute, second) = time;
    let mut lines = message.lines().peekable();
    if lines.peek().is_none() {
        writeln!(
            writer,
            "[{}] {hour:02}:{minute:02}:{second:02}",
            level.as_str()
        )?;
    } else {
        for line in lines {
            writeln!(
                writer,
                "[{}] {hour:02}:{minute:02}:{second:02} {line}",
                level.as_str()
            )?;
        }
    }
    writer.flush()
}

pub fn log(level: Level, message: impl Display) {
    if !enabled(level) {
        return;
    }
    let message = message.to_string();
    let time = local_time();
    let result = match level {
        Level::Error => write_lines(&mut std::io::stderr().lock(), level, time, &message),
        Level::Debug | Level::Info | Level::Warn => {
            write_lines(&mut std::io::stdout().lock(), level, time, &message)
        }
    };
    let _ = result;
}

pub fn debug(message: impl Display) {
    log(Level::Debug, message);
}

pub fn info(message: impl Display) {
    log(Level::Info, message);
}

pub fn warn(message: impl Display) {
    log(Level::Warn, message);
}

pub fn error(message: impl Display) {
    log(Level::Error, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_log_prefix() {
        let mut output = Vec::new();
        write_lines(&mut output, Level::Info, (8, 9, 7), "ready").unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "[info] 08:09:07 ready\n"
        );
    }

    #[test]
    fn prefixes_every_message_line() {
        let mut output = Vec::new();
        write_lines(&mut output, Level::Error, (23, 59, 1), "one\r\ntwo").unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "[error] 23:59:01 one\n[error] 23:59:01 two\n"
        );
    }

    #[test]
    fn default_level_ignores_debug_messages() {
        assert!(!enabled(Level::Debug));
        assert!(enabled(Level::Info));
        assert!(enabled(Level::Warn));
        assert!(enabled(Level::Error));
    }
}
