//! Locating the external tools the app drives.
//!
//! Tools fall into two classes, and they resolve in opposite directions.
//!
//! **System tools** — yt-dlp, ffmpeg, ffprobe. These are *not* bundled. The
//! user installs them with their own package manager and the app finds them.
//! Three reasons: yt-dlp breaks whenever YouTube changes and has to be updated
//! on its own cadence, not ours; FFmpeg is ~140 MB per binary and the installer
//! was already fighting a 2 GB ceiling; and shipping GPL FFmpeg builds inside a
//! closed bundle is a licensing question that simply does not arise when the
//! user supplies their own. They resolve `PATH` first — see [`locate_system`].
//!
//! **Bundled tools** — pandoc, whisper-cli, PDFium. These have no useful
//! system install to defer to, so they travel with the app and resolve from the
//! bundle first, falling back to `PATH` for a machine that happens to have one.
//!
//! Bundle-relative lookup order, first hit wins:
//!   1. Next to the running executable — where Tauri installs `externalBin`
//!      sidecars (the `-<triple>` suffix is stripped at bundle time).
//!   2. The bundle resource directory (macOS `.app/Contents/Resources`).
//!   3. `src-tauri/binaries/<name>[-<triple>]` — the `tauri dev` layout.
//!
//! A system tool still checks those three afterwards, so a dev tree that ran
//! `npm run binaries` keeps working without a system install.

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
pub const WHISPER: &str = "whisper-cli";
pub const PANDOC: &str = "pandoc";

/// True for the tools the user is expected to install themselves.
fn is_system_tool(stem: &str) -> bool {
    matches!(stem, YTDLP | FFMPEG | FFPROBE)
}

