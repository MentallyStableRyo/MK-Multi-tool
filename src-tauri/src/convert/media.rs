//! Audio and video conversion via the app's already-vendored FFmpeg.
//!
//! Video re-encoding probes for a hardware encoder the same way `render.rs`
//! does for the slice pipeline — NVENC → Quick Sync → AMF on Windows,
//! VideoToolbox on macOS, NVENC → Quick Sync on Linux — and falls back to the
//! software encoder on any runtime failure, which happens in practice on a
//! laptop whose discrete GPU is powered down. This module brings its own
//! quality tables (see [`h264_quality_args`]) rather than reusing
//! `render::h264_options`, which is tuned specifically for the slice
//! pipeline's fixed "visually transparent at interactive speed" target; only
//! encoder *discovery* (`render::h264_candidates`) is shared.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::{AppError, Result};
use crate::probe;
use crate::proc;
use crate::render::{self, RenderTick};
use crate::staging::PartialOutput;

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Mp3,
    Wav,
    Flac,
    Aac,
    Ogg,
    Opus,
    M4a,
}

impl AudioFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Aac => "aac",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::M4a => "m4a",
        }
    }

    fn muxer(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Aac => "adts",
            Self::Ogg | Self::Opus => "ogg",
            Self::M4a => "ipod",
        }
    }

    fn codec_args(self) -> &'static [&'static str] {
        match self {
            Self::Mp3 => &["-c:a", "libmp3lame", "-q:a", "2"],
            Self::Wav => &["-c:a", "pcm_s16le"],
            Self::Flac => &["-c:a", "flac"],
            Self::Aac => &["-c:a", "aac", "-b:a", "192k"],
            Self::Ogg => &["-c:a", "libvorbis", "-q:a", "5"],
            Self::Opus => &["-c:a", "libopus", "-b:a", "160k"],
            Self::M4a => &["-c:a", "aac", "-b:a", "192k"],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOptions {
    pub format: AudioFormat,
}

pub async fn convert_audio(
    tc: &Toolchain,
    input: &Path,
    output: &Path,
    options: &AudioOptions,
    cancel: &CancellationToken,
    on_progress: impl FnMut(f64, String),
) -> Result<()> {
    let duration = probe::local_duration(tc, input, cancel).await;
    ensure_parent(output)?;

    // Written to a `.mk-part` staging file and moved into place only on
    // success, exactly like the slice pipeline: a cancelled or failed
    // conversion must never leave a half-written file sitting under the name
    // the user asked for.
    let partial = PartialOutput::new(output.to_path_buf());
    let staging = partial.staging_path();

    let mut cmd = proc::command(&tc.ffmpeg);
    common_args(&mut cmd);
    cmd.arg("-i")
        .arg(input)
        .arg("-vn")
        .args(options.format.codec_args())
        .arg("-f")
        .arg(options.format.muxer())
        .arg(staging);

    run_ffmpeg(cmd, duration, cancel, on_progress).await?;
    verify(tc, staging, cancel).await?;
    partial.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoFormat {
    Mp4,
    Mkv,
    Avi,
    WebM,
    Mov,
    Gif,
}

impl VideoFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Avi => "avi",
            Self::WebM => "webm",
            Self::Mov => "mov",
            Self::Gif => "gif",
        }
    }

    fn muxer(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "matroska",
            Self::Avi => "avi",
            Self::WebM => "webm",
            Self::Mov => "mov",
            Self::Gif => "gif",
        }
    }

    pub fn has_audio(self) -> bool {
        !matches!(self, Self::Gif)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoQuality {
    Smaller,
    Balanced,
    Sharper,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoOptions {
    pub format: VideoFormat,
    #[serde(default)]
    pub quality: Option<VideoQuality>,
    /// Prefer a hardware encoder when the smart-detection probe finds one.
    /// Turning this off forces software encoding from the start; a hardware
    /// encoder that fails partway through is always retried in software
    /// regardless of this flag, since that failure has nothing to do with
    /// what the user asked for.
    #[serde(default = "default_true")]
    pub prefer_hardware: bool,
}

fn default_true() -> bool {
    true
}

impl VideoOptions {
    fn quality(&self) -> VideoQuality {
        self.quality.unwrap_or(VideoQuality::Balanced)
    }
}

/// H.264 quality/speed settings per encoder and tier. Values target
/// "visually solid" at each tier rather than archival quality — the same
/// design goal `render.rs` states for its own fixed table, just spread across
/// three levels instead of one.
fn h264_quality_args(encoder: &str, quality: VideoQuality) -> Vec<&'static str> {
    use VideoQuality::*;
    match (encoder, quality) {
        ("h264_nvenc", Smaller) => vec!["-preset", "p4", "-rc", "vbr", "-cq", "28", "-b:v", "0"],
        ("h264_nvenc", Balanced) => vec!["-preset", "p4", "-rc", "vbr", "-cq", "23", "-b:v", "0"],
        ("h264_nvenc", Sharper) => vec!["-preset", "p6", "-rc", "vbr", "-cq", "18", "-b:v", "0"],

        ("h264_qsv", Smaller) => vec!["-preset", "veryfast", "-global_quality", "28"],
        ("h264_qsv", Balanced) => vec!["-preset", "veryfast", "-global_quality", "23"],
        ("h264_qsv", Sharper) => vec!["-preset", "medium", "-global_quality", "18"],

        ("h264_amf", Smaller) => vec!["-quality", "speed", "-rc", "cqp", "-qp_i", "28", "-qp_p", "30"],
        ("h264_amf", Balanced) => vec!["-quality", "balanced", "-rc", "cqp", "-qp_i", "22", "-qp_p", "24"],
        ("h264_amf", Sharper) => vec!["-quality", "quality", "-rc", "cqp", "-qp_i", "16", "-qp_p", "18"],

        ("h264_videotoolbox", Smaller) => vec!["-q:v", "68", "-allow_sw", "1"],
        ("h264_videotoolbox", Balanced) => vec!["-q:v", "58", "-allow_sw", "1"],
        ("h264_videotoolbox", Sharper) => vec!["-q:v", "45", "-allow_sw", "1"],

        (_, Smaller) => vec!["-preset", "veryfast", "-crf", "28"],
        (_, Balanced) => vec!["-preset", "veryfast", "-crf", "21"],
        (_, Sharper) => vec!["-preset", "slow", "-crf", "17"],
    }
}

fn vp9_crf(quality: VideoQuality) -> &'static str {
    match quality {
        VideoQuality::Smaller => "36",
        VideoQuality::Balanced => "32",
        VideoQuality::Sharper => "26",
    }
}

