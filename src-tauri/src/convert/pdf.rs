//! PDF ↔ Image, and best-effort PDF text extraction — PDFium underneath.
//!
//! PDFium is a native *library*, not a program, so unlike every other sidecar
//! in this app it is `dlopen`ed via [`pdfium_render`] rather than spawned as a
//! child process. It is vendored the same way ONNX Runtime is for Remove
//! Background: a file resolved at runtime, touched only when a PDF conversion
//! is actually requested.

use std::path::{Path, PathBuf};

use pdfium_render::prelude::*;
use serde::Deserialize;
use tauri::AppHandle;
use tokio::sync::OnceCell;

use crate::binaries;
use crate::convert::raster::{self, ImageFormat, ImageOptions};
use crate::error::{AppError, Result};

/// One bound PDFium engine for the whole process.
///
/// `Pdfium::bind_to_library` may only succeed once per process — the crate
/// guards against loading the native library a second time — so this is
/// deliberately a single lazily-created global rather than something built
/// per call. `None` covers both "never found" and "the one bind attempt
/// failed"; either way the UI reports the same "PDF tools are unavailable"
/// rather than retrying something that cannot succeed the second time.
static ENGINE: OnceCell<Option<Pdfium>> = OnceCell::const_new();

async fn engine(app: &AppHandle) -> Option<&'static Pdfium> {
    ENGINE
        .get_or_init(|| async {
            let path = binaries::locate_pdfium(app)?;
            let bindings = Pdfium::bind_to_library(&path).ok()?;
            Some(Pdfium::new(bindings))
        })
        .await
        .as_ref()
}

pub async fn is_available(app: &AppHandle) -> bool {
    engine(app).await.is_some()
}

fn missing_pdfium() -> AppError {
    AppError::MissingBinary { name: "pdfium".to_string() }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfToImagesOptions {
    pub format: ImageFormat,
    /// Clamped to 36..600.
    #[serde(default = "default_dpi")]
    pub dpi: u32,
    /// `"1-3,5,8-10"`, or omitted for every page.
    #[serde(default)]
    pub pages: Option<String>,
}

fn default_dpi() -> u32 {
    150
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdfToTextOptions {
    #[serde(default)]
    pub pages: Option<String>,
}

/// Parse `"1-3,5,8-10"` into zero-indexed, ascending, deduplicated page
/// indices, validated against `total`. `None` (or blank) means every page.
pub fn parse_page_range(spec: Option<&str>, total: i32) -> Result<Vec<i32>> {
    if total <= 0 {
        return Err(AppError::other("That PDF has no pages."));
    }
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((0..total).collect());
    };

    let mut pages = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (parse_one(a, total)?, parse_one(b, total)?),
            None => {
                let n = parse_one(part, total)?;
                (n, n)
            }
        };
        if start > end {
            return Err(AppError::other(format!(
                "\"{part}\" is backwards — the page range should count up."
            )));
        }
        pages.extend(start - 1..end);
    }

    pages.sort_unstable();
    pages.dedup();
    if pages.is_empty() {
        return Err(AppError::other("That page range didn't select any pages."));
    }
    Ok(pages)
}

fn parse_one(text: &str, total: i32) -> Result<i32> {
    let n: i32 = text
        .trim()
        .parse()
        .map_err(|_| AppError::other(format!("\"{}\" is not a page number.", text.trim())))?;
    if n < 1 || n > total {
        return Err(AppError::other(format!(
            "Page {n} is out of range — this PDF has {total} page{}.",
            if total == 1 { "" } else { "s" }
        )));
    }
    Ok(n)
}

/// How many pages `path` has, or `None` if it cannot be opened (missing
/// PDFium, a corrupt file, a password) — the UI degrades to a free-text page
/// range rather than blocking on this.
pub async fn page_count(app: &AppHandle, path: PathBuf) -> Option<i32> {
    let pdfium = engine(app).await?;
    let pdfium: &'static Pdfium = pdfium;
    tokio::task::spawn_blocking(move || {
        pdfium
            .load_pdf_from_file(&path, None)
            .ok()
            .map(|doc| doc.pages().len())
    })
    .await
    .ok()
    .flatten()
}

