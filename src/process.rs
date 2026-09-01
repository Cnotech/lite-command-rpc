use crate::logger;
use serde::{Deserialize, Serialize, de};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const ELEVATED_OUTPUT_LIMIT: usize = 8 * 1024 * 1024 + 1;
const ELEVATED_CANCEL_WAIT: Duration = Duration::from_secs(30);
static SCRIPT_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(windows)]
static ELEVATION_PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Serialize)]
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

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptMode {
    #[default]
    Auto,
    Inline,
    File,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputEncoding {
    #[default]
    Utf8,
    Oem,
    Ansi,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecRequest {
    pub command: Option<String>,
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout: Option<u64>,
    pub interpreter: Option<ScriptInterpreter>,
    #[serde(default)]
    pub script_mode: ScriptMode,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub require_admin: bool,
    #[serde(default)]
    pub output_encoding: OutputEncoding,
    #[serde(skip)]
    pub cwd_guard: Option<crate::config::PathGuard>,
    #[serde(skip)]
    pub program_guard: Option<crate::config::PathGuard>,
}

impl ExecRequest {
    pub fn timeout_ms(&self) -> u64 {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT_MS)
    }

    pub fn output_encoding(&self) -> OutputEncoding {
        self.output_encoding
    }

    fn use_script_file(&self) -> bool {
        if self.program.is_some() {
            return false;
        }
        match self.script_mode {
            ScriptMode::Auto => self
                .command
                .as_deref()
                .is_some_and(|command| command.contains(['\r', '\n'])),
            ScriptMode::Inline => false,
            ScriptMode::File => true,
        }
    }
}

#[cfg(windows)]
pub fn is_process_elevated() -> Result<bool, String> {
    use std::mem::zeroed;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!(
                "failed to inspect the LCR process token: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut elevation: TOKEN_ELEVATION = zeroed();
        let mut returned = 0;
        let success = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) != 0;
        let error = (!success).then(std::io::Error::last_os_error);
        CloseHandle(token);
        error.map_or_else(
            || Ok(elevation.TokenIsElevated != 0),
            |error| Err(format!("failed to inspect the LCR process token: {error}")),
        )
    }
}

#[cfg(not(windows))]
pub fn is_process_elevated() -> Result<bool, String> {
    Err("require_admin is only supported on Windows".to_string())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub terminated: bool,
}

#[derive(Deserialize, Serialize)]
struct ElevatedResult {
    result: Option<RunResult>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    error: Option<String>,
}

fn append_elevated_output(target: &mut Vec<u8>, data: &[u8]) {
    let available = ELEVATED_OUTPUT_LIMIT.saturating_sub(target.len());
    target.extend_from_slice(&data[..data.len().min(available)]);
}

fn execute_elevated_request(
    request_path: &Path,
    cancel_path: &Path,
    ready_path: &Path,
) -> Result<ElevatedResult, String> {
    let mut request = serde_json::from_slice::<ExecRequest>(
        &std::fs::read(request_path)
            .map_err(|err| format!("failed to read elevated request: {err}"))?,
    )
    .map_err(|err| format!("invalid elevated request: {err}"))?;
    request.require_admin = false;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let watcher_cancel = Arc::clone(&cancellation);
    let watcher_finished = Arc::new(AtomicBool::new(false));
    let watcher_done = Arc::clone(&watcher_finished);
    let cancel_path = cancel_path.to_path_buf();
    let watcher = thread::spawn(move || {
        while !watcher_done.load(Ordering::Relaxed) {
            if std::fs::read(&cancel_path)
                .is_ok_and(|bytes| bytes.first().is_some_and(|value| *value != 0))
            {
                watcher_cancel.store(true, Ordering::Relaxed);
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    });
    let result = run_command_observed(
        &request,
        &cancellation,
        |pid| {
            let _ = std::fs::write(ready_path, pid.to_string());
        },
        |is_stdout, data| {
            let target = if is_stdout { &mut stdout } else { &mut stderr };
            append_elevated_output(target, data);
            Ok(())
        },
    )
    .map_err(|err| format!("elevated command failed to start: {err}"));
    watcher_finished.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let result = result?;
    Ok(ElevatedResult {
        result: Some(result),
        stdout,
        stderr,
        error: None,
    })
}

pub fn run_elevated_helper(
    request_path: &Path,
    result_path: &Path,
    cancel_path: &Path,
    ready_path: &Path,
) -> Result<(), String> {
    if !is_process_elevated()? {
        return Err("elevated helper is not running with administrator privileges".to_string());
    }
    let result = match execute_elevated_request(request_path, cancel_path, ready_path) {
        Ok(result) => result,
        Err(error) => ElevatedResult {
            result: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            error: Some(error),
        },
    };
    let bytes = serde_json::to_vec(&result)
        .map_err(|err| format!("failed to encode elevated result: {err}"))?;
    std::fs::write(result_path, bytes)
        .map_err(|err| format!("failed to write elevated result: {err}"))
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
pub(crate) struct ProcessJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
fn breakaway_flag_for_limits(limits: u32) -> u32 {
    use windows_sys::Win32::System::{
        JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK},
        Threading::CREATE_BREAKAWAY_FROM_JOB,
    };

    if limits & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0 {
        0
    } else if limits & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0 {
        CREATE_BREAKAWAY_FROM_JOB
    } else {
        0
    }
}

#[cfg(windows)]
fn detached_creation_flags() -> u32 {
    use std::mem::zeroed;
    use windows_sys::Win32::System::{
        JobObjects::{
            IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
        },
        Threading::GetCurrentProcess,
    };

    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) == 0
            || in_job == 0
        {
            return 0;
        }
        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        if QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            &mut information as *mut _ as _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        ) == 0
        {
            return 0;
        }
        breakaway_flag_for_limits(information.BasicLimitInformation.LimitFlags)
    }
}

