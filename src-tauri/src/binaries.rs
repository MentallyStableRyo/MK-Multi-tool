//! Locating the three external tools the app drives: yt-dlp, ffmpeg, ffprobe.
//!
//! Resolution order, first hit wins:
//!   1. Next to the running executable — where Tauri installs `externalBin`
//!      sidecars (the `-<triple>` suffix is stripped at bundle time).
//!   2. The bundle resource directory (macOS `.app/Contents/Resources`).
//!   3. `src-tauri/binaries/<name>[-<triple>]` — the `tauri dev` layout.
//!   4. The system `PATH`, so a Homebrew/apt/winget install just works.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};
use crate::proc;

/// Target triple this binary was compiled for, injected by `build.rs`.
const TARGET_TRIPLE: &str = env!("MK_TARGET_TRIPLE");

#[cfg(windows)]
const EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const EXE_SUFFIX: &str = "";

pub const YTDLP: &str = "yt-dlp";
pub const FFMPEG: &str = "ffmpeg";
pub const FFPROBE: &str = "ffprobe";

/// Resolved absolute paths to every tool the pipeline needs.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Per-tool discovery result surfaced to the UI on startup.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub name: String,
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolchainStatus {
    pub ready: bool,
    pub tools: Vec<ToolStatus>,
}

/// Every location worth checking for `stem`, in priority order.
fn candidates(app: &AppHandle, stem: &str) -> Vec<PathBuf> {
    let file = format!("{stem}{EXE_SUFFIX}");
    let dev_file = format!("{stem}-{TARGET_TRIPLE}{EXE_SUFFIX}");
    let mut out = Vec::with_capacity(6);

    // 1. Alongside the executable (installed sidecar).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join(&file));
        }
    }

    // 2. Bundle resources.
    if let Ok(dir) = app.path().resource_dir() {
        out.push(dir.join(&file));
        out.push(dir.join("binaries").join(&file));
    }

    // 3. Dev-tree vendored binaries. CARGO_MANIFEST_DIR is baked in at compile
    //    time and points at src-tauri/, which is exactly where `binaries/` lives.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries");
    out.push(manifest.join(&dev_file));
    out.push(manifest.join(&file));

    out
}

/// Locate a single tool, or `None` if it is nowhere to be found.
pub fn locate(app: &AppHandle, stem: &str) -> Option<PathBuf> {
    for candidate in candidates(app, stem) {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // 4. PATH fallback.
    which::which(stem).ok()
}

fn require(app: &AppHandle, stem: &str) -> Result<PathBuf> {
    locate(app, stem).ok_or_else(|| AppError::MissingBinary {
        name: stem.to_string(),
    })
}

/// Resolve the full toolchain, failing with a named missing binary.
pub fn resolve(app: &AppHandle) -> Result<Toolchain> {
    Ok(Toolchain {
        ytdlp: require(app, YTDLP)?,
        ffmpeg: require(app, FFMPEG)?,
        ffprobe: require(app, FFPROBE)?,
    })
}

/// First line of `<tool> --version`, trimmed. Best-effort: `None` if the tool
/// cannot be executed at all (wrong architecture, missing exec bit, …).
async fn version_of(path: &Path, flag: &str) -> Option<String> {
    let mut cmd = proc::command(path);
    cmd.arg(flag);
    let out = proc::run_capture(cmd, &CancellationToken::new()).await.ok()?;
    let text = if out.stdout.trim().is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    text.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty())
}

/// Probe all three tools for the startup diagnostics panel.
pub async fn status(app: &AppHandle) -> ToolchainStatus {
    let mut tools = Vec::with_capacity(3);

    for stem in [YTDLP, FFMPEG, FFPROBE] {
        match locate(app, stem) {
            Some(path) => {
                let version = version_of(&path, "--version").await;
                tools.push(ToolStatus {
                    name: stem.to_string(),
                    // A binary that exists but cannot report a version is broken
                    // (bad architecture, quarantined, not executable).
                    found: version.is_some(),
                    path: Some(path.to_string_lossy().into_owned()),
                    version,
                });
            }
            None => tools.push(ToolStatus {
                name: stem.to_string(),
                found: false,
                path: None,
                version: None,
            }),
        }
    }

    ToolchainStatus {
        ready: tools.iter().all(|t| t.found),
        tools,
    }
}
