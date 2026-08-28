use crate::logger;
use serde::{Deserialize, de};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
static SCRIPT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum ScriptInterpreter {
    Cmd,
    Pwsh,
    Absolute(String),
}

impl<'de> Deserialize<'de> for ScriptInterpreter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.eq_ignore_ascii_case("cmd") {
            return Ok(Self::Cmd);
        }
        if value.eq_ignore_ascii_case("pwsh") {
            return Ok(Self::Pwsh);
        }
        if is_windows_absolute_path(&value) {
            return Ok(Self::Absolute(value));
        }
        Err(de::Error::custom(
            "interpreter must be `cmd`, `pwsh`, or an absolute Windows path",
        ))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptMode {
    #[default]
    Auto,
    Inline,
    File,
}

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout: Option<u64>,
    pub interpreter: Option<ScriptInterpreter>,
    #[serde(default)]
    pub script_mode: ScriptMode,
}

impl ExecRequest {
    pub fn timeout_ms(&self) -> u64 {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    fn use_script_file(&self) -> bool {
        match self.script_mode {
            ScriptMode::Auto => self.command.contains(['\r', '\n']),
            ScriptMode::Inline => false,
            ScriptMode::File => true,
        }
    }
}

pub struct RunResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

enum OutputMessage {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Closed,
}

fn send_pipe_data(sender: &mpsc::Sender<OutputMessage>, stdout: bool, data: Vec<u8>) -> bool {
    if data.is_empty() {
        return true;
    }
    let message = if stdout {
        OutputMessage::Stdout(data)
    } else {
        OutputMessage::Stderr(data)
    };
    sender.send(message).is_ok()
}

#[cfg(windows)]
struct ProcessJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl ProcessJob {
    fn assign(child: &Child) -> std::io::Result<Self> {
        use std::{mem::zeroed, os::windows::io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            Ok(Self { handle })
        }
    }

    fn terminate(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
struct ProcessJob;

#[cfg(not(windows))]
impl ProcessJob {
    fn assign(_child: &Child) -> std::io::Result<Self> {
        Ok(Self)
    }
}

fn spawn_pipe_reader<R>(mut reader: R, stdout: bool, sender: mpsc::Sender<OutputMessage>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    pending.extend_from_slice(&buffer[..size]);
                    loop {
                        match std::str::from_utf8(&pending) {
                            Ok(_) => {
                                if !send_pipe_data(&sender, stdout, std::mem::take(&mut pending)) {
                                    return;
                                }
                                break;
                            }
                            Err(err) => {
                                let valid_up_to = err.valid_up_to();
                                if valid_up_to > 0 {
                                    let valid = pending.drain(..valid_up_to).collect();
                                    if !send_pipe_data(&sender, stdout, valid) {
                                        return;
                                    }
                                }
                                let Some(error_len) = err.error_len() else {
                                    break;
                                };
                                pending.drain(..error_len);
                                if !send_pipe_data(&sender, stdout, "�".as_bytes().to_vec()) {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    let _ = sender.send(OutputMessage::Stderr(
                        format!("failed to read process output: {err}\r\n").into_bytes(),
                    ));
                    break;
                }
            }
        }
        if !pending.is_empty()
            && !send_pipe_data(
                &sender,
                stdout,
                String::from_utf8_lossy(&pending).into_owned().into_bytes(),
            )
        {
            return;
        }
        let _ = sender.send(OutputMessage::Closed);
    });
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child, job: Option<&ProcessJob>) {
    if let Some(job) = job
        && job.terminate().is_ok()
    {
        return;
    }
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

#[cfg(not(windows))]
fn terminate_process_tree(child: &mut Child, _job: Option<&ProcessJob>) {
    let _ = child.kill();
}

fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let unc_absolute = bytes.len() >= 3
        && matches!(bytes[0], b'\\' | b'/')
        && bytes[1] == bytes[0]
        && !matches!(bytes[2], b'\\' | b'/');
    drive_absolute || unc_absolute
}

fn interpreter_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

fn is_cmd(path: &str) -> bool {
    matches!(
        interpreter_name(path).to_ascii_lowercase().as_str(),
        "cmd" | "cmd.exe"
    )
}

fn is_pwsh(path: &str) -> bool {
    matches!(
        interpreter_name(path).to_ascii_lowercase().as_str(),
        "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe"
    )
}

fn script_extension(req: &ExecRequest) -> &'static str {
    match req.interpreter.as_ref() {
        None | Some(ScriptInterpreter::Cmd) => "cmd",
        Some(ScriptInterpreter::Pwsh) => "ps1",
        Some(ScriptInterpreter::Absolute(path)) if is_cmd(path) => "cmd",
        Some(ScriptInterpreter::Absolute(path)) if is_pwsh(path) => "ps1",
        Some(ScriptInterpreter::Absolute(_)) => "script",
    }
}

struct TemporaryScript {
    path: PathBuf,
}

impl TemporaryScript {
    fn new(req: &ExecRequest) -> std::io::Result<Self> {
        let extension = script_extension(req);
        for _ in 0..100 {
            let id = SCRIPT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lcr-script-{}-{id}.{extension}",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let temporary = Self { path };
                    write_script(req, &mut file)?;
                    file.flush()?;
                    return Ok(temporary);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a temporary script file",
        ))
    }
}

