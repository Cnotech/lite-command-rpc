mod config;
#[cfg(windows)]
mod config_watch;
mod console;
mod encoding;
mod http;
mod logger;
mod process;
mod routes;

use crate::http::{MAX_JSON_BODY_SIZE, read_body, send_json_error};
use clap::{Parser, Subcommand};
use config::RuntimePolicy;
use std::{
    collections::HashMap,
    io::Read,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
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
///   /spawn/terminate Terminate an asynchronous command and return its result
///   /screenshot   Capture the primary screen as PNG
///   /windows      List top-level windows on the current desktop
///   /control      Focus a window or simulate keyboard and mouse input
///   /upload       Upload raw bytes using the X-File-Path header
///   /download     Download the file named by the JSON `path` field
///
/// Execution JSON fields:
///   command       Script or command text; mutually exclusive with program
///   program       Executable path for direct Unicode-safe execution
///   args          Arguments for direct program execution
///   cwd           Optional working directory
///   timeout       Timeout in milliseconds; default: 300000
///   interpreter   cmd, pwsh, or an absolute path; default: cmd
///   script_mode   auto, inline, or file; default: auto
///   detached      Let child processes survive after the wrapper exits; default: false
///   require_admin Request UAC elevation through an LCR helper; default: false
///   output_encoding  utf8, oem, or ansi; default: utf8
///
/// In auto mode, multiline commands are executed through temporary script files.
/// Only expose this unauthenticated service on a trusted or protected network.
#[derive(Debug, Parser)]
#[command(name = "lcr", version, verbatim_doc_comment)]
struct Cli {
    /// TOML config path; otherwise searches the current directory, then next to lcr.exe
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

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

    #[arg(long, global = true, hide = true)]
    config_watch_worker: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, PartialEq, Subcommand)]
enum CliCommand {
    /// Start the HTTP service
    Serve,
    #[command(hide = true)]
    ElevatedExec {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        result: PathBuf,
        #[arg(long)]
        cancel: PathBuf,
        #[arg(long)]
        ready: PathBuf,
    },
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
            let name = name.trim().to_ascii_lowercase();
            if matches!(name.as_str(), "content-length" | "transfer-encoding")
                && values.contains_key(&name)
            {
                return Err(format!("duplicate {name} header"));
            }
            values.insert(name, value.trim().to_string());
        }
    }
    if values.contains_key("transfer-encoding") && values.contains_key("content-length") {
        return Err("Transfer-Encoding and Content-Length must not be combined".to_string());
    }
    if values.contains_key("transfer-encoding") {
        return Err("unsupported Transfer-Encoding".to_string());
    }
    let content_length = values
        .get("content-length")
        .map(|value| value.parse::<usize>().map_err(|_| "invalid Content-Length"))
        .transpose()?
        .unwrap_or(0);
    Ok(RequestHead {
        method,
        path,
        content_length,
        headers: values,
    })
}

fn handle_json_route(
    stream: &mut TcpStream,
    path: &str,
    prefetched: &[u8],
    content_length: usize,
    policy: &RuntimePolicy,
) {
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
        "/exec" => routes::exec::handle(stream, &body, policy),
        "/exec/stream" => routes::exec::handle_stream(stream, &body, policy),
        "/spawn" => routes::spawn::handle_spawn(stream, &body, policy),
        "/spawn/result" => routes::spawn::handle_result(stream, &body),
        "/spawn/terminate" => routes::spawn::handle_terminate(stream, &body),
        #[cfg(windows)]
        "/screenshot" => routes::screenshot::handle(stream),
        #[cfg(windows)]
        "/windows" => routes::windows::handle(stream),
        #[cfg(windows)]
        "/control" => routes::control::handle(stream, &body),
        "/download" => routes::download::handle(stream, &body, policy),
        _ => send_json_error(stream, "404 Not Found", "not found"),
    }
}

fn handle_client(mut stream: TcpStream, policy: &RuntimePolicy) {
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
            send_json_error(&mut stream, "400 Bad Request", &error);
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
            policy,
        );
    } else {
        handle_json_route(
            &mut stream,
            &head.path,
            prefetched,
            head.content_length,
            policy,
        );
    }
    logger::debug(format_args!("client disconnected: {peer:?}"));
}

fn run_server(addr: SocketAddr, policy: Arc<RuntimePolicy>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    logger::info(format_args!("lcr listening on http://{addr}"));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let policy = Arc::clone(&policy);
                thread::spawn(move || handle_client(stream, &policy));
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
    if let Some(CliCommand::ElevatedExec {
        request,
        result,
        cancel,
        ready,
    }) = &cli.command
    {
        return match process::run_elevated_helper(request, result, cancel, ready) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                logger::error(format_args!("elevated helper failed: {err}"));
                ExitCode::FAILURE
            }
        };
    }
    let (policy, config_path) = match RuntimePolicy::load(cli.config.as_deref()) {
        Ok(value) => value,
        Err(err) => {
            logger::error(format_args!("{err}"));
            return ExitCode::FAILURE;
        }
    };
    if let Some(path) = config_path.as_deref() {
        logger::info(format_args!("loaded config: {}", path.display()));
    }
    #[cfg(windows)]
    if let Some(path) = config_path.as_deref()
        && !cli.config_watch_worker
    {
        return config_watch::supervise(
            path,
            config_watch::WorkerOptions {
                listen: cli.listen,
                log_level: cli.log_level,
            },
        );
    }
    let policy = Arc::new(policy);
    match cli.command {
        None | Some(CliCommand::Serve) => match run_server(cli.listen, policy) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                logger::error(format_args!("failed to start lcr: {err}"));
                ExitCode::FAILURE
            }
        },
        Some(CliCommand::ElevatedExec { .. }) => unreachable!("elevated helper exits above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, error::ErrorKind};

    #[test]
    fn missing_content_length_means_an_empty_request_body() {
        let head = parse_request_head(b"POST /windows HTTP/1.1\r\nHost: localhost\r\n")
            .expect("empty requests should not require Content-Length");
        assert_eq!(head.content_length, 0);
    }

    #[test]
    fn unsupported_or_ambiguous_transfer_encoding_is_rejected() {
        assert_eq!(
            parse_request_head(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: identity\r\n"
            )
            .err()
            .expect("transfer encodings should be rejected"),
            "unsupported Transfer-Encoding"
        );
        assert_eq!(
            parse_request_head(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: identity\r\nContent-Length: 4\r\n"
            )
            .err()
            .expect("ambiguous request framing should be rejected"),
            "Transfer-Encoding and Content-Length must not be combined"
        );
        assert_eq!(
            parse_request_head(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: identity\r\n"
            )
            .err()
            .expect("duplicate transfer encoding should be rejected"),
            "duplicate transfer-encoding header"
        );
        assert_eq!(
            parse_request_head(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nContent-Length: 4\r\n"
            )
            .err()
            .expect("duplicate content length should be rejected"),
            "duplicate content-length header"
        );
    }

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
    fn config_path_is_configurable_globally() {
        for arguments in [
            vec!["lcr", "--config", "D:\\lcr.toml"],
            vec!["lcr", "serve", "--config", "D:\\lcr.toml"],
        ] {
            let cli = Cli::try_parse_from(arguments).expect("config path should be valid");
            assert_eq!(cli.config, Some(PathBuf::from("D:\\lcr.toml")));
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
            "/spawn/terminate",
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
