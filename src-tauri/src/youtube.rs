//! Media ingestion: URL normalisation, metadata extraction, and stream
//! resolution.
//!
//! Named for YouTube because that is the platform whose quirks shape it, but
//! everything below [`normalize_url`] is site-agnostic: yt-dlp hands back the
//! same document shape for every extractor, so Instagram, TikTok, and X flow
//! through [`parse_info`] and [`resolve_streams`] unchanged. Per-platform link
//! rules live in [`crate::platform`].
//!
//! ## Why yt-dlp rather than `rusty_ytdl`
//!
//! YouTube rotates its signature cipher, throttling (`n`) parameter, and
//! anti-automation checks on a scale of weeks. yt-dlp ships fixes within days;
//! a pure-Rust extractor pinned in `Cargo.lock` silently rots and starts
//! returning 403s. yt-dlp is invoked as a sidecar process — never in-process —
//! so a hang or crash in the extractor cannot take the UI down with it, and the
//! binary can be updated independently of the app.
//!
//! yt-dlp is used strictly as a *resolver*: it produces the direct CDN URLs and
//! the headers needed to fetch them. All cutting is done by our own FFmpeg
//! pipeline in [`crate::render`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::{AppError, Result, YtErrorKind};
use crate::platform::Platform;
use crate::proc;

/// Where to source cookies from, for private / age-gated / members-only videos.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieOptions {
    /// Browser name understood by yt-dlp: chrome, firefox, edge, brave, …
    #[serde(default)]
    pub browser: Option<String>,
    /// Path to a Netscape-format cookies.txt.
    #[serde(default)]
    pub file: Option<String>,
}

impl CookieOptions {
    /// Append the matching yt-dlp cookie flags. An explicit file wins over a
    /// browser, since it is the more deliberate choice.
    pub fn apply(&self, args: &mut Vec<String>) {
        if let Some(file) = self.file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            args.push("--cookies".into());
            args.push(file.to_string());
            return;
        }
        if let Some(browser) = self.browser.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            args.push("--cookies-from-browser".into());
            args.push(browser.to_string());
        }
    }
}

/// Normalise any accepted YouTube link shape into a canonical watch URL.
///
/// Rejecting early keeps a typo from becoming a confusing yt-dlp error, and
/// stops arbitrary URLs (file://, other sites) from reaching the sidecar.
/// Reached through [`Platform::normalize`], which routes the other sites to
/// their own rules.
pub fn normalize_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Platform::Youtube.reject("(empty)"));
    }

    // Bare 11-character video IDs are a common paste.
    if is_video_id(trimmed) {
        return Ok(watch_url(trimmed));
    }

    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };

    let parsed = url::Url::parse(&with_scheme).map_err(|_| Platform::Youtube.reject(trimmed))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Platform::Youtube.reject(trimmed));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| Platform::Youtube.reject(trimmed))?
        .trim_start_matches("www.")
        .trim_start_matches("m.")
        .to_ascii_lowercase();

    let id = match host.as_str() {
        "youtu.be" => first_path_segment(&parsed),
        "youtube.com" | "youtube-nocookie.com" | "music.youtube.com" => {
            // /watch?v=ID
            if let Some(v) = parsed.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.into_owned())
            {
                Some(v)
            } else {
                // /shorts/ID, /live/ID, /embed/ID, /v/ID
                let mut segments = parsed.path_segments().into_iter().flatten();
                match segments.next() {
                    Some("shorts" | "live" | "embed" | "v") => segments.next().map(str::to_string),
                    _ => None,
                }
            }
        }
        _ => return Err(Platform::Youtube.reject(trimmed)),
    };

    let id = id
        .map(|s| s.trim().to_string())
        .filter(|s| is_video_id(s))
        .ok_or_else(|| Platform::Youtube.reject(trimmed))?;

    Ok(watch_url(&id))
}

fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

fn first_path_segment(u: &url::Url) -> Option<String> {
    u.path_segments()?.find(|s| !s.is_empty()).map(str::to_string)
}

/// YouTube video IDs are exactly 11 chars of base64url.
fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ---------------------------------------------------------------------------
// Metadata model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoInfo {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub thumbnail: Option<String>,
    /// Seconds. `0.0` means yt-dlp could not determine it.
    pub duration_seconds: f64,
    pub webpage_url: String,
    pub is_live: bool,
    pub view_count: Option<u64>,
    pub upload_date: Option<String>,
    pub tracks: Vec<QualityTrack>,
}

/// One selectable output quality. `id` is a yt-dlp format selector, either a
/// single format (`22`) or an explicit video+audio pair (`137+140`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityTrack {
    pub id: String,
    /// Primary label, e.g. `1080p60` or `Audio only`.
    pub label: String,
    /// Secondary line, e.g. `H.264 · AAC · 6.2 Mbps`.
    pub detail: String,
    pub height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio_codec: String,
    pub ext: String,
    pub bitrate_kbps: f64,
    pub hdr: bool,
    pub audio_only: bool,
    /// True when both streams can be stream-copied straight into an MP4.
    pub mp4_ready: bool,
}

/// A single resolved CDN stream plus the headers required to fetch it.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub codec: String,
    pub fps: f64,
}

