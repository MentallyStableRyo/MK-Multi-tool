//! FFprobe queries used to decide *how* to cut.
//!
//! The important one is [`keyframe_at_or_before`]. Stream-copying a slice is
//! only lossless-and-instant if the cut lands exactly on a keyframe; otherwise
//! the first GOP decodes to garbage. Rather than guessing, we read the actual
//! keyframe timestamps around the requested start — over HTTP this costs a
//! single ranged request for a few seconds of video.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::Result;
use crate::ffargs;
use crate::proc;
use crate::youtube::StreamSpec;

/// How far back from the requested start to look for a keyframe. YouTube uses
/// 2–5s GOPs; 20s covers even pathological encodes.
const LOOKBACK_SECONDS: f64 = 20.0;

/// Where a probe should read from.
pub enum Source<'a> {
    Remote(&'a StreamSpec),
    Local(&'a Path),
}

fn input_args(source: &Source<'_>) -> Vec<String> {
    match source {
        Source::Remote(spec) => ffargs::remote_input(spec, None),
        Source::Local(path) => ffargs::local_input(path, None),
    }
}

/// Keyframe presentation timestamps in `[around - LOOKBACK, around + 5s]`.
///
/// Returns an empty vec rather than an error when the probe yields nothing —
/// callers treat "unknown" as "fall back to re-encoding", which is always safe.
pub async fn keyframes_near(
    tc: &Toolchain,
    source: Source<'_>,
    around: f64,
    cancel: &CancellationToken,
) -> Result<Vec<f64>> {
    let from = (around - LOOKBACK_SECONDS).max(0.0);
    let window = around - from + 5.0;

    let mut args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        "v:0".into(),
        // Decode nothing but keyframes — this is what makes the probe cheap.
        "-skip_frame".into(),
        "nokey".into(),
        "-show_entries".into(),
        "frame=pts_time".into(),
        "-read_intervals".into(),
        format!("{from:.3}%+{window:.3}"),
        "-of".into(),
        "csv=p=0".into(),
    ];
    args.extend(input_args(&source));

    let mut cmd = proc::command(&tc.ffprobe);
    cmd.args(&args);

    let out = proc::run_capture(cmd, cancel).await?;
    if !out.ok() {
        return Ok(Vec::new());
    }

    let mut times: Vec<f64> = out
        .stdout
        .lines()
        // csv=p=0 still emits a trailing comma for empty columns.
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
        .filter(|t| t.is_finite() && *t >= 0.0)
        .collect();

    times.sort_by(f64::total_cmp);
    Ok(times)
}

/// The latest keyframe at or before `target`, if one was found.
pub async fn keyframe_at_or_before(
    tc: &Toolchain,
    source: Source<'_>,
    target: f64,
    cancel: &CancellationToken,
) -> Option<f64> {
    let times = keyframes_near(tc, source, target, cancel).await.ok()?;
    // Times are sorted ascending, so the last one at or before the target is the
    // containing keyframe. The 1 ms slack absorbs float noise: a keyframe
    // reported at 89.9997 for a request of 90.0 is the same frame.
    times.into_iter().rfind(|t| *t <= target + 0.001)
}

/// Codec names read out of the container, as `(video, audio)`.
///
/// Only needed when an extractor did not report them — see
/// [`crate::youtube`] on yt-dlp's three-state codec fields. Assuming the worst
/// instead would re-encode every X clip, which are plain H.264/AAC MP4s that
/// stream-copy perfectly; assuming the *best* would produce files the container
/// cannot legally hold. So we measure, exactly as the keyframe probe does.
///
/// Reads only the header, and returns `(None, None)` on any failure — callers
/// treat unknown as "re-encode", which is always safe.
pub async fn codecs(
    tc: &Toolchain,
    source: Source<'_>,
    cancel: &CancellationToken,
) -> (Option<String>, Option<String>) {
    let mut args: Vec<String> = [
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type,codec_name",
        "-of",
        "default=noprint_wrappers=1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(input_args(&source));

    let mut cmd = proc::command(&tc.ffprobe);
    cmd.args(&args);

    let Ok(out) = proc::run_capture(cmd, cancel).await else {
        return (None, None);
    };
    if !out.ok() {
        return (None, None);
    }

    // ffprobe emits `key=value` lines per stream, and does not promise the
    // order the keys were requested in — so carry the last name seen until its
    // type identifies it.
    let (mut video, mut audio) = (None, None);
    let mut name: Option<String> = None;
    for line in out.stdout.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "codec_name" => name = Some(value.trim().to_string()),
            "codec_type" => match value.trim() {
                "video" if video.is_none() => video = name.take(),
                "audio" if audio.is_none() => audio = name.take(),
                _ => name = None,
            },
            _ => {}
        }
    }
    (video, audio)
}

/// Duration in seconds of a local media file, used to verify a finished render.
pub async fn local_duration(
    tc: &Toolchain,
    path: &Path,
    cancel: &CancellationToken,
) -> Option<f64> {
    let mut args: Vec<String> = ["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(input_args(&Source::Local(path)));

    let mut cmd = proc::command(&tc.ffprobe);
    cmd.args(&args);

    let out = proc::run_capture(cmd, cancel).await.ok()?;
    if !out.ok() {
        return None;
    }
    out.stdout
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .find(|d| d.is_finite() && *d > 0.0)
}
