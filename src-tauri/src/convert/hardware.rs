//! Smart hardware-acceleration detection for the General Converter.
//!
//! There is exactly one place in this app hardware acceleration actually
//! matters: encoding video. Image, PDF, and document conversions are
//! CPU-bound by nature — a JPEG encode or a Pandoc pass is milliseconds
//! regardless of the GPU — so "detection" for those categories means whether
//! their tool is present at all, not which processor runs it.
//!
//! Video encoder discovery itself lives in `render.rs` (`h264_candidates`),
//! shared rather than duplicated: this module's job is turning that into a
//! human-readable capability report for the UI, plus rolling in the sidecar
//! checks the other categories need.

use serde::Serialize;
use tauri::AppHandle;

use crate::binaries::Toolchain;
use crate::convert::{document, pdf};
use crate::render;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareReport {
    /// Human name of the fastest H.264 encoder this FFmpeg build can actually
    /// run — e.g. "NVIDIA NVENC (CUDA)", "Intel Quick Sync", "Apple
    /// VideoToolbox" — or "CPU (libx264)" when no GPU path was found.
    pub video_encoder: String,
    pub video_encoder_is_hardware: bool,
    pub documents_available: bool,
    pub pdf_available: bool,
    /// Documents → PDF needs `wkhtmltopdf` on PATH; see
    /// `convert::document::to_pdf` for why it is not vendored.
    pub documents_to_pdf_available: bool,
}

fn encoder_label(name: &str) -> &'static str {
    match name {
        "h264_nvenc" => "NVIDIA NVENC (CUDA)",
        "h264_qsv" => "Intel Quick Sync",
        "h264_amf" => "AMD AMF",
        "h264_videotoolbox" => "Apple VideoToolbox",
        _ => "CPU (libx264)",
    }
}

/// `tc: None` means FFmpeg itself could not be resolved — still a valid state
/// to report (Documents/PDF may work fine without it), not a reason to fail
/// the whole capability check.
pub async fn detect(app: &AppHandle, tc: Option<&Toolchain>) -> HardwareReport {
    let (video_encoder, video_encoder_is_hardware) = match tc {
        Some(tc) => {
            let candidates = render::h264_candidates(tc).await;
            let best = candidates.first().map(String::as_str).unwrap_or("libx264");
            (encoder_label(best).to_string(), render::is_hardware(best))
        }
        None => ("Unavailable — FFmpeg not found".to_string(), false),
    };

    HardwareReport {
        video_encoder,
        video_encoder_is_hardware,
        documents_available: document::is_available(app),
        pdf_available: pdf::is_available(app).await,
        documents_to_pdf_available: document::wkhtmltopdf_available(),
    }
}
