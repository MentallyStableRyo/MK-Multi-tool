//! Keeping yt-dlp current.
//!
//! ## Why this exists
//!
//! YouTube's anti-download measures move faster than any release cadence we
//! could ship on. The failure they produce is not subtle: in August 2026 the
//! `ANDROID_VR` player client — the one yt-dlp had been defaulting to — was
//! clamped to serving only the first 10 MiB of any stream. Every ranged request
//! past that offset answers `403 Forbidden`, so FFmpeg dies at input-open time
//! for any slice that does not start near the beginning of a short video. The
//! fix (switching to the `VISIONOS` client) landed in yt-dlp within days, but
//! *not* in a stable release: the newest stable at the time, `2026.07.04`, was
//! itself the broken one.
//!
//! That is the whole argument for this module. A yt-dlp pinned at bundle time
//! is a bomb with a several-week fuse, and pointing users at "reinstall the
//! app" is not a fix when the app they reinstall carries the same stale binary.
//!
//! ## How it works
//!
//! yt-dlp already knows how to update itself — `--update-to <channel>` picks the
//! right platform asset, verifies the download against the release hashes, and
//! swaps the executable atomically. Re-implementing that here would mean an
//! HTTP client, GitHub API parsing, and hash verification, all to do worse what
//! the tool already does well. So this module's job is only to give yt-dlp
//! somewhere it is *allowed* to write, and to decide when to ask:
//!
//! 1. Seed a private copy at `<app-data>/tools/yt-dlp[.exe]` from the bundled
//!    sidecar. The sidecar itself lives beside the executable — under
//!    `C:\Program Files` or inside a signed `.app` — where a self-update would
//!    need elevation, or would break the bundle signature.
//! 2. Run `--update-to nightly` against that copy.
//! 3. Record the attempt so a user who restarts the app six times in a row
//!    makes one network request, not six.
//!
//! [`crate::binaries::locate`] prefers the copy over the sidecar, so everything
//! downstream picks up the update with no further plumbing.
//!
//! ## Why the nightly channel
//!
//! Because stable was the broken one. Extractor fixes land in nightly the day
//! they are written and reach stable weeks later, which is the wrong end of the
//! trade for a tool whose only job is to keep working against a hostile,
//! fast-moving target. Nightly builds are cut from the same repository and the
//! same test suite; the risk being taken is a regression in a build that is
//! days old, against the certainty of breakage in one that is months old.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use crate::binaries;
use crate::proc;

/// Release channel passed to `--update-to`. See the module docs for why this is
/// not `stable`.
const CHANNEL: &str = "nightly";

/// Minimum gap between update checks.
///
/// The check costs one GitHub API request and a couple of seconds, which is
/// nothing once a day and rude on every launch — the app is a tool people open
/// and close repeatedly, and unauthenticated GitHub API requests are capped at
/// 60 per hour per IP. Twelve hours keeps a daily user current without ever
/// making a burst of restarts look like abuse.
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Ceiling on one update attempt. A yt-dlp binary is ~18 MB, so a minute is
/// generous on a slow connection and still bounded enough that a hung request
/// cannot keep the task alive for the life of the process.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Where the writable copy lives, relative to the app-data directory.
const TOOLS_DIR: &str = "tools";

/// Marks which bundled build the copy was seeded from — see [`needs_seeding`].
const SEED_MARKER: &str = "seeded-from.txt";

/// Timestamp of the last completed check, as seconds since the Unix epoch.
const STAMP_FILE: &str = "last-checked.txt";

/// What happened, in terms the UI can show without interpreting prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateOutcome {
    /// yt-dlp replaced itself with a newer build.
    Updated,
    /// Already on the newest build in the channel.
    Current,
    /// Checked recently enough that this launch did not ask again.
    Skipped,
    /// The check could not be completed. The existing binary is untouched and
    /// perfectly usable — this is a note, never an error the user must act on.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReport {
    pub outcome: UpdateOutcome,
    /// Version in use after this run, when it could be determined.
    pub version: Option<String>,
    /// One line suitable for a status tooltip.
    pub detail: String,
}

impl UpdateReport {
    fn new(outcome: UpdateOutcome, version: Option<String>, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            version,
            detail: detail.into(),
        }
    }
}

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

