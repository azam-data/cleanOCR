//! Mixed Raster Content (MRC) 3-layer decomposition (Mask + FG + BG).

pub mod binarize;
pub mod inpaint;

use std::io::Cursor;

use anyhow::{Context, Result};
use cleanocr_ir::BBox;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{FilterType, resize};
use image::{ColorType, RgbImage};
use jbig2enc_rust::{encode_single_image, encode_single_image_lossless};

pub use binarize::{BinarizeConfig, build_photo_mask, extract_ink_mask, pack_mask_msb_first};
pub use inpaint::pull_push_inpaint;

/// MRC JBIG2 compression profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MrcCompressionProfile {
    /// Lossless generic JBIG2 (`Jbig2Config::lossless()`). Safe default for all documents.
    #[default]
    Lossless,
    /// Opt-in lossy symbol-dictionary JBIG2 (`Jbig2Config::text()`) for archival books only.
    BookMax,
}

/// Configuration for the 3-layer MRC decomposition pipeline.
#[derive(Debug, Clone, Copy)]
pub struct MrcConfig {
    pub profile: MrcCompressionProfile,
    pub binarize: BinarizeConfig,
    /// Downsample divisor for the foreground ink color layer (default: 3 -> 1/3 resolution).
    pub fg_downsample: u32,
    /// Downsample divisor for the background paper/photo layer (default: 2 -> 1/2 resolution).
    pub bg_downsample: u32,
    /// JPEG quality (1..=100) for the foreground ink layer (default: 55).
    pub fg_jpeg_quality: u8,
    /// JPEG quality (1..=100) for the background layer (default: 72).
    pub bg_jpeg_quality: u8,
}

impl Default for MrcConfig {
    fn default() -> Self {
        Self {
            profile: MrcCompressionProfile::Lossless,
            binarize: BinarizeConfig::default(),
            fg_downsample: 3,
            bg_downsample: 2,
            fg_jpeg_quality: 55,
            bg_jpeg_quality: 72,
        }
    }
}

/// Compressed 3-layer Mixed Raster Content representation of a single page.
///
/// - `mask_*`: Full-resolution 1-bit stencil for sharp text, thin lines, and stamps.
/// - `fg_*`: Downsampled RGB ink color map punched out by the mask.
/// - `bg_*`: Downsampled RGB background holding paper tone and photographs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrcLayers {
    pub width: u32,
    pub height: u32,
    /// Full-resolution 1-bit JBIG2 stream for PDF `/Mask` XObjects (text strokes only).
    pub mask_jbig2: Vec<u8>,
    /// Optional full-resolution 1-bit JBIG2 stream for PDF `Tables` OCG (table grid rulings only).
    pub table_jbig2: Option<Vec<u8>>,
    /// Full-resolution 1-bit PNG for HTML CSS `mask-image`.
    pub mask_png_1bit: Vec<u8>,
    /// Downsampled RGB JPEG foreground ink layer.
    pub fg_jpeg: Vec<u8>,
    pub fg_width: u32,
    pub fg_height: u32,
    /// Downsampled RGB JPEG background layer with text and table lines inpainted out.
    pub bg_jpeg: Vec<u8>,
    pub bg_width: u32,
    pub bg_height: u32,
}

impl MrcLayers {
    /// Total compressed payload size in bytes for PDF embedding (JBIG2 masks + FG JPEG + BG JPEG).
    pub fn total_pdf_bytes(&self) -> usize {
        self.mask_jbig2.len()
            + self.table_jbig2.as_ref().map_or(0, Vec::len)
            + self.fg_jpeg.len()
            + self.bg_jpeg.len()
    }
}

