//! Network-backed smoke test for the whole backend media path.
//!
//! Ignored by default — it hits YouTube, so it is slow and fails whenever the
//! network does, which is not something a unit-test run should depend on. Run
//! it deliberately after touching `youtube.rs`, `render.rs`, or `probe.rs`, or
//! when a YouTube-side change is suspected:
//!
//! ```text
//! cargo test --  --ignored --nocapture
//! ```
//!
//! It exercises the real pipeline end to end: resolve → parse → plan → render
//! → verify, and asserts the rendered slice is actually the requested length.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::pipeline::{self, Progress};
use crate::platform::Platform;
use crate::probe;
use crate::render::{Container, Precision};
use crate::timecode;
use crate::youtube::{self, CookieOptions};

/// Blender Foundation's Big Buck Bunny — Creative Commons, long enough to slice
/// from the middle, and unlikely to be taken down.
const TEST_URL: &str = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";

const SLICE_START: f64 = 90.0;
const SLICE_END: f64 = 100.0;

#[cfg(windows)]
const EXE: &str = ".exe";
#[cfg(not(windows))]
const EXE: &str = "";

/// Resolve the vendored sidecars without needing a Tauri `AppHandle`.
fn dev_toolchain() -> Option<Toolchain> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    let triple = env!("MK_TARGET_TRIPLE");

    let find = |stem: &str| -> Option<PathBuf> {
        let suffixed = dir.join(format!("{stem}-{triple}{EXE}"));
        if suffixed.is_file() {
            return Some(suffixed);
        }
        let plain = dir.join(format!("{stem}{EXE}"));
        if plain.is_file() {
            return Some(plain);
        }
        which::which(stem).ok()
    };

    Some(Toolchain {
        ytdlp: find("yt-dlp")?,
        ffmpeg: find("ffmpeg")?,
        ffprobe: find("ffprobe")?,
    })
}

#[tokio::test]
#[ignore = "hits the network; run with --ignored"]
async fn renders_a_real_slice_end_to_end() {
    let Some(toolchain) = dev_toolchain() else {
        panic!("sidecars not vendored — run `npm run binaries` first");
    };
    let cancel = CancellationToken::new();

    // --- resolve -----------------------------------------------------------
    let url = youtube::normalize_url(TEST_URL).expect("URL should normalise");
    let document = youtube::dump_json(
        &toolchain,
        &url,
        Platform::Youtube,
        &CookieOptions::default(),
        &cancel,
    )
    .await
    .expect("yt-dlp should return metadata");

    let info = youtube::parse_info(&document).expect("metadata should parse");
    println!("title:    {}", info.title);
    println!("duration: {}", timecode::format_ts(info.duration_seconds));
    println!("tracks:   {}", info.tracks.len());

    assert!(!info.tracks.is_empty(), "expected at least one quality track");
    assert!(info.duration_seconds > SLICE_END, "video is shorter than the test slice");
    assert!(!info.is_live);

    // Prefer a track that can stream-copy into MP4, matching the app's default.
    let track = info
        .tracks
        .iter()
        .find(|t| t.mp4_ready && !t.audio_only)
        .or_else(|| info.tracks.iter().find(|t| !t.audio_only))
        .expect("expected a video track");
    println!("track:    {} — {} ({})", track.label, track.detail, track.id);

    let streams = youtube::resolve_streams(&document, &track.id).expect("streams should resolve");
    assert!(streams.video.is_some(), "expected a video stream");
    assert!(streams.audio.is_some(), "expected an audio stream");

    let range = timecode::validate(SLICE_START, SLICE_END, info.duration_seconds)
        .expect("range should validate");

    // --- run the real pipeline ---------------------------------------------
    // Deliberately `pipeline::execute` rather than its individual steps: this
    // is the exact code path `start_slice` drives, retry behaviour included.
    let destination = std::env::temp_dir().join("mk-smoke-slice.mp4");
    let _ = std::fs::remove_file(&destination);

    let mut ticks = 0usize;
    let mut max_frames = 0u64;
    let mut planned_duration = 0.0f64;
    let mut planned_frames: Option<f64> = None;
    let mut retries = 0u32;

    let rendered = pipeline::execute(
        &toolchain,
        pipeline::Request {
            url: &url,
            platform: Platform::Youtube,
            track_id: &track.id,
            range,
            container: Container::Mp4,
            precision: Precision::Auto,
            transcode_av1: false,
            title: &info.title,
            cookies: &CookieOptions::default(),
        },
        destination.clone(),
        &cancel,
        |progress| match progress {
            Progress::Planned {
                note,
                lossless,
                duration,
                expected_frames,
            } => {
                planned_duration = duration;
                planned_frames = expected_frames;
                println!(
                    "plan:     {note} (lossless={lossless}, {duration:.3}s, \
                     frames={expected_frames:?})"
                );
            }
            Progress::Rendering(tick) => {
                ticks += 1;
                max_frames = max_frames.max(tick.frames.unwrap_or(0));
            }
            Progress::Retrying { attempt, message } => {
                retries += 1;
                println!("retry {attempt}: {message}");
            }
            _ => {}
        },
    )
    .await
    .expect("pipeline should succeed");

    println!("progress: {ticks} ticks, {max_frames} frames, {retries} retries");
    assert!(ticks > 0, "FFmpeg progress parsing produced no ticks");
    assert!(planned_duration > 0.0, "no plan was reported");

    // The progress denominator must actually match what FFmpeg produces —
    // otherwise the bar either stalls short of 100% or pins there early.
    let total_frames = planned_frames.expect("a video slice should predict its frame count");
    let ratio = max_frames as f64 / total_frames;
    println!("          final progress: {:.1}%", ratio * 100.0);
    assert!(
        (0.95..=1.05).contains(&ratio),
        "progress ended at {:.1}% — the frame estimate ({total_frames:.0}) does not match \
         the {max_frames} frames FFmpeg wrote",
        ratio * 100.0
    );

    // --- verify ------------------------------------------------------------
    assert_eq!(rendered, destination, "slice must land on the requested path");

    let actual = probe::local_duration(&toolchain, &rendered, &cancel)
        .await
        .expect("rendered file should have a readable duration");
    println!("output:   {actual:.3}s at {}", rendered.display());

    assert!(
        (actual - planned_duration).abs() < 0.5,
        "expected ~{planned_duration:.2}s, got {actual:.2}s"
    );
    assert!(
        std::fs::metadata(&rendered).unwrap().len() > 100_000,
        "a 10s 1080p slice should be substantially larger than this"
    );

    // No staging file may survive a successful run.
    let leftover = destination.with_extension("mp4.mk-part");
    assert!(!leftover.exists(), "staging file was not cleaned up");

    let _ = std::fs::remove_file(&rendered);
}