#[cfg(windows)]
impl ProcessJob {
    pub(crate) fn assign(
        child: &Child,
        kill_on_close: bool,
        allow_breakaway: bool,
    ) -> std::io::Result<Self> {
        use std::{mem::zeroed, os::windows::io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
            },
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            if kill_on_close || allow_breakaway {
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                limits.BasicLimitInformation.LimitFlags = (if kill_on_close {
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                } else {
                    0
                }) | if allow_breakaway {
                    JOB_OBJECT_LIMIT_BREAKAWAY_OK
                } else {
                    0
                };
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

    fn active_processes(&self) -> std::io::Result<u32> {
        use std::mem::zeroed;
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };

        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as *mut _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(accounting.ActiveProcesses)
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
pub(crate) struct ProcessJob;

#[cfg(not(windows))]
impl ProcessJob {
    pub(crate) fn assign(
        _child: &Child,
        _kill_on_close: bool,
        _allow_breakaway: bool,
    ) -> std::io::Result<Self> {
        Ok(Self)
    }

    fn active_processes(&self) -> std::io::Result<u32> {
        Ok(0)
    }
}

fn spawn_pipe_reader<R>(mut reader: R, stdout: bool, sender: mpsc::Sender<OutputMessage>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    if !send_pipe_data(&sender, stdout, buffer[..size].to_vec()) {
                        return;
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
        let _ = sender.send(OutputMessage::Closed);
    });
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child, job: Option<&ProcessJob>) -> std::io::Result<()> {
    let mut job_error = None;
    if let Some(job) = job {
        match job.terminate() {
            Ok(()) => return Ok(()),
            Err(err) => job_error = Some(err),
        }
    }
    let taskkill = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if taskkill.is_ok_and(|status| status.success()) {
        return Ok(());
    }
    let child_result = child.kill();
    if let Some(err) = job_error {
        return Err(err);
    }
    child_result.or_else(|err| {
        (err.kind() == std::io::ErrorKind::InvalidInput)
            .then_some(())
            .ok_or(err)
    })
}

#[cfg(not(windows))]
fn terminate_process_tree(child: &mut Child, _job: Option<&ProcessJob>) -> std::io::Result<()> {
    child.kill()
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
            let directory = req
                .cwd
                .as_deref()
                .map(Path::new)
                .map(Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir);
            let path = directory.join(format!(
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
    file.write_all(req.command.as_deref().unwrap_or_default().as_bytes())
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
    if let Some(program) = &req.program {
        let mut command = Command::new(program);
        command.args(&req.args);
        return command;
    }
    let script = req.command.as_deref().unwrap_or_default();
    match req.interpreter.as_ref() {
        None | Some(ScriptInterpreter::Cmd) => configure_cmd("cmd.exe", script),
        Some(ScriptInterpreter::Pwsh) => configure_pwsh("pwsh.exe", script),
        Some(ScriptInterpreter::Absolute(path)) if is_cmd(path) => configure_cmd(path, script),
        Some(ScriptInterpreter::Absolute(path)) if is_pwsh(path) => configure_pwsh(path, script),
        Some(ScriptInterpreter::Absolute(path)) => {
            let mut command = Command::new(path);
            command.args(["-c", script]);
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
    match (&req.command, &req.program) {
        (Some(_), Some(_)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "command and program are mutually exclusive",
            ));
        }
        (None, None) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "either command or program is required",
            ));
        }
        _ => {}
    }
    if req.program.is_some() && req.interpreter.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "interpreter cannot be used with program",
        ));
    }
    if !req.use_script_file() {
        return Ok((build_inline_command(req), None));
    }
    let temporary = TemporaryScript::new(req)?;
    let command = build_file_command(req, &temporary.path);
    Ok((command, Some(temporary)))
}

