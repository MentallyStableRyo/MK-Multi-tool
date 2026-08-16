//! Job lifecycle: cancellation registry and the progress event channel.
//!
//! Every long-running operation runs on the Tokio pool and reports back through
//! [`emit`], so the webview thread only ever handles small serialised structs
//! and can never be blocked by I/O or an encoder.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

use crate::error::AppError;

/// Single event name the frontend subscribes to.
pub const PROGRESS_EVENT: &str = "mk://job-progress";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Validating input and resolving fresh stream URLs.
    Resolving,
    /// Probing keyframes and choosing copy vs. re-encode.
    Planning,
    /// FFmpeg is pulling and writing the slice.
    Rendering,
    /// Verifying the output and moving it into place.
    Finalizing,
    Done,
    Failed,
    Cancelled,
}

/// Serialised form of an [`AppError`] carried on a failure event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub kind: String,
    pub message: String,
    pub hint: Option<String>,
}

impl From<&AppError> for ErrorPayload {
    fn from(e: &AppError) -> Self {
        Self {
            kind: e.kind().to_string(),
            message: e.to_string(),
            hint: e.hint(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    pub job_id: String,
    pub stage: Stage,
    /// Short status line for the UI.
    pub message: String,
    /// 0.0–100.0, or `None` when the work is not measurable yet.
    pub percent: Option<f64>,
    /// Preformatted rate, e.g. `18.4× realtime`.
    pub speed: Option<String>,
    pub eta_seconds: Option<f64>,
    pub bytes_written: Option<u64>,
    /// Set on the `Done` event.
    pub output_path: Option<String>,
    /// Set on the `Failed` event.
    pub error: Option<ErrorPayload>,
    /// Whether the chosen plan avoids re-encoding entirely.
    pub lossless: Option<bool>,
}

impl JobEvent {
    pub fn new(job_id: &str, stage: Stage, message: impl Into<String>) -> Self {
        Self {
            job_id: job_id.to_string(),
            stage,
            message: message.into(),
            percent: None,
            speed: None,
            eta_seconds: None,
            bytes_written: None,
            output_path: None,
            error: None,
            lossless: None,
        }
    }

    pub fn percent(mut self, value: f64) -> Self {
        self.percent = Some(value.clamp(0.0, 100.0));
        self
    }

    pub fn speed(mut self, value: Option<String>) -> Self {
        self.speed = value;
        self
    }

    pub fn eta(mut self, seconds: Option<f64>) -> Self {
        self.eta_seconds = seconds.filter(|s| s.is_finite() && *s >= 0.0);
        self
    }

    pub fn bytes(mut self, value: Option<u64>) -> Self {
        self.bytes_written = value;
        self
    }

    pub fn output(mut self, path: impl Into<String>) -> Self {
        self.output_path = Some(path.into());
        self
    }

    pub fn lossless(mut self, value: bool) -> Self {
        self.lossless = Some(value);
        self
    }

    pub fn failure(job_id: &str, error: &AppError) -> Self {
        let stage = if error.is_cancelled() {
            Stage::Cancelled
        } else {
            Stage::Failed
        };
        let mut event = Self::new(job_id, stage, error.to_string());
        if !error.is_cancelled() {
            event.error = Some(ErrorPayload::from(error));
        }
        event
    }
}

/// Push an event to the frontend. Emission failures are swallowed: the window
/// may already be closing, and that must never abort an in-flight job's cleanup.
pub fn emit(app: &AppHandle, event: JobEvent) {
    let _ = app.emit(PROGRESS_EVENT, event);
}

/// Tracks the cancellation token of every in-flight job.
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<HashMap<String, CancellationToken>>,
}

impl JobRegistry {
    /// Register a new job and hand back its token.
    pub fn register(&self, job_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        self.inner.lock().insert(job_id.to_string(), token.clone());
        token
    }

    /// Signal cancellation. Returns `false` if the job already finished.
    pub fn cancel(&self, job_id: &str) -> bool {
        match self.inner.lock().remove(job_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Drop a finished job's token so the map does not grow without bound.
    pub fn finish(&self, job_id: &str) {
        self.inner.lock().remove(job_id);
    }

    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.inner.lock().len()
    }

    /// Cancel everything, used when the last window closes.
    pub fn cancel_all(&self) {
        let mut guard = self.inner.lock();
        for (_, token) in guard.drain() {
            token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_and_releases_jobs() {
        let registry = JobRegistry::default();

        let token = registry.register("job-1");
        assert_eq!(registry.active_count(), 1);
        assert!(!token.is_cancelled());

        assert!(registry.cancel("job-1"));
        assert!(token.is_cancelled());
        assert_eq!(registry.active_count(), 0);

        // Cancelling an unknown or already-finished job is a no-op, not an error.
        assert!(!registry.cancel("job-1"));
        assert!(!registry.cancel("never-existed"));
    }

    #[test]
    fn finish_removes_without_cancelling() {
        let registry = JobRegistry::default();
        let token = registry.register("job-2");

        registry.finish("job-2");

        assert_eq!(registry.active_count(), 0);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_all_stops_every_job() {
        let registry = JobRegistry::default();
        let a = registry.register("a");
        let b = registry.register("b");

        registry.cancel_all();

        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn cancellation_events_carry_no_error_payload() {
        let cancelled = JobEvent::failure("j", &AppError::Cancelled);
        assert_eq!(cancelled.stage, Stage::Cancelled);
        assert!(cancelled.error.is_none());

        let failed = JobEvent::failure("j", &AppError::other("boom"));
        assert_eq!(failed.stage, Stage::Failed);
        assert_eq!(failed.error.unwrap().message, "boom");
    }

    #[test]
    fn percent_is_clamped_and_eta_rejects_nonsense() {
        assert_eq!(JobEvent::new("j", Stage::Rendering, "x").percent(140.0).percent, Some(100.0));
        assert_eq!(JobEvent::new("j", Stage::Rendering, "x").percent(-3.0).percent, Some(0.0));
        assert_eq!(JobEvent::new("j", Stage::Rendering, "x").eta(Some(f64::NAN)).eta_seconds, None);
        assert_eq!(JobEvent::new("j", Stage::Rendering, "x").eta(Some(-1.0)).eta_seconds, None);
    }
}
