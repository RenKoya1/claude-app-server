//! `command/exec` endpoints. Spawn a child process, capture stdout/stderr,
//! return when it exits.
//!
//! Codex parity (subset):
//! - `command/exec` — buffered: returns final stdout/stderr/exitCode.
//! - `command/exec` with `processId` + `streamStdoutStderr: true` — streams
//!   bytes as `command/exec/outputDelta` notifications, then returns the
//!   full result when the process exits.
//! - `command/exec/write` — write base64 stdin to a running streaming process,
//!   or close stdin.
//! - `command/exec/terminate` — kill a running streaming process by id.
//!
//! No sandboxing. Mirrors codex `externalSandbox` mode where the host is
//! responsible for restricting the process. Use this only for trusted local
//! UIs.

use crate::outgoing::OutgoingSender;
use base64::{engine::general_purpose, Engine};
use claude_app_server_protocol as proto;
use proto::{
    CommandExecOutputDeltaEvent, CommandExecParams, CommandExecResult, CommandExecTerminateParams,
    CommandExecWriteParams,
};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};

const DEFAULT_OUTPUT_CAP: usize = 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("empty command")]
    EmptyCommand,
    #[error("incompatible options: {0}")]
    BadOptions(String),
    #[error("no such process: {0}")]
    NoSuchProcess(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),
}

impl ExecError {
    pub fn error_code(&self) -> i64 {
        match self {
            ExecError::EmptyCommand | ExecError::BadOptions(_) | ExecError::NoSuchProcess(_)
            | ExecError::Base64(_) => proto::INVALID_PARAMS,
            ExecError::Io(_) => proto::INTERNAL_ERROR,
        }
    }
}

struct RunningProcess {
    stdin_tx: tokio::sync::mpsc::Sender<StdinCommand>,
    kill_signal: Arc<Notify>,
}

enum StdinCommand {
    Write(Vec<u8>),
    Close,
}

#[derive(Default, Clone)]
pub struct ExecRegistry {
    inner: Arc<Mutex<HashMap<String, RunningProcess>>>,
}

impl ExecRegistry {
    pub fn new() -> Self { Self::default() }

    pub async fn write(&self, params: CommandExecWriteParams) -> Result<(), ExecError> {
        let guard = self.inner.lock().await;
        let proc = guard
            .get(&params.process_id)
            .ok_or_else(|| ExecError::NoSuchProcess(params.process_id.clone()))?;
        if params.close_stdin {
            let _ = proc.stdin_tx.send(StdinCommand::Close).await;
            return Ok(());
        }
        let Some(b64) = params.data_base64 else {
            return Err(ExecError::BadOptions(
                "data_base64 or close_stdin required".into(),
            ));
        };
        let bytes = general_purpose::STANDARD.decode(b64.as_bytes())?;
        proc.stdin_tx
            .send(StdinCommand::Write(bytes))
            .await
            .map_err(|_| ExecError::NoSuchProcess(params.process_id.clone()))?;
        Ok(())
    }

    pub async fn terminate(
        &self,
        params: CommandExecTerminateParams,
    ) -> Result<(), ExecError> {
        let guard = self.inner.lock().await;
        let proc = guard
            .get(&params.process_id)
            .ok_or_else(|| ExecError::NoSuchProcess(params.process_id.clone()))?;
        proc.kill_signal.notify_waiters();
        Ok(())
    }

