mod http;
mod process;
mod routes;

use crate::http::{MAX_JSON_BODY_SIZE, read_body, send_json_error};
use std::{
    collections::HashMap,
    io::Read,
    net::{TcpListener, TcpStream},
    thread,
};

struct RequestHead {
    method: String,
    path: String,
    content_length: usize,
    headers: HashMap<String, String>,
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_request_head(data: &[u8]) -> Result<RequestHead, String> {
    let headers = std::str::from_utf8(data).map_err(|_| "invalid http headers")?;
    let mut lines = headers.lines();
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut values = HashMap::new();

    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            values.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = values
        .get("content-length")
        .ok_or("Content-Length is required")?
        .parse::<usize>()
        .map_err(|_| "invalid Content-Length")?;
    Ok(RequestHead {
        method,
        path,
        content_length,
        headers: values,
    })
}

fn handle_json_route(stream: &mut TcpStream, path: &str, prefetched: &[u8], content_length: usize) {
    if content_length > MAX_JSON_BODY_SIZE {
        send_json_error(stream, "413 Payload Too Large", "request body too large");
        return;
    }
    let body = match read_body(stream, prefetched, content_length) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
            send_json_error(stream, "400 Bad Request", "incomplete request body");
            return;
        }
        Err(err) => {
            eprintln!("read body error: {err}");
            return;
        }
    };

    match path {
        "/exec" => routes::exec::handle(stream, &body),
        "/exec/stream" => routes::exec::handle_stream(stream, &body),
        "/download" => routes::download::handle(stream, &body),
        _ => send_json_error(stream, "404 Not Found", "not found"),
    }
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
                    send_json_error(
                        &mut stream,
                        "431 Request Header Fields Too Large",
                        "request headers too large",
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

    let head = match parse_request_head(&buffer[..header_end]) {
        Ok(head) => head,
        Err(error) => {
            let status = if error == "Content-Length is required" {
                "411 Length Required"
            } else {
                "400 Bad Request"
            };
            send_json_error(&mut stream, status, &error);
            return;
        }
    };
    if head.method != "POST" {
        send_json_error(
            &mut stream,
            "405 Method Not Allowed",
            "only POST is supported",
        );
        return;
    }

    let body_start = header_end + 4;
    let prefetched = &buffer[body_start..];
    if head.path == "/upload" {
        routes::upload::handle(
            &mut stream,
            prefetched,
            head.content_length,
            head.headers.get("x-file-path").map(String::as_str),
        );
    } else {
        handle_json_route(&mut stream, &head.path, prefetched, head.content_length);
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
    println!("POST /upload");
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
