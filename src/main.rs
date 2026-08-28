mod console;
mod http;
mod logger;
mod process;
mod routes;

use crate::http::{MAX_JSON_BODY_SIZE, read_body, send_json_error};
use clap::{Parser, Subcommand};
use std::{
    collections::HashMap,
    io::Read,
    net::{SocketAddr, TcpListener, TcpStream},
    process::ExitCode,
    thread,
};

/// Lightweight Windows HTTP service for command execution and file transfer.
///
/// Running without a command starts the server on http://0.0.0.0:9527.
/// All endpoints use POST and the service has no authentication.
///
/// HTTP endpoints:
///   /exec         Execute a command and return one JSON response
///   /exec/stream  Execute a command and return NDJSON events
///   /spawn        Start a command asynchronously and return its session ID and PID
///   /spawn/result Query an asynchronous command and its captured output
///   /screenshot   Capture the primary screen as PNG
///   /windows      List top-level windows on the current desktop
///   /control      Focus a window or simulate keyboard and mouse input
///   /upload       Upload raw bytes using the X-File-Path header
///   /download     Download the file named by the JSON `path` field
///
/// Execution JSON fields:
///   command       Required script or command text
///   cwd           Optional working directory
///   timeout       Timeout in milliseconds; default: 300000
///   interpreter   cmd, pwsh, or an absolute path; default: cmd
///   script_mode   auto, inline, or file; default: auto
///
/// In auto mode, multiline commands are executed through temporary script files.
/// Only expose this unauthenticated service on a trusted or protected network.
#[derive(Debug, Parser)]
#[command(name = "lcr", version, verbatim_doc_comment)]
struct Cli {
    /// Address and port on which the HTTP service listens
    #[arg(
        long,
        global = true,
        value_name = "IP:PORT",
        default_value = "0.0.0.0:9527"
    )]
    listen: SocketAddr,

    /// Minimum level printed by the logger
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = logger::Level::Info
    )]
    log_level: logger::Level,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, PartialEq, Subcommand)]
enum CliCommand {
    /// Start the HTTP service
    Serve,
}

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
            logger::error(format_args!("read body error: {err}"));
            return;
        }
    };

    match path {
        "/exec" => routes::exec::handle(stream, &body),
        "/exec/stream" => routes::exec::handle_stream(stream, &body),
        "/spawn" => routes::spawn::handle_spawn(stream, &body),
        "/spawn/result" => routes::spawn::handle_result(stream, &body),
        #[cfg(windows)]
        "/screenshot" => routes::screenshot::handle(stream),
        #[cfg(windows)]
        "/windows" => routes::windows::handle(stream),
        #[cfg(windows)]
        "/control" => routes::control::handle(stream, &body),
        "/download" => routes::download::handle(stream, &body),
        _ => send_json_error(stream, "404 Not Found", "not found"),
    }
}

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().ok();
    logger::debug(format_args!("client connected: {peer:?}"));
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
                logger::error(format_args!("read error: {err}"));
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
    logger::debug(format_args!("client disconnected: {peer:?}"));
}

fn run_server(addr: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    logger::info(format_args!("lcr listening on http://{addr}"));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || handle_client(stream));
            }
            Err(err) => logger::error(format_args!("accept error: {err}")),
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    if let Err(err) = console::set_title("lcr") {
        logger::warn(format_args!("failed to set console title: {err}"));
    }
    let cli = Cli::parse();
    logger::set_level(cli.log_level);
    match cli.command {
        None | Some(CliCommand::Serve) => match run_server(cli.listen) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                logger::error(format_args!("failed to start lcr: {err}"));
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, error::ErrorKind};

    #[test]
    fn no_arguments_starts_server() {
        let cli = Cli::try_parse_from(["lcr"]).expect("arguments should be valid");
        assert_eq!(cli.command, None);
        assert_eq!(cli.listen, "0.0.0.0:9527".parse().unwrap());
        assert_eq!(cli.log_level, logger::Level::Info);
    }

    #[test]
    fn help_variants_show_help() {
        for value in ["--help", "-h", "help"] {
            let error = Cli::try_parse_from(["lcr", value]).expect_err("help should exit early");
            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        }
    }

    #[test]
    fn serve_command_is_supported() {
        let cli = Cli::try_parse_from(["lcr", "serve"]).expect("serve command should be valid");
        assert_eq!(cli.command, Some(CliCommand::Serve));
    }

    #[test]
    fn listen_address_is_configurable_globally() {
        for arguments in [
            vec!["lcr", "--listen", "127.0.0.1:8080"],
            vec!["lcr", "serve", "--listen", "127.0.0.1:8080"],
        ] {
            let cli = Cli::try_parse_from(arguments).expect("listen address should be valid");
            assert_eq!(cli.listen, "127.0.0.1:8080".parse().unwrap());
        }
    }

    #[test]
    fn invalid_listen_address_is_rejected() {
        assert!(Cli::try_parse_from(["lcr", "--listen", "localhost"]).is_err());
    }

    #[test]
    fn log_level_is_configurable_globally() {
        for arguments in [
            vec!["lcr", "--log-level", "debug"],
            vec!["lcr", "serve", "--log-level", "debug"],
        ] {
            let cli = Cli::try_parse_from(arguments).expect("log level should be valid");
            assert_eq!(cli.log_level, logger::Level::Debug);
        }
    }

    #[test]
    fn help_describes_the_http_api() {
        let help = Cli::command().render_long_help().to_string();
        for expected in [
            "http://0.0.0.0:9527",
            "/exec/stream",
            "/spawn/result",
            "/screenshot",
            "/windows",
            "/control",
            "/upload",
            "/download",
            "interpreter",
            "script_mode",
            "no authentication",
        ] {
            assert!(help.contains(expected), "help should contain {expected}");
        }
    }

    #[test]
    fn unsupported_arguments_are_rejected() {
        let unknown = Cli::try_parse_from(["lcr", "--unknown"]).expect_err("argument is unknown");
        assert_eq!(unknown.kind(), ErrorKind::UnknownArgument);

        assert!(Cli::try_parse_from(["lcr", "help", "extra"]).is_err());
    }
}
