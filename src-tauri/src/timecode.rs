//! Timecode parsing, formatting, and range validation.
//!
//! The frontend validates too, but never trust it: these checks are the
//! authoritative ones, because a malformed range reaching FFmpeg produces
//! either a zero-byte file or a full-length download.

use crate::error::{AppError, Result};

/// Shortest slice we will accept. Below this FFmpeg often emits an empty file.
pub const MIN_SLICE_SECONDS: f64 = 0.10;

/// Format seconds as `HH:MM:SS.mmm` — the unambiguous form for FFmpeg's `-ss`.
pub fn format_ts(seconds: f64) -> String {
    let clamped = seconds.max(0.0);
    let total_ms = (clamped * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let s = total_s % 60;
    let m = (total_s / 60) % 60;
    let h = total_s / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// A validated, in-bounds slice range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

/// Validate a range against the source duration.
///
/// `duration` of `0.0` means "unknown" (some livestream VODs report nothing),
/// in which case the upper bound is not enforced.
pub fn validate(start: f64, end: f64, duration: f64) -> Result<Range> {
    if !start.is_finite() || !end.is_finite() {
        return Err(AppError::TimeRange("Timecodes must be real numbers".into()));
    }
    if start < 0.0 {
        return Err(AppError::TimeRange("Start time cannot be negative".into()));
    }
    if end <= start {
        return Err(AppError::TimeRange(format!(
            "End time ({}) must be after start time ({})",
            format_ts(end),
            format_ts(start)
        )));
    }
    if end - start < MIN_SLICE_SECONDS {
        return Err(AppError::TimeRange(format!(
            "Slice is too short — minimum is {MIN_SLICE_SECONDS:.2}s"
        )));
    }
    if duration > 0.0 {
        if start >= duration {
            return Err(AppError::TimeRange(format!(
                "Start time ({}) is past the end of the video ({})",
                format_ts(start),
                format_ts(duration)
            )));
        }
        if end > duration + 0.5 {
            return Err(AppError::TimeRange(format!(
                "End time ({}) exceeds the video duration ({})",
                format_ts(end),
                format_ts(duration)
            )));
        }
    }

    // Clamp a slightly-over end back onto the timeline rather than rejecting it;
    // reported durations are routinely a few hundred ms off the real stream.
    let end = if duration > 0.0 { end.min(duration) } else { end };

    Ok(Range { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip() {
        assert_eq!(format_ts(3723.5), "01:02:03.500");
        assert_eq!(format_ts(0.0), "00:00:00.000");
        assert_eq!(format_ts(-4.0), "00:00:00.000");
    }

    #[test]
    fn range_validation_enforces_bounds() {
        assert!(validate(10.0, 20.0, 100.0).is_ok());
        assert!(validate(20.0, 10.0, 100.0).is_err(), "end before start");
        assert!(validate(10.0, 10.0, 100.0).is_err(), "zero length");
        assert!(validate(150.0, 160.0, 100.0).is_err(), "start past end");
        assert!(validate(10.0, 400.0, 100.0).is_err(), "end past duration");
        assert!(validate(10.0, 20.0, 0.0).is_ok(), "unknown duration is unbounded");
    }

    #[test]
    fn end_is_clamped_within_reported_tolerance() {
        let r = validate(10.0, 100.3, 100.0).unwrap();
        assert_eq!(r.end, 100.0);
    }
}
