use crate::{
    encoding::{decode_all, is_boundary},
    http::{send_json_error, send_response},
    logger,
    process::{ExecRequest, OutputEncoding, run_command_observed},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::TcpStream,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESULT_STREAM_BYTES: usize = 1024 * 1024;
const TERMINATE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const COMPLETED_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_SESSIONS: usize = 128;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static TOTAL_OUTPUT_BYTES: AtomicUsize = AtomicUsize::new(0);
static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Session>>>>> = OnceLock::new();
static RESULT_RESPONSE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionStatus {
    Starting,
    Running,
    Terminating,
    Terminated,
    Exited,
    TimedOut,
    Failed,
}

impl SessionStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Terminated | Self::Exited | Self::TimedOut | Self::Failed
        )
    }
}

#[derive(Debug)]
struct Session {
    pid: Option<u32>,
    status: SessionStatus,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error: Option<String>,
    finished_at: Option<Instant>,
    cancellation: Arc<AtomicBool>,
    output_encoding: OutputEncoding,
}

impl Session {
    fn new(output_encoding: OutputEncoding) -> Self {
        Self {
            pid: None,
            status: SessionStatus::Starting,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            error: None,
            finished_at: None,
            cancellation: Arc::new(AtomicBool::new(false)),
            output_encoding,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResultRequest {
    session_id: String,
    #[serde(default)]
    stdout_offset: usize,
    #[serde(default)]
    stderr_offset: usize,
}

#[derive(Serialize)]
struct SpawnResponse<'a> {
    session_id: &'a str,
    pid: u32,
    status: SessionStatus,
}

#[derive(Serialize)]
struct ResultResponse<'a> {
    session_id: &'a str,
    pid: Option<u32>,
    status: SessionStatus,
    exit_code: Option<i32>,
    stdout_offset: usize,
    stdout_next_offset: usize,
    stdout_complete: bool,
    stderr_offset: usize,
    stderr_next_offset: usize,
    stderr_complete: bool,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error: Option<String>,
}

fn sessions() -> &'static Mutex<HashMap<String, Arc<Mutex<Session>>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Mutex<Session>>>> {
    sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cleanup_sessions(registry: &mut HashMap<String, Arc<Mutex<Session>>>) {
    let mut released = 0usize;
    registry.retain(|_, session| {
        let session = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keep = session
            .finished_at
            .is_none_or(|finished| finished.elapsed() < COMPLETED_SESSION_TTL);
        if !keep {
            released = released.saturating_add(session.stdout.len() + session.stderr.len());
        }
        keep
    });
    release_output(released);
}

fn append_output(target: &mut Vec<u8>, truncated: &mut bool, data: &[u8]) {
    let per_session_available = MAX_OUTPUT_BYTES.saturating_sub(target.len());
    let requested = data.len().min(per_session_available);
    let reserved = reserve_output(requested);
    target.extend_from_slice(&data[..reserved]);
    if reserved < data.len() {
        *truncated = true;
    }
}

fn reserve_output(requested: usize) -> usize {
    let mut current = TOTAL_OUTPUT_BYTES.load(Ordering::Relaxed);
    loop {
        let reserved = output_reservation(current, requested);
        match TOTAL_OUTPUT_BYTES.compare_exchange_weak(
            current,
            current + reserved,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return reserved,
            Err(actual) => current = actual,
        }
    }
}

fn output_reservation(current: usize, requested: usize) -> usize {
    requested.min(MAX_TOTAL_OUTPUT_BYTES.saturating_sub(current))
}

fn result_chunk_end(output: &[u8], offset: usize, encoding: OutputEncoding) -> Option<usize> {
    if !is_boundary(output, offset, encoding) {
        return None;
    }
    let mut end = offset
        .saturating_add(MAX_RESULT_STREAM_BYTES)
        .min(output.len());
    while end > offset && !is_boundary(output, end, encoding) {
        end -= 1;
    }
    Some(end)
}

fn release_output(released: usize) {
    if released == 0 {
        return;
    }
    let _ = TOTAL_OUTPUT_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(released))
    });
}

fn evict_oldest_completed(registry: &mut HashMap<String, Arc<Mutex<Session>>>) -> bool {
    let oldest_id = registry
        .iter()
        .filter_map(|(id, session)| {
            let session = session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            session
                .finished_at
                .map(|finished_at| (id.clone(), finished_at))
        })
        .min_by_key(|(_, finished_at)| *finished_at)
        .map(|(id, _)| id);
    let Some(oldest_id) = oldest_id else {
        return false;
    };
    let Some(session) = registry.remove(&oldest_id) else {
        return false;
    };
    let session = session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    release_output(session.stdout.len() + session.stderr.len());
    true
}