/// Well-known install directories to check when a system tool is not on `PATH`.
///
/// This exists because of one specific trap: a GUI app launched from Finder,
/// the Dock, or Spotlight inherits launchd's minimal `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) rather than the shell's, so Homebrew's
/// `/opt/homebrew/bin` is invisible to `which` even though `ffmpeg` runs fine
/// in the user's terminal. The equivalent gap on Windows is a package manager
/// whose shim directory was added to `PATH` after the app was launched, and on
/// Linux a `~/.local/bin` that only the login shell exports.
fn system_dirs() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(8);
    let home = dirs::home_dir();

    #[cfg(target_os = "macos")]
    {
        // Apple Silicon Homebrew, Intel Homebrew, MacPorts.
        out.push(PathBuf::from("/opt/homebrew/bin"));
        out.push(PathBuf::from("/usr/local/bin"));
        out.push(PathBuf::from("/opt/local/bin"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        out.push(PathBuf::from("/usr/local/bin"));
        out.push(PathBuf::from("/usr/bin"));
        out.push(PathBuf::from("/snap/bin"));
    }

    #[cfg(windows)]
    {
        // winget's shim directory, Chocolatey, Scoop, and the two places a
        // hand-unzipped FFmpeg conventionally lands.
        if let Some(local) = dirs::data_local_dir() {
            out.push(local.join("Microsoft").join("WinGet").join("Links"));
        }
        if let Ok(data) = std::env::var("ProgramData") {
            out.push(Path::new(&data).join("chocolatey").join("bin"));
        }
        if let Some(ref home) = home {
            out.push(home.join("scoop").join("shims"));
        }
        if let Ok(files) = std::env::var("ProgramFiles") {
            out.push(Path::new(&files).join("ffmpeg").join("bin"));
        }
        out.push(PathBuf::from(r"C:\ffmpeg\bin"));
    }

    if let Some(home) = home {
        out.push(home.join(".local").join("bin"));
    }
    out
}

/// Find a tool the way a user would expect: `PATH` first, then the handful of
/// directories a package manager might have installed it into without the app
/// seeing the updated `PATH`.
///
/// `which_global` rather than `which`: the latter also searches the process's
/// current directory, which for a GUI app is whatever the OS happened to hand
/// it — not somewhere a user deliberately put a trusted `ffmpeg.exe`.
pub fn locate_system(stem: &str) -> Option<PathBuf> {
    if let Ok(found) = which::which_global(stem) {
        return Some(found);
    }
    let file = format!("{stem}{EXE_SUFFIX}");
    system_dirs().into_iter().map(|dir| dir.join(&file)).find(|p| p.is_file())
}

/// How to install a system tool, phrased for the host platform.
///
/// Naming the actual command matters more than it looks: "install ffmpeg and
/// put it on your PATH" is advice, `winget install ffmpeg` is something the
/// user can paste.
pub fn install_hint(name: &str) -> Option<String> {
    let (tool, command) = match name {
        // ffprobe has no package of its own — it ships inside FFmpeg, so a
        // missing ffprobe means the same install as a missing ffmpeg.
        FFMPEG | FFPROBE => (
            "FFmpeg",
            if cfg!(windows) {
                "winget install ffmpeg"
            } else if cfg!(target_os = "macos") {
                "brew install ffmpeg"
            } else {
                "sudo apt install ffmpeg"
            },
        ),
        YTDLP => (
            "yt-dlp",
            if cfg!(windows) {
                "winget install yt-dlp.yt-dlp"
            } else if cfg!(target_os = "macos") {
                "brew install yt-dlp"
            } else {
                "sudo apt install yt-dlp"
            },
        ),
        _ => return None,
    };

    let note = if name == FFPROBE {
        " (ffprobe ships with FFmpeg)"
    } else {
        ""
    };
    // Linux has no single right command, so apt is offered as an example
    // rather than an instruction a Fedora or Arch user would have to translate.
    let how = if cfg!(any(windows, target_os = "macos")) {
        format!("Install it with `{command}`")
    } else {
        format!("Install it with your package manager (e.g. `{command}`)")
    };
    Some(format!("{tool} is not installed{note}. {how}, then restart the app."))
}

/// whisper.cpp is not a lone sidecar.
///
/// `whisper-cli` dynamically loads `whisper.dll`/`libwhisper.so` and a set of
/// `ggml-cpu-*` backends chosen at runtime from the host's instruction set, so
/// it only works with its siblings beside it. Tauri's `externalBin` lays down
/// single renamed files and cannot express that, so the whole directory travels
/// as a bundle resource and is resolved here instead.
const WHISPER_DIR: &str = "whisper";

/// Speech models live beside the binary that reads them.
const MODEL_DIR: &str = "models";

/// Resolved absolute paths to every tool the pipeline needs.
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub ytdlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Toolchain {
    /// A placeholder for General Converter jobs that never touch FFmpeg,
    /// FFprobe, or yt-dlp — most of it (images, documents, PDF). These paths
    /// are never spawned; this exists only because `convert::run` takes one
    /// unconditional `&Toolchain`, and most of its match arms simply never
    /// read it.
    pub fn unused() -> Self {
        Self { ytdlp: PathBuf::new(), ffmpeg: PathBuf::new(), ffprobe: PathBuf::new() }
    }
}

/// Per-tool discovery result surfaced to the UI on startup.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub name: String,
    pub found: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// Install instructions, present only when the tool is missing and the
    /// user is the one expected to supply it.
    pub hint: Option<String>,
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
///
/// System tools look outward first and only then at the bundle; bundled tools
/// do the reverse. Either way both sets of locations are searched, so neither
/// class can be broken by the other's absence.
pub fn locate(app: &AppHandle, stem: &str) -> Option<PathBuf> {
    if is_system_tool(stem) {
        if let Some(found) = locate_system(stem) {
            return Some(found);
        }
    }
    for candidate in candidates(app, stem) {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // PATH fallback for the bundled tools, which skipped it above.
    which::which_global(stem).ok()
}

fn require(app: &AppHandle, stem: &str) -> Result<PathBuf> {
    locate(app, stem).ok_or_else(|| AppError::MissingBinary {
        name: stem.to_string(),
    })
}

/// Every directory that might hold the whisper bundle, in priority order.
///
/// The app-data directory is in here for a specific reason: neither Windows
/// installer format can carry the largest speech model. NSIS caps out near 2 GB
/// (32-bit offsets in makensis) and MSI's CAB at exactly 2 GB, while the full
/// payload is 2.6 GB. So the two smaller models ship inside the installer and
/// the large one is side-loaded afterwards into a directory the user can
/// actually write to — unlike Program Files, which needs elevation.
fn whisper_dirs(app: &AppHandle) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(5);

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join(WHISPER_DIR));
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        out.push(dir.join("binaries").join(WHISPER_DIR));
        out.push(dir.join(WHISPER_DIR));
    }
    // Side-loaded models, and the only location here that survives an upgrade.
    if let Ok(dir) = app.path().app_data_dir() {
        out.push(dir.join(WHISPER_DIR));
    }
    out.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join(WHISPER_DIR),
    );
    out
}