/// Render selected pages to image files beside `stem`, named
/// `<stem>-01.<ext>`, `<stem>-02.<ext>`, …
pub async fn to_images(
    app: &AppHandle,
    input: PathBuf,
    stem: PathBuf,
    options: PdfToImagesOptions,
    mut on_progress: impl FnMut(f64, String) + Send + 'static,
) -> Result<Vec<PathBuf>> {
    let pdfium = engine(app).await.ok_or_else(missing_pdfium)?;
    let pdfium: &'static Pdfium = pdfium;

    // PDFium's calls are synchronous and can be CPU-heavy for large pages at
    // high DPI; spawn_blocking keeps them off the async runtime's worker
    // threads, the in-process equivalent of how a subprocess never blocks them.
    tokio::task::spawn_blocking(move || render_pages(pdfium, &input, &stem, &options, &mut on_progress))
        .await
        .map_err(|e| AppError::other(format!("The PDF render task panicked: {e}")))?
}

fn render_pages(
    pdfium: &Pdfium,
    input: &Path,
    stem: &Path,
    options: &PdfToImagesOptions,
    on_progress: &mut dyn FnMut(f64, String),
) -> Result<Vec<PathBuf>> {
    let document = pdfium
        .load_pdf_from_file(input, None)
        .map_err(|e| AppError::other(format!("Could not open that PDF: {e}")))?;

    let total = document.pages().len();
    let indices = parse_page_range(options.pages.as_deref(), total)?;

    let dir = stem
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let base = stem.file_stem().and_then(|s| s.to_str()).unwrap_or("page").to_string();

    let digits = indices.len().to_string().len().max(2);
    let dpi = options.dpi.clamp(36, 600) as f32;
    let mut outputs = Vec::with_capacity(indices.len());

    for (i, &index) in indices.iter().enumerate() {
        let page = document
            .pages()
            .get(index)
            .map_err(|e| AppError::other(format!("Could not read page {}: {e}", index + 1)))?;

        let width_px = ((page.width().value / 72.0) * dpi).round().max(1.0) as i32;
        let height_px = ((page.height().value / 72.0) * dpi).round().max(1.0) as i32;

        let bitmap = page
            .render_with_config(
                &PdfRenderConfig::new()
                    .set_target_width(width_px)
                    .set_target_height(height_px),
            )
            .map_err(|e| AppError::other(format!("Could not render page {}: {e}", index + 1)))?;
        let image = bitmap
            .as_image()
            .map_err(|e| AppError::other(format!("Could not render page {}: {e}", index + 1)))?;

        let name = format!("{base}-{:0digits$}.{}", i + 1, options.format.extension(), digits = digits);
        let out = dir.join(name);
        raster::save(&image, &out, &ImageOptions { format: options.format, quality: None })?;
        outputs.push(out);

        on_progress(
            (i + 1) as f64 / indices.len().max(1) as f64,
            format!("Rendered page {} of {}", i + 1, indices.len()),
        );
    }

    Ok(outputs)
}

/// Combine one or more images into a single multi-page PDF, one page per
/// image, each page sized to match its source image exactly — 96 px per inch,
/// the same pixel-to-point assumption a browser makes.
pub async fn from_images(
    app: &AppHandle,
    inputs: Vec<PathBuf>,
    output: PathBuf,
    mut on_progress: impl FnMut(f64, String) + Send + 'static,
) -> Result<()> {
    let pdfium = engine(app).await.ok_or_else(missing_pdfium)?;
    let pdfium: &'static Pdfium = pdfium;

    tokio::task::spawn_blocking(move || build_pdf(pdfium, &inputs, &output, &mut on_progress))
        .await
        .map_err(|e| AppError::other(format!("The PDF build task panicked: {e}")))?
}

