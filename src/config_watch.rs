use crate::{config::RuntimePolicy, logger, process::ProcessJob};
use notify::{RecursiveMode, Watcher};
use std::{
    io,
    net::SocketAddr,
    path::Path,
    process::{Child, Command, ExitCode, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    time::Duration,
};

const DEBOUNCE_DELAY: Duration = Duration::from_millis(200);
const WORKER_CHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
pub(crate) struct WorkerOptions {
    pub listen: Option<SocketAddr>,
    pub log_level: logger::Level,
}

struct WorkerProcess {
    child: Child,
    // Closing this Job Object also kills the worker if the supervisor exits unexpectedly.
    _job: ProcessJob,
}

pub(crate) fn supervise(config_path: &Path, options: WorkerOptions) -> ExitCode {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(sender) {
        Ok(watcher) => watcher,
        Err(err) => {
            logger::error(format_args!(
                "failed to start config watcher for {}: {err}",
                config_path.display()
            ));
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = watcher.watch(config_path, RecursiveMode::NonRecursive) {
        logger::error(format_args!(
            "failed to watch config {}: {err}",
            config_path.display()
        ));
        return ExitCode::FAILURE;
    }

    let mut worker = match start_worker(config_path, options) {
        Ok(worker) => worker,
        Err(err) => {
            logger::error(format_args!("failed to start config worker: {err}"));
            return ExitCode::FAILURE;
        }
    };

    loop {
        match receiver.recv_timeout(WORKER_CHECK_INTERVAL) {
            Ok(Ok(_)) => {
                drain_events(&receiver);
                std::thread::sleep(DEBOUNCE_DELAY);
                drain_events(&receiver);
                if let Err(err) = RuntimePolicy::load(Some(config_path)) {
                    logger::error(format_args!(
                        "config change ignored; keeping current worker: {err}"
                    ));
                    continue;
                }
                logger::info(format_args!(
                    "config changed; restarting worker: {}",
                    config_path.display()
                ));
                stop_worker(worker);
                worker = match start_worker(config_path, options) {
                    Ok(worker) => worker,
                    Err(err) => {
                        logger::error(format_args!("failed to restart config worker: {err}"));
                        return ExitCode::FAILURE;
                    }
                };
            }
            Ok(Err(err)) => logger::error(format_args!("config watcher error: {err}")),
            Err(RecvTimeoutError::Timeout) => match worker.child.try_wait() {
                Ok(Some(status)) => return exit_code(status.code()),
                Ok(None) => {}
                Err(err) => {
                    logger::error(format_args!("failed to inspect config worker: {err}"));
                    return ExitCode::FAILURE;
                }
            },
            Err(RecvTimeoutError::Disconnected) => {
                logger::error(format_args!("config watcher stopped unexpectedly"));
                return ExitCode::FAILURE;
            }
        }
    }
}

fn start_worker(config_path: &Path, options: WorkerOptions) -> io::Result<WorkerProcess> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.arg("--config").arg(config_path);
    if let Some(listen) = options.listen {
        command.arg("--listen").arg(listen.to_string());
    }
    command
        .arg("--log-level")
        .arg(options.log_level.to_string())
        .arg("--config-watch-worker")
        .arg("serve")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command.spawn()?;
    // Only this outer Job permits breakaway, so an explicitly detached request can
    // outlive a worker restart without weakening ordinary command tree cleanup.
    let job = match ProcessJob::assign(&child, true, true) {
        Ok(job) => job,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    Ok(WorkerProcess { child, _job: job })
}

fn stop_worker(mut worker: WorkerProcess) {
    if let Err(err) = worker.child.kill()
        && err.kind() != io::ErrorKind::InvalidInput
    {
        logger::warn(format_args!("failed to stop config worker: {err}"));
    }
    let _ = worker.child.wait();
}

fn drain_events(receiver: &mpsc::Receiver<notify::Result<notify::Event>>) {
    while receiver.try_recv().is_ok() {}
}

fn exit_code(code: Option<i32>) -> ExitCode {
    ExitCode::from(code.unwrap_or(1).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_preserved_when_representable() {
        assert_eq!(exit_code(Some(0)), ExitCode::SUCCESS);
        assert_eq!(exit_code(None), ExitCode::FAILURE);
    }

    #[test]
    fn worker_options_copy_connection_settings() {
        let options = WorkerOptions {
            listen: Some("127.0.0.1:9527".parse().unwrap()),
            log_level: logger::Level::Debug,
        };
        assert_eq!(options.listen.unwrap().port(), 9527);
        assert_eq!(options.log_level.to_string(), "debug");
    }
}
