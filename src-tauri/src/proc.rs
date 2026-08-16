//! Async child-process plumbing shared by the yt-dlp and FFmpeg wrappers.
//!
//! Everything here is cancellation-aware: a cancelled [`CancellationToken`]
//! kills the child immediately rather than letting an orphaned encoder keep
//! burning CPU. `kill_on_drop(true)` is the backstop if a task is dropped
//! without going through the cancel path.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};

/// How many trailing stderr lines to retain for diagnostics. FFmpeg can emit
/// megabytes of warnings; only the tail is ever useful.
const STDERR_TAIL_LINES: usize = 60;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    /// Last [`STDERR_TAIL_LINES`] lines, newline-joined.
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Build a child-process command with our standard hardening applied.
pub fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Without this every sidecar invocation flashes a console window on Windows.
    // `creation_flags` is inherent on tokio's Command, so no trait import here.
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    cmd
}

/// Run a child to completion, forwarding each stdout line to `on_line`.
///
/// Returns `Err(AppError::Cancelled)` promptly on cancellation, after killing
/// the child. A non-zero exit is *not* an error here — inspect [`Output::code`]
/// so callers can attach domain-specific context.
pub async fn run_streaming<F>(
    mut cmd: Command,
    cancel: &CancellationToken,
    mut on_line: F,
) -> Result<Output>
where
    F: FnMut(&str),
{
    let mut child = cmd.spawn().map_err(|e| {
        AppError::other(format!(
            "could not start `{}`: {e}",
            cmd.as_std().get_program().to_string_lossy()
        ))
    })?;

    // Drain stderr concurrently into a bounded ring buffer. If we did not read
    // it, a chatty child would eventually block on a full pipe and deadlock.
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let err_handle = child.stderr.take().map(|stderr| {
        let tail = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = tail.lock();
                if guard.len() >= STDERR_TAIL_LINES {
                    guard.pop_front();
                }
                guard.push_back(line);
            }
        })
    });

    let mut stdout_buf = String::new();

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    if let Some(h) = err_handle.as_ref() { h.abort(); }
                    return Err(AppError::Cancelled);
                }

                next = lines.next_line() => match next {
                    Ok(Some(line)) => {
                        on_line(&line);
                        stdout_buf.push_str(&line);
                        stdout_buf.push('\n');
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = child.kill().await;
                        if let Some(h) = err_handle.as_ref() { h.abort(); }
                        return Err(AppError::Io(e));
                    }
                },
            }
        }
    }

    let status = tokio::select! {
        biased;

        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            if let Some(h) = err_handle.as_ref() { h.abort(); }
            return Err(AppError::Cancelled);
        }

        status = child.wait() => status.map_err(AppError::Io)?,
    };

    // The child is gone, so the stderr pipe is closed and this join completes.
    if let Some(h) = err_handle {
        let _ = h.await;
    }

    let stderr = {
        let guard = tail.lock();
        guard.iter().cloned().collect::<Vec<_>>().join("\n")
    };

    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout: stdout_buf,
        stderr,
    })
}

/// Run a child to completion and capture its output, ignoring per-line events.
pub async fn run_capture(cmd: Command, cancel: &CancellationToken) -> Result<Output> {
    run_streaming(cmd, cancel, |_| {}).await
}