fn pixel_format_args() -> [&'static str; 2] {
    // Sources can be 10-bit (HDR, VP9 profile 2, AV1); H.264/VP9 encoders here
    // are configured for 8-bit output, and 10-bit breaks many players anyway.
    ["-pix_fmt", "yuv420p"]
}

fn common_args(cmd: &mut Command) {
    cmd.arg("-hide_banner")
        .arg("-nostdin")
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-nostats");
}

fn ensure_parent(output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Run one FFmpeg attempt, translating its `-progress` stream into
/// `(fraction 0..1, message)`. `fraction` falls back to 0.0 (an indeterminate
/// bar, from the caller's point of view) when the source duration could not
/// be probed.
async fn run_ffmpeg(
    cmd: Command,
    duration: Option<f64>,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(f64, String),
) -> Result<()> {
    let mut tick = RenderTick::default();
    let out = proc::run_streaming(cmd, cancel, |line| {
        if render::apply_progress_line(line, &mut tick) {
            let fraction = duration
                .filter(|d| *d > 0.0)
                .map(|d| (tick.out_seconds / d).clamp(0.0, 1.0))
                .unwrap_or(0.0);
            let message = match tick.speed {
                Some(speed) => format!("Converting — {speed:.1}× realtime"),
                None => "Converting…".to_string(),
            };
            on_progress(fraction, message);
        }
    })
    .await?;

    if !out.ok() {
        return Err(AppError::Ffmpeg { code: out.code, detail: render::tail_of(&out.stderr) });
    }
    Ok(())
}

async fn verify(tc: &Toolchain, output: &Path, cancel: &CancellationToken) -> Result<()> {
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = std::fs::remove_file(output);
        return Err(AppError::Ffmpeg { code: 0, detail: "the conversion produced an empty file".into() });
    }
    if probe::local_duration(tc, output, cancel).await.is_none() {
        let _ = std::fs::remove_file(output);
        return Err(AppError::Ffmpeg { code: 0, detail: "the converted file has no readable duration".into() });
    }
    Ok(())
}