/// Where a user should drop a model the installer was too large to carry.
///
/// Returned to the UI so the message can name a real path rather than describe
/// one. Created on demand: naming a folder that does not exist invites the user
/// to mistype it.
pub fn model_dropbox(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?.join(WHISPER_DIR).join(MODEL_DIR);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The `whisper-cli` executable, wherever the bundle landed.
pub fn locate_whisper(app: &AppHandle) -> Option<PathBuf> {
    let file = format!("{WHISPER}{EXE_SUFFIX}");
    for dir in whisper_dirs(app) {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which::which_global(WHISPER).ok()
}

pub fn require_whisper(app: &AppHandle) -> Result<PathBuf> {
    locate_whisper(app).ok_or_else(|| AppError::MissingBinary {
        name: WHISPER.to_string(),
    })
}

/// A ggml speech model by filename, e.g. `ggml-small.bin`.
pub fn locate_model(app: &AppHandle, file: &str) -> Option<PathBuf> {
    for dir in whisper_dirs(app) {
        let candidate = dir.join(MODEL_DIR).join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Which speech models are actually installed, as bare filenames.
pub fn installed_models(app: &AppHandle) -> Vec<String> {
    let mut found = Vec::new();
    for dir in whisper_dirs(app) {
        let Ok(entries) = std::fs::read_dir(dir.join(MODEL_DIR)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("ggml-") && name.ends_with(".bin") && !found.contains(&name) {
                found.push(name);
            }
        }
    }
    found
}

/// Resolve the full toolchain, failing with a named missing binary.
pub fn resolve(app: &AppHandle) -> Result<Toolchain> {
    Ok(Toolchain {
        ytdlp: require(app, YTDLP)?,
        ffmpeg: require(app, FFMPEG)?,
        ffprobe: require(app, FFPROBE)?,
    })
}

/// FFmpeg + FFprobe only. The General Converter's audio/video path never
/// touches yt-dlp — it converts files already on disk — so a missing yt-dlp
/// must not block a local conversion the way it correctly blocks a network
/// resolve in [`resolve`].
pub fn resolve_media(app: &AppHandle) -> Result<Toolchain> {
    Ok(Toolchain {
        ytdlp: PathBuf::new(),
        ffmpeg: require(app, FFMPEG)?,
        ffprobe: require(app, FFPROBE)?,
    })
}

/// Pandoc is a single self-contained executable on every platform it ships
/// for, so it slots into the same sidecar convention as yt-dlp/ffmpeg/ffprobe
/// and needs no resolver of its own.
pub fn locate_pandoc(app: &AppHandle) -> Option<PathBuf> {
    locate(app, PANDOC)
}

/// PDFium's shared-library filename on the current platform.
fn pdfium_lib_name() -> &'static str {
    #[cfg(windows)]
    {
        "pdfium.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libpdfium.dylib"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "libpdfium.so"
    }
}

/// Locate the vendored PDFium library.
///
/// Unlike the executables above, this is never looked for on `PATH`: there is
/// no equivalent of "a system pdfium" to fall back to, and searching the
/// dynamic linker's search path for an arbitrary named library is far less
/// reliable across platforms than spawning a named executable is — so a
/// missing PDFium simply means PDF conversion is unavailable, reported the
/// same way a missing speech model is.
pub fn locate_pdfium(app: &AppHandle) -> Option<PathBuf> {
    let file = pdfium_lib_name();
    let mut candidates = Vec::with_capacity(4);

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(file));
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join(file));
        candidates.push(dir.join("binaries").join(file));
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries").join(file));

    candidates.into_iter().find(|p| p.is_file())
}

/// `wkhtmltopdf` is not vendored — see `convert::document::to_pdf` for why —
/// so this is `PATH`-only, the same fallback the rest of the toolchain already
/// has for a machine with ffmpeg or yt-dlp installed system-wide.
pub fn locate_wkhtmltopdf() -> Option<PathBuf> {
    which::which_global("wkhtmltopdf").ok()
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
                // A binary that exists but cannot report a version is broken
                // (bad architecture, quarantined, not executable) — which for
                // the user is the same problem as not having it at all, so it
                // gets the same install advice.
                let found = version.is_some();
                tools.push(ToolStatus {
                    name: stem.to_string(),
                    found,
                    path: Some(path.to_string_lossy().into_owned()),
                    version,
                    hint: if found { None } else { install_hint(stem) },
                });
            }
            None => tools.push(ToolStatus {
                name: stem.to_string(),
                found: false,
                path: None,
                version: None,
                hint: install_hint(stem),
            }),
        }
    }

    ToolchainStatus {
        ready: tools.iter().all(|t| t.found),
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PATH lookup is the entire contract for the user-supplied tools, so it is
    /// worth asserting — but a machine without FFmpeg installed must not fail
    /// the suite, hence the early return rather than an unwrap.
    #[test]
    fn system_lookup_finds_ffmpeg_when_it_is_installed() {
        let Some(found) = locate_system(FFMPEG) else {
            eprintln!("skipped: no ffmpeg on this machine");
            return;
        };
        eprintln!("resolved ffmpeg -> {}", found.display());
        assert!(found.is_file());
        assert_eq!(found.file_stem().unwrap().to_string_lossy(), FFMPEG);
    }

    /// Install advice is only ever right for the tools the user installs. A
    /// hint on a bundled tool would tell them to fix something they cannot.
    #[test]
    fn only_user_supplied_tools_get_install_hints() {
        for name in [FFMPEG, FFPROBE, YTDLP] {
            assert!(install_hint(name).is_some(), "{name} should have one");
            assert!(is_system_tool(name));
        }
        for name in [PANDOC, WHISPER, "pdfium"] {
            assert!(install_hint(name).is_none(), "{name} should not have one");
            assert!(!is_system_tool(name));
        }
    }
}
