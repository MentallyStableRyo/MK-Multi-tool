//! The FFmpeg slicing engine.
//!
//! # Strategy
//!
//! A slice is rendered in a **single FFmpeg pass that reads directly from
//! YouTube's CDN**. `-ss` placed *before* `-i` makes FFmpeg issue HTTP range
//! requests, so cutting 00:90:00–00:92:00 out of a three-hour video transfers
//! roughly two minutes of media, not three hours. There is no "download the
//! whole file, then trim" step.
//!
//! # Copy vs. re-encode
//!
//! Stream-copying is lossless and effectively instant, but only valid when the
//! cut lands on a keyframe *and* the source codec is legal in the target
//! container. [`plan_cut`] probes the real keyframe layout (see [`crate::probe`])
//! and decides:
//!
//! | Situation                                   | Video handling                    |
//! |---------------------------------------------|-----------------------------------|
//! | Keyframe at the cut, codec fits container   | `-c:v copy` — lossless, instant   |
//! | Cut between keyframes                       | Hardware-accelerated re-encode    |
//! | Codec illegal in container (VP9 → MP4)      | Hardware-accelerated re-encode    |
//!
//! Audio is copied whenever the codec fits, and transcoded otherwise; audio
//! frames are ~20 ms so seeking is accurate either way.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::{AppError, Result};
use crate::ffargs;
use crate::probe;
use crate::proc;
use crate::timecode::{format_ts, Range};
use crate::youtube::{self, StreamSet, StreamSpec};

// ---------------------------------------------------------------------------
// Output configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// H.264 + AAC. The default: plays everywhere.
    Mp4,
    /// Accepts every codec YouTube serves, so it always stream-copies.
    Mkv,
    /// VP9/AV1 + Opus.
    Webm,
    /// Audio only, AAC in an MP4 container.
    M4a,
    /// Audio only, transcoded to MP3 for maximum compatibility.
    Mp3,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
            Self::M4a => "m4a",
            Self::Mp3 => "mp3",
        }
    }

    pub fn is_audio_only(self) -> bool {
        matches!(self, Self::M4a | Self::Mp3)
    }

    /// FFmpeg muxer name, passed explicitly via `-f`.
    ///
    /// Never rely on FFmpeg inferring the format from the output extension: we
    /// render to a `.mk-part` staging file, which has no extension it
    /// recognises, and it aborts with "Unable to choose an output format".
    fn muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "matroska",
            Self::Webm => "webm",
            // The iPod muxer is the MP4 family's audio-only profile — it is what
            // produces a conformant .m4a rather than a video-less .mp4.
            Self::M4a => "ipod",
            Self::Mp3 => "mp3",
        }
    }

    /// Can this video codec family be muxed as-is?
    fn accepts_video(self, family: &str) -> bool {
        match self {
            Self::Mp4 => matches!(family, "h264" | "av1"),
            // Matroska is codec-agnostic.
            Self::Mkv => true,
            Self::Webm => matches!(family, "vp9" | "vp8" | "av1"),
            Self::M4a | Self::Mp3 => false,
        }
    }

    /// Can this audio codec family be muxed as-is?
    fn accepts_audio(self, family: &str) -> bool {
        match self {
            // Opus-in-MP4 is legal but poorly supported by players; transcode it.
            Self::Mp4 => matches!(family, "aac"),
            Self::Mkv => true,
            Self::Webm => matches!(family, "opus" | "vorbis"),
            Self::M4a => matches!(family, "aac"),
            Self::Mp3 => matches!(family, "mp3"),
        }
    }
}

/// How exact the cut boundary must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    /// Copy when the cut happens to be keyframe-aligned, re-encode otherwise.
    #[default]
    Auto,
    /// Always frame-accurate; re-encodes the video.
    Exact,
    /// Never re-encode; snaps the start back to the preceding keyframe.
    Fast,
}

// ---------------------------------------------------------------------------
// Encoder discovery
// ---------------------------------------------------------------------------

static ENCODERS: OnceCell<Vec<String>> = OnceCell::const_new();

