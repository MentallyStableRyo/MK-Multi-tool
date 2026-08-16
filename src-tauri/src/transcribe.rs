//! Audio → SRT, entirely on this machine.
//!
//! Three steps, in order:
//!
//!   1. FFmpeg decodes whatever the user dropped — any container, any codec,
//!      video included — into the 16 kHz mono PCM whisper.cpp requires. Doing
//!      this unconditionally is cheaper than the alternative: `whisper-cli`
//!      reads only flac/mp3/ogg/wav, so anything else would fail late with an
//!      unhelpful message.
//!   2. `whisper-cli` transcribes it, writing token-level JSON.
//!   3. [`crate::srt`] regroups those tokens into words and words into captions.
//!
//! Nothing is uploaded. whisper.cpp is a local binary and the model is a local
//! file, exactly like ffmpeg and yt-dlp elsewhere in this app.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use crate::binaries::{self, Toolchain};
use crate::error::{AppError, Result};
use crate::proc;
use crate::srt::{self, Transcript};

/// How hard to work for the transcript.
///
/// The tiers differ in two ways at once, because model size alone leaves easy
/// accuracy on the table. A larger model reads harder audio; a wider beam
/// search reconsiders more candidate transcriptions before committing. The
/// slowest tier also disables the greedy shortcuts whisper takes by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Accuracy {
    Fast,
    Normal,
    Accurate,
}

impl Accuracy {
    /// The ggml model this tier runs, without extension.
    pub fn model_stem(self) -> &'static str {
        match self {
            Self::Fast => "ggml-base",
            Self::Normal => "ggml-small",
            Self::Accurate => "ggml-large-v3-turbo",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Normal => "Normal",
            Self::Accurate => "Very accurate",
        }
    }

    /// Roughly how much slower than realtime this tier runs on a typical CPU.
    /// Used only to turn elapsed time into an estimate for the UI.
    pub fn realtime_factor(self) -> f64 {
        match self {
            Self::Fast => 0.15,
            Self::Normal => 0.45,
            Self::Accurate => 1.6,
        }
    }

    /// Decoder settings, appended to the command line.
    fn decoder_args(self) -> Vec<String> {
        match self {
            // Greedy: one candidate, no temperature fallback. Fastest path
            // through the decoder, and the one that gives up soonest on
            // difficult audio.
            Self::Fast => vec![
                "-bo".into(), "1".into(),
                "-bs".into(), "1".into(),
                "-nf".into(),
            ],
            // whisper.cpp's own defaults.
            Self::Normal => vec![
                "-bo".into(), "5".into(),
                "-bs".into(), "5".into(),
            ],
            // A wider beam, and thresholds loosened so the decoder retries a
            // failing window at a higher temperature instead of emitting the
            // first plausible guess. This is where the hallucinated-text and
            // dropped-word failures mostly go away.
            Self::Accurate => vec![
                "-bo".into(), "8".into(),
                "-bs".into(), "8".into(),
                "-et".into(), "2.8".into(),
                "-lpt".into(), "-1.2".into(),
                "-wt".into(), "0.005".into(),
            ],
        }
    }
}

/// What the frontend asks for.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeRequest {
    /// Absolute path to the audio or video file the user chose.
    pub input_path: String,
    pub accuracy: Accuracy,
    /// Ceiling on words per caption. Clamped to a sane range.
    pub words_per_caption: usize,
    /// BCP-47-ish language code, or `auto` to detect.
    pub language: String,
    /// Translate to English rather than transcribing verbatim.
    #[serde(default)]
    pub translate: bool,
    pub output_path: String,
}