#[cfg(windows)]
struct ElevatedFiles {
    request: PathBuf,
    result: PathBuf,
    cancel: PathBuf,
    ready: PathBuf,
    request_file: Option<File>,
    result_file: Option<File>,
    cancel_file: Option<File>,
    ready_file: Option<File>,
}

#[cfg(windows)]
impl ElevatedFiles {
    fn new(request: &ExecRequest) -> std::io::Result<Self> {
        let directory = std::env::temp_dir();
        let request_bytes = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        for _ in 0..100 {
            let id = SCRIPT_ID.fetch_add(1, Ordering::Relaxed);
            let request_path = directory.join(format!(
                "lcr-elevated-{}-{id}.request.json",
                std::process::id()
            ));
            let result_path = directory.join(format!(
                "lcr-elevated-{}-{id}.result.json",
                std::process::id()
            ));
            let cancel_path =
                directory.join(format!("lcr-elevated-{}-{id}.cancel", std::process::id()));
            let ready_path =
                directory.join(format!("lcr-elevated-{}-{id}.ready", std::process::id()));
            let mut request_options = OpenOptions::new();
            request_options.write(true).create_new(true);
            let mut result_options = OpenOptions::new();
            result_options.write(true).create_new(true);
            let shared_options = || {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create_new(true);
                options
            };
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            request_options.share_mode(FILE_SHARE_READ);
            result_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            let mut cancel_options = shared_options();
            let mut ready_options = shared_options();
            cancel_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            ready_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
            let mut request_file = match request_options.open(&request_path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            };
            let result_file = match result_options.open(&result_path) {
                Ok(file) => file,
                Err(err) => {
                    drop(request_file);
                    let _ = std::fs::remove_file(&request_path);
                    return Err(err);
                }
            };
            let cancel_file = match cancel_options.open(&cancel_path) {
                Ok(file) => file,
                Err(err) => {
                    drop(request_file);
                    drop(result_file);
                    let _ = std::fs::remove_file(&request_path);
                    let _ = std::fs::remove_file(&result_path);
                    return Err(err);
                }
            };
            let ready_file = match ready_options.open(&ready_path) {
                Ok(file) => file,
                Err(err) => {
                    drop(request_file);
                    drop(result_file);
                    drop(cancel_file);
                    let _ = std::fs::remove_file(&request_path);
                    let _ = std::fs::remove_file(&result_path);
                    let _ = std::fs::remove_file(&cancel_path);
                    return Err(err);
                }
            };
            if let Err(err) = request_file
                .write_all(&request_bytes)
                .and_then(|()| request_file.flush())
            {
                drop(request_file);
                drop(result_file);
                drop(cancel_file);
                drop(ready_file);
                let _ = std::fs::remove_file(&request_path);
                let _ = std::fs::remove_file(&result_path);
                let _ = std::fs::remove_file(&cancel_path);
                let _ = std::fs::remove_file(&ready_path);
                return Err(err);
            }
            return Ok(Self {
                request: request_path,
                result: result_path,
                cancel: cancel_path,
                ready: ready_path,
                request_file: Some(request_file),
                result_file: Some(result_file),
                cancel_file: Some(cancel_file),
                ready_file: Some(ready_file),
            });
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate elevated execution files",
        ))
    }

    fn signal_cancel(&mut self) -> std::io::Result<()> {
        let file = self
            .cancel_file
            .as_mut()
            .expect("cancel file is held until elevated execution completes");
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&[1])?;
        file.flush()
    }
}

