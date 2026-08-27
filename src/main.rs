use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    thread,
};

#[derive(Debug, Deserialize)]
struct ExecRequest {
    command: String,
    cwd: Option<String>,
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
    error: Option<String>,
}

fn execute(req: ExecRequest) -> ExecResponse {
    let mut command = Command::new("cmd.exe");

    command.args(["/d", "/s", "/c", &req.command]);

    if let Some(cwd) = &req.cwd {
        command.current_dir(cwd);
    }

    match command.output() {
        Ok(output) => ExecResponse {
            ok: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            error: None,
        },

        Err(err) => ExecResponse {
            ok: false,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(err.to_string()),
        },
    }
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

fn send_file_response(
    stream: &mut TcpStream,
    path: &Path,
) -> std::io::Result<()> {
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

    // 流式传输，不会把整个文件加载到内存
    std::io::copy(&mut file, stream)?;

    stream.flush()
}

fn handle_exec(stream: &mut TcpStream, body: &[u8]) {
    let request: ExecRequest = match serde_json::from_slice(body) {
        Ok(req) => req,

        Err(err) => {
            let body = serde_json::json!({
                "error": format!("invalid json: {err}")
            })
            .to_string();

            let _ = send_response(
                stream,
                "400 Bad Request",
                &body,
                "application/json",
            );

            return;
        }
    };

    println!("executing: {}", request.command);

    let result = execute(request);

    let body = match serde_json::to_string(&result) {
        Ok(body) => body,

        Err(err) => {
            eprintln!("serialize error: {err}");

            let body = serde_json::json!({
                "error": err.to_string()
            })
            .to_string();

            let _ = send_response(
                stream,
                "500 Internal Server Error",
                &body,
                "application/json",
            );

            return;
        }
    };

    let _ = send_response(
        stream,
        "200 OK",
        &body,
        "application/json",
    );
}

fn handle_download(stream: &mut TcpStream, body: &[u8]) {
    let request: DownloadRequest = match serde_json::from_slice(body) {
        Ok(req) => req,

        Err(err) => {
            let body = serde_json::json!({
                "error": format!("invalid json: {err}")
            })
            .to_string();

            let _ = send_response(
                stream,
                "400 Bad Request",
                &body,
                "application/json",
            );

            return;
        }
    };

    let path = Path::new(&request.path);

    if !path.exists() {
        let body = serde_json::json!({
            "error": "file not found"
        })
        .to_string();

        let _ = send_response(
            stream,
            "404 Not Found",
            &body,
            "application/json",
        );

        return;
    }

    if !path.is_file() {
        let body = serde_json::json!({
            "error": "path is not a file"
        })
        .to_string();

        let _ = send_response(
            stream,
            "400 Bad Request",
            &body,
            "application/json",
        );

        return;
    }

    println!("downloading: {}", request.path);

    if let Err(err) = send_file_response(stream, path) {
        eprintln!("download error: {err}");
    }
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|window| window == b"\r\n\r\n")
}

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().ok();

    println!("client connected: {:?}", peer);

    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];

    let header_end = loop {
        match stream.read(&mut temp) {
            Ok(0) => {
                println!("client disconnected before sending request");
                return;
            }

            Ok(n) => {
                buffer.extend_from_slice(&temp[..n]);

                if let Some(pos) = find_header_end(&buffer) {
                    break pos;
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

    // 在这个作用域内解析 header。
    // 最终只保留 owned String / usize，避免继续借用 buffer。
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

        let request_line = match lines.next() {
            Some(line) => line,

            None => {
                let _ = send_response(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"missing request line"}"#,
                    "application/json",
                );

                return;
            }
        };

        let mut parts = request_line.split_whitespace();

        let method = parts
            .next()
            .unwrap_or("")
            .to_string();

        let path = parts
            .next()
            .unwrap_or("")
            .to_string();

        let mut content_length: Option<usize> = None;

        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("Content-Length") {
                    match value.trim().parse::<usize>() {
                        Ok(value) => {
                            content_length = Some(value);
                        }

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

        let content_length = match content_length {
            Some(value) => value,

            None => {
                let _ = send_response(
                    &mut stream,
                    "411 Length Required",
                    r#"{"error":"Content-Length is required"}"#,
                    "application/json",
                );

                return;
            }
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

    // 两个接口的请求体都只是小 JSON。
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

    let expected_size = match body_start.checked_add(content_length) {
        Some(size) => size,

        None => {
            let _ = send_response(
                &mut stream,
                "400 Bad Request",
                r#"{"error":"invalid request size"}"#,
                "application/json",
            );

            return;
        }
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

            Ok(n) => {
                buffer.extend_from_slice(&temp[..n]);
            }

            Err(err) => {
                eprintln!("read body error: {err}");
                return;
            }
        }
    }

    let body = &buffer[body_start..expected_size];

    match path.as_str() {
        "/exec" => {
            handle_exec(&mut stream, body);
        }

        "/download" => {
            handle_download(&mut stream, body);
        }

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
    println!("POST /download");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    handle_client(stream);
                });
            }

            Err(err) => {
                eprintln!("accept error: {err}");
            }
        }
    }

    Ok(())
}