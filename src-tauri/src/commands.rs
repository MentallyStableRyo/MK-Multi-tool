//! The Tauri command surface — the only place the frontend can reach.
//!
//! Commands are thin: they validate, then hand off to the domain modules.
//! `start_slice` returns a job id immediately and does all real work on the
//! Tokio pool, so the webview never blocks on a download or an encoder.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::binaries::{self, Toolchain, ToolchainStatus};
use crate::disk;
use crate::error::{AppError, Result};
use crate::jobs::{self, JobEvent, JobRegistry, Stage};
use crate::pipeline;
use crate::platform::Platform;
use crate::probe;
use crate::render::{Container, Precision};
use crate::staging;
use crate::timecode::{self, Range};
use crate::transcribe::{self, Accuracy};
use crate::youtube::{self, CookieOptions, VideoInfo};

/// Minimum gap between progress events. At ~10 Hz the bar looks smooth without
/// flooding the IPC channel — FFmpeg emits progress far faster than that.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Report which of yt-dlp / ffmpeg / ffprobe were found, and their versions.
#[tauri::command]
pub async fn check_toolchain(app: AppHandle) -> ToolchainStatus {
    binaries::status(&app).await
}

/// Sensible default folder for saved slices.
#[tauri::command]
pub fn default_output_dir() -> String {
    dirs::video_dir()
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .to_string_lossy()
        .into_owned()
}

/// Build a filesystem-safe filename from the video title and slice bounds.
#[tauri::command]
pub fn suggest_filename(title: String, start: f64, end: f64, extension: String) -> String {
    let stem = sanitize_stem(&title);
    let from = timecode::format_ts(start).replace(':', "-").replace('.', "_");
    let to = timecode::format_ts(end).replace(':', "-").replace('.', "_");
    format!("{stem} [{from} - {to}].{extension}")
}

/// Strip characters that are illegal or troublesome in filenames on any of the
/// three target platforms, then bound the length.
fn sanitize_stem(title: &str) -> String {
    const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

    let cleaned: String = title
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();

    // Collapse runs of separators and trim the characters Windows strips from
    // the end of a name (which would otherwise change the file we write to).
    let mut out = String::with_capacity(cleaned.len());
    let mut last_dash = false;
    for c in cleaned.chars() {
        let is_dash = c == '-' || c == ' ';
        if is_dash && last_dash {
            continue;
        }
        last_dash = is_dash;
        out.push(c);
    }
    let out = out.trim().trim_end_matches('.').trim().to_string();

    // 120 chars leaves room for the timecode suffix inside a 255-byte limit.
    let truncated: String = out.chars().take(120).collect();
    let truncated = truncated.trim().to_string();

    if truncated.is_empty() {
        // Short-form posts frequently have no title at all.
        "slice".to_string()
    } else {
        truncated
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Normalise the link, fetch metadata, and build the quality list.
///
/// `platform` is the tab the link was pasted into; it decides which link shapes
/// are accepted. Absent, it defaults to YouTube.
#[tauri::command]
pub async fn analyze_url(
    app: AppHandle,
    url: String,
    platform: Option<Platform>,
    cookies: Option<CookieOptions>,
) -> Result<VideoInfo> {
    let platform = platform.unwrap_or_default();
    let normalized = platform.normalize(&url)?;
    let toolchain = binaries::resolve(&app)?;
    let cookies = cookies.unwrap_or_default();
    let cancel = tokio_util::sync::CancellationToken::new();

    let document = youtube::dump_json(&toolchain, &normalized, platform, &cookies, &cancel).await?;
    let info = youtube::parse_info(&document)?;

    if info.tracks.is_empty() {
        return Err(AppError::other(format!(
            "{} offered no downloadable formats for this link.",
            platform.label()
        )));
    }

    Ok(info)
}

// ---------------------------------------------------------------------------
// Slicing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceRequest {
    pub url: String,
    /// The tab this request came from, which decides how `url` is validated.
    #[serde(default)]
    pub platform: Platform,
    /// Selector from the chosen [`crate::youtube::QualityTrack`].
    pub track_id: String,
    pub start: f64,
    pub end: f64,
    /// Source duration, used to bounds-check the range.
    pub duration_seconds: f64,
    pub container: Container,
    #[serde(default)]
    pub precision: Precision,
    /// Re-encode AV1 sources to H.264 instead of stream-copying them.
    #[serde(default)]
    pub transcode_av1: bool,
    pub output_path: String,
    /// Combined bitrate of the chosen track, for the disk-space estimate.
    #[serde(default)]
    pub bitrate_kbps: f64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cookies: Option<CookieOptions>,
}

/// Start a slice job. Returns its id immediately; all progress arrives on the
/// [`jobs::PROGRESS_EVENT`] channel.
#[tauri::command]
pub async fn start_slice(
    app: AppHandle,
    registry: State<'_, JobRegistry>,
    request: SliceRequest,
) -> Result<String> {
    // Everything cheap and fallible is validated here so the caller gets a
    // synchronous error instead of a job that dies one event later.
    let normalized = request.platform.normalize(&request.url)?;
    let range = timecode::validate(request.start, request.end, request.duration_seconds)?;
    let toolchain = binaries::resolve(&app)?;

    let output_path = PathBuf::from(&request.output_path);
    let parent = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| AppError::other("Output path has no folder component"))?;
    if !parent.is_dir() {
        std::fs::create_dir_all(parent)?;
    }

    // Clear staging files a hard-killed previous run left in this folder before
    // measuring free space, so their bytes count as available.
    staging::sweep_orphans(parent);

    disk::ensure_free(
        parent,
        disk::estimate_bytes(request.bitrate_kbps, range.duration()),
    )?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let cancel = registry.register(&job_id);

    let task_app = app.clone();
    let task_id = job_id.clone();
    let prepared = Prepared {
        url: normalized,
        range,
        output_path,
        request,
    };

    tauri::async_runtime::spawn(async move {
        let result = run_slice(&task_app, &task_id, &toolchain, &prepared, &cancel).await;

        match result {
            Ok(path) => jobs::emit(
                &task_app,
                JobEvent::new(&task_id, Stage::Done, "Slice ready")
                    .percent(100.0)
                    .output(path.to_string_lossy().into_owned()),
            ),
            Err(error) => jobs::emit(&task_app, JobEvent::failure(&task_id, &error)),
        }

        task_app.state::<JobRegistry>().finish(&task_id);
    });

    Ok(job_id)
}