impl StreamSpec {
    pub fn is_http(&self) -> bool {
        self.url.starts_with("http://") || self.url.starts_with("https://")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The stream(s) backing one [`QualityTrack`].
#[derive(Debug, Clone)]
pub struct StreamSet {
    pub video: Option<StreamSpec>,
    pub audio: Option<StreamSpec>,
    /// True when `video` and `audio` are the same muxed input, so FFmpeg must
    /// open it once and map both streams from input 0.
    pub progressive: bool,
}

// ---------------------------------------------------------------------------
// yt-dlp invocation
// ---------------------------------------------------------------------------

/// Flags applied to every yt-dlp call.
///
/// Split out from [`base_args`] because search is the one caller that must not
/// pass `--no-playlist`: a `ytsearch:` query *is* a playlist, and suppressing
/// it collapses the results to a single entry.
fn common_args() -> Vec<String> {
    [
        "--no-warnings",
        "--no-progress",
        "--ignore-config",
        "--no-color",
        // Bounded retries: yt-dlp's default of 10 makes a dead link feel hung.
        "--retries",
        "3",
        "--socket-timeout",
        "20",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Base flags for the single-video calls: everything in [`common_args`], plus
/// the guard that keeps a link with a playlist in it from expanding.
fn base_args() -> Vec<String> {
    let mut args = common_args();
    args.push("--no-playlist".into());
    args
}

/// One hit from a keyword search, before any format resolution.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    /// Canonical watch URL, ready to hand to the existing analyze path.
    pub url: String,
    pub title: String,
    pub channel: Option<String>,
    /// Seconds. `None` for a live stream, which has no final duration yet.
    pub duration: Option<f64>,
    pub view_count: Option<f64>,
    pub thumbnail: Option<String>,
    pub live: bool,
}

/// Largest number of hits a single search may ask for.
///
/// yt-dlp resolves the result page in one request, but the page itself is
/// paginated at 20, so asking for much more turns one round trip into several.
const MAX_SEARCH_RESULTS: usize = 20;

/// Search YouTube by keyword.
///
/// `--flat-playlist` is what keeps this fast: yt-dlp reads the result page and
/// stops, rather than visiting all twenty videos to build full format tables.
/// That is enough for a title, channel, duration and thumbnail — the real
/// metadata fetch happens in [`dump_json`] once the user picks one.
pub async fn search(
    tc: &Toolchain,
    query: &str,
    cookies: &CookieOptions,
    cancel: &CancellationToken,
) -> Result<Vec<SearchResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let mut args = common_args();
    args.push("--flat-playlist".into());
    args.push("--dump-json".into());
    cookies.apply(&mut args);
    // yt-dlp's own search scheme, so there is no URL to build and no HTML to
    // scrape. The query rides as a single argument and is never shell-parsed.
    args.push(format!("ytsearch{MAX_SEARCH_RESULTS}:{query}"));

    let mut cmd = proc::command(&tc.ytdlp);
    cmd.args(&args);

    let out = proc::run_streaming(cmd, cancel, |_| {}).await?;

    if !out.ok() {
        let kind = YtErrorKind::classify(&out.stderr);
        return Err(AppError::Youtube {
            kind,
            message: match kind {
                YtErrorKind::NotFound => format!("No results for “{query}”."),
                _ => friendly_message(Platform::Youtube, kind, &out.stderr),
            },
        });
    }

    // `--dump-json` under `--flat-playlist` emits one object per line rather
    // than one document, so a single malformed entry costs that result instead
    // of the whole search.
    Ok(out
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|entry| parse_search_entry(&entry))
        .collect())
}

/// Shape one flat-playlist entry into a [`SearchResult`], or drop it.
///
/// Only the id is load-bearing — everything else has a sane absence — so a
/// result missing a title or thumbnail still shows up rather than vanishing.
fn parse_search_entry(entry: &Value) -> Option<SearchResult> {
    let id = as_str(entry, "id")?.to_string();

    // Flat entries usually carry `url`, but not always; the id is enough to
    // rebuild the canonical form, and the analyze path normalizes either.
    let url = as_str(entry, "url")
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));

    let live = entry
        .get("is_live")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Some(SearchResult {
        id,
        url,
        title: as_str(entry, "title").unwrap_or("Untitled").to_string(),
        channel: as_str(entry, "channel")
            .or_else(|| as_str(entry, "uploader"))
            .map(str::to_string),
        duration: as_f64(entry, "duration").filter(|d| *d > 0.0),
        view_count: as_f64(entry, "view_count"),
        thumbnail: pick_thumbnail_within(entry, SEARCH_THUMB_WIDTH),
        live,
    })
}

/// Widest thumbnail that still suits a result row.
///
/// Capped where the single-video preview is not: twenty uncapped hits would
/// pull twenty maxres stills for images rendered barely 160px wide.
const SEARCH_THUMB_WIDTH: f64 = 480.0;