/// Returns `true` when `figure_boxes` cover more than 65% of the page area, indicating
/// an image-heavy page that should bypass 4-layer MRC and use a single compressed JPEG.
pub fn should_bypass_mrc(figure_boxes: &[BBox]) -> bool {
    if figure_boxes.is_empty() {
        return false;
    }
    const GRID: usize = 100;
    let mut covered = [false; GRID * GRID];
    for b in figure_boxes {
        let x0 = ((b.x0.clamp(0.0, 1.0) * GRID as f32).floor() as usize).min(GRID);
        let y0 = ((b.y0.clamp(0.0, 1.0) * GRID as f32).floor() as usize).min(GRID);
        let x1 = ((b.x1.clamp(0.0, 1.0) * GRID as f32).ceil() as usize).min(GRID);
        let y1 = ((b.y1.clamp(0.0, 1.0) * GRID as f32).ceil() as usize).min(GRID);
        for y in y0..y1 {
            for x in x0..x1 {
                covered[y * GRID + x] = true;
            }
        }
    }
    let count = covered.iter().filter(|&&c| c).count();
    (count as f32 / (GRID * GRID) as f32) > 0.65
}

/// Decomposes a full-resolution RGB page into MRC layers (1-bit Text Mask + optional 1-bit Table
/// Grid Mask + FG JPEG + BG JPEG).
///
/// `figure_boxes` are normalized `[0.0, 1.0]` bounding boxes of photographs/figures that must
/// remain protected in the continuous-tone background layer.
pub fn decompose_mrc(
    page_img: &RgbImage,
    figure_boxes: &[BBox],
    table_grid_mask: Option<&[u8]>,
    config: &MrcConfig,
) -> Result<MrcLayers> {
    let width = page_img.width();
    let height = page_img.height();

    // 1. Extract 1-bit foreground ink mask (photo-protected Sauvola + colored ink + despeckle).
    let mut ink_mask = extract_ink_mask(page_img, figure_boxes, &config.binarize);

    // 2. Subtract table_grid_mask from the 1-bit text mask and encode table_jbig2 separately.
    let mut table_bool = vec![false; ink_mask.len()];
    let mut table_jbig2 = None;
    if let Some(tmask) = table_grid_mask
        && tmask.len() == ink_mask.len()
    {
        let mut any_table = false;
        for ((dst, &src), text_px) in table_bool
            .iter_mut()
            .zip(tmask.iter())
            .zip(ink_mask.iter_mut())
        {
            if src != 0 {
                *dst = true;
                *text_px = false;
                any_table = true;
            }
        }
        if any_table {
            table_jbig2 = Some(encode_mask_jbig2(
                &table_bool,
                width,
                height,
                config.profile,
            )?);
        }
    }

    // 3. Encode 1-bit text mask to JBIG2 (PDF fragment mode) and 1-bit PNG (HTML CSS mask-image).
    let mask_jbig2 = encode_mask_jbig2(&ink_mask, width, height, config.profile)?;
    let mask_png_1bit = encode_mask_png_1bit(&ink_mask, width, height)?;

    // 4. Halo-free Pull-Push inpainting at full resolution BEFORE downsampling
    //    (inpaint BOTH text pixels and table_grid_mask pixels out of BG).
    let bg_known: Vec<bool> = ink_mask
        .iter()
        .zip(table_bool.iter())
        .map(|(&is_ink, &is_tbl)| !is_ink && !is_tbl)
        .collect();
    let bg_full = pull_push_inpaint(page_img, &bg_known);
    let fg_full = pull_push_inpaint(page_img, &ink_mask);

    // 5. Downsample FG and BG layers and encode to RGB JPEG.
    let fg_width = (width / config.fg_downsample.max(1)).max(1);
    let fg_height = (height / config.fg_downsample.max(1)).max(1);
    let fg_small = resize(&fg_full, fg_width, fg_height, FilterType::Triangle);
    let fg_jpeg = encode_rgb_jpeg(&fg_small, config.fg_jpeg_quality)?;

    let bg_width = (width / config.bg_downsample.max(1)).max(1);
    let bg_height = (height / config.bg_downsample.max(1)).max(1);
    let bg_small = resize(&bg_full, bg_width, bg_height, FilterType::Triangle);
    let bg_jpeg = encode_rgb_jpeg(&bg_small, config.bg_jpeg_quality)?;

    Ok(MrcLayers {
        width,
        height,
        mask_jbig2,
        table_jbig2,
        mask_png_1bit,
        fg_jpeg,
        fg_width,
        fg_height,
        bg_jpeg,
        bg_width,
        bg_height,
    })
}

