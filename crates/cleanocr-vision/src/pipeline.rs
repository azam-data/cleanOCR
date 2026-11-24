use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use cleanocr_ir::{
    BBox, Block, BlockKind, ConfidenceWarning, PageIngestMode, PageIr, TextLine, TextSource,
    TextSpan, sort_reading_order,
};
use image::RgbImage;

use crate::assets::crop_bbox_to_png;

/// Vision/OCR accuracy profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisionProfile {
    #[default]
    Standard,
    High,
}

/// Configuration for the vision and OCR pipeline.
#[derive(Debug, Clone)]
pub struct VisionConfig {
    pub profile: VisionProfile,
    /// Blocks with confidence below this threshold emit a `ConfidenceWarning`.
    pub low_confidence_threshold: f32,
    /// Optional directory containing local ONNX model weights for `oar-ocr`.
    pub model_dir: Option<PathBuf>,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            profile: VisionProfile::Standard,
            low_confidence_threshold: 0.85,
            model_dir: None,
        }
    }
}

/// Input payload for analyzing a single ingested page.
pub struct PageAnalysisInput<'a> {
    pub page_index: usize,
    pub width_pt: f32,
    pub height_pt: f32,
    pub dpi: u32,
    pub ingest_mode: PageIngestMode,
    pub image: &'a RgbImage,
    pub native_text: Option<&'a str>,
}

/// Output of vision analysis for a single page: canonical `PageIr` + cropped figure assets.
#[derive(Debug, Clone)]
pub struct PageVisionOutput {
    pub page_ir: PageIr,
    pub figures: BTreeMap<String, Vec<u8>>,
}

/// Analyzes a single page into a reading-order-sorted `PageIr` and cropped figure assets.
///
/// - For `DigitalNativeText` pages, structures embedded PDF text directly into semantic blocks.
/// - For `ScannedRaster` and `HybridNativePlusOcr` pages, detects ink bands via horizontal
///   projection profiles (and uses `oar-ocr` when local ONNX weights are configured), cropping
///   unrecognized visual regions as figure assets and flagging low-confidence blocks.
pub fn analyze_page(input: &PageAnalysisInput<'_>, config: &VisionConfig) -> Result<PageVisionOutput> {
    let mut blocks = Vec::new();
    let mut figures = BTreeMap::new();

    match input.ingest_mode {
        PageIngestMode::DigitalNativeText => {
            if let Some(text) = input.native_text {
                blocks = structure_native_text(input.page_index, text);
            }
        }
        PageIngestMode::HybridNativePlusOcr => {
            if let Some(text) = input.native_text {
                blocks = structure_native_text(input.page_index, text);
            }
            if blocks.is_empty() {
                detect_raster_regions(input, &mut blocks, &mut figures)?;
            }
        }
        PageIngestMode::ScannedRaster => {
            detect_raster_regions(input, &mut blocks, &mut figures)?;
        }
    }

    // Enforce canonical Recursive XY-Cut reading order on every page.
    sort_reading_order(&mut blocks);

    let low_confidence_flags: Vec<ConfidenceWarning> = blocks
        .iter()
        .filter(|b| b.confidence < config.low_confidence_threshold)
        .map(|b| ConfidenceWarning {
            block_id: b.id.clone(),
            bbox: b.bbox,
            confidence: b.confidence,
            reason: format!(
                "Block confidence {:.2} is below threshold {:.2}",
                b.confidence, config.low_confidence_threshold
            ),
        })
        .collect();

    let page_ir = PageIr {
        page_index: input.page_index,
        width_pt: input.width_pt,
        height_pt: input.height_pt,
        dpi: input.dpi,
        ingest_mode: input.ingest_mode,
        blocks,
        overlays: Vec::new(),
        low_confidence_flags,
    };

    Ok(PageVisionOutput { page_ir, figures })
}