/// Everything `ffmpeg -encoders` reports, cached for the process lifetime.
async fn available_encoders(tc: &Toolchain) -> &'static Vec<String> {
    ENCODERS
        .get_or_init(|| async {
            let mut cmd = proc::command(&tc.ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-encoders"]);

            let Ok(out) = proc::run_capture(cmd, &CancellationToken::new()).await else {
                return Vec::new();
            };

            // Lines look like: " V....D h264_nvenc   NVIDIA NVENC H.264 encoder"
            out.stdout
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
                .collect()
        })
        .await
}

/// H.264 encoders in descending preference for this platform. The software
/// encoder is always last so the list is never empty.
fn h264_preference() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["h264_videotoolbox", "libx264"]
    }
    #[cfg(target_os = "windows")]
    {
        &["h264_nvenc", "h264_qsv", "h264_amf", "libx264"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        &["h264_nvenc", "h264_qsv", "libx264"]
    }
}

/// Ordered list of H.264 encoders this FFmpeg build can actually run.
async fn h264_candidates(tc: &Toolchain) -> Vec<String> {
    let available = available_encoders(tc).await;
    let mut list: Vec<String> = h264_preference()
        .iter()
        .filter(|name| available.iter().any(|a| a == *name))
        .map(|s| (*s).to_string())
        .collect();

    // If the probe failed entirely, libx264 is a safe assumption.
    if list.is_empty() {
        list.push("libx264".to_string());
    }
    list
}

/// Quality/speed settings per encoder. Values target "visually transparent at
/// interactive speed" rather than archival quality.
fn h264_options(encoder: &str) -> Vec<&'static str> {
    match encoder {
        "h264_nvenc" => vec!["-preset", "p4", "-rc", "vbr", "-cq", "21", "-b:v", "0"],
        "h264_qsv" => vec!["-preset", "veryfast", "-global_quality", "23"],
        "h264_amf" => vec!["-quality", "balanced", "-rc", "cqp", "-qp_i", "22", "-qp_p", "24"],
        "h264_videotoolbox" => vec!["-q:v", "58", "-allow_sw", "1"],
        _ => vec!["-preset", "veryfast", "-crf", "21"],
    }
}

fn is_hardware(encoder: &str) -> bool {
    encoder != "libx264"
}

// ---------------------------------------------------------------------------
// Cut planning
// ---------------------------------------------------------------------------

/// Lookback used when no keyframe could be probed but we still must re-encode:
/// FFmpeg needs to start decoding at *some* earlier keyframe.
const BLIND_LOOKBACK: f64 = 8.0;

/// How close a keyframe must be to the requested start to count as landing on
/// it: half a frame at the source's own frame rate, so a 24 fps source is not
/// held to a 60 fps tolerance. Falls back to half a frame at 60 fps when the
/// rate is unknown.
fn keyframe_tolerance(fps: f64) -> f64 {
    if fps > 1.0 {
        0.5 / fps
    } else {
        1.0 / 120.0
    }
}

#[derive(Debug, Clone)]
pub struct CutPlan {
    /// `-ss` before `-i`: where FFmpeg starts reading.
    pub seek_in: f64,
    /// `-ss` after the inputs: frames decoded then discarded. Always 0 when the
    /// video is copied, because dropping copied packets breaks the first GOP.
    pub trim_out: f64,
    /// `-t`: output duration.
    pub duration: f64,
    pub copy_video: bool,
    pub copy_audio: bool,
    /// Video frames the finished slice should contain, when the source frame
    /// rate is known. This is the denominator for progress — see
    /// [`RenderTick::out_seconds`] for why the muxer position is not.
    pub expected_frames: Option<f64>,
    /// One line describing the chosen approach, shown in the UI. Together with
    /// `seek_in` this is the plan's whole observable surface — where the probed
    /// keyframe landed is implied by `seek_in` on every path that used one.
    pub note: String,
}

impl CutPlan {
    pub fn is_lossless(&self) -> bool {
        self.copy_video && self.copy_audio
    }
}

/// Frames a slice of `duration` should contain at `fps`, if `fps` is known.
fn expected_frames(duration: f64, fps: f64) -> Option<f64> {
    (fps > 0.0 && duration > 0.0).then_some(duration * fps)
}

