//! Deterministic exporters for HTML5, PDF/A-3, LaTeX, Markdown, and DOCX.

pub mod docx;
pub mod html;
pub mod latex;
pub mod latex_math;
pub mod latex_pgfplots;
pub mod markdown;
pub mod pdf;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cleanocr_ir::DocumentIr;
use cleanocr_mrc::MrcLayers;

pub use docx::{DocxExporter, build_docx_bytes};
pub use html::{HtmlExporter, extract_ir_from_html, render_html};
pub use latex::{LatexExporter, render_latex};
pub use latex_math::{
    MathValidationError, escape_latex, sanitize_tex_comment, validate_latex_math,
};
pub use latex_pgfplots::{emit_pgfplots, is_supported_pgfplots_chart};
pub use markdown::{MarkdownExporter, render_markdown};
pub use pdf::{PdfExporter, bbox_to_pdf_rect, build_pdf_bytes, extract_ir_from_pdf};

/// Per-page raster, MRC, and cropped figure visual assets paired with a `PageIr`.
#[derive(Debug, Clone, Default)]
pub struct PageVisualAssets {
    pub page_index: usize,
    pub mrc: Option<MrcLayers>,
    pub fallback_raster_jpeg: Option<Vec<u8>>,
    /// Cropped PNG/JPEG/SVG assets keyed by `asset_id` or `fallback_asset_id`.
    pub figures: BTreeMap<String, Vec<u8>>,
}

/// Unified exporter contract compiling strictly from `DocumentIr` + `PageVisualAssets`.
pub trait Exporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()>;
}