    pub async fn run(
        &self,
        params: CommandExecParams,
        out: OutgoingSender,
    ) -> Result<CommandExecResult, ExecError> {
        if params.command.is_empty() {
            return Err(ExecError::EmptyCommand);
        }
        if params.disable_output_cap && params.output_bytes_cap.is_some() {
            return Err(ExecError::BadOptions(
                "disable_output_cap conflicts with output_bytes_cap".into(),
            ));
        }
        if params.disable_timeout && params.timeout_ms.is_some() {
            return Err(ExecError::BadOptions(
                "disable_timeout conflicts with timeout_ms".into(),
            ));
        }

        let cap = if params.disable_output_cap {
            usize::MAX
        } else {
            params.output_bytes_cap.unwrap_or(DEFAULT_OUTPUT_CAP)
        };
        let timeout = if params.disable_timeout {
            None
        } else {
            Some(std::time::Duration::from_millis(
                params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            ))
        };

        let mut cmd = Command::new(&params.command[0]);
        cmd.args(&params.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &params.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &params.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;

        let process_id = params.process_id.clone();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<StdinCommand>(16);
        let kill_signal = Arc::new(Notify::new());

        if let Some(pid) = &process_id {
            self.inner.lock().await.insert(
                pid.clone(),
                RunningProcess {
                    stdin_tx: stdin_tx.clone(),
                    kill_signal: kill_signal.clone(),
                },
            );
        }

        // stdin pump
        if let Some(mut stdin) = stdin {
            tokio::spawn(async move {
                while let Some(cmd) = stdin_rx.recv().await {
                    match cmd {
                        StdinCommand::Write(bytes) => {
                            if stdin.write_all(&bytes).await.is_err() {
                                break;
                            }
                            let _ = stdin.flush().await;
                        }
                        StdinCommand::Close => break,
                    }
                }
                drop(stdin);
            });
        }

        let (stdout_handle, stdout_done) = capture_stream(
            "stdout",
            stdout,
            cap,
            params.stream_stdout_stderr,
            process_id.clone(),
            out.clone(),
        );
        let (stderr_handle, stderr_done) = capture_stream(
            "stderr",
            stderr,
            cap,
            params.stream_stdout_stderr,
            process_id.clone(),
            out.clone(),
        );

        let result = run_to_completion(
            &mut child,
            timeout,
            kill_signal.clone(),
        )
        .await;

        let _ = stdout_done.await;
        let _ = stderr_done.await;
        let stdout_bytes = stdout_handle.await.unwrap_or_default();
        let stderr_bytes = stderr_handle.await.unwrap_or_default();

        if let Some(pid) = &process_id {
            self.inner.lock().await.remove(pid);
        }

        let (exit_code, timed_out) = match result {
            Ok(code) => (code, false),
            Err(true) => (-1, true),
            Err(false) => (-1, false),
        };

        Ok(CommandExecResult {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            timed_out,
        })
    }
}

fn capture_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    stream_name: &'static str,
    reader: Option<R>,
    cap: usize,
    stream_to_client: bool,
    process_id: Option<String>,
    out: OutgoingSender,
) -> (tokio::task::JoinHandle<Vec<u8>>, tokio::sync::oneshot::Receiver<()>) {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let mut collected = Vec::new();
        if let Some(mut r) = reader {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 8 * 1024];
            loop {
                match r.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if stream_to_client {
                            if let Some(pid) = &process_id {
                                let event = CommandExecOutputDeltaEvent {
                                    process_id: pid.clone(),
                                    stream: stream_name.into(),
                                    delta_base64: general_purpose::STANDARD.encode(chunk),
                                };
                                out.notify(
                                    proto::notification::COMMAND_EXEC_OUTPUT_DELTA,
                                    &event,
                                )
                                .await;
                            }
                        }
                        if collected.len() < cap {
                            let room = cap.saturating_sub(collected.len());
                            collected.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let _ = done_tx.send(());
        collected
    });
    (handle, done_rx)
}

/// Returns Ok(exit_code) on natural exit; Err(true) if timed out; Err(false) if killed.
async fn run_to_completion(
    child: &mut Child,
    timeout: Option<std::time::Duration>,
    kill_signal: Arc<Notify>,
) -> Result<i32, bool> {
    let timeout_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        if let Some(d) = timeout {
            Box::pin(tokio::time::sleep(d))
        } else {
            Box::pin(std::future::pending::<()>())
        };
    tokio::select! {
        wait = child.wait() => {
            match wait {
                Ok(status) => Ok(status.code().unwrap_or(-1)),
                Err(_) => Err(false),
            }
        }
        _ = timeout_fut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(true)
        }
        _ = kill_signal.notified() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(false)
        }
    }
}