/// Codec families for the resolved streams, measured when they were not
/// reported.
///
/// Extractors outside YouTube routinely omit codec names. An unnamed codec
/// collapses to `"other"`, which no container accepts, so it would force a
/// re-encode of sources — X's progressive MP4s above all — that would have
/// copied losslessly. One header read settles it; an extractor that *did* name
/// the codecs is trusted and costs nothing.
async fn codec_families(
    tc: &Toolchain,
    streams: &StreamSet,
    cancel: &CancellationToken,
) -> (&'static str, &'static str) {
    let unnamed = |spec: &Option<StreamSpec>| {
        spec.as_ref().is_some_and(|s| s.codec.trim().is_empty())
    };
    let video_unnamed = unnamed(&streams.video);
    let audio_unnamed = unnamed(&streams.audio);

    let family = |streams: &StreamSet, probed_v: Option<&str>, probed_a: Option<&str>| {
        let v = streams.video.as_ref().map_or("none", |s| {
            let name = if s.codec.trim().is_empty() { probed_v.unwrap_or("") } else { &s.codec };
            youtube::video_family(name)
        });
        let a = streams.audio.as_ref().map_or("none", |s| {
            let name = if s.codec.trim().is_empty() { probed_a.unwrap_or("") } else { &s.codec };
            youtube::audio_family(name)
        });
        (v, a)
    };

    if !video_unnamed && !audio_unnamed {
        return family(streams, None, None);
    }

    // A progressive source is one input carrying both streams, so a single
    // probe answers for both.
    let mut probed_video = None;
    let mut probed_audio = None;

    if let Some(spec) = streams.video.as_ref().filter(|_| video_unnamed) {
        let (v, a) = probe::codecs(tc, probe::Source::Remote(spec), cancel).await;
        probed_video = v;
        if streams.progressive {
            probed_audio = a;
        }
    }
    if probed_audio.is_none() && audio_unnamed {
        if let Some(spec) = streams.audio.as_ref() {
            let (_, a) = probe::codecs(tc, probe::Source::Remote(spec), cancel).await;
            probed_audio = a;
        }
    }

    family(streams, probed_video.as_deref(), probed_audio.as_deref())
}