impl Drop for TemporaryScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_script(req: &ExecRequest, file: &mut File) -> std::io::Result<()> {
    match script_extension(req) {
        "cmd" => file.write_all(b"@chcp 65001 >nul\r\n")?,
        "ps1" => file.write_all(&[0xef, 0xbb, 0xbf])?,
        _ => {}
    }
    file.write_all(req.command.as_bytes())
}

fn configure_cmd(program: &str, script: &str) -> Command {
    let mut command = Command::new(program);
    let command_line = format!("chcp 65001 >nul & {script}");
    command.args(["/d", "/s", "/c", &command_line]);
    command
}

fn configure_pwsh(program: &str, script: &str) -> Command {
    let mut command = Command::new(program);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    command
}

fn build_inline_command(req: &ExecRequest) -> Command {
    match req.interpreter.as_ref() {
        None | Some(ScriptInterpreter::Cmd) => configure_cmd("cmd.exe", &req.command),
        Some(ScriptInterpreter::Pwsh) => configure_pwsh("pwsh.exe", &req.command),
        Some(ScriptInterpreter::Absolute(path)) if is_cmd(path) => {
            configure_cmd(path, &req.command)
        }
        Some(ScriptInterpreter::Absolute(path)) if is_pwsh(path) => {
            configure_pwsh(path, &req.command)
        }
        Some(ScriptInterpreter::Absolute(path)) => {
            let mut command = Command::new(path);
            command.args(["-c", &req.command]);
            command
        }
    }
}

fn configure_cmd_file(program: &str, path: &Path) -> Command {
    let mut command = Command::new(program);
    command.args(["/d", "/c"]).arg(path);
    command
}

fn configure_pwsh_file(program: &str, path: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        .arg(path);
    command
}

fn build_file_command(req: &ExecRequest, path: &Path) -> Command {
    match req.interpreter.as_ref() {
        None | Some(ScriptInterpreter::Cmd) => configure_cmd_file("cmd.exe", path),
        Some(ScriptInterpreter::Pwsh) => configure_pwsh_file("pwsh.exe", path),
        Some(ScriptInterpreter::Absolute(program)) if is_cmd(program) => {
            configure_cmd_file(program, path)
        }
        Some(ScriptInterpreter::Absolute(program)) if is_pwsh(program) => {
            configure_pwsh_file(program, path)
        }
        Some(ScriptInterpreter::Absolute(program)) => {
            let mut command = Command::new(program);
            command.arg(path);
            command
        }
    }
}

fn prepare_command(req: &ExecRequest) -> std::io::Result<(Command, Option<TemporaryScript>)> {
    if !req.use_script_file() {
        return Ok((build_inline_command(req), None));
    }
    let temporary = TemporaryScript::new(req)?;
    let command = build_file_command(req, &temporary.path);
    Ok((command, Some(temporary)))
}