/// Extracts an embedded `DocumentIr` from an archived cleanOCR `.pdf` or `.html` file.
pub fn extract_ir_from_file(input_path: &Path) -> Result<DocumentIr> {
    let bytes = fs::read(input_path)
        .with_context(|| format!("failed to read archived file {}", input_path.display()))?;

    let ext = input_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());

    if bytes.starts_with(b"%PDF-") || ext.as_deref() == Some("pdf") {
        return extract_ir_from_pdf(&bytes);
    }

    if matches!(ext.as_deref(), Some("html" | "htm")) {
        let html = String::from_utf8(bytes).with_context(|| {
            format!("archived HTML is not valid UTF-8: {}", input_path.display())
        })?;
        return extract_ir_from_html(&html);
    }

    if let Ok(text) = std::str::from_utf8(&bytes)
        && text.contains("id=\"cleanocr-ir\"")
    {
        return extract_ir_from_html(text);
    }

    bail!(
        "unsupported archived file format for {}: expected a cleanOCR .pdf or .html file",
        input_path.display()
    );
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read as _};

    use super::*;
    use cleanocr_ir::{
        AxisValue, BBox, Block, BlockKind, ChartAxis, ChartIr, ChartKind, ChartPoint, ChartSeries,
        ConfidenceWarning, DocumentMetadata, IR_SCHEMA_VERSION, ListItem, OverlayAsset,
        OverlayKind, PageIngestMode, PageIr, TableCell, TableIr, TextLine, TextSource, TextSpan,
    };

    const PNG_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2,
        0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 255, 255, 63,
        0, 5, 254, 2, 254, 13, 239, 70, 184, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn rich_fixture(include_charts: bool) -> (DocumentIr, Vec<PageVisualAssets>) {
        let b = BBox::new(0.10, 0.10, 0.90, 0.14);
        let left_box = BBox::new(0.10, 0.18, 0.46, 0.30);
        let right_box = BBox::new(0.54, 0.18, 0.90, 0.30);

        let mut blocks = vec![
            Block {
                id: "p0_h0".to_string(),
                bbox: BBox::new(0.10, 0.05, 0.90, 0.09),
                reading_order: 0,
                column_index: None,
                confidence: 0.99,
                kind: BlockKind::Heading {
                    level: 1,
                    lines: vec![TextLine {
                        bbox: BBox::new(0.10, 0.05, 0.90, 0.09),
                        spans: vec![TextSpan {
                            text: "Annual Research & Benchmarks (100%)".to_string(),
                            bbox: BBox::new(0.10, 0.05, 0.90, 0.09),
                            confidence: 0.99,
                            is_bold: true,
                            is_italic: false,
                            inline_math_latex: None,
                            source: TextSource::NativePdf,
                        }],
                    }],
                },
            },
            Block {
                id: "p0_col0".to_string(),
                bbox: left_box,
                reading_order: 1,
                column_index: Some(0),
                confidence: 0.96,
                kind: BlockKind::Paragraph {
                    lines: vec![TextLine {
                        bbox: left_box,
                        spans: vec![
                            TextSpan {
                                text: "Valid inline ".to_string(),
                                bbox: left_box,
                                confidence: 0.98,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            },
                            TextSpan {
                                text: "x^2 + y^2".to_string(),
                                bbox: left_box,
                                confidence: 0.95,
                                is_bold: false,
                                is_italic: true,
                                inline_math_latex: Some(r"x^2 + y^2 = r^2".to_string()),
                                source: TextSource::FormulaModel,
                            },
                        ],
                    }],
                },
            },
            Block {
                id: "p0_col1".to_string(),
                bbox: right_box,
                reading_order: 2,
                column_index: Some(1),
                confidence: 0.80,
                kind: BlockKind::Paragraph {
                    lines: vec![TextLine {
                        bbox: right_box,
                        spans: vec![
                            TextSpan {
                                text: "Broken inline ".to_string(),
                                bbox: right_box,
                                confidence: 0.97,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            },
                            TextSpan {
                                text: "a_b & c".to_string(),
                                bbox: right_box,
                                confidence: 0.65,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: Some(r"\frac{a}{b".to_string()),
                                source: TextSource::FormulaModel,
                            },
                        ],
                    }],
                },
            },
            Block {
                id: "p0_eq_ok".to_string(),
                bbox: b,
                reading_order: 3,
                column_index: None,
                confidence: 0.98,
                kind: BlockKind::DisplayFormula {
                    latex: r"\int_0^1 x^2 \, dx = \frac{1}{3}".to_string(),
                    fallback_asset_id: Some("eq_ok_png".to_string()),
                },
            },
            Block {
                id: "p0_eq_bad".to_string(),
                bbox: b,
                reading_order: 4,
                column_index: None,
                confidence: 0.55,
                kind: BlockKind::DisplayFormula {
                    latex: r"\begin{matrix} 1 & 2 \end{pmatrix}".to_string(),
                    fallback_asset_id: Some("eq_bad_png".to_string()),
                },
            },
            Block {
                id: "p0_tbl".to_string(),
                bbox: b,
                reading_order: 5,
                column_index: None,
                confidence: 0.97,
                kind: BlockKind::Table(TableIr {
                    num_rows: 2,
                    num_cols: 2,
                    caption: Some("Merged Table".to_string()),
                    cells: vec![
                        TableCell {
                            row: 0,
                            col: 0,
                            row_span: 1,
                            col_span: 2,
                            is_header: true,
                            bbox: b,
                            text: "Metrics \x26amp; Scores".to_string(),
                        },
                        TableCell {
                            row: 1,
                            col: 0,
                            row_span: 1,
                            col_span: 1,
                            is_header: false,
                            bbox: b,
                            text: "Precision".to_string(),
                        },
                        TableCell {
                            row: 1,
                            col: 1,
                            row_span: 1,
                            col_span: 1,
                            is_header: false,
                            bbox: b,
                            text: "99.1%".to_string(),
                        },
                    ],
                }),
            },
            Block {
                id: "p0_list".to_string(),
                bbox: b,
                reading_order: 6,
                column_index: None,
                confidence: 0.99,
                kind: BlockKind::List {
                    ordered: false,
                    items: vec![ListItem {
                        marker: "-".to_string(),
                        lines: vec![TextLine {
                            bbox: b,
                            spans: vec![TextSpan {
                                text: "Lossless JBIG2 default".to_string(),
                                bbox: b,
                                confidence: 0.99,
                                is_bold: true,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            }],
                        }],
                    }],
                },
            },
        ];

        if include_charts {
            blocks.push(Block {
                id: "p0_chart_bar".to_string(),
                bbox: BBox::new(0.10, 0.50, 0.90, 0.70),
                reading_order: 7,
                column_index: None,
                confidence: 0.90,
                kind: BlockKind::Figure {
                    asset_id: "chart_bar".to_string(),
                    caption: Some("Quarterly Sales".to_string()),
                    alt_text: Some("Bar chart".to_string()),
                    is_chart: true,
                    chart_data: Some(ChartIr {
                        kind: ChartKind::Bar,
                        title: Some("Sales".to_string()),
                        x_axis: ChartAxis {
                            label: Some("Quarter".to_string()),
                            is_log_scale: false,
                            min: None,
                            max: None,
                            categories: vec!["Q1".to_string(), "Q2".to_string()],
                        },
                        y_axis: ChartAxis {
                            label: Some("Units".to_string()),
                            is_log_scale: false,
                            min: Some(0.0),
                            max: Some(100.0),
                            categories: Vec::new(),
                        },
                        series: vec![ChartSeries {
                            name: Some("FY26".to_string()),
                            color_hex: Some("#1f77b4".to_string()),
                            points: vec![
                                ChartPoint {
                                    x: AxisValue::Category("Q1".to_string()),
                                    y: 45.0,
                                },
                                ChartPoint {
                                    x: AxisValue::Category("Q2".to_string()),
                                    y: 72.0,
                                },
                            ],
                        }],
                        has_legend: true,
                        grid: true,
                        extraction_confidence: 0.90,
                    }),
                    tikz_source: None,
                },
            });

            blocks.push(Block {
                id: "p0_chart_line".to_string(),
                bbox: BBox::new(0.10, 0.72, 0.90, 0.88),
                reading_order: 8,
                column_index: None,
                confidence: 0.92,
                kind: BlockKind::Figure {
                    asset_id: "chart_line".to_string(),
                    caption: Some("Throughput Scaling".to_string()),
                    alt_text: Some("Line chart".to_string()),
                    is_chart: true,
                    chart_data: Some(ChartIr {
                        kind: ChartKind::Line,
                        title: Some("Throughput".to_string()),
                        x_axis: ChartAxis {
                            label: Some("Threads".to_string()),
                            is_log_scale: false,
                            min: Some(1.0),
                            max: Some(8.0),
                            categories: Vec::new(),
                        },
                        y_axis: ChartAxis {
                            label: Some("Pages/sec".to_string()),
                            is_log_scale: true,
                            min: Some(1.0),
                            max: Some(100.0),
                            categories: Vec::new(),
                        },
                        series: vec![ChartSeries {
                            name: Some("cleanOCR".to_string()),
                            color_hex: None,
                            points: vec![
                                ChartPoint {
                                    x: AxisValue::Numeric(1.0),
                                    y: 4.0,
                                },
                                ChartPoint {
                                    x: AxisValue::Numeric(8.0),
                                    y: 28.5,
                                },
                            ],
                        }],
                        has_legend: true,
                        grid: true,
                        extraction_confidence: 0.92,
                    }),
                    tikz_source: None,
                },
            });
        }

        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Milestone 2 Golden Fixture".to_string()),
                source_sha256: "abcdef0123456789".to_string(),
                source_filename: Some("fixture.pdf".to_string()),
                detected_languages: vec!["en".to_string()],
                created_utc: "2026-11-23T00:00:00Z".to_string(),
                engine_version: "0.1.0".to_string(),
            },
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::HybridNativePlusOcr,
                blocks,
                overlays: vec![OverlayAsset {
                    asset_id: "seal_01".to_string(),
                    kind: OverlayKind::SealOrStamp,
                    bbox: BBox::new(0.75, 0.85, 0.90, 0.95),
                    svg_inline: Some("<svg width=\"10\" height=\"10\"></svg>".to_string()),
                    aria_hidden: true,
                    extracted_text: Some("VERIFIED".to_string()),
                }],
                low_confidence_flags: vec![ConfidenceWarning {
                    block_id: "p0_eq_bad".to_string(),
                    bbox: b,
                    confidence: 0.55,
                    reason: "Invalid display formula fallback triggered".to_string(),
                }],
            }],
        };

        let mut figures = BTreeMap::new();
        figures.insert("eq_ok_png".to_string(), PNG_1X1.to_vec());
        figures.insert("eq_bad_png".to_string(), PNG_1X1.to_vec());
        figures.insert("chart_bar".to_string(), PNG_1X1.to_vec());
        figures.insert("chart_line".to_string(), PNG_1X1.to_vec());

        let assets = vec![PageVisualAssets {
            page_index: 0,
            mrc: None,
            fallback_raster_jpeg: None,
            figures,
        }];

        (ir, assets)
    }

    #[test]
    fn all_five_exporters_compile_rich_fixture_and_roundtrip_ir() {
        let (ir, assets) = rich_fixture(true);

        // 1. LaTeX: checks multicols, valid math, broken math fallbacks, merged table, and pgfplots
        let tex = render_latex(&ir);
        assert!(tex.contains(r"\usepackage{pgfplots}"));
        assert!(tex.contains(r"\begin{multicols}{2}"));
        assert!(tex.contains(r"$x^2 + y^2 = r^2$"));
        assert!(tex.contains(r"a\_b \& c% CLEANOCR: invalid inline math"));
        assert!(tex.contains(r"\int_0^1 x^2 \, dx = \frac{1}{3}"));
        assert!(tex.contains(r"\includegraphics[width=0.80\linewidth]{assets/eq_bad_png.png}"));
        assert!(tex.contains(r"\multicolumn{2}{l}{Metrics \& Scores}"));
        assert!(tex.contains("symbolic x coords={Q1,Q2}"));
        assert!(tex.contains("ymode=log"));

        // 2. Markdown: checks valid math, fallback image, and HTML table for merged cells
        let md = render_markdown(&ir);
        assert!(md.contains(r"$x^2 + y^2 = r^2$"));
        assert!(md.contains("Broken inline a_b & c"));
        assert!(md.contains("![Formula](assets/eq_bad_png.png)"));
        assert!(md.contains("<th colspan=\"2\">Metrics \x26amp; Scores</th>"));

        // 3. HTML5: checks dual mode + lossless DocumentIr round-trip
        let html = render_html(&ir, &assets).expect("render HTML");
        let ir_from_html = extract_ir_from_html(&html).expect("extract IR from HTML");
        assert_eq!(ir_from_html, ir);

        // 4. PDF/A-3: checks OCGs, invisible text, + lossless DocumentIr round-trip
        let pdf_bytes = build_pdf_bytes(&ir, &assets).expect("build PDF");
        let ir_from_pdf = extract_ir_from_pdf(&pdf_bytes).expect("extract IR from PDF");
        assert_eq!(ir_from_pdf, ir);

        // 5. DOCX: checks valid OOXML zip package
        let docx_bytes = build_docx_bytes(&ir, &assets).expect("build DOCX");
        let mut zip = zip::ZipArchive::new(Cursor::new(docx_bytes)).expect("valid docx zip");
        let mut xml = String::new();
        zip.by_name("word/document.xml")
            .expect("document.xml")
            .read_to_string(&mut xml)
            .expect("read document.xml");
        assert!(xml.contains("Milestone 2 Golden Fixture"));
        assert!(xml.contains("Metrics \x26amp; Scores"));
    }

    #[test]
    fn latex_omits_pgfplots_when_document_has_no_charts() {
        let (ir_no_charts, _) = rich_fixture(false);
        let tex = render_latex(&ir_no_charts);
        assert!(!tex.contains(r"\usepackage{pgfplots}"));
        assert!(!tex.contains(r"\pgfplotsset"));
    }
}