/// Bounds on `words_per_caption`. One word per caption is legitimate (karaoke
/// styling); beyond about twenty a caption no longer fits on screen.
pub const MIN_WORDS: usize = 1;
pub const MAX_WORDS: usize = 20;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSummary {
    pub captions: usize,
    pub words: usize,
    pub language: String,
    pub duration_ms: i64,
    /// Mean word confidence, 0–1.
    pub confidence: f32,
    /// The first few captions, so the UI can show a preview without re-reading
    /// the file it just wrote.
    pub preview: Vec<PreviewCaption>,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewCaption {
    pub index: usize,
    pub start: String,
    pub end: String,
    pub text: String,
    pub confidence: f32,
}

/// Locate the model file for a tier, or say precisely which one is missing.
pub fn model_path(app: &AppHandle, accuracy: Accuracy) -> Result<PathBuf> {
    let file = format!("{}.bin", accuracy.model_stem());
    binaries::locate_model(app, &file).ok_or_else(|| {
        AppError::other(format!(
            "The {} speech model ({file}) is not installed.",
            accuracy.label().to_lowercase()
        ))
    })
}

/// Decode any input to the 16 kHz mono PCM whisper.cpp expects.
async fn to_wav(
    toolchain: &Toolchain,
    input: &Path,
    output: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut cmd = proc::command(&toolchain.ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-nostdin")
        .arg("-y")
        .arg("-i")
        .arg(input)
        // Drop any video stream rather than letting ffmpeg try to carry it.
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg("-f")
        .arg("wav")
        .arg(output);

    let out = proc::run_streaming(cmd, cancel, |_| {}).await?;
    if !out.ok() {
        return Err(AppError::Ffmpeg {
            code: out.code,
            detail: format!(
                "could not decode audio from that file. {}",
                out.stderr.lines().last().unwrap_or_default()
            ),
        });
    }
    if !output.is_file() || std::fs::metadata(output).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(AppError::other(
            "That file contains no audio track to transcribe.",
        ));
    }
    Ok(())
}

/// Parse the end timestamp out of a whisper progress line.
///
/// whisper prints its percentage to stderr, which `run_streaming` reserves for
/// error context. Its stdout carries one line per finished segment —
/// `[00:00:00.000 --> 00:00:10.500]   text…` — and the right-hand timestamp
/// against the known duration is a better progress signal anyway: monotonic,
/// and expressed in the audio's own terms.
fn segment_end_ms(line: &str) -> Option<i64> {
    let rest = line.trim().strip_prefix('[')?;
    let (span, _) = rest.split_once(']')?;
    let (_, end) = span.split_once("-->")?;
    parse_clock(end.trim())
}

/// `HH:MM:SS.mmm` → milliseconds.
fn parse_clock(text: &str) -> Option<i64> {
    let (hms, millis) = text.split_once('.')?;
    let mut parts = hms.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds: i64 = parts.next()?.parse().ok()?;
    let millis: i64 = millis.parse().ok()?;
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

/// Run whisper over `wav`, reporting progress as a fraction of `duration_ms`.
async fn run_whisper<F>(
    whisper: &Path,
    model: &Path,
    wav: &Path,
    request: &TranscribeRequest,
    duration_ms: i64,
    cancel: &CancellationToken,
    mut on_progress: F,
) -> Result<Transcript>
where
    F: FnMut(f64),
{
    // whisper writes `<stem>.json`; keep it beside the wav so both are swept
    // together when the temporary directory goes.
    let out_stem = wav.with_extension("");

    let mut cmd = proc::command(whisper);
    cmd.arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav)
        .arg("-of")
        .arg(&out_stem)
        // Token-level JSON: the timings that make words-per-caption exact.
        .arg("-ojf")
        .arg("-l")
        .arg(&request.language)
        // Leave one core for the UI, and never ask for zero.
        .arg("-t")
        .arg(
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(1).max(1))
                .unwrap_or(4)
                .to_string(),
        );

    if request.translate {
        cmd.arg("-tr");
    }
    for arg in request.accuracy.decoder_args() {
        cmd.arg(arg);
    }

    let out = proc::run_streaming(cmd, cancel, |line| {
        if let Some(reached) = segment_end_ms(line) {
            if duration_ms > 0 {
                on_progress((reached as f64 / duration_ms as f64).clamp(0.0, 1.0));
            }
        }
    })
    .await?;

    if !out.ok() {
        let detail = out
            .stderr
            .lines()
            .filter(|l| !l.starts_with("load_backend") && !l.starts_with("read_audio_data"))
            .next_back()
            .unwrap_or("whisper-cli exited without explanation");
        return Err(AppError::other(format!("Transcription failed: {detail}")));
    }

    let json_path = out_stem.with_extension("json");
    let raw = std::fs::read_to_string(&json_path).map_err(|e| {
        AppError::other(format!(
            "whisper produced no transcript file ({}): {e}",
            json_path.display()
        ))
    })?;

    serde_json::from_str::<Transcript>(&raw)
        .map_err(|e| AppError::parse(format!("could not read whisper's transcript: {e}")))
}