#[cfg(windows)]
impl Drop for ElevatedFiles {
    fn drop(&mut self) {
        drop(self.request_file.take());
        drop(self.result_file.take());
        drop(self.cancel_file.take());
        drop(self.ready_file.take());
        let _ = std::fs::remove_file(&self.request);
        let _ = std::fs::remove_file(&self.result);
        let _ = std::fs::remove_file(&self.cancel);
        let _ = std::fs::remove_file(&self.ready);
    }
}

#[cfg(windows)]
struct ElevationPromptPermit;

#[cfg(windows)]
impl ElevationPromptPermit {
    fn acquire() -> std::io::Result<Self> {
        ELEVATION_PROMPT_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .map(|_| Self)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "another administrator elevation prompt is already active",
                )
            })
    }
}

#[cfg(windows)]
impl Drop for ElevationPromptPermit {
    fn drop(&mut self) {
        ELEVATION_PROMPT_ACTIVE.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
fn quote_windows_argument(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_string();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push(character);
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(windows)]
fn launch_elevated_helper(
    files: &ElevatedFiles,
    cwd: Option<&str>,
) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use std::{ffi::OsStr, mem::zeroed, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::{
        Foundation::ERROR_CANCELLED,
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_HIDE,
        },
    };

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    let executable = std::env::current_exe()?;
    let parameters = format!(
        "elevated-exec --request {} --result {} --cancel {} --ready {}",
        quote_windows_argument(&files.request.to_string_lossy()),
        quote_windows_argument(&files.result.to_string_lossy()),
        quote_windows_argument(&files.cancel.to_string_lossy()),
        quote_windows_argument(&files.ready.to_string_lossy())
    );
    let verb = wide(OsStr::new("runas"));
    let executable = wide(executable.as_os_str());
    let parameters = wide(OsStr::new(&parameters));
    let directory = cwd.map(|value| wide(OsStr::new(value)));
    let mut execute: SHELLEXECUTEINFOW = unsafe { zeroed() };
    execute.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    execute.fMask = SEE_MASK_NOCLOSEPROCESS;
    execute.lpVerb = verb.as_ptr();
    execute.lpFile = executable.as_ptr();
    execute.lpParameters = parameters.as_ptr();
    execute.lpDirectory = directory
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr());
    execute.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut execute) } == 0 {
        let error = std::io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "administrator elevation was cancelled",
            )
        } else {
            std::io::Error::new(
                error.kind(),
                format!("failed to start elevated helper: {error}"),
            )
        });
    }
    Ok(execute.hProcess)
}

#[cfg(windows)]
fn run_elevated_command<S, F>(
    request: &ExecRequest,
    cancellation: Option<&AtomicBool>,
    on_started: S,
    mut on_output: F,
) -> std::io::Result<RunResult>
where
    S: FnOnce(u32),
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{GetExitCodeProcess, WaitForSingleObject},
    };

    let mut files = ElevatedFiles::new(request)?;
    let process = {
        let _prompt_permit = ElevationPromptPermit::acquire()?;
        launch_elevated_helper(&files, request.cwd.as_deref())?
    };
    let mut on_started = Some(on_started);
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(request.timeout_ms()))
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut terminated = false;
    let mut cancel_deadline = None;
    loop {
        if on_started.is_some()
            && let Ok(text) = std::fs::read_to_string(&files.ready)
            && let Ok(pid) = text.trim().parse::<u32>()
            && pid != 0
            && let Some(callback) = on_started.take()
        {
            callback(pid);
        }
        let wait = unsafe { WaitForSingleObject(process, POLL_INTERVAL.as_millis() as u32) };
        if wait == WAIT_OBJECT_0 {
            break;
        }
        if wait != WAIT_TIMEOUT {
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(process) };
            return Err(error);
        }
        if cancellation.is_some_and(|value| value.load(Ordering::Relaxed)) {
            terminated = true;
        }
        if Instant::now() >= deadline {
            timed_out = true;
        }
        if (timed_out || terminated) && cancel_deadline.is_none() {
            if let Err(err) = files.signal_cancel() {
                unsafe { CloseHandle(process) };
                return Err(std::io::Error::new(
                    err.kind(),
                    format!("failed to signal elevated helper cancellation: {err}"),
                ));
            }
            cancel_deadline = Some(Instant::now() + ELEVATED_CANCEL_WAIT);
        }
        if cancel_deadline.is_some_and(|value| Instant::now() >= value) {
            unsafe { CloseHandle(process) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "elevated helper did not stop within 30 seconds after cancellation",
            ));
        }
    }
    if on_started.is_some()
        && let Ok(text) = std::fs::read_to_string(&files.ready)
        && let Ok(pid) = text.trim().parse::<u32>()
        && pid != 0
        && let Some(callback) = on_started.take()
    {
        callback(pid);
    }
    let mut exit_code = 0;
    let exit_code_ok = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0;
    let exit_code_error = (!exit_code_ok).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(process) };
    if let Some(error) = exit_code_error {
        return Err(error);
    }
    let result = serde_json::from_slice::<ElevatedResult>(&std::fs::read(&files.result)?)
        .map_err(std::io::Error::other)?;
    on_output(true, &result.stdout)?;
    on_output(false, &result.stderr)?;
    if let Some(error) = result.error {
        return Err(std::io::Error::other(error));
    }
    let mut result = result.result.ok_or_else(|| {
        std::io::Error::other(format!(
            "elevated helper exited without a result (exit code {exit_code})"
        ))
    })?;
    if on_started.is_some() {
        return Err(std::io::Error::other(
            "elevated helper exited before reporting the command PID",
        ));
    }
    if timed_out {
        result.timed_out = true;
        result.terminated = false;
    } else if terminated {
        result.terminated = true;
    }
    Ok(result)
}