/// Run `yt-dlp -J` and return the parsed metadata document.
///
/// `platform` only shapes the failure message — yt-dlp picks its own extractor
/// from the URL.
pub async fn dump_json(
    tc: &Toolchain,
    url: &str,
    platform: Platform,
    cookies: &CookieOptions,
    cancel: &CancellationToken,
) -> Result<Value> {
    let mut args = base_args();
    args.push("--dump-single-json".into());
    cookies.apply(&mut args);
    args.push(url.to_string());

    let mut cmd = proc::command(&tc.ytdlp);
    cmd.args(&args);

    let out = proc::run_streaming(cmd, cancel, |_| {}).await?;

    if !out.ok() {
        let kind = YtErrorKind::classify(&out.stderr);
        return Err(AppError::Youtube {
            kind,
            message: friendly_message(platform, kind, &out.stderr),
        });
    }

    serde_json::from_str(&out.stdout)
        .map_err(|e| AppError::parse(format!("yt-dlp returned malformed JSON: {e}")))
}

/// Turn yt-dlp's stderr into one clean sentence, in terms of the site the user
/// pasted a link from.
fn friendly_message(platform: Platform, kind: YtErrorKind, stderr: &str) -> String {
    let site = platform.label();
    match kind {
        YtErrorKind::Private => "This video is private.".into(),
        YtErrorKind::BotCheck => {
            format!("{site} served a verification page instead of the video.")
        }
        YtErrorKind::LoginRequired => {
            format!("{site} will not serve this post without a signed-in session.")
        }
        YtErrorKind::AgeRestricted => {
            format!("{site} requires a signed-in session for this video.")
        }
        YtErrorKind::MembersOnly => "This video is restricted to channel members.".into(),
        YtErrorKind::GeoBlocked => "This video is not available in your region.".into(),
        YtErrorKind::Removed => "This video is unavailable or has been removed.".into(),
        YtErrorKind::LiveStream => "This is a live or upcoming stream, so it can't be sliced.".into(),
        YtErrorKind::NotFound => format!("No video was found at that {site} link."),
        YtErrorKind::Network => format!("Could not reach {site}."),
        YtErrorKind::RateLimited => {
            format!("{site} is rate-limiting requests from this network.")
        }
        YtErrorKind::Unknown => {
            // Surface yt-dlp's own last ERROR line; it is usually the useful one.
            stderr
                .lines()
                .rev()
                .find(|l| l.contains("ERROR"))
                .map(|l| l.trim_start_matches("ERROR:").trim().to_string())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "yt-dlp could not process this video.".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Metadata parsing
// ---------------------------------------------------------------------------

fn as_f64(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|f| f.is_finite())
}

fn as_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Collapse a codec string into a stable family tag.
pub fn video_family(vcodec: &str) -> &'static str {
    let c = vcodec.to_ascii_lowercase();
    if c.starts_with("avc") || c.starts_with("h264") {
        "h264"
    } else if c.starts_with("vp9") || c.starts_with("vp09") {
        "vp9"
    } else if c.starts_with("av01") || c.starts_with("av1") {
        "av1"
    } else if c.starts_with("vp8") {
        "vp8"
    } else {
        "other"
    }
}

pub fn audio_family(acodec: &str) -> &'static str {
    let c = acodec.to_ascii_lowercase();
    if c.starts_with("mp4a") || c.starts_with("aac") {
        "aac"
    } else if c.starts_with("opus") {
        "opus"
    } else if c.starts_with("vorbis") {
        "vorbis"
    } else if c.starts_with("ec-3") || c.starts_with("ac-3") {
        "ac3"
    } else if c.starts_with("mp3") {
        "mp3"
    } else {
        "other"
    }
}

fn pretty_video(family: &str) -> &'static str {
    match family {
        "h264" => "H.264",
        "vp9" => "VP9",
        "av1" => "AV1",
        "vp8" => "VP8",
        _ => "Video",
    }
}

fn pretty_audio(family: &str) -> &'static str {
    match family {
        "aac" => "AAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "ac3" => "AC-3",
        "mp3" => "MP3",
        _ => "Audio",
    }
}

/// Whether a format carries a video stream.
///
/// yt-dlp's codec fields have *three* states, and conflating two of them was a
/// real bug: a codec name means the stream is present, the literal `"none"`
/// means it is definitively absent, and a **missing** field means the extractor
/// simply did not say. YouTube always fills both in, so absent-means-unknown
/// never came up; X omits them entirely on its progressive MP4s, and treating
/// that as "no streams" discarded every downloadable format it offers.
///
/// Only an explicit `"none"` counts as absent. Unknown is assumed present,
/// which is the safe reading: a standalone media file with no codec metadata is
/// a muxed one, and [`crate::render::plan_cut`] measures the real codecs before
/// it commits to copying anything.
fn is_video(f: &Value) -> bool {
    f.get("vcodec").and_then(Value::as_str).unwrap_or("") != "none"
}

fn is_audio(f: &Value) -> bool {
    f.get("acodec").and_then(Value::as_str).unwrap_or("") != "none"
}

/// True when the extractor named neither codec, so the pairing is a guess.
fn codecs_unreported(f: &Value) -> bool {
    let named = |key: &str| {
        f.get(key)
            .and_then(Value::as_str)
            .is_some_and(|c| !c.is_empty() && c != "none")
    };
    !named("vcodec") && !named("acodec")
}

