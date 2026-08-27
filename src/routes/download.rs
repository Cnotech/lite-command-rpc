use crate::http::send_response;
use serde::Deserialize;
use std::{fs::File, io::Write, net::TcpStream, path::Path};

#[derive(Debug, Deserialize)]
struct DownloadRequest {
    path: String,
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

pub fn handle(stream: &mut TcpStream, body: &[u8]) {
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
