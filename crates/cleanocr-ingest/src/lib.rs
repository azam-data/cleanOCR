//! Document ingestion, smart PDF classification, deskew, and illumination normalization.

pub mod pdf_render;
pub mod pdf_router;
pub mod preprocess;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cleanocr_ir::PageIngestMode;
use image::RgbImage;
use sha2::{Digest, Sha256};

pub use pdf_render::render_pdf_pages;
pub use pdf_router::{PdfPageClassification, classify_pdf_pages};
pub use preprocess::{
    PreprocessConfig, PreprocessedPage, estimate_skew_angle_deg, normalize_illumination,
    preprocess_raster,
};

/// Configuration for ingesting a PDF or raster image document.
#[derive(Debug, Clone, Copy)]
pub struct IngestConfig {
    /// Target rasterization DPI for PDF pages (default: 300).
    pub dpi: u32,
    /// Preprocessing settings applied to scanned/raster pages.
    pub preprocess: PreprocessConfig,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            dpi: 300,
            preprocess: PreprocessConfig::default(),
        }
    }
}

/// A single ingested page ready for layout analysis, OCR, and MRC compression.
#[derive(Debug, Clone)]
pub struct IngestedPage {
    pub page_index: usize,
    pub width_pt: f32,
    pub height_pt: f32,
    pub dpi: u32,
    pub ingest_mode: PageIngestMode,
    pub image: RgbImage,
    pub native_text: Option<String>,
    pub detected_skew_deg: f32,
}

/// Complete ingested document with source SHA-256 fingerprint and ordered pages.
#[derive(Debug, Clone)]
pub struct IngestedDocument {
    pub source_sha256: String,
    pub source_filename: Option<String>,
    pub pages: Vec<IngestedPage>,
}

/// Computes lowercase hex SHA-256 of raw input bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Ingests a PDF, PNG, JPEG, or TIFF file from disk, classifying PDF pages and running
/// deskew + illumination normalization on scanned/raster inputs.
pub fn ingest_document(path: &Path, config: &IngestConfig) -> Result<IngestedDocument> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read input document {}", path.display()))?;
    let source_filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(ToString::to_string);

    ingest_bytes(&bytes, source_filename, config)
}

/// Ingests a PDF or raster image directly from an in-memory byte slice.
pub fn ingest_bytes(
    bytes: &[u8],
    source_filename: Option<String>,
    config: &IngestConfig,
) -> Result<IngestedDocument> {
    let source_sha256 = sha256_hex(bytes);

    if bytes.starts_with(b"%PDF-") {
        let classifications = classify_pdf_pages(bytes)?;
        let rendered_pages = render_pdf_pages(bytes.to_vec(), config.dpi)?;

        if classifications.len() != rendered_pages.len() {
            bail!(
                "PDF page count mismatch: lopdf found {} pages, hayro rendered {} pages",
                classifications.len(),
                rendered_pages.len()
            );
        }

        let mut pages = Vec::with_capacity(classifications.len());
        for (cls, raw_img) in classifications.into_iter().zip(rendered_pages.into_iter()) {
            let (image, detected_skew_deg) = if cls.mode == PageIngestMode::ScannedRaster {
                let pre = preprocess_raster(&raw_img, &config.preprocess);
                (pre.image, pre.detected_skew_deg)
            } else {
                (raw_img, 0.0)
            };

            let native_text = if cls.native_text.is_empty() {
                None
            } else {
                Some(cls.native_text)
            };

            tracing::debug!(
                page = cls.page_index,
                mode = ?cls.mode,
                skew_deg = detected_skew_deg,
                "Ingested PDF page"
            );

            pages.push(IngestedPage {
                page_index: cls.page_index,
                width_pt: cls.width_pt,
                height_pt: cls.height_pt,
                dpi: config.dpi,
                ingest_mode: cls.mode,
                image,
                native_text,
                detected_skew_deg,
            });
        }

        return Ok(IngestedDocument {
            source_sha256,
            source_filename,
            pages,
        });
    }

    // Otherwise decode as a raster image (PNG, JPEG, TIFF, WebP, etc.).
    let dyn_img = image::load_from_memory(bytes)
        .context("failed to decode input as PDF or supported raster image")?;
    let rgb = dyn_img.to_rgb8();
    let pre = preprocess_raster(&rgb, &config.preprocess);

    let dpi = config.dpi.max(72);
    let width_pt = (pre.image.width() as f32 / dpi as f32) * 72.0;
    let height_pt = (pre.image.height() as f32 / dpi as f32) * 72.0;

    Ok(IngestedDocument {
        source_sha256,
        source_filename,
        pages: vec![IngestedPage {
            page_index: 0,
            width_pt,
            height_pt,
            dpi,
            ingest_mode: PageIngestMode::ScannedRaster,
            image: pre.image,
            native_text: None,
            detected_skew_deg: pre.detected_skew_deg,
        }],
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use image::{ImageFormat, Rgb};

    /// Minimal valid 1-page digital PDF containing embedded Helvetica text.
    fn make_minimal_digital_pdf() -> Vec<u8> {
        let content = b"BT /F1 18 Tf 50 750 Td (Hello cleanOCR native digital PDF document!) Tj ET";
        let mut pdf = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.4\n");

        let mut offsets = Vec::new();
        let push_obj = |buf: &mut Vec<u8>, offs: &mut Vec<usize>, body: &[u8]| {
            offs.push(buf.len());
            buf.extend_from_slice(body);
        };

        push_obj(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        push_obj(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        push_obj(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>\nendobj\n",
        );
        push_obj(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
        );

        let stream_obj = format!(
            "5 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
            content.len(),
            std::str::from_utf8(content).unwrap()
        );
        push_obj(&mut pdf, &mut offsets, stream_obj.as_bytes());

        let xref_pos = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
        for off in &offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn ingests_digital_pdf_and_classifies_as_native_text() {
        let pdf = make_minimal_digital_pdf();
        let doc = ingest_bytes(
            &pdf,
            Some("digital.pdf".to_string()),
            &IngestConfig {
                dpi: 72,
                ..IngestConfig::default()
            },
        )
        .expect("ingest digital PDF");

        assert_eq!(doc.source_sha256.len(), 64);
        assert_eq!(doc.pages.len(), 1);
        let p = &doc.pages[0];
        assert_eq!(p.ingest_mode, PageIngestMode::DigitalNativeText);
        assert!(p.native_text.as_deref().unwrap_or("").contains("cleanOCR"));
        assert_eq!(p.image.width(), 595);
        assert_eq!(p.image.height(), 842);
    }

    #[test]
    fn ingests_png_raster_as_scanned_page() {
        let img = RgbImage::from_pixel(120, 160, Rgb([245, 245, 245]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).expect("encode png");

        let doc = ingest_bytes(
            buf.get_ref(),
            Some("scan.png".to_string()),
            &IngestConfig::default(),
        )
        .expect("ingest PNG");

        assert_eq!(doc.pages.len(), 1);
        assert_eq!(doc.pages[0].ingest_mode, PageIngestMode::ScannedRaster);
        assert_eq!(doc.pages[0].image.width(), 120);
        assert_eq!(doc.pages[0].image.height(), 160);
    }
}
