//! Shared FFmpeg/FFprobe input-argument construction.
//!
//! Both the prober and the renderer open the same remote CDN streams, so the
//! header forwarding and reconnect policy live in one place — a mismatch
//! between them shows up as "ffprobe worked but the render 403s".

use std::path::Path;

use crate::timecode::format_ts;
use crate::youtube::StreamSpec;

/// Options accepted only by the HTTP protocol. Passing them to a `file:` input
/// makes FFmpeg abort with "Option not found", hence the protocol check.
fn http_resilience() -> Vec<String> {
    [
        "-reconnect",
        "1",
        "-reconnect_streamed",
        "1",
        "-reconnect_on_network_error",
        "1",
        "-reconnect_delay_max",
        "10",
        // Reuse the connection across ranged reads. Seeking a DASH stream issues
        // several requests; without this each one pays a fresh TLS handshake.
        "-multiple_requests",
        "1",
        // 30s without progress on a read is a dead connection, not slow media.
        "-rw_timeout",
        "30000000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Input arguments for a remote stream, ending in `-i <url>`.
///
/// `seek` is applied as an *input* option, which makes FFmpeg issue a ranged
/// request and skip straight to the containing keyframe — this is what keeps a
/// slice at 01:23:00 of a 3-hour video from downloading the first 83 minutes.
pub fn remote_input(spec: &StreamSpec, seek: Option<f64>) -> Vec<String> {
    let mut args = Vec::with_capacity(16);

    if let Some(ua) = spec.header("User-Agent") {
        args.push("-user_agent".into());
        args.push(ua.to_string());
    }

    // Everything except User-Agent goes through -headers as a CRLF-joined blob.
    let blob: String = spec
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("user-agent"))
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    if !blob.is_empty() {
        args.push("-headers".into());
        args.push(blob);
    }

    if spec.is_http() {
        args.extend(http_resilience());
    }

    if let Some(t) = seek.filter(|t| *t > 0.0) {
        args.push("-ss".into());
        args.push(format_ts(t));
    }

    args.push("-i".into());
    args.push(spec.url.clone());
    args
}

/// Input arguments for a file already on disk.
pub fn local_input(path: &Path, seek: Option<f64>) -> Vec<String> {
    let mut args = Vec::with_capacity(4);
    if let Some(t) = seek.filter(|t| *t > 0.0) {
        args.push("-ss".into());
        args.push(format_ts(t));
    }
    args.push("-i".into());
    args.push(path.to_string_lossy().into_owned());
    args
}
