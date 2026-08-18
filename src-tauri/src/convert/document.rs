//! Documents: TXT, Markdown, HTML, DOCX, ODT, RTF, EPUB — via a vendored
//! Pandoc, resolved and invoked exactly like FFmpeg and yt-dlp elsewhere in
//! this app.
//!
//! PDF is deliberately not a Pandoc target here: Pandoc cannot reliably read
//! arbitrary PDFs (see [`crate::convert::pdf`] for the PDFium-based text
//! extraction that actually can, best-effort), and writing one needs a LaTeX
//! engine this app does not bundle. Documents → PDF instead goes through
//! Pandoc → standalone HTML → `wkhtmltopdf`, and only appears when a
//! `wkhtmltopdf` happens to be on PATH — see [`to_pdf`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use crate::binaries;
use crate::error::{AppError, Result};
use crate::proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocFormat {
    Txt,
    Markdown,
    Html,
    Docx,
    Odt,
    Rtf,
    Epub,
}

impl DocFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Rtf => "rtf",
            Self::Epub => "epub",
        }
    }

    /// Pandoc's `-f`/`-t` format name.
    ///
    /// Plain text has no dedicated Pandoc reader/writer; Pandoc's own
    /// documented way to mean "just the text" is Markdown with every
    /// extension switched off, which is what `markdown_strict` is.
    fn pandoc_name(self) -> &'static str {
        match self {
            Self::Txt => "markdown_strict",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Rtf => "rtf",
            Self::Epub => "epub",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "txt" => Some(Self::Txt),
            "md" | "markdown" => Some(Self::Markdown),
            "html" | "htm" => Some(Self::Html),
            "docx" => Some(Self::Docx),
            "odt" => Some(Self::Odt),
            "rtf" => Some(Self::Rtf),
            "epub" => Some(Self::Epub),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentOptions {
    pub format: DocFormat,
}

pub fn is_available(app: &AppHandle) -> bool {
    binaries::locate_pandoc(app).is_some()
}

pub fn wkhtmltopdf_available() -> bool {
    binaries::locate_wkhtmltopdf().is_some()
}

fn missing_pandoc() -> AppError {
    AppError::MissingBinary { name: "pandoc".to_string() }
}

/// Convert one document to another, inferring the source format from its
/// extension the same way the file dialog offered it.
pub async fn convert(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    options: &DocumentOptions,
    cancel: &CancellationToken,
) -> Result<()> {
    let pandoc = binaries::locate_pandoc(app).ok_or_else(missing_pandoc)?;

    let source_ext = input.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let from = DocFormat::from_extension(source_ext)
        .ok_or_else(|| AppError::other(format!("\"{source_ext}\" isn't a document format this app reads.")))?;

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut cmd = proc::command(&pandoc);
    cmd.arg("-f")
        .arg(from.pandoc_name())
        .arg("-t")
        .arg(options.format.pandoc_name())
        .arg("--standalone")
        .arg("-o")
        .arg(output)
        .arg(input);

    let out = proc::run_streaming(cmd, cancel, |_| {}).await?;
    if !out.ok() {
        return Err(AppError::other(format!(
            "Pandoc could not convert that file: {}",
            out.stderr.lines().last().unwrap_or("no diagnostic output")
        )));
    }
    if !output.is_file() {
        return Err(AppError::other("Pandoc reported success but wrote no file."));
    }
    Ok(())
}

/// Documents → PDF: Pandoc renders a standalone HTML file, then `wkhtmltopdf`
/// prints it.
///
/// Unlike Pandoc, `wkhtmltopdf` is not vendored — its upstream stopped
/// publishing Windows/macOS builds, only Debian/RPM packages remain — so this
/// only works when it happens to be on PATH, exactly the way the app already
/// falls back to a PATH ffmpeg or yt-dlp if the vendored ones are absent.
/// Callers should check [`wkhtmltopdf_available`] before offering this in the
/// UI, the same way the largest whisper tier stays hidden until its model file
/// exists.
pub async fn to_pdf(app: &AppHandle, input: &Path, output: &Path, cancel: &CancellationToken) -> Result<()> {
    let wkhtmltopdf =
        binaries::locate_wkhtmltopdf().ok_or_else(|| AppError::MissingBinary { name: "wkhtmltopdf".to_string() })?;

    let work = scratch_dir_for(output)?;
    let html_path = work.join("source.html");

    let result = async {
        convert(app, input, &html_path, &DocumentOptions { format: DocFormat::Html }, cancel).await?;

        if let Some(parent) = output.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let mut cmd = proc::command(&wkhtmltopdf);
        cmd.arg("--quiet").arg(&html_path).arg(output);

        let out = proc::run_streaming(cmd, cancel, |_| {}).await?;
        if !out.ok() || !output.is_file() {
            return Err(AppError::other(format!(
                "wkhtmltopdf could not print that document: {}",
                out.stderr.lines().last().unwrap_or("no diagnostic output")
            )));
        }
        Ok(())
    }
    .await;

    std::fs::remove_dir_all(&work).ok();
    result
}

/// A scratch directory next to the output, mirroring `transcribe::tempdir_for`.
fn scratch_dir_for(output: &Path) -> Result<PathBuf> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);

    let dir = parent.join(format!(".mk-convert-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_document_extension_maps_to_exactly_one_format() {
        for (ext, expected) in [
            ("txt", DocFormat::Txt),
            ("md", DocFormat::Markdown),
            ("markdown", DocFormat::Markdown),
            ("html", DocFormat::Html),
            ("htm", DocFormat::Html),
            ("docx", DocFormat::Docx),
            ("odt", DocFormat::Odt),
            ("rtf", DocFormat::Rtf),
            ("epub", DocFormat::Epub),
        ] {
            assert_eq!(DocFormat::from_extension(ext), Some(expected), "extension {ext}");
        }
        assert_eq!(DocFormat::from_extension("pdf"), None);
        assert_eq!(DocFormat::from_extension("exe"), None);
    }

    #[test]
    fn plain_text_has_no_dedicated_pandoc_reader_so_uses_strict_markdown() {
        // Regression: passing "txt" straight through to Pandoc's `-f`/`-t`
        // fails outright, since Pandoc has no txt reader/writer at all.
        assert_eq!(DocFormat::Txt.pandoc_name(), "markdown_strict");
    }

    /// Exercises the real vendored `pandoc` executable through the exact
    /// `proc::command`/`proc::run_streaming` machinery `convert()` uses,
    /// bypassing only the `AppHandle`-based resolver — there is no running
    /// app in a unit test — by pointing straight at the dev-tree binary
    /// `binaries::locate_pandoc` would otherwise find.
    ///
    /// `#[ignore]`d because it needs `npm run pandoc` to have vendored the
    /// binary first, matching how `smoke.rs` ignores its network dependency.
    #[tokio::test]
    #[ignore = "needs `npm run pandoc` to have vendored the binary first"]
    async fn markdown_to_html_round_trips_through_the_real_binary() {
        let triple = env!("MK_TARGET_TRIPLE");
        let exe = if cfg!(windows) { ".exe" } else { "" };
        let pandoc = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("binaries")
            .join(format!("pandoc-{triple}{exe}"));
        assert!(pandoc.is_file(), "{} not vendored — run `npm run pandoc`", pandoc.display());

        let dir = std::env::temp_dir().join(format!("mk-pandoc-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.md");
        let output = dir.join("out.html");
        std::fs::write(&input, "# Heading\n\nA **bold** word.\n").unwrap();

        let mut cmd = proc::command(&pandoc);
        cmd.arg("-f")
            .arg(DocFormat::Markdown.pandoc_name())
            .arg("-t")
            .arg(DocFormat::Html.pandoc_name())
            .arg("--standalone")
            .arg("-o")
            .arg(&output)
            .arg(&input);

        let out = proc::run_streaming(cmd, &tokio_util::sync::CancellationToken::new(), |_| {}).await.unwrap();
        assert!(out.ok(), "pandoc exited with an error: {}", out.stderr);

        let html = std::fs::read_to_string(&output).unwrap();
        assert!(html.contains("<h1"), "heading did not survive conversion: {html}");
        assert!(html.contains("<strong>bold</strong>"), "bold text did not survive conversion: {html}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