/// 72 PDF points per inch, 96 CSS/device pixels per inch.
const POINTS_PER_PIXEL: f32 = 72.0 / 96.0;

fn build_pdf(
    pdfium: &Pdfium,
    inputs: &[PathBuf],
    output: &Path,
    on_progress: &mut dyn FnMut(f64, String),
) -> Result<()> {
    if inputs.is_empty() {
        return Err(AppError::other("No images were given to combine into a PDF."));
    }

    let mut document = pdfium
        .create_new_pdf()
        .map_err(|e| AppError::other(format!("Could not start a new PDF: {e}")))?;

    for (i, path) in inputs.iter().enumerate() {
        let image = raster::load(path)?;
        let width_pt = PdfPoints::new(image.width() as f32 * POINTS_PER_PIXEL);
        let height_pt = PdfPoints::new(image.height() as f32 * POINTS_PER_PIXEL);

        // Built before the page: this only needs `&document` (to read the
        // engine handle/bindings), not the page itself, so it does not
        // conflict with the mutable borrow `pages_mut()` takes next.
        let image_object = PdfPageImageObject::new_with_size(&document, &image, width_pt, height_pt)
            .map_err(|e| AppError::other(format!("Could not place {}: {e}", path.display())))?;

        let mut page = document
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::from_points(width_pt, height_pt))
            .map_err(|e| AppError::other(format!("Could not add a page for {}: {e}", path.display())))?;
        page.objects_mut()
            .add_image_object(image_object)
            .map_err(|e| AppError::other(format!("Could not place {}: {e}", path.display())))?;

        on_progress(
            (i + 1) as f64 / inputs.len() as f64,
            format!("Added page {} of {}", i + 1, inputs.len()),
        );
    }

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    document
        .save_to_file(output)
        .map_err(|e| AppError::other(format!("Could not write the PDF: {e}")))
}

/// Extract raw text from selected pages, joined by a page-break rule.
///
/// This is text extraction, not document reconstruction — layout, columns,
/// and tables are not reflowed, only pulled out in reading order. That is the
/// honest limit of every PDF text extractor, this one included.
pub async fn to_text(app: &AppHandle, input: PathBuf, options: PdfToTextOptions) -> Result<String> {
    let pdfium = engine(app).await.ok_or_else(missing_pdfium)?;
    let pdfium: &'static Pdfium = pdfium;

    tokio::task::spawn_blocking(move || extract_text(pdfium, &input, &options))
        .await
        .map_err(|e| AppError::other(format!("The PDF text task panicked: {e}")))?
}