/// The whole pipeline. `on_stage` receives (fraction, human-readable step).
pub async fn run<F>(
    app: &AppHandle,
    toolchain: &Toolchain,
    request: &TranscribeRequest,
    duration_ms: i64,
    cancel: &CancellationToken,
    mut on_stage: F,
) -> Result<TranscriptSummary>
where
    F: FnMut(f64, &str),
{
    let whisper = binaries::require_whisper(app)?;
    let model = model_path(app, request.accuracy)?;

    // One temporary directory per job, removed on the way out whatever happens.
    let work = tempdir_for(&request.output_path)?;
    let wav = work.join("audio.wav");

    let result = async {
        on_stage(0.02, "Decoding audio…");
        to_wav(toolchain, Path::new(&request.input_path), &wav, cancel).await?;

        on_stage(0.06, "Transcribing…");
        let transcript = run_whisper(
            &whisper,
            &model,
            &wav,
            request,
            duration_ms,
            cancel,
            |fraction| {
                // Decode occupies the first 6%; the rest is whisper.
                on_stage(0.06 + fraction * 0.90, "Transcribing…");
            },
        )
        .await?;

        on_stage(0.97, "Building captions…");

        let words = srt::words_from(&transcript);
        if words.is_empty() {
            return Err(AppError::other(
                "No speech was found in that file.",
            ));
        }

        let limit = request.words_per_caption.clamp(MIN_WORDS, MAX_WORDS);
        let captions = srt::captions(&words, limit);
        let text = srt::render(&captions);

        std::fs::write(&request.output_path, text.as_bytes())?;

        let confidence = words.iter().map(|w| w.confidence).sum::<f32>() / words.len() as f32;
        let preview = captions
            .iter()
            .take(6)
            .enumerate()
            .map(|(i, c)| PreviewCaption {
                index: i + 1,
                start: srt::timestamp(c.start_ms),
                end: srt::timestamp(c.end_ms),
                text: c.text.clone(),
                confidence: c.confidence,
            })
            .collect();

        Ok(TranscriptSummary {
            captions: captions.len(),
            words: words.len(),
            language: transcript.result.language.clone(),
            duration_ms: captions.last().map(|c| c.end_ms).unwrap_or(0),
            confidence,
            preview,
            output_path: request.output_path.clone(),
        })
    }
    .await;

    std::fs::remove_dir_all(&work).ok();
    result
}

/// A scratch directory next to the output, so the temporary WAV lands on the
/// same volume the user already chose to have space on.
fn tempdir_for(output_path: &str) -> Result<PathBuf> {
    let parent = Path::new(output_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);

    let dir = parent.join(format!(".mk-transcribe-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_read_from_whispers_segment_lines() {
        let line = "[00:00:00.000 --> 00:00:10.500]   And so my fellow Americans";
        assert_eq!(segment_end_ms(line), Some(10_500));

        let long = "[00:59:59.000 --> 01:01:01.007]  text";
        assert_eq!(segment_end_ms(long), Some(3_661_007));

        // Lines that are not segments must not be mistaken for progress.
        assert_eq!(segment_end_ms("whisper_print_progress: 40%"), None);
        assert_eq!(segment_end_ms(""), None);
        assert_eq!(segment_end_ms("[malformed]"), None);
    }

    #[test]
    fn each_tier_names_a_distinct_model_and_decoder() {
        let tiers = [Accuracy::Fast, Accuracy::Normal, Accuracy::Accurate];

        let models: Vec<_> = tiers.iter().map(|t| t.model_stem()).collect();
        assert_eq!(models.len(), 3);
        // A tier that reused another's model would be a mislabelled option.
        assert!(models.iter().all(|m| models.iter().filter(|o| o == &m).count() == 1));

        // Accuracy must widen the beam, not just swap weights.
        let beam = |t: Accuracy| {
            let args = t.decoder_args();
            let i = args.iter().position(|a| a == "-bs").unwrap();
            args[i + 1].parse::<u32>().unwrap()
        };
        assert!(beam(Accuracy::Fast) < beam(Accuracy::Normal));
        assert!(beam(Accuracy::Normal) < beam(Accuracy::Accurate));
    }

    #[test]
    fn clock_parsing_rejects_junk() {
        assert_eq!(parse_clock("00:00:01.250"), Some(1_250));
        assert_eq!(parse_clock("not a clock"), None);
        assert_eq!(parse_clock("00:00"), None);
    }
}