/// Formats we can never use: storyboards and DASH manifests without a URL.
fn usable(f: &Value) -> bool {
    if as_str(f, "url").is_none() {
        return false;
    }
    if as_str(f, "ext") == Some("mhtml") {
        return false;
    }
    if as_str(f, "format_note").is_some_and(|n| n.eq_ignore_ascii_case("storyboard")) {
        return false;
    }
    is_video(f) || is_audio(f)
}

/// The document actually carrying the media.
///
/// Most extractors return the clip directly, but a post holding several of them
/// — an Instagram carousel, a tweet with two videos — comes back as a playlist
/// whose `entries` hold the formats, leaving nothing at the top level.
/// `--no-playlist` does not flatten those, so the first entry with formats is
/// the one to work from. Deterministic, which matters: the render re-resolves
/// the same URL and must land on the same entry the user picked a quality from.
fn media_document(root: &Value) -> &Value {
    let has_formats = |v: &Value| {
        v.get("formats")
            .and_then(Value::as_array)
            .is_some_and(|f| !f.is_empty())
    };

    if has_formats(root) {
        return root;
    }
    root.get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| entries.iter().find(|e| has_formats(e)))
        .unwrap_or(root)
}

fn bitrate(f: &Value) -> f64 {
    as_f64(f, "tbr")
        .or_else(|| as_f64(f, "vbr"))
        .or_else(|| as_f64(f, "abr"))
        .unwrap_or(0.0)
}

fn pick_thumbnail(root: &Value) -> Option<String> {
    pick_thumbnail_within(root, f64::INFINITY)
}

/// Largest thumbnail no wider than `max_width`, falling back to whatever single
/// `thumbnail` string the document carries.
///
/// An entry with no declared width is treated as acceptable rather than
/// discarded — yt-dlp omits the field on some extractors, and dropping those
/// would leave a result with no image at all.
fn pick_thumbnail_within(root: &Value, max_width: f64) -> Option<String> {
    // `thumbnails` is ordered worst → best; prefer the largest with a real URL.
    if let Some(list) = root.get("thumbnails").and_then(Value::as_array) {
        let best = list
            .iter()
            .filter(|t| as_str(t, "url").is_some())
            .filter(|t| as_f64(t, "width").unwrap_or(0.0) <= max_width)
            .max_by_key(|t| {
                let w = as_f64(t, "width").unwrap_or(0.0) as i64;
                let h = as_f64(t, "height").unwrap_or(0.0) as i64;
                w * h
            });
        if let Some(url) = best.and_then(|t| as_str(t, "url")) {
            return Some(url.to_string());
        }
    }
    as_str(root, "thumbnail").map(str::to_string)
}

fn format_bitrate(kbps: f64) -> String {
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", kbps / 1000.0)
    } else if kbps > 0.0 {
        format!("{kbps:.0} kbps")
    } else {
        "bitrate n/a".to_string()
    }
}

/// Build the user-facing quality list from a yt-dlp metadata document.
pub fn parse_info(root: &Value) -> Result<VideoInfo> {
    let doc = media_document(root);

    // Descriptive fields can sit on either level: a playlist keeps the post's
    // title and uploader at the top while the formats live in an entry.
    let text = |key: &str| as_str(doc, key).or_else(|| as_str(root, key));
    let number = |key: &str| as_f64(doc, key).or_else(|| as_f64(root, key));

    let id = text("id")
        .ok_or_else(|| AppError::parse("yt-dlp response has no video id"))?
        .to_string();

    let live_status = text("live_status").unwrap_or("");
    let is_live = doc.get("is_live").and_then(Value::as_bool).unwrap_or(false)
        || matches!(live_status, "is_live" | "is_upcoming");

    let empty = Vec::new();
    let formats: Vec<&Value> = doc
        .get("formats")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter(|f| usable(f))
        .collect();

    let tracks = build_tracks(&formats);

    Ok(VideoInfo {
        id,
        title: text("title").unwrap_or("Untitled").to_string(),
        uploader: text("uploader")
            .or_else(|| text("channel"))
            .or_else(|| text("uploader_id"))
            .unwrap_or("Unknown channel")
            .to_string(),
        thumbnail: pick_thumbnail(doc).or_else(|| pick_thumbnail(root)),
        duration_seconds: number("duration").unwrap_or(0.0),
        webpage_url: text("webpage_url")
            .unwrap_or("https://www.youtube.com")
            .to_string(),
        is_live,
        view_count: number("view_count").map(|v| v as u64),
        upload_date: text("upload_date").map(str::to_string),
        tracks,
    })
}

/// Best audio-only format for a given family, by bitrate.
fn best_audio<'a>(formats: &[&'a Value], family: &str) -> Option<&'a Value> {
    formats
        .iter()
        .copied()
        .filter(|f| !is_video(f) && is_audio(f))
        .filter(|f| {
            let ac = f.get("acodec").and_then(Value::as_str).unwrap_or("");
            family.is_empty() || audio_family(ac) == family
        })
        .max_by(|a, b| bitrate(a).total_cmp(&bitrate(b)))
}

