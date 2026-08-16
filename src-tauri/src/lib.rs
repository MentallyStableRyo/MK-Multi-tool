//! MK Multi-tool — precision slice downloader for YouTube, Instagram, TikTok,
//! and X.
//!
//! ## Architecture
//!
//! ```text
//!   webview (TypeScript)          one tab per platform
//!        │  invoke()                    ▲  mk://job-progress events
//!        ▼                              │
//!   commands.rs ── validation, job spawn, event throttling
//!        │
//!        ├── platform.rs  per-site link rules, one per tab
//!        ├── youtube.rs   yt-dlp resolution and the track model
//!        ├── probe.rs     ffprobe keyframe + duration queries
//!        ├── render.rs    cut planning and the FFmpeg pipeline
//!        ├── disk.rs      free-space verification
//!        └── staging.rs   atomic output commits
//!
//!   proc.rs / ffargs.rs / binaries.rs  — shared process + tool plumbing
//! ```
//!
//! Layers only ever depend downward. Nothing below `commands.rs` knows what a
//! window is, which keeps the whole media pipeline unit-testable without Tauri.
//!
//! Everything past link normalisation is site-agnostic: yt-dlp normalises the
//! four platforms into one metadata shape, so a slice is cut the same way
//! regardless of where it came from.

mod binaries;
mod commands;
mod disk;
mod error;
mod ffargs;
mod jobs;
mod pipeline;
mod platform;
mod probe;
mod proc;
mod render;
mod srt;
mod staging;
mod timecode;
mod transcribe;
mod youtube;

/// Network-backed end-to-end test. Ignored by default; see the module docs.
#[cfg(test)]
mod smoke;

pub use error::{AppError, Result};

use tauri::{Manager, RunEvent, WindowEvent};

use jobs::JobRegistry;

/// Build and run the application.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(JobRegistry::default())
        .on_window_event(|window, event| {
            // Closing the window must not leave an FFmpeg process encoding in
            // the background with nowhere to report to.
            if matches!(event, WindowEvent::Destroyed) {
                window.app_handle().state::<JobRegistry>().cancel_all();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::check_toolchain,
            commands::default_output_dir,
            commands::suggest_filename,
            commands::analyze_url,
            commands::start_slice,
            commands::cancel_job,
            commands::transcription_tiers,
            commands::start_transcription,
            commands::write_file,
            commands::reveal_in_folder,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build MK Multi-tool")
        .run(|app, event| {
            if let RunEvent::ExitRequested { .. } = event {
                app.state::<JobRegistry>().cancel_all();
            }
        });
}