fn encode_mask_jbig2(
    ink_mask: &[bool],
    width: u32,
    height: u32,
    profile: MrcCompressionProfile,
) -> Result<Vec<u8>> {
    // jbig2enc-rust takes one byte per pixel, row-major, non-zero = black/ink.
    let pixels: Vec<u8> = ink_mask.iter().map(|&b| u8::from(b)).collect();

    let encoded = match profile {
        MrcCompressionProfile::Lossless => {
            encode_single_image_lossless(&pixels, width, height, true)
                .context("lossless JBIG2 mask encoding failed")?
        }
        MrcCompressionProfile::BookMax => encode_single_image(&pixels, width, height, true)
            .context("book-max JBIG2 mask encoding failed")?,
    };

    Ok(encoded.page_data)
}

fn encode_mask_png_1bit(ink_mask: &[bool], width: u32, height: u32) -> Result<Vec<u8>> {
    let packed = pack_mask_msb_first(ink_mask, width, height);
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        let mut writer = encoder
            .write_header()
            .context("failed to write 1-bit PNG mask header")?;
        writer
            .write_image_data(&packed)
            .context("failed to write 1-bit PNG mask data")?;
    }
    Ok(out)
}

fn encode_rgb_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ColorType::Rgb8.into(),
        )
        .context("failed to encode RGB JPEG layer")?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn lossless_mrc_achieves_high_compression_on_synthetic_a4_page() {
        // 300 DPI A4-proportioned synthetic document (1240x1754 for fast CI execution)
        // with paper background and dense dark text-like horizontal strokes.
        let w = 1240u32;
        let h = 1754u32;
        let mut page = RgbImage::from_pixel(w, h, Rgb([248, 246, 240]));

        for row in 0..45u32 {
            let y_base = 120 + row * 34;
            for word in 0..10u32 {
                let x_base = 100 + word * 104;
                for dy in 0..12u32 {
                    for dx in 0..76u32 {
                        if (dx + dy) % 3 != 0 {
                            page.put_pixel(x_base + dx, y_base + dy, Rgb([20, 20, 22]));
                        }
                    }
                }
            }
        }

        let baseline_jpeg = encode_rgb_jpeg(&page, 85).expect("encode baseline JPEG");
        let layers = decompose_mrc(&page, &[], None, &MrcConfig::default()).expect("decompose MRC");

        assert_eq!(layers.width, w);
        assert_eq!(layers.height, h);
        assert!(!layers.mask_jbig2.is_empty());
        assert!(layers.table_jbig2.is_none());
        assert!(layers.mask_png_1bit.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(layers.fg_jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(layers.bg_jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));

        let ratio = baseline_jpeg.len() as f64 / layers.total_pdf_bytes() as f64;
        assert!(
            ratio >= 3.0,
            "expected strong MRC compression ratio vs Q85 JPEG, got {ratio:.2}x (baseline={} B, mrc={} B)",
            baseline_jpeg.len(),
            layers.total_pdf_bytes()
        );
    }

    #[test]
    fn bypasses_mrc_when_photo_coverage_exceeds_65_percent() {
        assert!(!should_bypass_mrc(&[BBox::new(0.1, 0.1, 0.5, 0.5)]));
        assert!(should_bypass_mrc(&[BBox::new(0.05, 0.05, 0.90, 0.94)]));
    }

    #[test]
    fn splits_table_grid_mask_into_separate_table_jbig2_stream() {
        let w = 120u32;
        let h = 120u32;
        let mut page = RgbImage::from_pixel(w, h, Rgb([250, 250, 250]));
        let mut table_mask = vec![0u8; (w * h) as usize];

        for x in 10..110 {
            page.put_pixel(x, 30, Rgb([10, 10, 10]));
            table_mask[30 * w as usize + x as usize] = 1;
        }
        for y in 50..62 {
            for x in 20..80 {
                page.put_pixel(x, y, Rgb([15, 15, 15]));
            }
        }

        let layers = decompose_mrc(&page, &[], Some(&table_mask), &MrcConfig::default())
            .expect("decompose MRC with table mask");
        assert!(layers.table_jbig2.is_some());
        assert!(!layers.table_jbig2.unwrap().is_empty());
    }
}
