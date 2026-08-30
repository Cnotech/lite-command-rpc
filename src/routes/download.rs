use crate::{config::RuntimePolicy, http::send_response, logger};
use serde::Deserialize;
use std::{fs::File, io::Write, net::TcpStream, path::Path};

#[derive(Debug, Deserialize)]
struct DownloadRequest {
    path: String,
}

fn send_file_response(stream: &mut TcpStream, file: &mut File, path: &Path) -> std::io::Result<()> {
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
    std::io::copy(file, stream)?;
    stream.flush()
}

pub fn handle(stream: &mut TcpStream, body: &[u8], policy: &RuntimePolicy) {
    let request: DownloadRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(err) => {
            let body = serde_json::json!({ "error": format!("invalid json: {err}") }).to_string();
            let _ = send_response(stream, "400 Bad Request", &body, "application/json");
            return;
        }
    };
    let path = match policy.resolve_download(Path::new(&request.path)) {
        Ok(path) => path,
        Err(err) => {
            let body = serde_json::json!({ "error": err }).to_string();
            let _ = send_response(stream, "403 Forbidden", &body, "application/json");
            return;
        }
    };
    let mut path = path;
    let file_path = path.path.clone();
    logger::info(format_args!("downloading: {}", request.path));
    if let Err(err) = send_file_response(stream, &mut path.file, &file_path) {
        logger::error(format_args!("download error: {err}"));
    }
}