/// Decide how to cut, probing the source's keyframe layout when it matters.
///
/// Codec families are read from the freshly resolved streams rather than from
/// anything the caller passed in: YouTube can change which format ids map to
/// which codecs between the analysis and the render, and a wrong guess here
/// produces a file the container cannot legally hold.
/// `transcode_av1` forces an AV1 source down to H.264. AV1 is legal in MP4 and
/// MKV, so it would otherwise be stream-copied — but older TVs, phones, and
/// editors cannot decode it. This trades the lossless copy for playability.
pub async fn plan_cut(
    tc: &Toolchain,
    streams: &StreamSet,
    range: Range,
    container: Container,
    precision: Precision,
    transcode_av1: bool,
    cancel: &CancellationToken,
) -> CutPlan {
    let (video_family, audio_family) = codec_families(tc, streams, cancel).await;

    let wants_video = !container.is_audio_only() && streams.video.is_some();
    let copy_audio = container.accepts_audio(audio_family);

    if !wants_video {
        // Audio-only: frame granularity is ~20 ms, so an input seek is accurate.
        return CutPlan {
            seek_in: range.start,
            trim_out: 0.0,
            duration: range.duration(),
            copy_video: false,
            copy_audio,
            // No video stream in the output, so the muxer position is the only
            // signal — and with a single stream it is accurate.
            expected_frames: None,
            note: if copy_audio {
                "Audio stream copy — lossless".into()
            } else {
                format!("Audio transcode to {}", container.extension())
            },
        };
    }

    // Only meaningful where H.264 is actually legal in the target container —
    // asking for H.264 inside WebM would produce an unplayable file, so the
    // request is ignored there rather than honoured into a broken output.
    let force_h264 = transcode_av1 && video_family == "av1" && container.accepts_video("h264");
    let codec_fits = container.accepts_video(video_family) && !force_h264;

    // Probing costs one small ranged request; skip it when the answer cannot
    // change the outcome (Exact always re-encodes; an unfit codec always does).
    let keyframe = if precision == Precision::Exact || !codec_fits {
        None
    } else if let Some(video) = streams.video.as_ref() {
        probe::keyframe_at_or_before(tc, probe::Source::Remote(video), range.start, cancel).await
    } else {
        None
    };

    let fps = streams.video.as_ref().map_or(0.0, |s| s.fps);
    let tolerance = keyframe_tolerance(fps);
    let aligned = keyframe.is_some_and(|kf| (range.start - kf).abs() <= tolerance);

    let copy_video = match precision {
        Precision::Exact => false,
        Precision::Fast => codec_fits,
        Precision::Auto => codec_fits && aligned,
    };

    if copy_video {
        // Start at the keyframe. In Auto that is within tolerance of the
        // request; in Fast it may be up to a GOP early, which is the documented
        // trade-off for a zero-CPU cut.
        let seek_in = keyframe.unwrap_or(range.start);
        let duration = (range.end - seek_in).max(0.0);
        let note = if aligned {
            "Stream copy — lossless, keyframe-aligned".to_string()
        } else {
            format!(
                "Stream copy — starts at the keyframe {} ({:.2}s early)",
                format_ts(seek_in),
                range.start - seek_in
            )
        };
        return CutPlan {
            seek_in,
            trim_out: 0.0,
            duration,
            copy_video: true,
            copy_audio,
            expected_frames: expected_frames(duration, fps),
            note,
        };
    }

    // Re-encode path: begin decoding at a keyframe at or before the cut, then
    // discard the lead-in with an output-side seek.
    let seek_in = keyframe.unwrap_or((range.start - BLIND_LOOKBACK).max(0.0));
    let note = if force_h264 {
        // Checked before `!codec_fits`, which is also true here — otherwise this
        // would report the misleading "AV1 is not supported in MP4".
        "Re-encoding — converting AV1 to H.264 for wider playback support".to_string()
    } else if !codec_fits {
        format!(
            "Re-encoding — {} is not supported in {}",
            video_family.to_uppercase(),
            container.extension().to_uppercase()
        )
    } else {
        "Re-encoding — the cut falls between keyframes".to_string()
    };

    CutPlan {
        seek_in,
        trim_out: (range.start - seek_in).max(0.0),
        duration: range.duration(),
        copy_video: false,
        copy_audio,
        expected_frames: expected_frames(range.duration(), fps),
        note,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Live state parsed out of FFmpeg's `-progress` stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderTick {
    /// Muxer position in seconds.
    ///
    /// Beware: this is the furthest-muxed packet across *all* streams. When
    /// video is re-encoded while audio is copied, the copied audio races ahead
    /// and this overshoots the real output length by ~50%. Use [`Self::frames`]
    /// for progress whenever a video stream is present.
    pub out_seconds: f64,
    /// Video frames written so far. Excludes frames decoded and discarded by an
    /// output-side seek, so it tracks real progress linearly.
    pub frames: Option<u64>,
    /// Encode rate relative to realtime, e.g. `12.4` for "12.4x".
    pub speed: Option<f64>,
    /// Bytes written to the output file.
    pub bytes: Option<u64>,
}

/// Parse one `key=value` line from `-progress pipe:1` into `tick`.
///
/// Returns `true` if the line advanced the output position, so callers only
/// emit an event when something actually moved.
fn apply_progress_line(line: &str, tick: &mut RenderTick) -> bool {
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let value = value.trim();

    match key.trim() {
        // Despite the name, out_time_ms is microseconds in FFmpeg's output.
        "out_time_us" | "out_time_ms" => {
            if let Ok(us) = value.parse::<i64>() {
                tick.out_seconds = (us.max(0) as f64) / 1_000_000.0;
                return true;
            }
        }
        // FFmpeg emits `frame` before `out_time_us` within one progress block,
        // so the count is already current when the block triggers an event.
        "frame" => {
            if let Ok(n) = value.parse::<u64>() {
                tick.frames = Some(n);
            }
        }
        "total_size" => {
            if let Ok(bytes) = value.parse::<u64>() {
                tick.bytes = Some(bytes);
            }
        }
        "speed" => {
            tick.speed = value.trim_end_matches('x').trim().parse::<f64>().ok();
        }
        _ => {}
    }
    false
}

/// Everything one render attempt needs, apart from the toolchain and the
/// encoder being tried. Grouped so the call sites stay readable.
#[derive(Debug, Clone, Copy)]
pub struct RenderJob<'a> {
    pub streams: &'a StreamSet,
    pub plan: &'a CutPlan,
    pub container: Container,
    /// Written into the container's title metadata.
    pub title: &'a str,
    /// Where FFmpeg writes. Usually a staging path, not the final destination.
    pub output: &'a Path,
}