fn next_session_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn handle_spawn(stream: &mut TcpStream, body: &[u8]) {
    let request: ExecRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(err) => {
            send_json_error(stream, "400 Bad Request", &format!("invalid json: {err}"));
            return;
        }
    };

    let session_id = next_session_id();
    let output_encoding = request.output_encoding();
    let session = Arc::new(Mutex::new(Session::new(output_encoding)));
    {
        let mut registry = lock_sessions();
        cleanup_sessions(&mut registry);
        while registry.len() >= MAX_SESSIONS && evict_oldest_completed(&mut registry) {}
        if registry.len() >= MAX_SESSIONS {
            send_json_error(stream, "503 Service Unavailable", "too many spawn sessions");
            return;
        }
        registry.insert(session_id.clone(), Arc::clone(&session));
    }

    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let worker_session = Arc::clone(&session);
    let cancellation = {
        let session = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&session.cancellation)
    };
    let worker_id = session_id.clone();
    thread::spawn(move || {
        let mut start_sender = Some(started_tx);
        let result = run_command_observed(
            &request,
            &cancellation,
            |pid| {
                {
                    let mut session = worker_session
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    session.pid = Some(pid);
                    session.status = SessionStatus::Running;
                }
                if let Some(sender) = start_sender.take() {
                    let _ = sender.send(Ok(pid));
                }
            },
            |stdout, data| {
                let mut session = worker_session
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if stdout {
                    let Session {
                        stdout,
                        stdout_truncated,
                        ..
                    } = &mut *session;
                    append_output(stdout, stdout_truncated, data);
                } else {
                    let Session {
                        stderr,
                        stderr_truncated,
                        ..
                    } = &mut *session;
                    append_output(stderr, stderr_truncated, data);
                }
                Ok(())
            },
        );

        let mut session = worker_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(result) => {
                session.exit_code = result.exit_code;
                session.status = if result.terminated {
                    SessionStatus::Terminated
                } else if result.timed_out {
                    SessionStatus::TimedOut
                } else {
                    SessionStatus::Exited
                };
            }
            Err(err) => {
                let message = err.to_string();
                session.status = SessionStatus::Failed;
                session.error = Some(message.clone());
                if let Some(sender) = start_sender.take() {
                    let _ = sender.send(Err(message));
                }
            }
        }
        session.finished_at = Some(Instant::now());
        logger::info(format_args!(
            "spawn session finished: session_id={worker_id}, status={:?}, exit_code={:?}",
            session.status, session.exit_code
        ));
    });

    match started_rx.recv() {
        Ok(Ok(pid)) => {
            logger::info(format_args!(
                "spawn session started: session_id={session_id}, pid={pid}"
            ));
            let status = session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .status;
            let response = SpawnResponse {
                session_id: &session_id,
                pid,
                status,
            };
            let body = serde_json::to_string(&response).expect("spawn response is serializable");
            let _ = send_response(stream, "202 Accepted", &body, "application/json");
        }
        Ok(Err(err)) => {
            lock_sessions().remove(&session_id);
            send_json_error(
                stream,
                "500 Internal Server Error",
                &format!("failed to spawn command: {err}"),
            );
        }
        Err(_) => {
            lock_sessions().remove(&session_id);
            send_json_error(
                stream,
                "500 Internal Server Error",
                "spawn worker stopped before starting the command",
            );
        }
    }
}

fn parse_result_request(stream: &mut TcpStream, body: &[u8]) -> Option<ResultRequest> {
    match serde_json::from_slice(body) {
        Ok(request) => Some(request),
        Err(err) => {
            send_json_error(stream, "400 Bad Request", &format!("invalid json: {err}"));
            None
        }
    }
}

fn find_session(session_id: &str) -> Option<Arc<Mutex<Session>>> {
    let mut registry = lock_sessions();
    cleanup_sessions(&mut registry);
    registry.get(session_id).cloned()
}

fn send_session_result(
    stream: &mut TcpStream,
    request: &ResultRequest,
    session: Arc<Mutex<Session>>,
) {
    let result_lock = RESULT_RESPONSE_LOCK.get_or_init(|| Mutex::new(()));
    let _result_guard = result_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let response = {
        let session = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(stdout_end) = result_chunk_end(
            &session.stdout,
            request.stdout_offset,
            session.output_encoding,
        ) else {
            drop(session);
            send_json_error(
                stream,
                "400 Bad Request",
                "stdout_offset exceeds the captured output or splits an encoded character",
            );
            return;
        };
        let Some(stderr_end) = result_chunk_end(
            &session.stderr,
            request.stderr_offset,
            session.output_encoding,
        ) else {
            drop(session);
            send_json_error(
                stream,
                "400 Bad Request",
                "stderr_offset exceeds the captured output or splits an encoded character",
            );
            return;
        };
        ResultResponse {
            session_id: &request.session_id,
            pid: session.pid,
            status: session.status,
            exit_code: session.exit_code,
            stdout_offset: request.stdout_offset,
            stdout_next_offset: stdout_end,
            stdout_complete: stdout_end == session.stdout.len(),
            stderr_offset: request.stderr_offset,
            stderr_next_offset: stderr_end,
            stderr_complete: stderr_end == session.stderr.len(),
            stdout: decode_all(
                &session.stdout[request.stdout_offset..stdout_end],
                session.output_encoding,
            ),
            stderr: decode_all(
                &session.stderr[request.stderr_offset..stderr_end],
                session.output_encoding,
            ),
            stdout_truncated: session.stdout_truncated,
            stderr_truncated: session.stderr_truncated,
            error: session.error.clone(),
        }
    };
    let body = serde_json::to_string(&response).expect("result response is serializable");
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = send_response(stream, "200 OK", &body, "application/json");
}