fn build_tracks(formats: &[&Value]) -> Vec<QualityTrack> {
    // Prefer AAC: it stream-copies into MP4, which is the default container and
    // the one with the widest playback support.
    let aac = best_audio(formats, "aac");
    let opus = best_audio(formats, "opus");
    let any_audio = best_audio(formats, "");
    let chosen_audio = aac.or(opus).or(any_audio);

    // Deduplicate video-only formats by (height, fps, codec family), keeping the
    // highest-bitrate representative of each.
    let mut groups: BTreeMap<(u32, u32, &'static str), &Value> = BTreeMap::new();
    for f in formats.iter().copied() {
        if !is_video(f) || is_audio(f) {
            continue;
        }
        let height = as_f64(f, "height").unwrap_or(0.0) as u32;
        if height == 0 {
            continue;
        }
        let fps = as_f64(f, "fps").unwrap_or(0.0).round() as u32;
        let family = video_family(f.get("vcodec").and_then(Value::as_str).unwrap_or(""));
        let key = (height, fps, family);
        groups
            .entry(key)
            .and_modify(|existing| {
                if bitrate(f) > bitrate(existing) {
                    *existing = f;
                }
            })
            .or_insert(f);
    }

    let mut tracks: Vec<QualityTrack> = Vec::with_capacity(groups.len() + 4);

    for ((height, fps, family), vf) in &groups {
        let Some(af) = chosen_audio else {
            continue;
        };
        let (Some(vid), Some(aid)) = (as_str(vf, "format_id"), as_str(af, "format_id")) else {
            continue;
        };

        let a_family = audio_family(af.get("acodec").and_then(Value::as_str).unwrap_or(""));
        let total_kbps = bitrate(vf) + bitrate(af);
        let hdr = as_str(vf, "dynamic_range").is_some_and(|d| d.to_ascii_uppercase().contains("HDR"));

        tracks.push(QualityTrack {
            id: format!("{vid}+{aid}"),
            label: label_for(*height, *fps),
            detail: format!(
                "{} · {} · {}",
                pretty_video(family),
                pretty_audio(a_family),
                format_bitrate(total_kbps)
            ),
            height: *height,
            fps: *fps as f64,
            video_codec: (*family).to_string(),
            audio_codec: a_family.to_string(),
            ext: as_str(vf, "ext").unwrap_or("mp4").to_string(),
            bitrate_kbps: total_kbps,
            hdr,
            audio_only: false,
            mp4_ready: *family == "h264" && a_family == "aac",
        });
    }

    // Progressive (pre-muxed) formats. These are skipped where an adaptive
    // track already covers the resolution *and* we know what its codecs are —
    // on YouTube that is always, so the redundant pre-muxed copies drop out
    // exactly as they did before.
    //
    // Where the adaptive pairing rests on codecs the extractor never reported,
    // the progressive format is kept instead of being hidden behind it. On X
    // that is the difference between offering the direct MP4 — which answers
    // byte ranges, so the whole `-ss` before `-i` trick works — and offering
    // only an HLS pair stitched together from guesses.
    for f in formats.iter().copied() {
        if !(is_video(f) && is_audio(f)) {
            continue;
        }
        let height = as_f64(f, "height").unwrap_or(0.0) as u32;
        if height == 0 {
            continue;
        }
        let covered = tracks
            .iter()
            .any(|t| t.height == height && t.video_codec != "other" && t.audio_codec != "other");
        if covered {
            continue;
        }
        let Some(fid) = as_str(f, "format_id") else {
            continue;
        };
        let v_family = video_family(f.get("vcodec").and_then(Value::as_str).unwrap_or(""));
        let a_family = audio_family(f.get("acodec").and_then(Value::as_str).unwrap_or(""));
        let kbps = bitrate(f);

        // Saying "Video · Audio" for a format whose codecs were never reported
        // would read as though we had looked and found something generic.
        let detail = if codecs_unreported(f) {
            format!(
                "{} · single stream · codecs read from the media at render time",
                format_bitrate(kbps)
            )
        } else {
            format!(
                "{} · {} · {} · single stream",
                pretty_video(v_family),
                pretty_audio(a_family),
                format_bitrate(kbps)
            )
        };

        tracks.push(QualityTrack {
            id: fid.to_string(),
            label: label_for(height, as_f64(f, "fps").unwrap_or(0.0).round() as u32),
            detail,
            height,
            fps: as_f64(f, "fps").unwrap_or(0.0),
            video_codec: v_family.to_string(),
            audio_codec: a_family.to_string(),
            ext: as_str(f, "ext").unwrap_or("mp4").to_string(),
            bitrate_kbps: kbps,
            hdr: false,
            audio_only: false,
            mp4_ready: v_family == "h264" && a_family == "aac",
        });
    }

    // Highest quality first.
    tracks.sort_by(|a, b| {
        b.height
            .cmp(&a.height)
            .then(b.fps.total_cmp(&a.fps))
            .then(b.bitrate_kbps.total_cmp(&a.bitrate_kbps))
    });

    // Audio-only option, always last.
    if let Some(af) = chosen_audio {
        if let Some(aid) = as_str(af, "format_id") {
            let a_family = audio_family(af.get("acodec").and_then(Value::as_str).unwrap_or(""));
            let kbps = bitrate(af);
            tracks.push(QualityTrack {
                id: aid.to_string(),
                label: "Audio only".to_string(),
                detail: format!("{} · {}", pretty_audio(a_family), format_bitrate(kbps)),
                height: 0,
                fps: 0.0,
                video_codec: "none".to_string(),
                audio_codec: a_family.to_string(),
                ext: as_str(af, "ext").unwrap_or("m4a").to_string(),
                bitrate_kbps: kbps,
                hdr: false,
                audio_only: true,
                mp4_ready: a_family == "aac",
            });
        }
    }

    tracks
}

fn label_for(height: u32, fps: u32) -> String {
    let base = match height {
        4320 => "4320p (8K)".to_string(),
        2160 => "2160p (4K)".to_string(),
        1440 => "1440p (2K)".to_string(),
        h => format!("{h}p"),
    };
    // Only annotate high frame rates; "1080p30" is noise.
    if fps > 30 {
        format!("{base}{fps}")
    } else {
        base
    }
}

// ---------------------------------------------------------------------------
// Stream resolution
// ---------------------------------------------------------------------------

fn headers_of(f: &Value) -> Vec<(String, String)> {
    f.get("http_headers")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                // FFmpeg manages these itself; forwarding them breaks ranged reads.
                .filter(|(k, _)| {
                    !k.eq_ignore_ascii_case("range") && !k.eq_ignore_ascii_case("accept-encoding")
                })
                .collect()
        })
        .unwrap_or_default()
}

