//! Image ↔ image conversion: PNG, JPEG, WebP, BMP, TIFF, GIF, ICO — plus SVG as
//! a rasterization-only input.
//!
//! Pure Rust throughout (the `image` crate, and `resvg`/`usvg`/`tiny-skia` for
//! SVG), so unlike every other category in the General Converter there is
//! nothing to vendor and nothing that can be "missing" at runtime.

use std::path::Path;

use image::{DynamicImage, ImageFormat as Codec};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
    WebP,
    Bmp,
    Tiff,
    Gif,
    Ico,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Gif => "gif",
            Self::Ico => "ico",
        }
    }

    fn codec(self) -> Codec {
        match self {
            Self::Png => Codec::Png,
            Self::Jpeg => Codec::Jpeg,
            Self::WebP => Codec::WebP,
            Self::Bmp => Codec::Bmp,
            Self::Tiff => Codec::Tiff,
            Self::Gif => Codec::Gif,
            Self::Ico => Codec::Ico,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageOptions {
    pub format: ImageFormat,
    /// 1–100. Only JPEG honours this — the `image` crate's WebP encoder is
    /// lossless-only, and every other target here has no quality knob at all.
    #[serde(default)]
    pub quality: Option<u8>,
}

/// Decode any input this app accepts as an "image" — including SVG, which is
/// rasterized rather than decoded, since nothing downstream of this function
/// deals in vector data.
pub fn load(path: &Path) -> Result<DynamicImage> {
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));

    if is_svg {
        rasterize_svg(path)
    } else {
        image::open(path).map_err(|e| AppError::other(format!("Could not read that image: {e}")))
    }
}

/// Rasterize an SVG at its intrinsic size (its `viewBox`/`width`/`height`, or
/// 100×100 if it specifies neither — the SVG spec's own fallback).
///
/// Best-effort on text: system fonts are loaded into `usvg`'s font database,
/// but an SVG that embeds a font it doesn't ship falls back to a substitute
/// rather than failing outright.
fn rasterize_svg(path: &Path) -> Result<DynamicImage> {
    let data = std::fs::read(path)?;

    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();

    let tree = usvg::Tree::from_data(&data, &opt)
        .map_err(|e| AppError::other(format!("Could not parse that SVG: {e}")))?;

    let size = tree.size();
    let width = size.width().ceil().max(1.0) as u32;
    let height = size.height().ceil().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| AppError::other("That SVG has no visible area to rasterize."))?;

    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());

    let png = pixmap
        .encode_png()
        .map_err(|e| AppError::other(format!("Could not rasterize that SVG: {e}")))?;
    image::load_from_memory(&png)
        .map_err(|e| AppError::other(format!("Could not rasterize that SVG: {e}")))
}

/// Encode `image` to `output` in `options.format`, creating parent directories
/// as needed.
pub fn save(image: &DynamicImage, output: &Path, options: &ImageOptions) -> Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    match options.format {
        ImageFormat::Jpeg => {
            // JPEG carries no alpha channel; flatten onto white rather than
            // leaving it to whatever the encoder does with a dropped channel.
            let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
            let quality = options.quality.unwrap_or(90).clamp(1, 100);
            let file = std::fs::File::create(output)?;
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, quality);
            rgb.write_with_encoder(encoder)
                .map_err(|e| AppError::other(format!("Could not write the JPEG: {e}")))
        }
        ImageFormat::WebP => {
            let file = std::fs::File::create(output)?;
            let encoder = image::codecs::webp::WebPEncoder::new_lossless(file);
            image
                .write_with_encoder(encoder)
                .map_err(|e| AppError::other(format!("Could not write the WebP: {e}")))
        }
        ImageFormat::Ico => {
            // The ICO format caps out at 256px per side; scale down rather
            // than let the encoder reject a full-size photo.
            let capped = if image.width() > 256 || image.height() > 256 {
                image.resize(256, 256, image::imageops::FilterType::Lanczos3)
            } else {
                image.clone()
            };
            capped
                .save_with_format(output, options.format.codec())
                .map_err(|e| AppError::other(format!("Could not write the ICO: {e}")))
        }
        other => image.save_with_format(output, other.codec()).map_err(|e| {
            AppError::other(format!(
                "Could not write the {}: {e}",
                other.extension().to_uppercase()
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_image() -> DynamicImage {
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 255])))
    }

    #[test]
    fn png_round_trips_through_save_and_load() {
        let dir = std::env::temp_dir().join(format!("mk-raster-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("nested").join("dot.png");

        save(&tiny_image(), &out, &ImageOptions { format: ImageFormat::Png, quality: None }).unwrap();
        let read_back = load(&out).unwrap();

        assert_eq!(read_back.width(), 4);
        assert_eq!(read_back.height(), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_images_are_capped_before_writing_ico() {
        let dir = std::env::temp_dir().join(format!("mk-raster-ico-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("big.ico");

        let big = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(600, 300, image::Rgba([10, 10, 10, 255])));
        save(&big, &out, &ImageOptions { format: ImageFormat::Ico, quality: None }).unwrap();

        let read_back = load(&out).unwrap();
        assert!(read_back.width() <= 256 && read_back.height() <= 256);
        std::fs::remove_dir_all(&dir).ok();
    }
}
