//! Layout detection, OCR, table structure, formula recognition, and asset extraction.

pub mod assets;
pub mod ocr_engine;
pub mod pipeline;

pub use assets::{crop_bbox_to_jpeg, crop_bbox_to_png, vectorize_overlay_svg};
pub use ocr_engine::{OcrEngine, OcrModelPaths, discover_default_models_dir};
pub use pipeline::{
    PageAnalysisInput, PageVisionOutput, VisionConfig, VisionProfile, analyze_page,
};

#[cfg(test)]
mod tests {
    use super::*;
    use cleanocr_ir::{BlockKind, PageIngestMode};
    use image::{Rgb, RgbImage};

    #[test]
    fn structures_native_digital_text_into_heading_and_paragraphs() {
        let img = RgbImage::from_pixel(200, 280, Rgb([255, 255, 255]));
        let input = PageAnalysisInput {
            page_index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            dpi: 300,
            ingest_mode: PageIngestMode::DigitalNativeText,
            image: &img,
            native_text: Some("Quarterly Financial Report\nRevenue grew by 18% year over year."),
        };

        let out =
            analyze_page(&input, &VisionConfig::default(), None).expect("analyze native page");
        assert_eq!(out.page_ir.blocks.len(), 2);
        assert!(matches!(
            out.page_ir.blocks[0].kind,
            BlockKind::Heading { level: 1, .. }
        ));
        assert!(matches!(
            out.page_ir.blocks[1].kind,
            BlockKind::Paragraph { .. }
        ));
        assert!(out.page_ir.low_confidence_flags.is_empty());
    }

    #[test]
    fn detects_scanned_ink_regions_and_flags_low_confidence() {
        let mut img = RgbImage::from_pixel(200, 200, Rgb([250, 250, 250]));
        for y in 40..70 {
            for x in 20..180 {
                img.put_pixel(x, y, Rgb([20, 20, 20]));
            }
        }

        let input = PageAnalysisInput {
            page_index: 0,
            width_pt: 595.0,
            height_pt: 842.0,
            dpi: 300,
            ingest_mode: PageIngestMode::ScannedRaster,
            image: &img,
            native_text: None,
        };

        let out =
            analyze_page(&input, &VisionConfig::default(), None).expect("analyze scanned page");
        assert_eq!(out.page_ir.blocks.len(), 1);
        assert_eq!(out.figures.len(), 1);
        assert_eq!(out.page_ir.low_confidence_flags.len(), 1);
    }
}