fn build_video_args(input: &Path, output: &Path, options: &VideoOptions, encoder: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-i".into(), input.to_string_lossy().into_owned()];

    match options.format {
        VideoFormat::WebM => {
            args.extend(["-c:v", "libvpx-vp9", "-crf", vp9_crf(options.quality()), "-b:v", "0", "-row-mt", "1"].map(String::from));
            args.extend(pixel_format_args().map(String::from));
            args.extend(["-c:a", "libopus", "-b:a", "160k"].map(String::from));
        }
        _ => {
            args.push("-c:v".into());
            args.push(encoder.to_string());
            args.extend(h264_quality_args(encoder, options.quality()).into_iter().map(String::from));
            args.extend(pixel_format_args().map(String::from));
            if options.format.has_audio() {
                args.extend(["-c:a", "aac", "-b:a", "192k"].map(String::from));
            } else {
                args.push("-an".into());
            }
        }
    }

    if matches!(options.format, VideoFormat::Mp4 | VideoFormat::Mov) {
        // Move the moov atom to the front so the file is seekable immediately.
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    args.push("-f".into());
    args.push(options.format.muxer().into());
    args.push(output.to_string_lossy().into_owned());
    args
}

/// A single-pass FFmpeg palette-generate-then-use pipeline. GIF has no
/// encoder to pick between — this is a `-filter_complex`, not a codec — so it
/// bypasses the hardware/encoder machinery entirely.
fn build_gif_args(input: &Path, output: &Path, quality: VideoQuality) -> Vec<String> {
    let fps = match quality {
        VideoQuality::Smaller => 10,
        VideoQuality::Balanced => 15,
        VideoQuality::Sharper => 24,
    };
    let width = match quality {
        VideoQuality::Smaller => 360,
        VideoQuality::Balanced => 480,
        VideoQuality::Sharper => 720,
    };
    vec![
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-filter_complex".into(),
        format!(
            "[0:v] fps={fps},scale={width}:-1:flags=lanczos,split [a][b];[a] palettegen [p];[b][p] paletteuse"
        ),
        "-f".into(),
        "gif".into(),
        output.to_string_lossy().into_owned(),
    ]
}

/// Convert a video, probing for and preferring a hardware encoder unless
/// `options.prefer_hardware` is false. On any encoder's runtime failure the
/// next candidate is tried automatically, ending at the software encoder —
/// `libx264` for H.264 targets, always attempted last.
pub async fn convert_video(
    tc: &Toolchain,
    input: &Path,
    output: &Path,
    options: &VideoOptions,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(f64, String),
) -> Result<()> {
    let duration = probe::local_duration(tc, input, cancel).await;
    ensure_parent(output)?;

    // Same atomic-commit reasoning as `convert_audio`: a killed encoder must
    // never leave a half-written file under the destination name.
    let partial = PartialOutput::new(output.to_path_buf());
    let staging = partial.staging_path();

    if options.format == VideoFormat::Gif {
        let mut cmd = proc::command(&tc.ffmpeg);
        common_args(&mut cmd);
        cmd.args(build_gif_args(input, staging, options.quality()));
        run_ffmpeg(cmd, duration, cancel, &mut on_progress).await?;
        verify(tc, staging, cancel).await?;
        partial.commit()?;
        return Ok(());
    }

    let encoders: Vec<String> = if options.format == VideoFormat::WebM {
        // No codec choice to make for VP9: `build_video_args` hardcodes
        // libvpx-vp9 there. One pass through the loop below is enough.
        vec!["libvpx-vp9".to_string()]
    } else if options.prefer_hardware {
        render::h264_candidates(tc).await
    } else {
        vec!["libx264".to_string()]
    };

    let mut last_error: Option<AppError> = None;

    for (attempt, encoder) in encoders.iter().enumerate() {
        let args = build_video_args(input, staging, options, encoder);
        let mut cmd = proc::command(&tc.ffmpeg);
        common_args(&mut cmd);
        cmd.args(&args);

        match run_ffmpeg(cmd, duration, cancel, &mut on_progress).await {
            Err(e) if e.is_cancelled() => {
                let _ = std::fs::remove_file(staging);
                return Err(e);
            }
            Err(e) => {
                let _ = std::fs::remove_file(staging);
                let retryable = render::is_hardware(encoder) && attempt + 1 < encoders.len();
                last_error = Some(e);
                if !retryable {
                    break;
                }
            }
            Ok(()) => {
                verify(tc, staging, cancel).await?;
                partial.commit()?;
                return Ok(());
            }
        }
    }

    Err(last_error.unwrap_or_else(|| AppError::other("FFmpeg produced no output")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_quality_tier_produces_distinct_h264_settings() {
        let low = h264_quality_args("libx264", VideoQuality::Smaller);
        let mid = h264_quality_args("libx264", VideoQuality::Balanced);
        let high = h264_quality_args("libx264", VideoQuality::Sharper);
        assert_ne!(low, mid);
        assert_ne!(mid, high);
    }

    #[test]
    fn gif_has_no_audio_track_by_construction() {
        assert!(!VideoFormat::Gif.has_audio());
        assert!(VideoFormat::Mp4.has_audio());
    }

    #[test]
    fn gif_pipeline_is_a_single_pass_palette_filter() {
        let args = build_gif_args(Path::new("in.mp4"), Path::new("out.gif"), VideoQuality::Balanced);
        assert!(args.iter().any(|a| a == "-filter_complex"));
        // Exactly one -i: this must not shell out twice (generate, then use).
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);
    }

    #[test]
    fn every_video_target_names_its_muxer_explicitly() {
        let options = |format| VideoOptions { format, quality: None, prefer_hardware: true };
        for format in [VideoFormat::Mp4, VideoFormat::Mkv, VideoFormat::Avi, VideoFormat::WebM, VideoFormat::Mov] {
            let args = build_video_args(Path::new("in.mp4"), Path::new("out"), &options(format), "libx264");
            let position = args.iter().position(|a| a == "-f").unwrap_or_else(|| panic!("{format:?} did not emit -f"));
            assert_eq!(args[position + 1], format.muxer());
        }
    }

    #[test]
    fn webm_always_encodes_vp9_regardless_of_the_encoder_argument() {
        // The "encoder" parameter is only meaningful for the H.264 path; WebM
        // must ignore it rather than accidentally emitting an h264 codec name.
        let options = VideoOptions { format: VideoFormat::WebM, quality: None, prefer_hardware: true };
        let args = build_video_args(Path::new("in.mp4"), Path::new("out.webm"), &options, "h264_nvenc");
        assert!(args.windows(2).any(|w| w == ["-c:v", "libvpx-vp9"]));
        assert!(!args.iter().any(|a| a == "h264_nvenc"));
    }
}
