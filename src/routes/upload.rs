use crate::{
    config::RuntimePolicy,
    http::{send_json_error, send_response},
    logger,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static UPLOAD_ID: AtomicU64 = AtomicU64::new(1);

struct TemporaryUpload {
    path: PathBuf,
}

impl TemporaryUpload {
    fn new(destination: &Path) -> std::io::Result<(Self, File)> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let filename = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("upload");
        for _ in 0..100 {
            let id = UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".{filename}.upload-{}-{id}.tmp",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a temporary upload file",
        ))
    }

    fn commit(self, destination: &Path) -> std::io::Result<()> {
        // 部分 WinPE RAM 文件系统会错误地报告 hard_link 成功，但删除临时文件时
        // 目标也随之消失。始终使用 create_new + copy，保留 no-clobber 语义。
        self.copy_without_overwrite(destination)
    }

    fn copy_without_overwrite(&self, destination: &Path) -> std::io::Result<()> {
        let mut source = File::open(&self.path)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        let result = std::io::copy(&mut source, &mut target).and_then(|_| target.sync_all());
        if let Err(err) = result {
            drop(target);
            let _ = fs::remove_file(destination);
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for TemporaryUpload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn handle(
    stream: &mut TcpStream,
    prefetched: &[u8],
    content_length: usize,
    destination: Option<&str>,
    policy: &RuntimePolicy,
) {
    let Some(destination) = destination else {
        send_json_error(stream, "400 Bad Request", "X-File-Path header is required");
        return;
    };
    let path = match policy.resolve_upload(Path::new(destination)) {
        Ok(path) => path,
        Err(err) => {
            send_json_error(stream, "403 Forbidden", &err);
            return;
        }
    };
    let path = path.path.as_path();
    if path.file_name().is_none() {
        send_json_error(
            stream,
            "400 Bad Request",
            "destination must include a file name",
        );
        return;
    }
    if path.exists() {
        send_json_error(stream, "409 Conflict", "destination already exists");
        return;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        send_json_error(
            stream,
            "400 Bad Request",
            "destination directory does not exist",
        );
        return;
    }

    let (temporary, mut file) = match TemporaryUpload::new(path) {
        Ok(value) => value,
        Err(err) => {
            send_json_error(
                stream,
                "500 Internal Server Error",
                &format!("failed to create upload file: {err}"),
            );
            return;
        }
    };
    let initial_size = prefetched.len().min(content_length);
    if let Err(err) = file.write_all(&prefetched[..initial_size]) {
        send_json_error(
            stream,
            "500 Internal Server Error",
            &format!("failed to write upload: {err}"),
        );
        return;
    }

    let mut received = initial_size;
    let mut buffer = [0u8; 64 * 1024];
    while received < content_length {
        let remaining = content_length - received;
        let capacity = remaining.min(buffer.len());
        match stream.read(&mut buffer[..capacity]) {
            Ok(0) => {
                send_json_error(stream, "400 Bad Request", "incomplete upload body");
                return;
            }
            Ok(size) => {
                if let Err(err) = file.write_all(&buffer[..size]) {
                    send_json_error(
                        stream,
                        "500 Internal Server Error",
                        &format!("failed to write upload: {err}"),
                    );
                    return;
                }
                received += size;
            }
            Err(err) => {
                logger::error(format_args!("upload read error: {err}"));
                return;
            }
        }
    }
    if let Err(err) = file.sync_all() {
        send_json_error(
            stream,
            "500 Internal Server Error",
            &format!("failed to flush upload: {err}"),
        );
        return;
    }
    drop(file);
    if let Err(err) = temporary.commit(path) {
        let (status, message) = if err.kind() == std::io::ErrorKind::AlreadyExists {
            ("409 Conflict", "destination already exists".to_string())
        } else {
            (
                "500 Internal Server Error",
                format!("failed to commit upload: {err}"),
            )
        };
        send_json_error(stream, status, &message);
        return;
    }

    logger::info(format_args!(
        "uploaded: {} ({content_length} bytes)",
        path.display()
    ));
    let body = serde_json::json!({
        "ok": true,
        "path": path,
        "bytes": content_length,
    })
    .to_string();
    let _ = send_response(stream, "201 Created", &body, "application/json");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn commit_copies_without_overwriting_an_existing_destination() {
        let root = std::env::temp_dir().join(format!(
            "lcr-upload-test-{}-{}",
            std::process::id(),
            UPLOAD_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("test directory should be created");
        let destination = root.join("result.bin");

        let (temporary, mut file) =
            TemporaryUpload::new(&destination).expect("temporary upload should be created");
        file.write_all(b"first").expect("upload should be written");
        drop(file);
        temporary
            .commit(&destination)
            .expect("upload should be committed");
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        let (temporary, mut file) =
            TemporaryUpload::new(&destination).expect("second upload should be created");
        file.write_all(b"second").expect("upload should be written");
        drop(file);
        assert_eq!(
            temporary.commit(&destination).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&destination).unwrap(), b"first");

        fs::remove_dir_all(root).expect("test directory should be removed");
    }
}