/// Directory holding self-updated tools, created on demand.
///
/// `None` when the platform gives us no app-data directory, in which case
/// updating is simply unavailable and the bundled sidecar is used as-is.
pub fn tools_dir(app: &AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().ok()?.join(TOOLS_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Path the updated yt-dlp is kept at, whether or not it exists yet.
pub fn managed_ytdlp(app: &AppHandle) -> Option<PathBuf> {
    Some(tools_dir(app)?.join(format!("{}{EXE_SUFFIX}", binaries::YTDLP)))
}

/// Check for a newer yt-dlp and install it, honouring [`CHECK_INTERVAL`].
///
/// Never returns `Err`: a failed update is a note, not a failure of the app.
/// Whatever yt-dlp was already there keeps working, so every problem here
/// degrades to "we are running a slightly older extractor".
pub async fn refresh(app: &AppHandle, cancel: &CancellationToken) -> UpdateReport {
    refresh_inner(app, cancel, false).await
}

/// Same, but ignoring the interval — for a user who explicitly asked.
pub async fn refresh_now(app: &AppHandle, cancel: &CancellationToken) -> UpdateReport {
    refresh_inner(app, cancel, true).await
}

async fn refresh_inner(app: &AppHandle, cancel: &CancellationToken, force: bool) -> UpdateReport {
    let Some(target) = managed_ytdlp(app) else {
        return UpdateReport::new(
            UpdateOutcome::Failed,
            None,
            "no writable app-data directory, so yt-dlp cannot be kept up to date",
        );
    };

    if let Err(e) = seed(app, &target).await {
        return UpdateReport::new(UpdateOutcome::Failed, None, e);
    }

    if !force && !due(&target) {
        return UpdateReport::new(
            UpdateOutcome::Skipped,
            version_of(&target, cancel).await,
            "yt-dlp was checked within the last 12 hours",
        );
    }

    let before = version_of(&target, cancel).await;

    let mut cmd = proc::command(&target);
    cmd.arg("--update-to").arg(CHANNEL);

    let run = tokio::time::timeout(TIMEOUT, proc::run_capture(cmd, cancel));

    let out = match run.await {
        Err(_) => {
            return UpdateReport::new(
                UpdateOutcome::Failed,
                before,
                "the yt-dlp update timed out",
            )
        }
        Ok(Err(e)) => {
            return UpdateReport::new(
                UpdateOutcome::Failed,
                before,
                format!("could not run the yt-dlp updater: {e}"),
            )
        }
        Ok(Ok(out)) => out,
    };

    if !out.ok() {
        // A failed update leaves the previous binary in place — yt-dlp writes
        // the replacement beside the original and only then swaps it.
        return UpdateReport::new(
            UpdateOutcome::Failed,
            before,
            summarize_failure(&out.stdout, &out.stderr),
        );
    }

    stamp(&target);

    let after = version_of(&target, cancel).await;
    let changed = matches!((&before, &after), (Some(a), Some(b)) if a != b);

    if changed {
        let from = before.unwrap_or_else(|| "an older build".into());
        let to = after.clone().unwrap_or_else(|| CHANNEL.into());
        UpdateReport::new(
            UpdateOutcome::Updated,
            after,
            format!("yt-dlp updated from {from} to {to}"),
        )
    } else {
        let at = after.clone().unwrap_or_else(|| CHANNEL.into());
        UpdateReport::new(
            UpdateOutcome::Current,
            after,
            format!("yt-dlp is up to date ({at})"),
        )
    }
}

/// Copy the bundled sidecar into the writable directory if needed.
///
/// Re-seeds when the app itself has been upgraded and now carries a different
/// bundled build, which is the one case where "the copy is newer" can be wrong:
/// a user who has not launched the app in months would otherwise keep limping
/// along on a stale copy while a fresher binary sits unused in the bundle.
async fn seed(app: &AppHandle, target: &Path) -> std::result::Result<(), String> {
    let Some(bundled) = binaries::bundled_ytdlp(app) else {
        // Nothing to seed from. If a copy already exists it is still usable —
        // this only means the sidecar is missing and PATH is doing the work.
        return if target.is_file() {
            Ok(())
        } else {
            Err("yt-dlp is not installed, so there is nothing to update".into())
        };
    };

    let stamp = bundled_identity(&bundled);
    let marker = target.with_file_name(SEED_MARKER);

    if target.is_file() && std::fs::read_to_string(&marker).ok().as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }

    std::fs::copy(&bundled, target)
        .map_err(|e| format!("could not copy yt-dlp into the app data directory: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
    }

    let _ = std::fs::write(&marker, stamp);
    // A fresh seed deserves an immediate check, whatever the old stamp said.
    let _ = std::fs::remove_file(target.with_file_name(STAMP_FILE));
    Ok(())
}

/// Cheap fingerprint of the bundled binary: size and mtime.
///
/// Hashing 18 MB on every launch to detect a change that only happens on app
/// upgrade would be a poor trade; a differing size or mtime is enough to notice
/// that the bundle was replaced.
fn bundled_identity(path: &Path) -> String {
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{size}:{modified}")
}

/// Has [`CHECK_INTERVAL`] elapsed since the last completed check?
fn due(target: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(target.with_file_name(STAMP_FILE)) else {
        return true;
    };
    let Ok(last) = text.trim().parse::<u64>() else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // A stamp in the future means the clock moved backwards; check anyway
    // rather than locking the user out of updates until it catches up.
    now < last || now - last >= CHECK_INTERVAL.as_secs()
}

fn stamp(target: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(target.with_file_name(STAMP_FILE), now.to_string());
}

async fn version_of(path: &Path, cancel: &CancellationToken) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let mut cmd = proc::command(path);
    cmd.arg("--version");
    let out = proc::run_capture(cmd, cancel).await.ok()?;
    out.stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
}

/// Turn the updater's output into one line worth showing.
///
/// yt-dlp reports update problems on stdout as often as stderr, and the useful
/// line is the one it prefixes with `ERROR:`.
fn summarize_failure(stdout: &str, stderr: &str) -> String {
    let flagged = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|l| l.starts_with("ERROR:") || l.starts_with("WARNING:"));

    match flagged {
        Some(line) => format!("yt-dlp could not update: {line}"),
        None => "yt-dlp could not update — continuing with the installed build".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mk-updater-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_stamp_means_a_check_is_due() {
        let dir = temp_dir("no-stamp");
        assert!(due(&dir.join("yt-dlp")));
    }

    #[test]
    fn a_recent_stamp_suppresses_the_check() {
        let dir = temp_dir("recent");
        let target = dir.join("yt-dlp");
        stamp(&target);
        assert!(!due(&target));
    }

    #[test]
    fn an_old_stamp_lets_the_check_through() {
        let dir = temp_dir("old");
        let target = dir.join("yt-dlp");
        let long_ago = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - CHECK_INTERVAL.as_secs()
            - 1;
        std::fs::write(target.with_file_name(STAMP_FILE), long_ago.to_string()).unwrap();
        assert!(due(&target));
    }

    #[test]
    fn a_corrupt_or_future_stamp_never_blocks_updating() {
        let dir = temp_dir("corrupt");
        let target = dir.join("yt-dlp");

        std::fs::write(target.with_file_name(STAMP_FILE), "not a number").unwrap();
        assert!(due(&target));

        let far_future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 999_999;
        std::fs::write(target.with_file_name(STAMP_FILE), far_future.to_string()).unwrap();
        assert!(due(&target));
    }

    #[test]
    fn failures_quote_the_line_yt_dlp_flagged() {
        let detail = summarize_failure(
            "Current version: nightly@2026.08.18\nERROR: Unable to fetch the release info\n",
            "",
        );
        assert!(detail.contains("Unable to fetch the release info"));

        // Silence still has to say something actionable-shaped.
        assert!(!summarize_failure("", "").is_empty());
    }

    #[test]
    fn the_identity_stamp_changes_when_the_bundle_does() {
        let dir = temp_dir("identity");
        let file = dir.join("yt-dlp");

        std::fs::write(&file, b"one").unwrap();
        let first = bundled_identity(&file);

        std::fs::write(&file, b"a longer payload").unwrap();
        assert_ne!(first, bundled_identity(&file));

        // A path that does not exist must still produce a stable string rather
        // than panicking — seeding runs before anything is guaranteed present.
        assert!(!bundled_identity(&dir.join("absent")).is_empty());
    }
}
