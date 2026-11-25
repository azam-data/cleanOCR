use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use cleanocr_ir::{
    BBox, Block, BlockKind, ConfidenceWarning, PageIngestMode, PageIr, TableCell, TableIr,
    TextLine, TextSource, TextSpan, sort_reading_order,
};
use image::RgbImage;

use crate::assets::crop_bbox_to_png;
use crate::ocr_engine::{OcrEngine, TextRecognizer};
use crate::table_lattice::{crop_cell_padded, detect_lattice_tables, erase_table_grid_lines};

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
/// - For `ScannedRaster` and `HybridNativePlusOcr` pages, runs `oar-ocr` when `ocr_engine`
///   or `config.model_dir` is provided, or falls back to deterministic ink-band segmentation offline.
pub fn analyze_page(
    input: &PageAnalysisInput<'_>,
    config: &VisionConfig,
    ocr_engine: Option<&OcrEngine>,
) -> Result<PageVisionOutput> {
    analyze_page_with_recognizer(input, config, ocr_engine)
}

/// Generic version of `analyze_page` accepting any `TextRecognizer` implementation (e.g. a mock
/// recognizer for offline tests).
pub fn analyze_page_with_recognizer<R: TextRecognizer + ?Sized>(
    input: &PageAnalysisInput<'_>,
    config: &VisionConfig,
    recognizer: Option<&R>,
) -> Result<PageVisionOutput> {
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
                if let Some(rec) = recognizer {
                    blocks = recognize_with_lattice_tables(input.image, input.page_index, rec)?;
                } else if let Some(model_dir) = &config.model_dir {
                    let engine = OcrEngine::from_model_dir(model_dir)?;
                    blocks = recognize_with_lattice_tables(input.image, input.page_index, &engine)?;
                } else {
                    detect_raster_regions(input, &mut blocks, &mut figures)?;
                }
            }
        }
        PageIngestMode::ScannedRaster => {
            if let Some(rec) = recognizer {
                blocks = recognize_with_lattice_tables(input.image, input.page_index, rec)?;
            } else if let Some(model_dir) = &config.model_dir {
                let engine = OcrEngine::from_model_dir(model_dir)?;
                blocks = recognize_with_lattice_tables(input.image, input.page_index, &engine)?;
            } else {
                detect_raster_regions(input, &mut blocks, &mut figures)?;
            }
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
        let bbox = BBox::new(0.05, y0 as f32 / h as f32, 0.95, y1 as f32 / h as f32);
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

fn recognize_with_lattice_tables<R: TextRecognizer + ?Sized>(
    img: &RgbImage,
    page_index: usize,
    recognizer: &R,
) -> Result<Vec<Block>> {
    let lattice = detect_lattice_tables(img);
    if lattice.tables.is_empty() {
        return recognizer.recognize_page(img, page_index);
    }

    let cleaned = erase_table_grid_lines(img, &lattice.table_grid_mask);
    let flat_blocks = recognizer.recognize_page(&cleaned, page_index)?;

    // Exclude flat OCR blocks that overlap any detected lattice table so table text is never duplicated.
    let mut blocks: Vec<Block> = flat_blocks
        .into_iter()
        .filter(|b| {
            let cx = 0.5 * (b.bbox.x0 + b.bbox.x1);
            let cy = 0.5 * (b.bbox.y0 + b.bbox.y1);
            !lattice.tables.iter().any(|tbl| {
                let inside_center = cx >= tbl.bbox.x0
                    && cx <= tbl.bbox.x1
                    && cy >= tbl.bbox.y0
                    && cy <= tbl.bbox.y1;
                let area = b.bbox.area();
                let high_overlap = area > 0.0 && (b.bbox.intersection_area(&tbl.bbox) / area) > 0.5;
                inside_center || high_overlap
            })
        })
        .collect();

    for (t_idx, table) in lattice.tables.iter().enumerate() {
        let mut cells = Vec::with_capacity(table.cells.len());
        let mut conf_sum = 0.0f32;

        for cell in &table.cells {
            let crop = crop_cell_padded(&cleaned, cell.bbox);
            let (text, conf) = recognizer.recognize_crop(&crop)?;
            conf_sum += conf;

            cells.push(TableCell {
                row: cell.row,
                col: cell.col,
                row_span: cell.row_span,
                col_span: cell.col_span,
                is_header: cell.row == 0,
                bbox: cell.bbox,
                text: text.trim().to_string(),
            });
        }

        let table_conf = if cells.is_empty() {
            1.0
        } else {
            conf_sum / cells.len() as f32
        };

        blocks.push(Block {
            id: format!("p{page_index}_tbl{t_idx:02}"),
            bbox: table.bbox,
            reading_order: 0,
            column_index: None,
            confidence: table_conf,
            kind: BlockKind::Table(TableIr {
                num_rows: table.num_rows,
                num_cols: table.num_cols,
                cells,
                caption: None,
            }),
        });
    }

    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use cleanocr_ir::{DocumentIr, DocumentMetadata, IR_SCHEMA_VERSION};
    use image::Rgb;

    use super::*;

    struct MockTableRecognizer {
        crop_dims: Mutex<Vec<(u32, u32)>>,
    }

    impl TextRecognizer for MockTableRecognizer {
        fn recognize_page(&self, _img: &RgbImage, page_index: usize) -> Result<Vec<Block>> {
            // Emit one paragraph outside the table and one duplicate span inside the table bbox.
            let outside_bbox = BBox::new(0.10, 0.05, 0.90, 0.12);
            let inside_table_bbox = BBox::new(0.20, 0.30, 0.40, 0.36);

            Ok(vec![
                Block {
                    id: format!("p{page_index}_ocr000"),
                    bbox: outside_bbox,
                    reading_order: 0,
                    column_index: None,
                    confidence: 0.98,
                    kind: BlockKind::Paragraph {
                        lines: vec![TextLine {
                            bbox: outside_bbox,
                            spans: vec![TextSpan {
                                text: "Circular Header".to_string(),
                                bbox: outside_bbox,
                                confidence: 0.98,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            }],
                        }],
                    },
                },
                Block {
                    id: format!("p{page_index}_ocr001"),
                    bbox: inside_table_bbox,
                    reading_order: 1,
                    column_index: None,
                    confidence: 0.95,
                    kind: BlockKind::Paragraph {
                        lines: vec![TextLine {
                            bbox: inside_table_bbox,
                            spans: vec![TextSpan {
                                text: "DUPLICATE_SHOULD_BE_EXCLUDED".to_string(),
                                bbox: inside_table_bbox,
                                confidence: 0.95,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            }],
                        }],
                    },
                },
            ])
        }

        fn recognize_crop(&self, crop: &RgbImage) -> Result<(String, f32)> {
            let mut dims = self.crop_dims.lock().expect("lock crop_dims");
            let idx = dims.len();
            dims.push((crop.width(), crop.height()));
            let label = match idx {
                0 => "Tenure",
                1 => "Rate",
                2 => "1 Year",
                _ => "10.00%",
            };
            Ok((label.to_string(), 0.99))
        }
    }

    #[test]
    fn per_cell_padded_table_ocr_emits_table_without_duplicates_and_renders_gfm() {
        let w = 400u32;
        let h = 300u32;
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        let black = Rgb([10, 10, 10]);

        // Draw a 2x2 bordered table in [x: 60..340, y: 80..200].
        let xs = [60u32, 200, 340];
        let ys = [80u32, 140, 200];
        for &y in &ys {
            for x in xs[0]..=xs[2] {
                img.put_pixel(x, y, black);
            }
        }
        for &x in &xs {
            for y in ys[0]..=ys[2] {
                img.put_pixel(x, y, black);
            }
        }

        let rec = MockTableRecognizer {
            crop_dims: Mutex::new(Vec::new()),
        };

        let out = analyze_page_with_recognizer(
            &PageAnalysisInput {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::ScannedRaster,
                image: &img,
                native_text: None,
            },
            &VisionConfig::default(),
            Some(&rec),
        )
        .expect("analyze page with mock recognizer");

        // Should have 1 outside paragraph + 1 Table block (no duplicate paragraph inside table).
        assert_eq!(out.page_ir.blocks.len(), 2);
        assert!(matches!(
            out.page_ir.blocks[0].kind,
            BlockKind::Paragraph { .. }
        ));
        assert!(matches!(out.page_ir.blocks[1].kind, BlockKind::Table(_)));

        // Every cell crop (140x60 inset by 2px on each side = 136x56) + (8x6 white pad) = 144x62.
        let dims = rec.crop_dims.lock().expect("lock crop_dims");
        assert_eq!(dims.len(), 4);
        for &(cw, ch) in dims.iter() {
            assert_eq!((cw, ch), (144, 62));
        }

        let doc_ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: None,
                source_sha256: "test".to_string(),
                source_filename: None,
                detected_languages: vec!["en".to_string()],
                created_utc: "2026-11-25T00:00:00Z".to_string(),
                engine_version: "0.1.0".to_string(),
            },
            pages: vec![out.page_ir],
        };

        let md = cleanocr_export::render_markdown(&doc_ir);
        assert!(md.contains("Circular Header"));
        assert!(!md.contains("DUPLICATE_SHOULD_BE_EXCLUDED"));
        assert!(md.contains("| 1 Year | 10.00% |"));
    }
}
