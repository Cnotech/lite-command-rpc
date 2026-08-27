use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Deserialize)]
struct ExecRequest {
    command: String,
    cwd: Option<String>,
    timeout: Option<u64>,
}

impl ExecRequest {
    fn timeout_ms(&self) -> u64 {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT_MS)
    }
}

#[derive(Debug, Deserialize)]
struct DownloadRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct ExecResponse {
    ok: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    error: Option<String>,
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

struct RunResult {
    exit_code: Option<i32>,
    timed_out: bool,
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

    fn terminate(&self) -> std::io::Result<()> {
        Ok(())
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
                                    // UTF-8 字符可能刚好跨越两次 pipe read，留到下一批再解码。
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
    if let Some(job) = job {
        if job.terminate().is_ok() {
            return;
        }
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

fn run_command<F>(req: &ExecRequest, mut on_output: F) -> std::io::Result<RunResult>
where
    F: FnMut(bool, &[u8]) -> std::io::Result<()>,
{
    let mut command = Command::new("cmd.exe");
    let command_line = format!("chcp 65001 >nul & {}", req.command);
    command
        .args(["/d", "/s", "/c", &command_line])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let job = match ProcessJob::assign(&child) {
        Ok(job) => Some(job),
        Err(err) => {
            eprintln!(
                "failed to assign process {} to Job Object: {err}",
                child.id()
            );
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

fn send_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    content_type: &str,
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn start_chunked_response(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: application/x-ndjson; charset=utf-8\r\n\
          Transfer-Encoding: chunked\r\n\
          Cache-Control: no-cache\r\n\
          Connection: close\r\n\
          \r\n",
    )?;
    stream.flush()
}

fn send_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:X}\r\n", data.len())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn send_stream_event(stream: &mut TcpStream, event: serde_json::Value) -> std::io::Result<()> {
    let mut data = serde_json::to_vec(&event)?;
    data.push(b'\n');
    send_chunk(stream, &data)
}

fn finish_chunked_response(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

fn send_file_response(stream: &mut TcpStream, path: &Path) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download.bin")
        .replace('"', "_");
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {file_size}\r\n\
         Content-Disposition: attachment; filename=\"{filename}\"\r\n\
         Connection: close\r\n\
         \r\n"
    );
    stream.write_all(headers.as_bytes())?;
    std::io::copy(&mut file, stream)?;
    stream.flush()
}

fn parse_exec_request(stream: &mut TcpStream, body: &[u8]) -> Option<ExecRequest> {
    match serde_json::from_slice(body) {
        Ok(req) => Some(req),
        Err(err) => {
            let body = serde_json::json!({ "error": format!("invalid json: {err}") }).to_string();
            let _ = send_response(stream, "400 Bad Request", &body, "application/json");
            None
        }
    }
}

fn handle_exec(stream: &mut TcpStream, body: &[u8]) {
    let Some(request) = parse_exec_request(stream, body) else {
        return;
    };
    println!("executing: {}", request.command);
    let timeout_ms = request.timeout_ms();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let run_result = run_command(&request, |is_stdout, data| {
        if is_stdout {
            stdout.extend_from_slice(data);
        } else {
            stderr.extend_from_slice(data);
        }
        Ok(())
    });

    let result = match run_result {
        Ok(result) => ExecResponse {
            ok: !result.timed_out && result.exit_code == Some(0),
            exit_code: result.exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out: result.timed_out,
            error: result
                .timed_out
                .then(|| format!("command timed out after {timeout_ms} ms")),
        },
        Err(err) => ExecResponse {
            ok: false,
            exit_code: None,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out: false,
            error: Some(err.to_string()),
        },
    };

    match serde_json::to_string(&result) {
        Ok(body) => {
            let _ = send_response(stream, "200 OK", &body, "application/json");
        }
        Err(err) => {
            let body = serde_json::json!({ "error": err.to_string() }).to_string();
            let _ = send_response(
                stream,
                "500 Internal Server Error",
                &body,
                "application/json",
            );
        }
    }
}

fn handle_exec_stream(stream: &mut TcpStream, body: &[u8]) {
    let Some(request) = parse_exec_request(stream, body) else {
        return;
    };
    println!("stream executing: {}", request.command);
    let timeout_ms = request.timeout_ms();
    if start_chunked_response(stream).is_err() {
        return;
    }

    let result = run_command(&request, |is_stdout, data| {
        send_stream_event(
            stream,
            serde_json::json!({
                "type": if is_stdout { "stdout" } else { "stderr" },
                "data": String::from_utf8_lossy(data),
            }),
        )
    });
    match result {
        Ok(result) if result.timed_out => {
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "timeout", "timeout": timeout_ms }),
            );
        }
        Ok(result) => {
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "exit", "exit_code": result.exit_code }),
            );
        }
        Err(err) => {
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "error", "error": err.to_string() }),
            );
        }
    }
    let _ = finish_chunked_response(stream);
}