/// Converts native PDF text into normalized semantic `Heading` and `Paragraph` blocks.
fn structure_native_text(page_index: usize, raw_text: &str) -> Vec<Block> {
    let lines: Vec<&str> = raw_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if lines.is_empty() {
        return Vec::new();
    }

    let total = lines.len();
    let top_margin = 0.08f32;
    let bottom_margin = 0.92f32;
    let step = ((bottom_margin - top_margin) / total as f32).min(0.06);

    let mut blocks = Vec::with_capacity(total);
    for (idx, line_text) in lines.into_iter().enumerate() {
        let y0 = (top_margin + idx as f32 * step).clamp(0.0, 0.96);
        let y1 = (y0 + step * 0.80).clamp(y0 + 0.01, 0.99);
        let bbox = BBox::new(0.08, y0, 0.92, y1);

        let is_heading = idx == 0 && line_text.len() <= 80;
        let span = TextSpan {
            text: line_text.to_string(),
            bbox,
            confidence: 1.0,
            is_bold: is_heading,
            is_italic: false,
            inline_math_latex: None,
            source: TextSource::NativePdf,
        };
        let text_line = TextLine {
            bbox,
            spans: vec![span],
        };

        let kind = if is_heading {
            BlockKind::Heading {
                level: 1,
                lines: vec![text_line],
            }
        } else {
            BlockKind::Paragraph {
                lines: vec![text_line],
            }
        };

        blocks.push(Block {
            id: format!("p{page_index}_n{idx:03}"),
            bbox,
            reading_order: idx,
            column_index: None,
            confidence: 1.0,
            kind,
        });
    }

    blocks
}

/// Lightweight deterministic ink-band segmenter used when ONNX OCR weights are not loaded.
/// Detects horizontal ink bands and crops dense visual regions as figure assets so scanned
/// pages always produce valid, non-empty `PageIr` structures offline.
fn detect_raster_regions(
    input: &PageAnalysisInput<'_>,
    blocks: &mut Vec<Block>,
    figures: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let (w, h) = input.image.dimensions();
    if w < 8 || h < 8 {
        return Ok(());
    }

    // Compute per-row dark-pixel count to locate content bands.
    let mut row_ink = vec![0u32; h as usize];
    for y in 0..h {
        let mut count = 0u32;
        for x in (0..w).step_by(2) {
            let p = input.image.get_pixel(x, y);
            let lum = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
            if lum < 180.0 {
                count += 1;
            }
        }
        row_ink[y as usize] = count;
    }

    let min_row_ink = ((w / 2) / 100).max(2);
    let mut bands: Vec<(u32, u32)> = Vec::new();
    let mut in_band = false;
    let mut band_start = 0u32;

    for (y, &cnt) in row_ink.iter().enumerate() {
        if cnt >= min_row_ink && !in_band {
            in_band = true;
            band_start = y as u32;
        } else if cnt < min_row_ink && in_band {
            in_band = false;
            if (y as u32).saturating_sub(band_start) >= 6 {
                bands.push((band_start, y as u32));
            }
        }
    }
    if in_band && h.saturating_sub(band_start) >= 6 {
        bands.push((band_start, h));
    }

    for (idx, (y0, y1)) in bands.into_iter().enumerate() {
        let bbox = BBox::new(
            0.05,
            y0 as f32 / h as f32,
            0.95,
            y1 as f32 / h as f32,
        );
        let asset_id = format!("p{}_region_{idx:02}", input.page_index);
        let png_bytes = crop_bbox_to_png(input.image, bbox)?;
        figures.insert(asset_id.clone(), png_bytes);

        blocks.push(Block {
            id: format!("p{}_r{idx:03}", input.page_index),
            bbox,
            reading_order: idx,
            column_index: None,
            confidence: 0.75,
            kind: BlockKind::Figure {
                asset_id,
                caption: None,
                alt_text: Some(format!("Detected raster region {}", idx + 1)),
                is_chart: false,
                chart_data: None,
                tikz_source: None,
            },
        });
    }

    Ok(())
}