/// A [`SliceRequest`] whose fallible parts have already been validated, so the
/// spawned task cannot fail on anything the caller could have been told about
/// synchronously.
struct Prepared {
    /// Normalised by [`Platform::normalize`].
    url: String,
    /// Bounds-checked by [`timecode::validate`].
    range: Range,
    output_path: PathBuf,
    request: SliceRequest,
}

/// Drive one slice, translating [`pipeline::Progress`] into UI events.
///
/// All the domain logic lives in [`pipeline`]; this function's only job is the
/// mapping, plus throttling the render ticks so the IPC channel is not flooded.
async fn run_slice(
    app: &AppHandle,
    job_id: &str,
    toolchain: &Toolchain,
    prepared: &Prepared,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PathBuf> {
    let request = &prepared.request;
    let cookies = request.cookies.clone().unwrap_or_default();

    // Carried across callbacks so every render event repeats the plan summary
    // and percentages have a denominator.
    let mut note = String::new();
    let mut lossless = false;
    let mut total = 1.0f64;
    let mut total_frames: Option<f64> = None;
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;

    pipeline::execute(
        toolchain,
        pipeline::Request {
            url: &prepared.url,
            platform: request.platform,
            track_id: &request.track_id,
            range: prepared.range,
            container: request.container,
            precision: request.precision,
            transcode_av1: request.transcode_av1,
            title: &request.title,
            cookies: &cookies,
        },
        prepared.output_path.clone(),
        cancel,
        |progress| match progress {
            pipeline::Progress::Resolving => jobs::emit(
                app,
                JobEvent::new(job_id, Stage::Resolving, "Resolving stream URLs…"),
            ),

            pipeline::Progress::Planning => jobs::emit(
                app,
                JobEvent::new(job_id, Stage::Planning, "Analysing keyframes…"),
            ),

            pipeline::Progress::Planned {
                note: n,
                lossless: l,
                duration,
                expected_frames,
            } => {
                note = n;
                lossless = l;
                total = duration.max(0.001);
                total_frames = expected_frames.filter(|f| *f > 0.0);
                jobs::emit(
                    app,
                    JobEvent::new(job_id, Stage::Rendering, note.clone())
                        .percent(0.0)
                        .lossless(lossless),
                );
            }

            pipeline::Progress::Rendering(tick) => {
                if last_emit.elapsed() < PROGRESS_INTERVAL {
                    return;
                }
                last_emit = Instant::now();

                // Frames beat the muxer position: with video re-encoded and
                // audio copied, the copied stream races ahead and `out_time`
                // overshoots the real length by roughly half.
                let done = match (tick.frames, total_frames) {
                    (Some(done_frames), Some(all)) => (done_frames as f64 / all).clamp(0.0, 1.0),
                    _ => (tick.out_seconds / total).clamp(0.0, 1.0),
                };

                // ETA from the fraction actually completed, in output seconds.
                let remaining = (total * (1.0 - done)).max(0.0);
                let eta = tick.speed.filter(|s| *s > 0.01).map(|s| remaining / s);

                jobs::emit(
                    app,
                    JobEvent::new(job_id, Stage::Rendering, note.clone())
                        .percent(done * 100.0)
                        .speed(tick.speed.map(|s| format!("{s:.1}× realtime")))
                        .eta(eta)
                        .bytes(tick.bytes)
                        .lossless(lossless),
                );
            }

            pipeline::Progress::Retrying { attempt, message } => {
                // Show which attempt this is, so a slow retry does not look
                // like the job has silently stalled.
                jobs::emit(
                    app,
                    JobEvent::new(
                        job_id,
                        Stage::Resolving,
                        format!(
                            "{message} (attempt {attempt} of {})",
                            pipeline::MAX_ATTEMPTS
                        ),
                    ),
                );
            }

            pipeline::Progress::Verifying => jobs::emit(
                app,
                JobEvent::new(job_id, Stage::Finalizing, "Verifying output…").percent(99.0),
            ),
        },
    )
    .await
}

/// Cancel a running job. Returns `false` if it had already finished.
#[tauri::command]
pub fn cancel_job(registry: State<'_, JobRegistry>, job_id: String) -> bool {
    registry.cancel(&job_id)
}

// ---------------------------------------------------------------------------
// Transcription
// ---------------------------------------------------------------------------

/// Which speech tiers are usable right now.
///
/// The models are large enough that a user may reasonably install only some of
/// them, so the UI asks rather than assuming, and offers only what will work.
#[tauri::command]
pub fn transcription_tiers(app: AppHandle) -> Vec<TierStatus> {
    let installed = binaries::installed_models(&app);
    let have_binary = binaries::locate_whisper(&app).is_some();
    let dropbox = binaries::model_dropbox(&app).map(|p| p.to_string_lossy().into_owned());

    [Accuracy::Fast, Accuracy::Normal, Accuracy::Accurate]
        .into_iter()
        .map(|accuracy| {
            let file = format!("{}.bin", accuracy.model_stem());
            TierStatus {
                id: accuracy,
                label: accuracy.label().to_string(),
                model: file.clone(),
                available: have_binary && installed.iter().any(|m| m == &file),
                realtime_factor: accuracy.realtime_factor(),
                install_dir: dropbox.clone(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TierStatus {
    pub id: Accuracy,
    pub label: String,
    pub model: String,
    pub available: bool,
    pub realtime_factor: f64,
    /// Where to put this model if it is missing. Same for every tier, but
    /// carried per-tier so the UI can show it against the one that needs it.
    pub install_dir: Option<String>,
}

/// Start a transcription job. Returns its id immediately; progress arrives on
/// the same channel as slice renders.
#[tauri::command]
pub async fn start_transcription(
    app: AppHandle,
    registry: State<'_, JobRegistry>,
    request: transcribe::TranscribeRequest,
) -> Result<String> {
    let input = PathBuf::from(&request.input_path);
    if !input.is_file() {
        return Err(AppError::other(format!(
            "That file no longer exists: {}",
            request.input_path
        )));
    }

    // Fail here, synchronously, rather than one event into a job the user is
    // already watching.
    let toolchain = binaries::resolve(&app)?;
    binaries::require_whisper(&app)?;
    transcribe::model_path(&app, request.accuracy)?;

    let output_path = PathBuf::from(&request.output_path);
    let parent = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| AppError::other("Output path has no folder component"))?;
    if !parent.is_dir() {
        std::fs::create_dir_all(parent)?;
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let cancel = registry.register(&job_id);

    // Duration turns whisper's segment timestamps into a percentage. A file we
    // cannot probe still transcribes; it just reports an indeterminate bar.
    let duration_ms = probe::local_duration(&toolchain, &input, &cancel)
        .await
        .map(|s| (s * 1000.0) as i64)
        .unwrap_or(0);

    let task_app = app.clone();
    let task_id = job_id.clone();

    tauri::async_runtime::spawn(async move {
        let mut last_emit = Instant::now() - PROGRESS_INTERVAL;

        let result = transcribe::run(
            &task_app,
            &toolchain,
            &request,
            duration_ms,
            &cancel,
            |fraction, step| {
                // whisper emits a line per segment, which on dense speech is
                // far faster than the UI needs.
                if last_emit.elapsed() < PROGRESS_INTERVAL && fraction < 1.0 {
                    return;
                }
                last_emit = Instant::now();
                jobs::emit(
                    &task_app,
                    JobEvent::new(&task_id, Stage::Rendering, step).percent(fraction * 100.0),
                );
            },
        )
        .await;

        match result {
            Ok(summary) => {
                let message = format!(
                    "{} captions from {} words",
                    summary.captions, summary.words
                );
                jobs::emit(
                    &task_app,
                    JobEvent::new(&task_id, Stage::Done, message)
                        .percent(100.0)
                        .output(summary.output_path.clone()),
                );
                // The summary rides its own event so the UI can show a preview
                // without reading the file back off disk.
                let _ = task_app.emit("mk://transcript-ready", &summary);
            }
            Err(error) if error.is_cancelled() => {
                jobs::emit(
                    &task_app,
                    JobEvent::new(&task_id, Stage::Cancelled, "Cancelled"),
                );
            }
            Err(error) => jobs::emit(&task_app, JobEvent::failure(&task_id, &error)),
        }

        task_app.state::<JobRegistry>().finish(&task_id);
    });

    Ok(job_id)
}

/// Write raw bytes the frontend produced to a path the user chose.
///
/// Used by the Remove Background tool, whose output is a PNG encoded in the
/// webview rather than by a sidecar. The payload arrives base64-encoded because
/// Tauri's IPC serialises a `Vec<u8>` as a JSON array of numbers — a 6 MB image
/// would become a ~30 MB string of digits and take seconds to parse.
///
/// The path is not scoped: it comes from the system save dialog, so the user has
/// already named the destination themselves.
#[tauri::command]
pub fn write_file(path: String, contents_base64: String) -> Result<()> {
    let bytes = BASE64
        .decode(contents_base64.as_bytes())
        .map_err(|e| AppError::other(format!("The image data was malformed: {e}")))?;

    let target = PathBuf::from(&path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    std::fs::write(&target, &bytes)?;
    Ok(())
}

/// Show the finished file in the OS file manager.
#[tauri::command]
pub fn reveal_in_folder(app: AppHandle, path: String) -> Result<()> {
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| AppError::other(format!("Could not open the file manager: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_bytes_survive_the_base64_round_trip() {
        let dir = std::env::temp_dir().join(format!("mk-write-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("cutout.png");

        // A PNG header, including the bytes that make naive text handling fail.
        let payload: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];

        write_file(
            path.to_string_lossy().into_owned(),
            BASE64.encode(payload),
        )
        .expect("write should succeed");

        assert_eq!(std::fs::read(&path).unwrap(), payload);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_payloads_are_rejected_rather_than_written() {
        let path = std::env::temp_dir().join("mk-write-never.png");

        let error = write_file(path.to_string_lossy().into_owned(), "not base64!!".into())
            .expect_err("malformed base64 must not reach the filesystem");

        assert_eq!(error.kind(), "other");
        assert!(!path.exists());
    }

    #[test]
    fn filenames_survive_hostile_titles() {
        let name = suggest_filename(
            r#"Why C:\ is <weird> | a "deep" dive?*"#.into(),
            83.0,
            165.0,
            "mp4".into(),
        );

        assert!(!name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']));
        assert!(name.ends_with(".mp4"));
        assert!(name.contains("00-01-23_000"));
        assert!(name.contains("00-02-45_000"));
    }

    #[test]
    fn empty_or_symbol_only_titles_get_a_fallback_stem() {
        assert!(suggest_filename("".into(), 0.0, 1.0, "mkv".into()).starts_with("slice"));
        assert_eq!(sanitize_stem("   "), "slice");
        // A title made only of illegal characters still yields a usable stem.
        assert_eq!(sanitize_stem("///"), "-");
    }

    #[test]
    fn trailing_dots_are_stripped_for_windows() {
        // "name..." would silently become "name" on NTFS, changing our target.
        assert_eq!(sanitize_stem("report..."), "report");
        assert_eq!(sanitize_stem("v1.2 final."), "v1.2 final");
    }

    #[test]
    fn long_titles_are_truncated_to_a_safe_length() {
        let long = "x".repeat(400);
        let stem = sanitize_stem(&long);
        assert_eq!(stem.chars().count(), 120);
    }

    #[test]
    fn separator_runs_are_collapsed() {
        assert_eq!(sanitize_stem("a///b"), "a-b");
        assert_eq!(sanitize_stem("a    b"), "a b");
    }
}
