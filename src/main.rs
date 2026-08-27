use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
};

#[derive(Debug, Deserialize)]
struct ExecRequest {
    command: String,
    cwd: Option<String>,
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
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.as_bytes().len()
    );

    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().ok();

    println!("client connected: {:?}", peer);

    let mut buffer = Vec::new();
    let mut temp = [0u8; 4096];

    let header_end;

    loop {
        match stream.read(&mut temp) {
            Ok(0) => {
                return;
            }

            Ok(n) => {
                buffer.extend_from_slice(&temp[..n]);

                if let Some(pos) = find_header_end(&buffer) {
                    header_end = pos;
                    break;
                }
            }

            Err(err) => {
                eprintln!("read error: {err}");
                return;
            }
        }
    }

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
        None => return,
    };

    let mut parts = request_line.split_whitespace();

    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if method != "POST" {
        let _ = send_response(
            &mut stream,
            "405 Method Not Allowed",
            r#"{"error":"only POST is supported"}"#,
            "application/json",
        );
        return;
    }

    if path != "/exec" {
        let _ = send_response(
            &mut stream,
            "404 Not Found",
            r#"{"error":"not found"}"#,
            "application/json",
        );
        return;
    }

    let mut content_length = 0usize;

    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let body_start = header_end + 4;

    while buffer.len() < body_start + content_length {
        match stream.read(&mut temp) {
            Ok(0) => break,

            Ok(n) => {
                buffer.extend_from_slice(&temp[..n]);
            }

            Err(err) => {
                eprintln!("read body error: {err}");
                return;
            }
        }
    }

    if buffer.len() < body_start + content_length {
        let _ = send_response(
            &mut stream,
            "400 Bad Request",
            r#"{"error":"incomplete request body"}"#,
            "application/json",
        );
        return;
    }

    let body = &buffer[body_start..body_start + content_length];

    let request: ExecRequest = match serde_json::from_slice(body) {
        Ok(req) => req,

        Err(err) => {
            let body = serde_json::json!({
                "error": format!("invalid json: {err}")
            })
            .to_string();

            let _ = send_response(
                &mut stream,
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
            return;
        }
    };

    let _ = send_response(
        &mut stream,
        "200 OK",
        &body,
        "application/json",
    );

    println!("client disconnected: {:?}", peer);
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|window| window == b"\r\n\r\n")
}

fn main() -> std::io::Result<()> {
    let addr = "0.0.0.0:9527";

    let listener = TcpListener::bind(addr)?;

    println!("pe-agent listening on http://{addr}");
    println!("POST /exec");

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