fn spec_of(f: &Value, codec_key: &str) -> Option<StreamSpec> {
    Some(StreamSpec {
        url: as_str(f, "url")?.to_string(),
        headers: headers_of(f),
        codec: f
            .get(codec_key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        fps: as_f64(f, "fps").unwrap_or(0.0),
    })
}

fn find_format<'a>(root: &'a Value, format_id: &str) -> Option<&'a Value> {
    media_document(root)
        .get("formats")
        .and_then(Value::as_array)?
        .iter()
        .find(|f| as_str(f, "format_id") == Some(format_id))
}

/// Resolve a track selector (`"137+140"` or `"22"`) into concrete streams.
pub fn resolve_streams(root: &Value, track_id: &str) -> Result<StreamSet> {
    let mut video = None;
    let mut audio = None;
    let mut progressive = false;

    for part in track_id.split('+').filter(|p| !p.is_empty()) {
        let f = find_format(root, part).ok_or_else(|| {
            AppError::other(format!(
                "Format `{part}` is no longer offered for this video. Re-analyse the link and \
                 pick a quality again."
            ))
        })?;

        match (is_video(f), is_audio(f)) {
            (true, true) => {
                // Pre-muxed: one input carries both streams.
                progressive = true;
                video = spec_of(f, "vcodec");
                audio = spec_of(f, "acodec");
            }
            (true, false) => video = spec_of(f, "vcodec"),
            (false, true) => audio = spec_of(f, "acodec"),
            (false, false) => {}
        }
    }

    if video.is_none() && audio.is_none() {
        return Err(AppError::other(format!(
            "Could not resolve any stream for format `{track_id}`"
        )));
    }

    Ok(StreamSet {
        video,
        audio,
        progressive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_flat_playlist_search_entry() {
        let entry = json!({
            "id": "dQw4w9WgXcQ",
            "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "title": "Never Gonna Give You Up",
            "channel": "Rick Astley",
            "duration": 213.0,
            "view_count": 1_600_000_000.0,
            "thumbnails": [
                { "url": "https://i.ytimg.com/small.jpg", "width": 168 },
                { "url": "https://i.ytimg.com/medium.jpg", "width": 360 },
                { "url": "https://i.ytimg.com/maxres.jpg", "width": 1280 },
            ],
        });

        let hit = parse_search_entry(&entry).expect("entry should parse");
        assert_eq!(hit.id, "dQw4w9WgXcQ");
        assert_eq!(hit.title, "Never Gonna Give You Up");
        assert_eq!(hit.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(hit.duration, Some(213.0));
        assert!(!hit.live);
        // Capped: the 1280px still is rejected in favour of the 360px one.
        assert_eq!(hit.thumbnail.as_deref(), Some("https://i.ytimg.com/medium.jpg"));
    }

    #[test]
    fn search_entry_survives_missing_fields() {
        // Only the id is load-bearing; everything else has a sane absence, so a
        // sparse entry still reaches the list rather than being dropped.
        let hit = parse_search_entry(&json!({ "id": "abc123" })).expect("id alone is enough");
        assert_eq!(hit.title, "Untitled");
        assert_eq!(hit.channel, None);
        assert_eq!(hit.duration, None);
        assert_eq!(hit.thumbnail, None);
        assert_eq!(hit.url, "https://www.youtube.com/watch?v=abc123");

        assert!(parse_search_entry(&json!({ "title": "no id" })).is_none());
    }

    #[test]
    fn live_search_entries_report_no_duration() {
        let hit = parse_search_entry(&json!({
            "id": "livestream1",
            "title": "Live now",
            "is_live": true,
            // yt-dlp reports 0 for a stream that has not ended.
            "duration": 0.0,
        }))
        .expect("entry should parse");

        assert!(hit.live);
        assert_eq!(hit.duration, None, "a zero duration is an absence, not a length");
    }

    #[test]
    fn search_falls_back_to_the_single_thumbnail_field() {
        let hit = parse_search_entry(&json!({
            "id": "abc123",
            "thumbnail": "https://i.ytimg.com/only.jpg",
        }))
        .expect("entry should parse");

        assert_eq!(hit.thumbnail.as_deref(), Some("https://i.ytimg.com/only.jpg"));
    }

    #[test]
    fn search_args_do_not_suppress_the_playlist() {
        // `ytsearch:` results *are* a playlist, so --no-playlist would collapse
        // twenty hits down to one. It belongs to base_args and nothing else.
        assert!(!common_args().iter().any(|a| a == "--no-playlist"));
        assert!(base_args().iter().any(|a| a == "--no-playlist"));
    }

    #[test]
    fn normalises_every_supported_link_shape() {
        let expected = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
        for input in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "http://youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "youtube.com/watch?v=dQw4w9WgXcQ",
            "  dQw4w9WgXcQ  ",
        ] {
            assert_eq!(normalize_url(input).unwrap(), expected, "input: {input}");
        }
    }

    #[test]
    fn rejects_non_youtube_and_malformed_links() {
        for input in [
            "",
            "not a url",
            "https://vimeo.com/123456",
            "file:///etc/passwd",
            "https://www.youtube.com/watch?v=tooshort",
            "https://www.youtube.com/feed/subscriptions",
            "https://evil.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert!(normalize_url(input).is_err(), "should reject: {input}");
        }
    }

    fn sample_doc() -> Value {
        json!({
            "id": "dQw4w9WgXcQ",
            "title": "Test Video",
            "uploader": "Test Channel",
            "duration": 212.0,
            "webpage_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "thumbnails": [
                {"url": "https://i.ytimg.com/small.jpg", "width": 120, "height": 90},
                {"url": "https://i.ytimg.com/big.jpg", "width": 1280, "height": 720}
            ],
            "formats": [
                {"format_id": "sb0", "ext": "mhtml", "url": "https://x/sb", "vcodec": "none", "acodec": "none"},
                {"format_id": "140", "ext": "m4a", "url": "https://x/140", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 128.0, "tbr": 129.0,
                 "http_headers": {"User-Agent": "yt", "Range": "bytes=0-"}},
                {"format_id": "251", "ext": "webm", "url": "https://x/251", "vcodec": "none", "acodec": "opus", "abr": 160.0, "tbr": 160.0},
                {"format_id": "137", "ext": "mp4", "url": "https://x/137", "vcodec": "avc1.640028", "acodec": "none", "height": 1080, "fps": 30.0, "tbr": 4500.0},
                {"format_id": "299", "ext": "mp4", "url": "https://x/299", "vcodec": "avc1.64002a", "acodec": "none", "height": 1080, "fps": 60.0, "tbr": 6800.0},
                {"format_id": "313", "ext": "webm", "url": "https://x/313", "vcodec": "vp9", "acodec": "none", "height": 2160, "fps": 30.0, "tbr": 18000.0},
                {"format_id": "18",  "ext": "mp4", "url": "https://x/18",  "vcodec": "avc1.42001E", "acodec": "mp4a.40.2", "height": 360, "fps": 30.0, "tbr": 700.0}
            ]
        })
    }

    #[test]
    fn parses_metadata_and_builds_a_sorted_track_list() {
        let info = parse_info(&sample_doc()).unwrap();

        assert_eq!(info.title, "Test Video");
        assert_eq!(info.duration_seconds, 212.0);
        assert_eq!(info.thumbnail.as_deref(), Some("https://i.ytimg.com/big.jpg"));

        // Storyboards are dropped.
        assert!(!info.tracks.iter().any(|t| t.ext == "mhtml"));

        // Highest resolution first, audio-only last.
        assert_eq!(info.tracks.first().unwrap().height, 2160);
        assert!(info.tracks.last().unwrap().audio_only);

        // AAC beats Opus so MP4 stream-copy stays available.
        let hd = info.tracks.iter().find(|t| t.height == 1080 && t.fps == 60.0).unwrap();
        assert_eq!(hd.id, "299+140");
        assert!(hd.mp4_ready);
        assert_eq!(hd.label, "1080p60");

        // 30fps is not annotated.
        assert_eq!(
            info.tracks.iter().find(|t| t.height == 1080 && t.fps == 30.0).unwrap().label,
            "1080p"
        );

        // VP9 4K cannot be copied into MP4.
        assert!(!info.tracks.iter().find(|t| t.height == 2160).unwrap().mp4_ready);

        // 360p only exists as a progressive format, so it is kept.
        assert!(info.tracks.iter().any(|t| t.height == 360));
    }

    /// Trimmed from a real `yt-dlp -J` run against an x.com post. The shape is
    /// the point: the progressive MP4s name no codecs at all, and the
    /// audio-only renditions name only `vcodec: none`.
    fn x_doc() -> Value {
        json!({
            "id": "2016900969389289568",
            "title": "NASA - For the crew of Artemis II",
            "uploader": "NASA",
            "duration": 43.877,
            "webpage_url": "https://x.com/NASA/status/2016900969389289568",
            "formats": [
                {"format_id": "hls-audio-32000-Audio", "ext": "mp4", "url": "https://x/a32.m3u8",
                 "vcodec": "none", "tbr": 32.0},
                {"format_id": "hls-audio-128000-Audio", "ext": "mp4", "url": "https://x/a128.m3u8",
                 "vcodec": "none", "tbr": 128.0},
                {"format_id": "http-288", "ext": "mp4", "url": "https://x/288.mp4",
                 "height": 270, "width": 480, "tbr": 288.0},
                {"format_id": "hls-100", "ext": "mp4", "url": "https://x/100.m3u8",
                 "vcodec": "avc1.4D4015", "acodec": "none", "height": 270, "tbr": 100.12},
                {"format_id": "http-2176", "ext": "mp4", "url": "https://x/2176.mp4",
                 "height": 720, "width": 1280, "tbr": 2176.0},
                {"format_id": "hls-792", "ext": "mp4", "url": "https://x/792.m3u8",
                 "vcodec": "avc1.64001F", "acodec": "none", "height": 720, "tbr": 792.834}
            ]
        })
    }

    #[test]
    fn unreported_codecs_do_not_erase_every_format() {
        // The regression: absent `vcodec`/`acodec` were read as "no streams",
        // so X's progressive MP4s and audio renditions were all discarded and
        // the app reported that the site offered no downloadable formats.
        let info = parse_info(&x_doc()).unwrap();
        assert!(!info.tracks.is_empty(), "X offered formats; none survived parsing");

        // The direct MP4 wins at 720p: it outranks the HLS pair on bitrate, and
        // it is the one that answers the byte ranges the cut depends on.
        let best = info.tracks.first().unwrap();
        assert_eq!(best.id, "http-2176");
        assert_eq!(best.height, 720);
        assert!(!best.detail.contains("Video · Audio"), "detail: {}", best.detail);

        // The audio-only rendition names no codec either, and is still offered.
        let audio = info.tracks.last().unwrap();
        assert!(audio.audio_only);
        assert_eq!(audio.id, "hls-audio-128000-Audio");

        // A progressive format is still resolvable as one muxed input.
        let streams = resolve_streams(&x_doc(), "http-2176").unwrap();
        assert!(streams.progressive);
        assert_eq!(streams.video.as_ref().unwrap().url, "https://x/2176.mp4");
        // No codec to report — render.rs probes the media rather than guessing.
        assert!(streams.video.as_ref().unwrap().codec.is_empty());
    }

    #[test]
    fn a_post_holding_several_clips_resolves_to_the_first_playable_one() {
        // Instagram carousels and multi-video posts come back as a playlist,
        // which leaves nothing at the top level for `formats` to be read from.
        let doc = json!({
            "_type": "playlist",
            "id": "CabcDEF1234",
            "title": "Carousel post",
            "uploader": "someone",
            "entries": [
                {"id": "image-only", "title": "no media"},
                {
                    "id": "clip-2", "title": "Second slide", "duration": 12.5,
                    "webpage_url": "https://www.instagram.com/p/CabcDEF1234/",
                    "formats": [{"format_id": "dash-1", "ext": "mp4", "url": "https://ig/2.mp4",
                                 "vcodec": "avc1.4d401f", "acodec": "mp4a.40.2",
                                 "height": 1080, "tbr": 2400.0}]
                }
            ]
        });

        let info = parse_info(&doc).unwrap();
        assert_eq!(info.duration_seconds, 12.5);
        assert_eq!(info.tracks.len(), 1);
        assert_eq!(info.tracks[0].height, 1080);
        // Descriptive fields fall back to the post when the entry omits them.
        assert_eq!(info.uploader, "someone");

        // The render re-resolves the same URL, so it must land on the same entry.
        assert!(resolve_streams(&doc, "dash-1").is_ok());
    }

    #[test]
    fn resolves_adaptive_and_progressive_selectors() {
        let doc = sample_doc();

        let adaptive = resolve_streams(&doc, "299+140").unwrap();
        assert!(!adaptive.progressive);
        assert_eq!(adaptive.video.as_ref().unwrap().url, "https://x/299");
        assert_eq!(adaptive.audio.as_ref().unwrap().url, "https://x/140");
        // Range/Accept-Encoding are stripped; User-Agent survives.
        let audio_headers = &adaptive.audio.as_ref().unwrap().headers;
        assert!(audio_headers.iter().any(|(k, _)| k == "User-Agent"));
        assert!(!audio_headers.iter().any(|(k, _)| k == "Range"));

        let progressive = resolve_streams(&doc, "18").unwrap();
        assert!(progressive.progressive);
        assert_eq!(progressive.video.as_ref().unwrap().url, "https://x/18");

        let audio_only = resolve_streams(&doc, "140").unwrap();
        assert!(audio_only.video.is_none());
        assert!(audio_only.audio.is_some());

        assert!(resolve_streams(&doc, "9999").is_err());
    }
}
