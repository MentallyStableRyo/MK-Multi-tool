//! The General Converter: images, audio, video, documents, and PDF ↔ image —
//! one tab, five media families, each routed to the tool actually suited to
//! it rather than one dependency stretched over all of them.
//!
//! ```text
//!   raster.rs     images: PNG/JPEG/WebP/BMP/TIFF/GIF/ICO, + SVG rasterized in
//!                 pure Rust (`image`, `resvg`) — nothing to vendor
//!   media.rs      audio/video via the app's own FFmpeg, hardware-accelerated
//!                 encoding with automatic software fallback
//!   document.rs   TXT/Markdown/HTML/DOCX/ODT/RTF/EPUB via a vendored Pandoc
//!   pdf.rs        PDF ↔ image and PDF → text via PDFium, dlopen'd like the
//!                 ONNX Runtime the Remove Background tab already vendors
//!   hardware.rs   the capability report the UI shows before you convert
//! ```
//!
//! Every category degrades the same way the rest of the app does when a tool
//! is missing: the UI is told exactly what to name and where to put it,
//! rather than the conversion silently doing nothing or crashing.

mod document;
mod hardware;
mod media;
mod pdf;
mod raster;

pub use hardware::{detect as detect_hardware, HardwareReport};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use crate::binaries::Toolchain;
use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Image,
    Audio,
    Video,
    Document,
    Pdf,
    Unknown,
}

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif", "ico", "svg"];
const AUDIO_EXT: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "opus", "m4a", "wma"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "avi", "webm", "mov", "m4v", "flv", "wmv"];
const DOC_EXT: &[&str] = &["txt", "md", "markdown", "html", "htm", "docx", "odt", "rtf", "epub"];