fn extract_text(pdfium: &Pdfium, input: &Path, options: &PdfToTextOptions) -> Result<String> {
    let document = pdfium
        .load_pdf_from_file(input, None)
        .map_err(|e| AppError::other(format!("Could not open that PDF: {e}")))?;

    let total = document.pages().len();
    let indices = parse_page_range(options.pages.as_deref(), total)?;

    let mut out = String::new();
    for &index in &indices {
        let page = document
            .pages()
            .get(index)
            .map_err(|e| AppError::other(format!("Could not read page {}: {e}", index + 1)))?;
        let text = page
            .text()
            .map_err(|e| AppError::other(format!("Could not read text on page {}: {e}", index + 1)))?
            .all();

        if !out.is_empty() {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(text.trim_end());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pages_ranges_and_lists_all_parse() {
        assert_eq!(parse_page_range(Some("1"), 5).unwrap(), vec![0]);
        assert_eq!(parse_page_range(Some("1-3"), 5).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_page_range(Some("1,3,5"), 5).unwrap(), vec![0, 2, 4]);
        assert_eq!(parse_page_range(Some("1-2,4-5"), 5).unwrap(), vec![0, 1, 3, 4]);
        assert_eq!(parse_page_range(None, 5).unwrap(), vec![0, 1, 2, 3, 4]);
        assert_eq!(parse_page_range(Some("  "), 5).unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn overlapping_ranges_are_deduplicated_and_sorted() {
        assert_eq!(parse_page_range(Some("3,1-3,2"), 5).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn out_of_range_and_malformed_specs_are_rejected() {
        assert!(parse_page_range(Some("0"), 5).is_err());
        assert!(parse_page_range(Some("6"), 5).is_err());
        assert!(parse_page_range(Some("3-1"), 5).is_err());
        assert!(parse_page_range(Some("abc"), 5).is_err());
        assert!(parse_page_range(Some("1-"), 5).is_err());
    }

    #[test]
    fn a_document_with_no_pages_is_rejected_up_front() {
        assert!(parse_page_range(None, 0).is_err());
    }

    /// Exercises the real vendored `pdfium.dll`/`.so`/`.dylib` end to end:
    /// bind the library, build a two-page PDF from synthetic images, then
    /// render it back and check the pixels round-trip. Bypasses the
    /// `AppHandle`-based resolver — there is no running app in a unit test —
    /// binding directly to `CARGO_MANIFEST_DIR/binaries/`, the same dev-tree
    /// location `binaries::locate_pdfium` checks last.
    ///
    /// `#[ignore]`d because it needs `npm run pdfium` to have vendored the
    /// library first, matching how `smoke.rs` ignores its network dependency.
    #[test]
    #[ignore = "needs `npm run pdfium` to have vendored the library first"]
    fn images_to_pdf_and_back_round_trips_through_the_real_library() {
        let lib = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join(if cfg!(windows) {
                "pdfium.dll"
            } else if cfg!(target_os = "macos") {
                "libpdfium.dylib"
            } else {
                "libpdfium.so"
            });
        assert!(lib.is_file(), "{} not vendored — run `npm run pdfium`", lib.display());

        let bindings = Pdfium::bind_to_library(&lib).expect("bind to the vendored pdfium");
        let pdfium = Pdfium::new(bindings);

        let dir = std::env::temp_dir().join(format!("mk-pdfium-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Two distinctly colored, distinctly sized images, so a mixed-up page
        // order or a wrong page size would show up as a failed assertion
        // rather than silently passing.
        let red = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 20, image::Rgb([220, 20, 20])));
        let blue = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(20, 40, image::Rgb([20, 20, 220])));
        let red_path = dir.join("red.png");
        let blue_path = dir.join("blue.png");
        raster::save(&red, &red_path, &ImageOptions { format: ImageFormat::Png, quality: None }).unwrap();
        raster::save(&blue, &blue_path, &ImageOptions { format: ImageFormat::Png, quality: None }).unwrap();

        let pdf_path = dir.join("combined.pdf");
        build_pdf(&pdfium, &[red_path, blue_path], &pdf_path, &mut |_, _| {}).unwrap();
        assert!(pdf_path.is_file());

        let stem = dir.join("page");
        let options = PdfToImagesOptions { format: ImageFormat::Png, dpi: 96, pages: None };
        let outputs = render_pages(&pdfium, &pdf_path, &stem, &options, &mut |_, _| {}).unwrap();
        assert_eq!(outputs.len(), 2, "one rendered image per PDF page");

        let page1 = raster::load(&outputs[0]).unwrap();
        let page2 = raster::load(&outputs[1]).unwrap();

        // 96 DPI == the 96 px/inch assumption `build_pdf` itself uses, so the
        // rendered pixel dimensions should reproduce the source images exactly.
        assert_eq!((page1.width(), page1.height()), (40, 20), "page 1 should keep the red image's aspect ratio");
        assert_eq!((page2.width(), page2.height()), (20, 40), "page 2 should keep the blue image's aspect ratio");

        let p1 = page1.to_rgb8().get_pixel(20, 10).0;
        let p2 = page2.to_rgb8().get_pixel(10, 20).0;
        assert!(p1[0] > 150 && p1[2] < 100, "page 1 center should be red-ish, got {p1:?}");
        assert!(p2[2] > 150 && p2[0] < 100, "page 2 center should be blue-ish, got {p2:?}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
