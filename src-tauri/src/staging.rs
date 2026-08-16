//! Atomic output handling.
//!
//! The render pipeline is single-pass — FFmpeg reads from the CDN and writes
//! the finished slice directly — so there are no intermediate media files to
//! manage. The one transient artifact is the staging file each render writes
//! before being moved into place, and this module owns its whole lifetime.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::Result;

/// Suffix appended to the destination while a render is in flight.
const STAGING_SUFFIX: &str = ".mk-part";

/// Staging files older than this belong to a process that was killed outright.
const ORPHAN_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// A render target written under a [`STAGING_SUFFIX`] name and moved onto the
/// destination only once the render succeeds.
///
/// This gives the user's chosen path all-or-nothing semantics: a cancelled or
/// failed job never leaves a half-written `.mp4` that looks playable until you
/// scrub to the end. Staging beside the destination (rather than in a cache
/// directory) keeps the commit a same-volume rename — atomic and instant, with
/// no second pass over the bytes.
#[derive(Debug)]
pub struct PartialOutput {
    staging: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl PartialOutput {
    pub fn new(destination: PathBuf) -> Self {
        let mut staging = destination.clone().into_os_string();
        staging.push(STAGING_SUFFIX);
        Self {
            staging: PathBuf::from(staging),
            destination,
            committed: false,
        }
    }

    /// Path FFmpeg should write to.
    pub fn staging_path(&self) -> &Path {
        &self.staging
    }

    /// Move the finished render onto the destination path.
    pub fn commit(mut self) -> Result<PathBuf> {
        // Windows rejects a rename onto an existing file; POSIX would silently
        // replace it. Removing first makes both platforms behave the same.
        if self.destination.exists() {
            std::fs::remove_file(&self.destination)?;
        }
        std::fs::rename(&self.staging, &self.destination)?;
        self.committed = true;
        Ok(self.destination.clone())
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort: on Windows a still-dying child can briefly hold the
            // handle open. `sweep_orphans` catches whatever we miss here.
            let _ = std::fs::remove_file(&self.staging);
        }
    }
}

/// Remove staging files a previous run left behind in `dir`.
///
/// `Drop` covers every path the process controls; this covers the ones it does
/// not — a SIGKILL, a power cut, an OS update reboot. Deliberately narrow: only
/// files carrying our own suffix, only ones old enough that no live job could
/// own them. Never fails the caller, because a cleanup problem must not stop a
/// render from starting.
pub fn sweep_orphans(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let path = entry.path();

        let is_ours = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(STAGING_SUFFIX));
        if !is_ours {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let aged_out = metadata
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > ORPHAN_AFTER);

        if aged_out {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mk-multitool-test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn partial_output_is_discarded_unless_committed() {
        let dir = scratch("abandon");
        let target = dir.join("clip.mp4");

        {
            let partial = PartialOutput::new(target.clone());
            std::fs::write(partial.staging_path(), b"half a render").unwrap();
        }

        assert!(!target.exists(), "destination must stay untouched");
        assert!(
            !dir.join("clip.mp4.mk-part").exists(),
            "staging file must be cleaned up on drop"
        );
    }

    #[test]
    fn commit_moves_the_render_into_place() {
        let dir = scratch("commit");
        let target = dir.join("clip.mp4");

        let partial = PartialOutput::new(target.clone());
        std::fs::write(partial.staging_path(), b"finished render").unwrap();
        let done = partial.commit().unwrap();

        assert_eq!(done, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"finished render");
        assert!(!dir.join("clip.mp4.mk-part").exists());
    }

    #[test]
    fn commit_overwrites_an_existing_destination() {
        let dir = scratch("overwrite");
        let target = dir.join("clip.mp4");
        std::fs::write(&target, b"previous version").unwrap();

        let partial = PartialOutput::new(target.clone());
        std::fs::write(partial.staging_path(), b"new version").unwrap();
        partial.commit().unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new version");
    }

    #[test]
    fn sweep_only_touches_aged_staging_files() {
        let dir = scratch("sweep");

        let fresh = dir.join("recent.mp4.mk-part");
        let unrelated = dir.join("holiday.mp4");
        std::fs::write(&fresh, b"in flight").unwrap();
        std::fs::write(&unrelated, b"user's actual video").unwrap();

        sweep_orphans(&dir);

        // Neither is old enough / ours, so both must survive.
        assert!(fresh.exists(), "a staging file from a live job must survive");
        assert!(unrelated.exists(), "unrelated files must never be touched");
    }
}
