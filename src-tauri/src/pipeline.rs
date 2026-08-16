//! The slice pipeline: resolve → plan → render → verify → commit.
//!
//! This is the whole backend workflow, with no knowledge of Tauri, windows, or
//! events. Callers observe it through a [`Progress`] callback, which keeps the
//! pipeline testable without an `AppHandle` — see `smoke.rs`, which drives the
//! exact code path production uses rather than a reimplementation of it.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::{AppError, Result};
use crate::platform::Platform;
use crate::probe;
use crate::render::{self, Container, Precision, RenderJob, RenderTick};
use crate::staging::PartialOutput;
use crate::timecode::Range;
use crate::youtube::{self, CookieOptions};

/// Attempts allowed for one slice.
///
/// Google's media CDN intermittently answers a valid, unexpired URL with `403
/// Forbidden` — observed repeatedly during development, sometimes for several
/// consecutive URL batches. Each retry re-resolves through yt-dlp to get a
/// fresh set, which costs ~2s and turns a spurious hard failure into a delay
/// the user barely notices.
pub const MAX_ATTEMPTS: u32 = 3;

/// What the pipeline is doing right now.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Fetching fresh stream URLs from yt-dlp.
    Resolving,
    /// Probing the source's keyframe layout.
    Planning,
    /// The cut strategy has been chosen; rendering is about to start.
    Planned {
        /// Human-readable description of the chosen approach.
        note: String,
        /// True when neither stream is being re-encoded.
        lossless: bool,
        /// Output length in seconds. Only the denominator for percent complete
        /// when `expected_frames` is `None`.
        duration: f64,
        /// Preferred denominator for percent complete — see
        /// [`crate::render::RenderTick::out_seconds`] for why frames beat time.
        expected_frames: Option<f64>,
    },
    /// FFmpeg reported forward progress.
    Rendering(RenderTick),
    /// A transient failure is being retried with freshly resolved URLs.
    Retrying { attempt: u32, message: String },
    /// Checking the rendered file before moving it into place.
    Verifying,
}

/// Everything needed to produce one slice.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// Already normalised by [`Platform::normalize`].
    pub url: &'a str,
    /// The site `url` points at. Only used to word failures.
    pub platform: Platform,
    /// Track selector, e.g. `"299+258"`.
    pub track_id: &'a str,
    /// Already validated by [`crate::timecode::validate`].
    pub range: Range,
    pub container: Container,
    pub precision: Precision,
    /// Force an AV1 source down to H.264 for players that cannot decode AV1.
    pub transcode_av1: bool,
    pub title: &'a str,
    pub cookies: &'a CookieOptions,
}

/// Run the pipeline, retrying once per [`MAX_ATTEMPTS`] on transient failures.
///
/// On success the slice sits at `destination`. On any failure — including
/// cancellation — nothing is written there: the render targets a staging file
/// that [`PartialOutput`] removes on drop.
pub async fn execute<F>(
    toolchain: &Toolchain,
    request: Request<'_>,
    destination: PathBuf,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<PathBuf>
where
    F: FnMut(Progress),
{
    let mut last_error = AppError::other("no attempt was made");

    for attempt in 1..=MAX_ATTEMPTS {
        match attempt_once(toolchain, request, &destination, cancel, &mut on_progress).await {
            Ok(path) => return Ok(path),

            // Cancellation and permanent failures both stop here; only a
            // transient network fault earns another go.
            Err(e) if e.is_cancelled() || !e.is_transient() || attempt == MAX_ATTEMPTS => {
                return Err(e);
            }

            Err(e) => {
                on_progress(Progress::Retrying {
                    attempt: attempt + 1,
                    message: "The CDN rejected the request — refreshing stream URLs and retrying…"
                        .to_string(),
                });
                last_error = e;
            }
        }
    }

    Err(last_error)
}

/// One pass. Every early return leaves a clean filesystem.
async fn attempt_once<F>(
    toolchain: &Toolchain,
    request: Request<'_>,
    destination: &Path,
    cancel: &CancellationToken,
    on_progress: &mut F,
) -> Result<PathBuf>
where
    F: FnMut(Progress),
{
    // 1. Resolve. URLs captured during analysis are short-lived and IP-bound,
    //    so a stale set would 403 partway through the render.
    on_progress(Progress::Resolving);
    let document =
        youtube::dump_json(toolchain, request.url, request.platform, request.cookies, cancel)
            .await?;
    let streams = youtube::resolve_streams(&document, request.track_id)?;

    // 2. Decide copy vs. re-encode from the source's real keyframe layout.
    on_progress(Progress::Planning);
    let plan = render::plan_cut(
        toolchain,
        &streams,
        request.range,
        request.container,
        request.precision,
        request.transcode_av1,
        cancel,
    )
    .await;

    on_progress(Progress::Planned {
        note: plan.note.clone(),
        lossless: plan.is_lossless(),
        duration: plan.duration,
        expected_frames: plan.expected_frames,
    });

    // 3. Render into a staging file beside the destination.
    let staged = PartialOutput::new(destination.to_path_buf());
    render::render(
        toolchain,
        RenderJob {
            streams: &streams,
            plan: &plan,
            container: request.container,
            title: request.title,
            output: staged.staging_path(),
        },
        cancel,
        |tick| on_progress(Progress::Rendering(tick)),
    )
    .await?;

    // 4. Verify, then commit.
    on_progress(Progress::Verifying);
    warn_if_short(toolchain, staged.staging_path(), plan.duration, cancel).await;

    staged.commit()
}

/// A render much shorter than requested usually means the range ran past the
/// real end of the stream. Not fatal — the file plays — so this only logs.
async fn warn_if_short(
    toolchain: &Toolchain,
    path: &Path,
    expected: f64,
    cancel: &CancellationToken,
) {
    if let Some(actual) = probe::local_duration(toolchain, path, cancel).await {
        if actual + 1.0 < expected {
            eprintln!(
                "[mk] rendered {actual:.2}s but {expected:.2}s was requested — \
                 the source stream likely ends earlier than reported"
            );
        }
    }
}