fn output_args(
    job: &RenderJob<'_>,
    encoder: &str,
    map: (Option<usize>, Option<usize>),
) -> Vec<String> {
    let RenderJob {
        plan,
        container,
        title,
        output,
        ..
    } = *job;

    let mut args: Vec<String> = Vec::with_capacity(32);
    let (video_idx, audio_idx) = map;

    // Output-side seek: only meaningful when we are decoding.
    if plan.trim_out > 0.0 && !plan.copy_video {
        args.push("-ss".into());
        args.push(format_ts(plan.trim_out));
    }
    args.push("-t".into());
    args.push(format_ts(plan.duration));

    if let Some(i) = video_idx {
        args.push("-map".into());
        args.push(format!("{i}:v:0"));
    }
    if let Some(i) = audio_idx {
        args.push("-map".into());
        args.push(format!("{i}:a:0"));
    }

    // Video codec.
    if video_idx.is_some() {
        if plan.copy_video {
            args.push("-c:v".into());
            args.push("copy".into());
        } else if container == Container::Webm {
            args.extend(
                [
                    "-c:v", "libvpx-vp9", "-crf", "32", "-b:v", "0", "-row-mt", "1", "-deadline",
                    "good", "-cpu-used", "4", "-pix_fmt", "yuv420p",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
        } else {
            args.push("-c:v".into());
            args.push(encoder.to_string());
            args.extend(h264_options(encoder).iter().map(|s| s.to_string()));
            // Sources can be 10-bit (VP9 Profile 2, AV1). H.264 hardware
            // encoders reject that, and 10-bit MP4 breaks many players anyway.
            args.push("-pix_fmt".into());
            args.push("yuv420p".into());
        }
    }

    // Audio codec.
    if audio_idx.is_some() {
        if plan.copy_audio {
            args.push("-c:a".into());
            args.push("copy".into());
        } else {
            match container {
                Container::Webm => args.extend(
                    ["-c:a", "libopus", "-b:a", "160k"].iter().map(|s| s.to_string()),
                ),
                Container::Mp3 => args.extend(
                    ["-c:a", "libmp3lame", "-q:a", "2"].iter().map(|s| s.to_string()),
                ),
                _ => args.extend(
                    ["-c:a", "aac", "-b:a", "192k"].iter().map(|s| s.to_string()),
                ),
            }
        }
    }

    if matches!(container, Container::Mp4 | Container::M4a) {
        // Move the moov atom to the front so the file is seekable immediately.
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    // A copied stream can carry negative timestamps after an input seek; this
    // rebases them so players do not report a bogus start offset.
    args.push("-avoid_negative_ts".into());
    args.push("make_zero".into());

    // Explicit muxer — the staging filename ends in `.mk-part`, so extension
    // inference would fail.
    args.push("-f".into());
    args.push(container.muxer().to_string());

    if !title.is_empty() {
        args.push("-metadata".into());
        args.push(format!("title={title}"));
    }

    args.push(output.to_string_lossy().into_owned());
    args
}

/// Build the complete FFmpeg argument vector for one render attempt.
fn build_args(job: &RenderJob<'_>, encoder: &str) -> Vec<String> {
    let RenderJob {
        streams,
        plan,
        container,
        ..
    } = *job;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
    ];

    let wants_video = !container.is_audio_only() && streams.video.is_some();
    let mut next_index = 0usize;
    let mut video_idx = None;
    let mut audio_idx = None;

    if wants_video {
        if let Some(video) = streams.video.as_ref() {
            args.extend(ffargs::remote_input(video, Some(plan.seek_in)));
            video_idx = Some(next_index);
            next_index += 1;
        }
    }

    if let Some(audio) = streams.audio.as_ref() {
        if streams.progressive && video_idx.is_some() {
            // Same muxed input; do not open the URL twice.
            audio_idx = video_idx;
        } else {
            args.extend(ffargs::remote_input(audio, Some(plan.seek_in)));
            audio_idx = Some(next_index);
        }
    }

    args.extend(output_args(job, encoder, (video_idx, audio_idx)));
    args
}

/// Render the slice. Calls `on_tick` whenever the output position advances.
///
/// If a hardware encoder fails at runtime — a real possibility even when
/// `-encoders` lists it, e.g. a laptop dGPU that is powered down — the render
/// is retried on the next candidate, ending at libx264.
pub async fn render<F>(
    tc: &Toolchain,
    job: RenderJob<'_>,
    cancel: &CancellationToken,
    mut on_tick: F,
) -> Result<PathBuf>
where
    F: FnMut(RenderTick),
{
    let output = job.output;

    // Only the H.264 re-encode path has anything to choose between; every other
    // path has exactly one way to run, so skip the encoder probe entirely.
    let encoders = if job.plan.copy_video
        || job.container == Container::Webm
        || job.container.is_audio_only()
    {
        vec!["copy".to_string()]
    } else {
        h264_candidates(tc).await
    };

    let mut last_error: Option<AppError> = None;

    for (attempt, encoder) in encoders.iter().enumerate() {
        let args = build_args(&job, encoder);

        let mut cmd = proc::command(&tc.ffmpeg);
        cmd.args(&args);

        let mut tick = RenderTick::default();
        let result = proc::run_streaming(cmd, cancel, |line| {
            // Non-positional keys (speed, total_size) fold into `tick` and
            // return false; only a new output position is worth an event.
            if apply_progress_line(line, &mut tick) {
                on_tick(tick);
            }
        })
        .await;

        match result {
            Err(e) if e.is_cancelled() => {
                // A partially written file is worse than none.
                let _ = std::fs::remove_file(output);
                return Err(e);
            }
            Err(e) => {
                let _ = std::fs::remove_file(output);
                last_error = Some(e);
            }
            Ok(out) if out.ok() => {
                return verify_output(tc, output, cancel).await;
            }
            Ok(out) => {
                let _ = std::fs::remove_file(output);
                last_error = Some(AppError::Ffmpeg {
                    code: out.code,
                    detail: tail_of(&out.stderr),
                });

                // Only a hardware encoder is worth retrying; a bad URL or a
                // full disk will fail identically on every encoder.
                let retryable = is_hardware(encoder) && attempt + 1 < encoders.len();
                if !retryable {
                    break;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| AppError::other("FFmpeg produced no output")))
}

/// Confirm FFmpeg actually wrote playable media. A zero-byte or zero-duration
/// file with exit code 0 is rare but not impossible (e.g. a range past EOF).
async fn verify_output(tc: &Toolchain, output: &Path, cancel: &CancellationToken) -> Result<PathBuf> {
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = std::fs::remove_file(output);
        return Err(AppError::Ffmpeg {
            code: 0,
            detail: "the render produced an empty file".into(),
        });
    }

    if probe::local_duration(tc, output, cancel).await.is_none() {
        let _ = std::fs::remove_file(output);
        return Err(AppError::Ffmpeg {
            code: 0,
            detail: "the rendered file has no readable duration".into(),
        });
    }

    Ok(output.to_path_buf())
}

/// Last few stderr lines, which is where FFmpeg puts the actual cause.
fn tail_of(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(4);
    let detail = lines[start..].join(" / ");
    if detail.is_empty() {
        "no diagnostic output".to_string()
    } else {
        detail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::StreamSpec;

    fn spec(url: &str, codec: &str) -> StreamSpec {
        StreamSpec {
            url: url.to_string(),
            headers: vec![("User-Agent".into(), "test-agent".into())],
            codec: codec.to_string(),
            fps: 30.0,
        }
    }

    fn adaptive() -> StreamSet {
        StreamSet {
            video: Some(spec("https://cdn/v", "avc1.640028")),
            audio: Some(spec("https://cdn/a", "mp4a.40.2")),
            progressive: false,
        }
    }

    fn av1_source() -> StreamSet {
        StreamSet {
            video: Some(spec("https://cdn/v", "av01.0.08M.08")),
            audio: Some(spec("https://cdn/a", "mp4a.40.2")),
            progressive: false,
        }
    }

    /// Plan a cut with no network available. `Precision::Exact` skips the
    /// keyframe probe, so nothing shells out to the fake ffprobe path.
    async fn plan_offline(streams: &StreamSet, container: Container, transcode_av1: bool) -> CutPlan {
        let tc = Toolchain {
            ytdlp: PathBuf::from("yt-dlp"),
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
        };
        plan_cut(
            &tc,
            streams,
            Range { start: 90.0, end: 120.0 },
            container,
            Precision::Exact,
            transcode_av1,
            &CancellationToken::new(),
        )
        .await
    }

    fn plan(copy_video: bool, copy_audio: bool) -> CutPlan {
        CutPlan {
            seek_in: 90.0,
            trim_out: if copy_video { 0.0 } else { 1.5 },
            duration: 30.0,
            copy_video,
            copy_audio,
            expected_frames: Some(900.0),
            note: String::new(),
        }
    }

    /// Assemble the argument vector the way `render` would for one attempt.
    fn args_for(
        streams: &StreamSet,
        plan: &CutPlan,
        container: Container,
        encoder: &str,
    ) -> Vec<String> {
        let output = Path::new("/tmp/out.media");
        build_args(
            &RenderJob {
                streams,
                plan,
                container,
                title: "Clip",
                output,
            },
            encoder,
        )
    }

    #[test]
    fn container_codec_rules_match_real_muxer_limits() {
        assert!(Container::Mp4.accepts_video("h264"));
        assert!(!Container::Mp4.accepts_video("vp9"));
        assert!(Container::Mkv.accepts_video("vp9"));
        assert!(Container::Webm.accepts_video("vp9"));
        assert!(!Container::Webm.accepts_video("h264"));

        assert!(Container::Mp4.accepts_audio("aac"));
        assert!(!Container::Mp4.accepts_audio("opus"));
        assert!(Container::Webm.accepts_audio("opus"));
    }

    #[test]
    fn copy_plan_never_uses_an_output_side_seek() {
        // Dropping copied packets would leave the first GOP undecodable.
        let args = args_for(&adaptive(), &plan(true, true), Container::Mp4, "copy");

        // Every -ss must sit in the input section, i.e. before the final -i.
        let last_input = args.iter().rposition(|a| a == "-i").unwrap();
        assert!(
            !args[last_input..].iter().any(|a| a == "-ss"),
            "copy plans must not emit an output-side seek: {args:?}"
        );

        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "copy"]));
    }

    #[test]
    fn encode_plan_seeks_on_the_output_and_pins_pixel_format() {
        let args = args_for(&adaptive(), &plan(false, true), Container::Mp4, "libx264");

        assert!(args.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]));
        assert!(args.windows(2).any(|w| w == ["-crf", "21"]));
        assert!(args.windows(2).any(|w| w == ["-movflags", "+faststart"]));

        // Both inputs are seeked to the keyframe.
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 2);
    }

    #[test]
    fn progressive_sources_open_a_single_input() {
        let streams = StreamSet {
            video: Some(spec("https://cdn/prog", "avc1")),
            audio: Some(spec("https://cdn/prog", "mp4a")),
            progressive: true,
        };
        let args = args_for(&streams, &plan(true, true), Container::Mp4, "copy");

        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a:0"]));
    }

    #[test]
    fn audio_only_containers_drop_the_video_stream() {
        let args = args_for(&adaptive(), &plan(false, true), Container::M4a, "libx264");

        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
        assert!(!args.iter().any(|a| a == "-c:v"));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a:0"]));
    }

    #[test]
    fn headers_are_forwarded_to_every_remote_input() {
        let args = args_for(&adaptive(), &plan(true, true), Container::Mp4, "copy");
        assert_eq!(args.iter().filter(|a| *a == "-user_agent").count(), 2);
        assert!(args.iter().any(|a| a == "-reconnect"));
    }

    #[test]
    fn every_container_names_its_muxer_explicitly() {
        // Regression: renders go to a `<name>.mk-part` staging file, which
        // FFmpeg cannot map to a muxer by extension. Without an explicit `-f`
        // it aborts with "Unable to choose an output format" — and because the
        // other tests only inspect argv, nothing else here would notice.
        for (container, expected) in [
            (Container::Mp4, "mp4"),
            (Container::Mkv, "matroska"),
            (Container::Webm, "webm"),
            (Container::M4a, "ipod"),
            (Container::Mp3, "mp3"),
        ] {
            let args = args_for(&adaptive(), &plan(false, false), container, "libx264");
            let position = args
                .iter()
                .position(|a| a == "-f")
                .unwrap_or_else(|| panic!("{container:?} did not emit -f"));
            assert_eq!(args[position + 1], expected, "wrong muxer for {container:?}");

            // -f is an output option, so it must follow the last input.
            let last_input = args.iter().rposition(|a| a == "-i").unwrap();
            assert!(position > last_input, "-f must be an output option");
        }
    }

    #[tokio::test]
    async fn av1_is_copied_into_mp4_unless_the_user_asks_otherwise() {
        // AV1 is legal in MP4, so by default it costs nothing to keep it.
        let plan = plan_offline(&av1_source(), Container::Mp4, false).await;
        assert!(
            !plan.note.contains("H.264"),
            "should not mention a conversion it is not doing: {}",
            plan.note
        );
    }

    #[tokio::test]
    async fn av1_to_h264_forces_a_re_encode_with_an_honest_note() {
        let plan = plan_offline(&av1_source(), Container::Mp4, true).await;

        assert!(!plan.copy_video, "AV1 → H.264 cannot be a stream copy");
        assert!(plan.copy_audio, "audio is already AAC and must still be copied");
        assert!(
            plan.note.contains("AV1 to H.264"),
            "note must explain the conversion, not claim AV1 is unsupported: {}",
            plan.note
        );
    }

    #[tokio::test]
    async fn h264_sources_ignore_the_av1_option() {
        // The flag must not disturb a source that is already H.264.
        let plan = plan_offline(&adaptive(), Container::Mp4, true).await;
        assert!(!plan.note.contains("AV1"), "note: {}", plan.note);
    }

    #[tokio::test]
    async fn av1_to_h264_is_ignored_for_webm_which_cannot_hold_h264() {
        // Honouring the request here would produce an unplayable file.
        let plan = plan_offline(&av1_source(), Container::Webm, true).await;
        assert!(
            !plan.note.contains("H.264"),
            "WebM cannot hold H.264, so the request must be dropped: {}",
            plan.note
        );
    }

    #[test]
    fn keyframe_tolerance_tracks_the_source_frame_rate() {
        // Half a frame: 24 fps gets a looser window than 60 fps.
        assert!((keyframe_tolerance(24.0) - 0.5 / 24.0).abs() < 1e-12);
        assert!((keyframe_tolerance(60.0) - 0.5 / 60.0).abs() < 1e-12);
        // Unknown or nonsensical rates fall back to the 60 fps assumption.
        assert!((keyframe_tolerance(0.0) - 1.0 / 120.0).abs() < 1e-12);
    }

    #[test]
    fn progress_lines_are_parsed_into_ticks() {
        let mut tick = RenderTick::default();

        assert!(!apply_progress_line("total_size=1048576", &mut tick));
        assert_eq!(tick.bytes, Some(1_048_576));

        assert!(!apply_progress_line("speed=12.4x", &mut tick));
        assert_eq!(tick.speed, Some(12.4));

        // `frame` arrives before `out_time_us` in a block, so it must fold in
        // without firing an event of its own.
        assert!(!apply_progress_line("frame=284", &mut tick));
        assert_eq!(tick.frames, Some(284));

        assert!(apply_progress_line("out_time_us=2500000", &mut tick));
        assert!((tick.out_seconds - 2.5).abs() < 1e-9);

        assert!(!apply_progress_line("garbage", &mut tick));
        // Negative positions occur briefly at startup; clamp rather than panic.
        assert!(apply_progress_line("out_time_us=-42", &mut tick));
        assert_eq!(tick.out_seconds, 0.0);
    }

    #[test]
    fn expected_frames_is_known_only_when_the_rate_is() {
        assert_eq!(expected_frames(10.0, 30.0), Some(300.0));
        assert_eq!(expected_frames(10.0, 0.0), None, "unknown fps");
        assert_eq!(expected_frames(0.0, 30.0), None, "zero-length slice");
    }

    #[tokio::test]
    async fn audio_only_plan_skips_the_keyframe_probe() {
        let tc = Toolchain {
            ytdlp: PathBuf::from("yt-dlp"),
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
        };
        let range = Range { start: 90.0, end: 120.0 };

        // Would hang or error if it actually shelled out to the fake ffprobe.
        let plan = plan_cut(
            &tc,
            &adaptive(),
            range,
            Container::M4a,
            Precision::Auto,
            false,
            &CancellationToken::new(),
        )
        .await;

        assert!(plan.copy_audio);
        assert!(!plan.copy_video);
        assert_eq!(plan.seek_in, 90.0);
        assert_eq!(plan.duration, 30.0);
        // No video in the output, so progress must fall back to the muxer clock.
        assert_eq!(plan.expected_frames, None);
    }
}
