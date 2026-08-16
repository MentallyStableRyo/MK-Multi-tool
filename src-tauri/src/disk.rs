//! Free-space verification, run before any write is started.
//!
//! Rationale: a half-written 4K slice that dies at 98% because the volume filled
//! up is the single most annoying failure mode of a downloader. Checking up
//! front costs microseconds.

use std::path::{Path, PathBuf};

use sysinfo::Disks;

use crate::error::{AppError, Result};

/// Headroom multiplier over the estimated payload: the intermediate file plus
/// the muxed output can both be live at once, plus container overhead.
pub const SPACE_SAFETY_FACTOR: f64 = 2.4;

/// Never demand less than this, so tiny clips still get a sane sanity check.
pub const MIN_REQUIRED_BYTES: u64 = 192 * 1024 * 1024;

/// Walk up until we hit a directory that exists — the target file itself, and
/// possibly its parent, may not have been created yet.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut cur = path;
    loop {
        if cur.exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p,
            _ => return PathBuf::from("."),
        }
    }
}

/// Bytes available on the volume containing `path`, or `None` if the platform
/// did not report a matching mount point.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let probe = nearest_existing(path);
    let probe = std::fs::canonicalize(&probe).unwrap_or(probe);

    let disks = Disks::new_with_refreshed_list();

    // Longest matching mount point wins: on Linux, `/home` must beat `/`.
    disks
        .list()
        .iter()
        .filter(|d| probe.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Human label for the volume, used in the error message.
fn volume_label(path: &Path) -> String {
    let probe = nearest_existing(path);
    let probe = std::fs::canonicalize(&probe).unwrap_or(probe);

    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| probe.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.mount_point().to_string_lossy().into_owned())
        .unwrap_or_else(|| probe.to_string_lossy().into_owned())
}

/// Fail fast unless `needed` bytes are available on `path`'s volume.
///
/// If the platform cannot report free space we allow the operation: refusing to
/// work because of a missing statistic would be worse than the failure we are
/// trying to prevent.
pub fn ensure_free(path: &Path, needed: u64) -> Result<()> {
    let needed = needed.max(MIN_REQUIRED_BYTES);

    let Some(available) = available_bytes(path) else {
        return Ok(());
    };

    if available >= needed {
        return Ok(());
    }

    Err(AppError::DiskSpace {
        volume: volume_label(path),
        needed_mb: needed / (1024 * 1024),
        available_mb: available / (1024 * 1024),
    })
}

/// Estimated payload for a slice, given its media bitrate and duration.
pub fn estimate_bytes(bitrate_kbps: f64, seconds: f64) -> u64 {
    if bitrate_kbps <= 0.0 || seconds <= 0.0 {
        return MIN_REQUIRED_BYTES;
    }
    let bytes = (bitrate_kbps * 1000.0 / 8.0) * seconds * SPACE_SAFETY_FACTOR;
    bytes.max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_scales_with_bitrate_and_duration() {
        // 8000 kbps for 60s ≈ 60 MB of payload, times the safety factor.
        let bytes = estimate_bytes(8000.0, 60.0);
        assert!(bytes > 100 * 1024 * 1024, "got {bytes}");
        assert!(bytes < 200 * 1024 * 1024, "got {bytes}");
    }

    #[test]
    fn degenerate_inputs_fall_back_to_the_floor() {
        assert_eq!(estimate_bytes(0.0, 60.0), MIN_REQUIRED_BYTES);
        assert_eq!(estimate_bytes(8000.0, 0.0), MIN_REQUIRED_BYTES);
    }

    #[test]
    fn nearest_existing_walks_up_to_a_real_directory() {
        let fake = std::env::temp_dir().join("mk-does-not-exist-xyz/deeper/file.mp4");
        assert!(nearest_existing(&fake).exists());
    }
}