fn handle_download(stream: &mut TcpStream, body: &[u8]) {
    let request: DownloadRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(err) => {
            let body = serde_json::json!({ "error": format!("invalid json: {err}") }).to_string();
            let _ = send_response(stream, "400 Bad Request", &body, "application/json");
            return;
        }
    };
    let path = Path::new(&request.path);
    if !path.exists() {
        let body =
            serde_json::json!({ "error": "file not found", "path": request.path }).to_string();
        let _ = send_response(stream, "404 Not Found", &body, "application/json");
        return;
    }
    if !path.is_file() {
        let body =
            serde_json::json!({ "error": "path is not a file", "path": request.path }).to_string();
        let _ = send_response(stream, "400 Bad Request", &body, "application/json");
        return;
    }
    println!("downloading: {}", request.path);
    if let Err(err) = send_file_response(stream, path) {
        eprintln!("download error: {err}");
    }
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().ok();
    println!("client connected: {:?}", peer);
    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut temp) {
            Ok(0) => return,
            Ok(size) => {
                buffer.extend_from_slice(&temp[..size]);
                if let Some(position) = find_header_end(&buffer) {
                    break position;
                }
                if buffer.len() > 64 * 1024 {
                    let _ = send_response(
                        &mut stream,
                        "431 Request Header Fields Too Large",
                        r#"{"error":"request headers too large"}"#,
                        "application/json",
                    );
                    return;
                }
            }
            Err(err) => {
                eprintln!("read error: {err}");
                return;
            }
        }
    };

    let (method, path, content_length) = {
        let headers = match std::str::from_utf8(&buffer[..header_end]) {
            Ok(headers) => headers,
            Err(_) => {
                let _ = send_response(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"invalid http headers"}"#,
                    "application/json",
                );
                return;
            }
        };
        let mut lines = headers.lines();
        let Some(request_line) = lines.next() else {
            let _ = send_response(
                &mut stream,
                "400 Bad Request",
                r#"{"error":"missing request line"}"#,
                "application/json",
            );
            return;
        };
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut content_length = None;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("Content-Length") {
                    match value.trim().parse::<usize>() {
                        Ok(value) => content_length = Some(value),
                        Err(_) => {
                            let _ = send_response(
                                &mut stream,
                                "400 Bad Request",
                                r#"{"error":"invalid Content-Length"}"#,
                                "application/json",
                            );
                            return;
                        }
                    }
                }
            }
        }
        let Some(content_length) = content_length else {
            let _ = send_response(
                &mut stream,
                "411 Length Required",
                r#"{"error":"Content-Length is required"}"#,
                "application/json",
            );
            return;
        };
        (method, path, content_length)
    };

    if method != "POST" {
        let _ = send_response(
            &mut stream,
            "405 Method Not Allowed",
            r#"{"error":"only POST is supported"}"#,
            "application/json",
        );
        return;
    }
    if content_length > 1024 * 1024 {
        let _ = send_response(
            &mut stream,
            "413 Payload Too Large",
            r#"{"error":"request body too large"}"#,
            "application/json",
        );
        return;
    }
    let body_start = header_end + 4;
    let Some(expected_size) = body_start.checked_add(content_length) else {
        let _ = send_response(
            &mut stream,
            "400 Bad Request",
            r#"{"error":"invalid request size"}"#,
            "application/json",
        );
        return;
    };
    while buffer.len() < expected_size {
        match stream.read(&mut temp) {
            Ok(0) => {
                let _ = send_response(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"incomplete request body"}"#,
                    "application/json",
                );
                return;
            }
            Ok(size) => buffer.extend_from_slice(&temp[..size]),
            Err(err) => {
                eprintln!("read body error: {err}");
                return;
            }
        }
    }

    let body = &buffer[body_start..expected_size];
    match path.as_str() {
        "/exec" => handle_exec(&mut stream, body),
        "/exec/stream" => handle_exec_stream(&mut stream, body),
        "/download" => handle_download(&mut stream, body),
        _ => {
            let _ = send_response(
                &mut stream,
                "404 Not Found",
                r#"{"error":"not found"}"#,
                "application/json",
            );
        }
    }
    println!("client disconnected: {:?}", peer);
}

fn main() -> std::io::Result<()> {
    let addr = "0.0.0.0:9527";
    let listener = TcpListener::bind(addr)?;
    println!("lite-command-rpc listening on http://{addr}");
    println!("POST /exec");
    println!("POST /exec/stream");
    println!("POST /download");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || handle_client(stream));
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}
