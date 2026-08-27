use std::{
    io::{Read, Write},
    net::TcpStream,
};

pub const MAX_JSON_BODY_SIZE: usize = 1024 * 1024;

pub fn send_response(
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

pub fn send_json_error(stream: &mut TcpStream, status: &str, error: &str) {
    let body = serde_json::json!({ "error": error }).to_string();
    let _ = send_response(stream, status, &body, "application/json");
}

pub fn read_body(
    stream: &mut TcpStream,
    prefetched: &[u8],
    content_length: usize,
) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(content_length);
    let initial_size = prefetched.len().min(content_length);
    body.extend_from_slice(&prefetched[..initial_size]);
    let mut buffer = [0u8; 8192];
    while body.len() < content_length {
        let remaining = content_length - body.len();
        let capacity = remaining.min(buffer.len());
        let size = stream.read(&mut buffer[..capacity])?;
        if size == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete request body",
            ));
        }
        body.extend_from_slice(&buffer[..size]);
    }
    Ok(body)
}

pub fn start_chunked_response(stream: &mut TcpStream) -> std::io::Result<()> {
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

pub fn send_stream_event(stream: &mut TcpStream, event: serde_json::Value) -> std::io::Result<()> {
    let mut data = serde_json::to_vec(&event)?;
    data.push(b'\n');
    send_chunk(stream, &data)
}

pub fn finish_chunked_response(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}