pub fn run_command<F>(req: &ExecRequest, mut on_output: F) -> std::io::Result<RunResult>
where
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    let (mut command, _temporary_script) = prepare_command(req)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let job = match ProcessJob::assign(&child) {
        Ok(job) => Some(job),
        Err(err) => {
            logger::error(format_args!(
                "failed to assign process {} to Job Object: {err}",
                child.id()
            ));
            None
        }
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let (sender, receiver) = mpsc::channel();
    spawn_pipe_reader(stdout, true, sender.clone());
    spawn_pipe_reader(stderr, false, sender);

    let timeout_ms = req.timeout_ms();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut process_status = None;
    let mut closed_pipes = 0;
    while process_status.is_none() || closed_pipes < 2 {
        if !timed_out
            && (process_status.is_none() || closed_pipes < 2)
            && Instant::now() >= deadline
        {
            timed_out = true;
            terminate_process_tree(&mut child, job.as_ref());
        }
        if process_status.is_none() {
            process_status = child.try_wait()?;
        }
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(OutputMessage::Stdout(data)) => {
                if let Err(err) = on_output(true, &data) {
                    terminate_process_tree(&mut child, job.as_ref());
                    let _ = child.wait();
                    return Err(err);
                }
            }
            Ok(OutputMessage::Stderr(data)) => {
                if let Err(err) = on_output(false, &data) {
                    terminate_process_tree(&mut child, job.as_ref());
                    let _ = child.wait();
                    return Err(err);
                }
            }
            Ok(OutputMessage::Closed) => closed_pipes += 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => closed_pipes = 2,
        }
    }
    let status = match process_status {
        Some(status) => status,
        None => child.wait()?,
    };
    Ok(RunResult {
        exit_code: if timed_out { None } else { status.code() },
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: &str) -> ExecRequest {
        serde_json::from_str(json).expect("request should be valid")
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn defaults_to_cmd() {
        let command = build_inline_command(&request(r#"{"command":"echo hello"}"#));
        assert_eq!(command.get_program(), "cmd.exe");
        assert_eq!(
            args(&command),
            ["/d", "/s", "/c", "chcp 65001 >nul & echo hello"]
        );
    }

    #[test]
    fn supports_pwsh() {
        let command = build_inline_command(&request(
            r#"{"command":"Get-Process","interpreter":"pwsh"}"#,
        ));
        assert_eq!(command.get_program(), "pwsh.exe");
        assert_eq!(
            args(&command),
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Process"
            ]
        );
    }

    #[test]
    fn supports_absolute_interpreter_path() {
        let command = build_inline_command(&request(
            r#"{"command":"print('hello')","interpreter":"C:\\Python313\\python.exe"}"#,
        ));
        assert_eq!(command.get_program(), r"C:\Python313\python.exe");
        assert_eq!(args(&command), ["-c", "print('hello')"]);
    }

    #[test]
    fn applies_known_arguments_to_absolute_pwsh_path() {
        let command = build_inline_command(&request(
            r#"{"command":"Get-Date","interpreter":"C:\\Program Files\\PowerShell\\7\\pwsh.exe"}"#,
        ));
        assert_eq!(
            args(&command),
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Date"
            ]
        );
    }

    #[test]
    fn rejects_relative_interpreter_path() {
        let result = serde_json::from_str::<ExecRequest>(
            r#"{"command":"echo hello","interpreter":"tools\\shell.exe"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn recognizes_windows_absolute_paths() {
        assert!(is_windows_absolute_path(r"C:\Tools\shell.exe"));
        assert!(is_windows_absolute_path(r"\\server\share\shell.exe"));
        assert!(!is_windows_absolute_path(r"C:shell.exe"));
        assert!(!is_windows_absolute_path(r"Tools\shell.exe"));
    }

    #[test]
    fn auto_mode_uses_file_only_for_multiline_scripts() {
        assert!(!request(r#"{"command":"echo hello"}"#).use_script_file());
        assert!(request("{\"command\":\"echo one\\necho two\"}").use_script_file());
        assert!(request("{\"command\":\"echo one\\recho two\"}").use_script_file());
    }

    #[test]
    fn script_mode_can_force_inline_or_file_execution() {
        assert!(request(r#"{"command":"echo hello","script_mode":"file"}"#).use_script_file());
        assert!(
            !request("{\"command\":\"echo one\\necho two\",\"script_mode\":\"inline\"}")
                .use_script_file()
        );
    }

    #[test]
    fn temporary_cmd_script_is_created_and_removed() {
        let req = request(r#"{"command":"echo hello","script_mode":"file"}"#);
        let (command, temporary) = prepare_command(&req).expect("command should be prepared");
        let temporary = temporary.expect("temporary script should exist");
        let path = temporary.path.clone();

        assert_eq!(command.get_program(), "cmd.exe");
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("cmd")
        );
        assert_eq!(
            std::fs::read(&path).expect("script should be readable"),
            b"@chcp 65001 >nul\r\necho hello"
        );
        assert_eq!(args(&command)[..2], ["/d", "/c"]);
        assert_eq!(args(&command)[2], path.to_string_lossy());

        drop(temporary);
        assert!(!path.exists());
    }
}