/// Which family a file belongs to, decided purely from its extension — the
/// same "trust the extension the OS/dialog already gave us" approach
/// `transcribe.ts`'s file filters use.
pub fn category_of(extension: &str) -> Category {
    let ext = extension.to_ascii_lowercase();
    if ext == "pdf" {
        Category::Pdf
    } else if IMAGE_EXT.contains(&ext.as_str()) {
        Category::Image
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Category::Audio
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Category::Video
    } else if DOC_EXT.contains(&ext.as_str()) {
        Category::Document
    } else {
        Category::Unknown
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub category: Category,
    pub extension: String,
    /// PDFs only, and only when PDFium could actually open the file.
    pub page_count: Option<i32>,
}

pub async fn inspect(app: &AppHandle, path: &Path) -> Inspection {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let category = category_of(&extension);
    let page_count = if category == Category::Pdf {
        pdf::page_count(app, path.to_path_buf()).await
    } else {
        None
    };
    Inspection { category, extension, page_count }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConvertOptions {
    Image(raster::ImageOptions),
    Audio(media::AudioOptions),
    Video(media::VideoOptions),
    Document(document::DocumentOptions),
    /// Pandoc → standalone HTML → `wkhtmltopdf`; see `document::to_pdf`.
    DocumentToPdf,
    PdfToImages(pdf::PdfToImagesOptions),
    /// Combines every entry in `ConvertRequest::input_paths` into one PDF, one
    /// page per image — the only mode that takes more than one input.
    ImagesToPdf,
    PdfToText(pdf::PdfToTextOptions),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertRequest {
    pub input_paths: Vec<String>,
    pub output_path: String,
    pub options: ConvertOptions,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertSummary {
    /// Almost always one path. PDF → images is the exception: one file per
    /// rendered page.
    pub output_paths: Vec<String>,
    pub note: String,
}

/// The file extension a given target produces — the single source of truth
/// the frontend asks for when building a save-dialog filter or a default
/// output name, rather than keeping its own copy of this table in sync with
/// each format enum's `extension()`.
pub fn target_extension(options: &ConvertOptions) -> &'static str {
    match options {
        ConvertOptions::Image(o) => o.format.extension(),
        ConvertOptions::Audio(o) => o.format.extension(),
        ConvertOptions::Video(o) => o.format.extension(),
        ConvertOptions::Document(o) => o.format.extension(),
        ConvertOptions::DocumentToPdf => "pdf",
        ConvertOptions::PdfToImages(o) => o.format.extension(),
        ConvertOptions::ImagesToPdf => "pdf",
        ConvertOptions::PdfToText(_) => "txt",
    }
}

fn single_input(inputs: &[PathBuf]) -> Result<&PathBuf> {
    match inputs {
        [one] => Ok(one),
        [] => Err(AppError::other("No input file was given.")),
        _ => Err(AppError::other("This conversion takes exactly one input file at a time.")),
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Run one conversion end to end. `on_progress` receives a 0.0–1.0 fraction
/// (0.0 also covers "not measurable yet", matching every other job in this
/// app's indeterminate-progress convention) and a short status line.
pub async fn run(
    app: &AppHandle,
    toolchain: &Toolchain,
    request: ConvertRequest,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(f64, String) + Send + 'static,
) -> Result<ConvertSummary> {
    let inputs: Vec<PathBuf> = request.input_paths.iter().map(PathBuf::from).collect();
    let output = PathBuf::from(&request.output_path);

    match request.options {
        ConvertOptions::Image(options) => {
            let input = single_input(&inputs)?;
            on_progress(0.15, "Reading image…".to_string());
            let image = raster::load(input)?;
            on_progress(0.6, "Encoding…".to_string());
            raster::save(&image, &output, &options)?;
            on_progress(1.0, "Done".to_string());
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Converted".to_string() })
        }

        ConvertOptions::Audio(options) => {
            let input = single_input(&inputs)?.clone();
            media::convert_audio(toolchain, &input, &output, &options, cancel, on_progress).await?;
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Converted".to_string() })
        }

        ConvertOptions::Video(options) => {
            let input = single_input(&inputs)?.clone();
            media::convert_video(toolchain, &input, &output, &options, cancel, on_progress).await?;
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Converted".to_string() })
        }

        ConvertOptions::Document(options) => {
            let input = single_input(&inputs)?.clone();
            on_progress(0.2, "Converting with Pandoc…".to_string());
            document::convert(app, &input, &output, &options, cancel).await?;
            on_progress(1.0, "Done".to_string());
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Converted".to_string() })
        }

        ConvertOptions::DocumentToPdf => {
            let input = single_input(&inputs)?.clone();
            on_progress(0.1, "Rendering to PDF…".to_string());
            document::to_pdf(app, &input, &output, cancel).await?;
            on_progress(1.0, "Done".to_string());
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Converted".to_string() })
        }

        ConvertOptions::PdfToImages(options) => {
            let input = single_input(&inputs)?.clone();
            // Not redundant despite what clippy thinks: `pdf::to_images` needs
            // its callback to be `'static` (it crosses a `spawn_blocking`), and
            // only a closure that *moves* `on_progress` in can satisfy that —
            // `&mut on_progress` would borrow this stack frame instead.
            #[allow(clippy::redundant_closure)]
            let outputs = pdf::to_images(app, input, output, options, move |f, m| on_progress(f, m)).await?;
            let count = outputs.len();
            Ok(ConvertSummary {
                output_paths: outputs.iter().map(|p| path_string(p)).collect(),
                note: format!("{count} page{} rendered", if count == 1 { "" } else { "s" }),
            })
        }

        ConvertOptions::ImagesToPdf => {
            if inputs.is_empty() {
                return Err(AppError::other("Add at least one image to combine into a PDF."));
            }
            let count = inputs.len();
            // See the identical `#[allow]` above `PdfToImages`: this closure
            // must move `on_progress` in to satisfy `from_images`'s `'static`
            // bound, which clippy's suggested `&mut on_progress` cannot.
            #[allow(clippy::redundant_closure)]
            pdf::from_images(app, inputs, output.clone(), move |f, m| on_progress(f, m)).await?;
            Ok(ConvertSummary {
                output_paths: vec![path_string(&output)],
                note: format!("{count} image{} combined", if count == 1 { "" } else { "s" }),
            })
        }

        ConvertOptions::PdfToText(options) => {
            let input = single_input(&inputs)?.clone();
            on_progress(0.25, "Extracting text…".to_string());
            let text = pdf::to_text(app, input, options).await?;
            if let Some(parent) = output.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&output, text)?;
            on_progress(1.0, "Done".to_string());
            Ok(ConvertSummary { output_paths: vec![path_string(&output)], note: "Text extracted".to_string() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_extension_resolves_to_the_right_family() {
        for ext in IMAGE_EXT {
            assert_eq!(category_of(ext), Category::Image, "{ext}");
        }
        for ext in AUDIO_EXT {
            assert_eq!(category_of(ext), Category::Audio, "{ext}");
        }
        for ext in VIDEO_EXT {
            assert_eq!(category_of(ext), Category::Video, "{ext}");
        }
        for ext in DOC_EXT {
            assert_eq!(category_of(ext), Category::Document, "{ext}");
        }
        assert_eq!(category_of("pdf"), Category::Pdf);
        assert_eq!(category_of("PDF"), Category::Pdf, "extension matching must be case-insensitive");
        assert_eq!(category_of("exe"), Category::Unknown);
    }

    #[test]
    fn single_input_rejects_zero_or_many() {
        assert!(single_input(&[]).is_err());
        assert!(single_input(&[PathBuf::from("a"), PathBuf::from("b")]).is_err());
        assert!(single_input(&[PathBuf::from("a")]).is_ok());
    }
}