fn result_offsets_are_valid(session: &Session, request: &ResultRequest) -> bool {
    result_chunk_end(
        &session.stdout,
        request.stdout_offset,
        session.output_encoding,
    )
    .is_some()
        && result_chunk_end(
            &session.stderr,
            request.stderr_offset,
            session.output_encoding,
        )
        .is_some()
}

pub fn handle_result(stream: &mut TcpStream, body: &[u8]) {
    let Some(request) = parse_result_request(stream, body) else {
        return;
    };
    let Some(session) = find_session(&request.session_id) else {
        send_json_error(stream, "404 Not Found", "spawn session not found");
        return;
    };
    send_session_result(stream, &request, session);
}

pub fn handle_terminate(stream: &mut TcpStream, body: &[u8]) {
    let Some(request) = parse_result_request(stream, body) else {
        return;
    };
    let Some(session) = find_session(&request.session_id) else {
        send_json_error(stream, "404 Not Found", "spawn session not found");
        return;
    };

    let should_wait = {
        let mut state = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !result_offsets_are_valid(&state, &request) {
            drop(state);
            send_json_error(
                stream,
                "400 Bad Request",
                "stdout_offset or stderr_offset exceeds the captured output or splits an encoded character",
            );
            return;
        }
        if state.status.is_terminal() {
            false
        } else {
            state.status = SessionStatus::Terminating;
            state.cancellation.store(true, Ordering::Relaxed);
            true
        }
    };

    if should_wait {
        logger::info(format_args!(
            "terminating spawn session: session_id={}",
            request.session_id
        ));
        let deadline = Instant::now() + TERMINATE_WAIT_TIMEOUT;
        loop {
            let terminated = session
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .status
                .is_terminal();
            if terminated {
                break;
            }
            if Instant::now() >= deadline {
                send_json_error(
                    stream,
                    "500 Internal Server Error",
                    "spawn session did not terminate within 30 seconds",
                );
                return;
            }
            thread::sleep(TERMINATE_POLL_INTERVAL);
        }
    }

    send_session_result(stream, &request, session);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_bounded_and_marked_truncated() {
        let mut output = vec![b'a'; MAX_OUTPUT_BYTES - 2];
        let mut truncated = false;
        append_output(&mut output, &mut truncated, b"hello");
        assert_eq!(output.len(), MAX_OUTPUT_BYTES);
        assert!(truncated);
        assert_eq!(&output[MAX_OUTPUT_BYTES - 2..], b"he");
        release_output(2);
    }

    #[test]
    fn global_output_reservation_never_exceeds_budget() {
        assert_eq!(output_reservation(MAX_TOTAL_OUTPUT_BYTES - 3, 8), 3);
        assert_eq!(output_reservation(MAX_TOTAL_OUTPUT_BYTES, 1), 0);
    }

    #[test]
    fn result_chunks_preserve_utf8_boundaries() {
        let mut output = vec![b'a'; MAX_RESULT_STREAM_BYTES - 1];
        output.extend_from_slice("界".as_bytes());
        assert_eq!(
            result_chunk_end(&output, 0, OutputEncoding::Utf8),
            Some(MAX_RESULT_STREAM_BYTES - 1)
        );
        assert_eq!(
            result_chunk_end(&output, MAX_RESULT_STREAM_BYTES, OutputEncoding::Utf8),
            None
        );
        assert_eq!(
            result_chunk_end(&output, MAX_RESULT_STREAM_BYTES - 1, OutputEncoding::Utf8,),
            Some(output.len())
        );
    }

    #[test]
    fn termination_validates_offsets_before_cancelling() {
        let mut session = Session::new(OutputEncoding::Utf8);
        session.stdout.extend_from_slice("世界".as_bytes());
        let request = ResultRequest {
            session_id: "test".to_string(),
            stdout_offset: 1,
            stderr_offset: 0,
        };

        assert!(!result_offsets_are_valid(&session, &request));
        assert!(!session.cancellation.load(Ordering::Relaxed));
    }

    #[test]
    fn evicts_the_oldest_completed_session() {
        let mut registry = HashMap::new();
        let mut older = Session::new(OutputEncoding::Utf8);
        older.finished_at = Instant::now().checked_sub(Duration::from_secs(2));
        let mut newer = Session::new(OutputEncoding::Utf8);
        newer.finished_at = Instant::now().checked_sub(Duration::from_secs(1));
        registry.insert("older".to_string(), Arc::new(Mutex::new(older)));
        registry.insert("newer".to_string(), Arc::new(Mutex::new(newer)));

        assert!(evict_oldest_completed(&mut registry));
        assert!(!registry.contains_key("older"));
        assert!(registry.contains_key("newer"));
    }
}