pub fn run_command<F>(req: &ExecRequest, on_output: F) -> std::io::Result<RunResult>
where
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    run_command_inner(req, false, None, |_| {}, on_output)
}

pub fn run_command_observed<S, F>(
    req: &ExecRequest,
    cancellation: &AtomicBool,
    on_started: S,
    on_output: F,
) -> std::io::Result<RunResult>
where
    S: FnOnce(u32),
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    run_command_inner(req, true, Some(cancellation), on_started, on_output)
}

fn run_command_inner<S, F>(
    req: &ExecRequest,
    require_job: bool,
    cancellation: Option<&AtomicBool>,
    on_started: S,
    mut on_output: F,
) -> std::io::Result<RunResult>
where
    S: FnOnce(u32),
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    #[cfg(windows)]
    if req.require_admin {
        let elevated = is_process_elevated().map_err(std::io::Error::other)?;
        if !elevated {
            return run_elevated_command(req, cancellation, on_started, on_output);
        }
    }
    #[cfg(not(windows))]
    if req.require_admin {
        return Err(std::io::Error::other(
            "require_admin is only supported on Windows",
        ));
    }
    let (mut command, _temporary_script) = prepare_command(req)?;
    #[cfg(windows)]
    if req.detached {
        use std::os::windows::process::CommandExt;

        command.creation_flags(detached_creation_flags());
    }
    if req.detached {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let job = match ProcessJob::assign(&child, !req.detached, false) {
        Ok(job) => Some(job),
        Err(err) if require_job => {
            let message = format!(
                "failed to assign process {} to Job Object: {err}",
                child.id()
            );
            logger::error(format_args!("{message}"));
            let _ = terminate_process_tree(&mut child, None);
            let _ = child.wait();
            return Err(std::io::Error::new(err.kind(), message));
        }
        Err(err) => {
            logger::error(format_args!(
                "failed to assign process {} to Job Object: {err}",
                child.id()
            ));
            None
        }
    };
    on_started(child.id());
    let (sender, receiver) = mpsc::channel();
    let mut closed_pipes = 0;
    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_reader(stdout, true, sender.clone());
    } else {
        closed_pipes += 1;
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_reader(stderr, false, sender.clone());
    } else {
        closed_pipes += 1;
    }
    drop(sender);

    let timeout_ms = req.timeout_ms();
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut terminated = false;
    let mut process_status = None;
    while process_status.is_none() || closed_pipes < 2 {
        if process_status.is_none() {
            process_status = child.try_wait()?;
        }
        if !timed_out
            && !terminated
            && cancellation.is_some_and(|value| value.load(Ordering::Relaxed))
        {
            let active_processes = job
                .as_ref()
                .expect("observed commands require a Job Object")
                .active_processes();
            if process_status.is_none() || !matches!(active_processes, Ok(0)) {
                terminate_process_tree(&mut child, job.as_ref())?;
                terminated = true;
            }
        } else if !timed_out
            && !terminated
            && (process_status.is_none() || closed_pipes < 2)
            && Instant::now() >= deadline
        {
            terminate_process_tree(&mut child, job.as_ref())?;
            timed_out = true;
        }
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(OutputMessage::Stdout(data)) => {
                if let Err(err) = on_output(true, &data) {
                    let _ = terminate_process_tree(&mut child, job.as_ref());
                    let _ = child.wait();
                    return Err(err);
                }
            }
            Ok(OutputMessage::Stderr(data)) => {
                if let Err(err) = on_output(false, &data) {
                    let _ = terminate_process_tree(&mut child, job.as_ref());
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
        exit_code: if timed_out || terminated {
            None
        } else {
            status.code()
        },
        timed_out,
        terminated,
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
    fn supports_direct_program_with_unicode_arguments() {
        let req = request(
            r#"{"program":"C:\\工具\\启动器.exe","args":["C:\\资源\\搜狗拼音.7zf","--静默"]}"#,
        );
        let (command, temporary) = prepare_command(&req).expect("command should be prepared");
        assert!(temporary.is_none());
        assert_eq!(command.get_program(), r"C:\工具\启动器.exe");
        assert_eq!(args(&command), [r"C:\资源\搜狗拼音.7zf", "--静默"]);
    }

    #[test]
    fn direct_program_rejects_shell_fields() {
        for json in [
            r#"{"command":"echo hi","program":"C:\\tool.exe"}"#,
            r#"{"program":"C:\\tool.exe","interpreter":"cmd"}"#,
            r#"{"args":["orphan"]}"#,
        ] {
            assert!(prepare_command(&request(json)).is_err());
        }
    }

    #[test]
    fn require_admin_defaults_to_false() {
        assert!(!request(r#"{"command":"echo hello"}"#).require_admin);
        assert!(request(r#"{"command":"echo hello","require_admin":true}"#).require_admin);
    }

    #[test]
    fn elevated_helper_runs_a_serialized_request_and_captures_output() {
        let request = request(r#"{"command":"echo elevated-helper-ok"}"#);
        let files = ElevatedFiles::new(&request).unwrap();

        let result = execute_elevated_request(&files.request, &files.cancel, &files.ready).unwrap();
        assert_eq!(result.result.unwrap().exit_code, Some(0));
        assert!(String::from_utf8_lossy(&result.stdout).contains("elevated-helper-ok"));
        assert!(
            std::fs::read_to_string(&files.ready)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
                > 0
        );
    }

    #[cfg(windows)]
    #[test]
    fn elevated_helper_cancels_its_own_job_tree() {
        let request = request(r#"{"command":"ping 127.0.0.1 -n 30","timeout":30000}"#);
        let mut files = ElevatedFiles::new(&request).unwrap();
        files.signal_cancel().unwrap();

        let result = execute_elevated_request(&files.request, &files.cancel, &files.ready).unwrap();
        assert!(result.result.unwrap().terminated);
    }

    #[test]
    fn elevated_output_is_bounded() {
        let mut output = vec![0; ELEVATED_OUTPUT_LIMIT - 2];
        append_elevated_output(&mut output, b"hello");
        assert_eq!(output.len(), ELEVATED_OUTPUT_LIMIT);
        assert_eq!(&output[ELEVATED_OUTPUT_LIMIT - 2..], b"he");
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
    fn detached_is_opt_in() {
        assert!(!request(r#"{"command":"echo hello"}"#).detached);
        assert!(request(r#"{"command":"echo hello","detached":true}"#).detached);
    }

    #[cfg(windows)]
    #[test]
    fn detached_only_requests_breakaway_when_the_outer_job_allows_it() {
        use windows_sys::Win32::System::{
            JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK},
            Threading::CREATE_BREAKAWAY_FROM_JOB,
        };

        assert_eq!(breakaway_flag_for_limits(0), 0);
        assert_eq!(
            breakaway_flag_for_limits(JOB_OBJECT_LIMIT_BREAKAWAY_OK),
            CREATE_BREAKAWAY_FROM_JOB
        );
        assert_eq!(
            breakaway_flag_for_limits(JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK),
            0
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
