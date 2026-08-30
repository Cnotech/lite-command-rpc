use crate::{
    config::RuntimePolicy,
    encoding::{TextDecoder, decode_all},
    http::{finish_chunked_response, send_response, send_stream_event, start_chunked_response},
    logger,
    process::{ExecRequest, run_command},
};
use serde::Serialize;
use std::net::TcpStream;

#[derive(Debug, Serialize)]
struct ExecResponse {
    ok: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    error: Option<String>,
}
fn parse_request(
    stream: &mut TcpStream,
    body: &[u8],
    policy: &RuntimePolicy,
) -> Option<ExecRequest> {
    match serde_json::from_slice(body) {
        Ok(mut req) => match policy.prepare_exec(&mut req) {
            Ok(()) => Some(req),
            Err(err) => {
                let body = serde_json::json!({ "msg": err }).to_string();
                let _ = send_response(stream, "403 Forbidden", &body, "application/json");
                None
            }
        },
        Err(err) => {
            let body = serde_json::json!({ "error": format!("invalid json: {err}") }).to_string();
            let _ = send_response(stream, "400 Bad Request", &body, "application/json");
            None
        }
    }
}

pub fn handle(stream: &mut TcpStream, body: &[u8], policy: &RuntimePolicy) {
    let Some(request) = parse_request(stream, body, policy) else {
        return;
    };
    logger::info(format_args!(
        "executing: {}",
        request
            .command
            .as_deref()
            .or(request.program.as_deref())
            .unwrap_or("<invalid request>")
    ));
    let timeout_ms = request.timeout_ms();
    let output_encoding = request.output_encoding();
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
        Ok(result) => {
            logger::info(format_args!(
                "execution finished: exit_code={:?}, timed_out={}",
                result.exit_code, result.timed_out
            ));
            ExecResponse {
                ok: !result.timed_out && result.exit_code == Some(0),
                exit_code: result.exit_code,
                stdout: decode_all(&stdout, output_encoding),
                stderr: decode_all(&stderr, output_encoding),
                timed_out: result.timed_out,
                error: result
                    .timed_out
                    .then(|| format!("command timed out after {timeout_ms} ms")),
            }
        }
        Err(err) => {
            logger::info(format_args!("execution finished: error={err}"));
            ExecResponse {
                ok: false,
                exit_code: None,
                stdout: decode_all(&stdout, output_encoding),
                stderr: decode_all(&stderr, output_encoding),
                timed_out: false,
                error: Some(err.to_string()),
            }
        }
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

pub fn handle_stream(stream: &mut TcpStream, body: &[u8], policy: &RuntimePolicy) {
    let Some(request) = parse_request(stream, body, policy) else {
        return;
    };
    logger::info(format_args!(
        "stream executing: {}",
        request
            .command
            .as_deref()
            .or(request.program.as_deref())
            .unwrap_or("<invalid request>")
    ));
    let timeout_ms = request.timeout_ms();
    let mut stdout_decoder = TextDecoder::new(request.output_encoding());
    let mut stderr_decoder = TextDecoder::new(request.output_encoding());
    if let Err(err) = start_chunked_response(stream) {
        logger::info(format_args!(
            "stream finished: error=failed to start response: {err}"
        ));
        return;
    }
    let result = run_command(&request, |is_stdout, data| {
        let text = if is_stdout {
            stdout_decoder.push(data)
        } else {
            stderr_decoder.push(data)
        };
        if text.is_empty() {
            return Ok(());
        }
        logger::info(format_args!(
            "stream {}: {}",
            if is_stdout { "stdout" } else { "stderr" },
            text
        ));
        send_stream_event(
            stream,
            serde_json::json!({
                "type": if is_stdout { "stdout" } else { "stderr" },
                "data": text,
            }),
        )
    });
    for (is_stdout, data) in [
        (true, stdout_decoder.finish()),
        (false, stderr_decoder.finish()),
    ] {
        if !data.is_empty()
            && send_stream_event(
                stream,
                serde_json::json!({
                    "type": if is_stdout { "stdout" } else { "stderr" },
                    "data": data,
                }),
            )
            .is_err()
        {
            return;
        }
    }
    match result {
        Ok(result) if result.timed_out => {
            logger::info(format_args!(
                "stream finished: timed_out=true, timeout={timeout_ms}ms"
            ));
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "timeout", "timeout": timeout_ms }),
            );
        }
        Ok(result) => {
            logger::info(format_args!(
                "stream finished: exit_code={:?}, timed_out=false",
                result.exit_code
            ));
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "exit", "exit_code": result.exit_code }),
            );
        }
        Err(err) => {
            logger::info(format_args!("stream finished: error={err}"));
            let _ = send_stream_event(
                stream,
                serde_json::json!({ "type": "error", "error": err.to_string() }),
            );
        }
    }
    let _ = finish_chunked_response(stream);
}
