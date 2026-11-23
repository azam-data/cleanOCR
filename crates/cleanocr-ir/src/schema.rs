use serde::{Deserialize, Serialize};

pub const IR_SCHEMA_VERSION: &str = "1.1.0";

/// Normalized top-left origin bounding box in `[0.0, 1.0]`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BBox {
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    pub fn width(&self) -> f32 {
        (self.x1 - self.x0).max(0.0)
    }

    pub fn height(&self) -> f32 {
        (self.y1 - self.y0).max(0.0)
    }

    pub fn area(&self) -> f32 {
        self.width() * self.height()
    }

    pub fn intersection_area(&self, other: &BBox) -> f32 {
        let ix0 = self.x0.max(other.x0);
        let iy0 = self.y0.max(other.y0);
        let ix1 = self.x1.min(other.x1);
        let iy1 = self.y1.min(other.y1);
        (ix1 - ix0).max(0.0) * (iy1 - iy0).max(0.0)
    }

    pub fn iou(&self, other: &BBox) -> f32 {
        let inter = self.intersection_area(other);
        let union = self.area() + other.area() - inter;
        if union <= 0.0 { 0.0 } else { inter / union }
    }

    pub fn to_pixels(&self, img_w: u32, img_h: u32) -> (u32, u32, u32, u32) {
        (
            (self.x0 * img_w as f32).round().clamp(0.0, img_w as f32) as u32,
            (self.y0 * img_h as f32).round().clamp(0.0, img_h as f32) as u32,
            (self.x1 * img_w as f32).round().clamp(0.0, img_w as f32) as u32,
            (self.y1 * img_h as f32).round().clamp(0.0, img_h as f32) as u32,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentIr {
    pub schema_version: String,
    pub metadata: DocumentMetadata,
    pub pages: Vec<PageIr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub source_sha256: String,
    pub source_filename: Option<String>,
    pub detected_languages: Vec<String>,
    pub created_utc: String,
    pub engine_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageIr {
    pub page_index: usize,
    pub width_pt: f32,
    pub height_pt: f32,
    pub dpi: u32,
    pub ingest_mode: PageIngestMode,
    pub blocks: Vec<Block>,
    pub overlays: Vec<OverlayAsset>,
    pub low_confidence_flags: Vec<ConfidenceWarning>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PageIngestMode {
    ScannedRaster,
    DigitalNativeText,
    HybridNativePlusOcr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub id: String,
    pub bbox: BBox,
    pub reading_order: usize,
    pub column_index: Option<u8>,
    pub confidence: f32,
    pub kind: BlockKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Heading {
        level: u8,
        lines: Vec<TextLine>,
    },
    Paragraph {
        lines: Vec<TextLine>,
    },
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    Table(TableIr),
    DisplayFormula {
        latex: String,
        /// Cropped PNG asset key in `PageVisualAssets::figures` used as compile-safe fallback.
        fallback_asset_id: Option<String>,
    },
    Figure {
        asset_id: String,
        caption: Option<String>,
        alt_text: Option<String>,
        is_chart: bool,
        /// Tier 2 (optional): Structured numerical data for deterministic pgfplots emission.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chart_data: Option<ChartIr>,
        /// Tier 3 (optional): Pre-verified TikZ body for schematic/diagram synthesis.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tikz_source: Option<String>,
    },
    HeaderFooter {
        is_header: bool,
        text: String,
    },
    Footnote {
        marker: Option<String>,
        lines: Vec<TextLine>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextLine {
    pub bbox: BBox,
    pub spans: Vec<TextSpan>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextSpan {
    pub text: String,
    pub bbox: BBox,
    pub confidence: f32,
    pub is_bold: bool,
    pub is_italic: bool,
    /// Raw LaTeX without enclosing `$` delimiters. Validated before TeX emission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_math_latex: Option<String>,
    pub source: TextSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TextSource {
    NativePdf,
    Ocr,
    FormulaModel,
    VlmFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ListItem {
    pub marker: String,
    pub lines: Vec<TextLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableIr {
    pub num_rows: usize,
    pub num_cols: usize,
    pub cells: Vec<TableCell>,
    pub caption: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableCell {
    pub row: usize,
    pub col: usize,
    pub row_span: usize,
    pub col_span: usize,
    pub is_header: bool,
    pub bbox: BBox,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlayAsset {
    pub asset_id: String,
    pub kind: OverlayKind,
    pub bbox: BBox,
    pub svg_inline: Option<String>,
    pub aria_hidden: bool,
    pub extracted_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OverlayKind {
    Signature,
    SealOrStamp,
    Watermark,
    Logo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfidenceWarning {
    pub block_id: String,
    pub bbox: BBox,
    pub confidence: f32,
    pub reason: String,
}

// ---------- Tier 2 Structured Chart IR (Deterministic pgfplots) ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartIr {
    pub kind: ChartKind,
    pub title: Option<String>,
    pub x_axis: ChartAxis,
    pub y_axis: ChartAxis,
    pub series: Vec<ChartSeries>,
    pub has_legend: bool,
    pub grid: bool,
    pub extraction_confidence: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    HorizontalBar,
    StackedBar,
    Line,
    Scatter,
    /// Pie/donut or complex plots always fall back to `\includegraphics` for TeX portability.
    UnsupportedFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartAxis {
    pub label: Option<String>,
    pub is_log_scale: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Populated for categorical bar axes (maps to pgfplots `symbolic x coords`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartSeries {
    pub name: Option<String>,
    pub color_hex: Option<String>,
    pub points: Vec<ChartPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChartPoint {
    pub x: AxisValue,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AxisValue {
    Numeric(f64),
    Category(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_geometry_and_iou() {
        let a = BBox::new(0.0, 0.0, 0.5, 0.5);
        let b = BBox::new(0.25, 0.25, 0.75, 0.75);
        let disjoint = BBox::new(0.6, 0.6, 1.0, 1.0);

        assert!((a.area() - 0.25).abs() < 1e-6);
        assert!((a.intersection_area(&b) - 0.0625).abs() < 1e-6);
        assert!((a.iou(&b) - (0.0625 / 0.4375)).abs() < 1e-6);
        assert_eq!(a.intersection_area(&disjoint), 0.0);
        assert_eq!(a.iou(&disjoint), 0.0);
        assert!((a.iou(&a) - 1.0).abs() < 1e-6);

        let inverted = BBox::new(0.8, 0.8, 0.2, 0.2);
        assert_eq!(inverted.width(), 0.0);
        assert_eq!(inverted.height(), 0.0);
        assert_eq!(inverted.area(), 0.0);
        assert_eq!(inverted.iou(&inverted), 0.0);
    }

    #[test]
    fn bbox_to_pixels_clamps_out_of_bounds() {
        let normal = BBox::new(0.1, 0.2, 0.5, 0.75);
        assert_eq!(normal.to_pixels(1000, 2000), (100, 400, 500, 1500));

        let out_of_bounds = BBox::new(-0.2, -0.1, 1.4, 1.25);
        assert_eq!(out_of_bounds.to_pixels(800, 600), (0, 0, 800, 600));
    }

    #[test]
    fn document_ir_json_roundtrip_with_inline_math_and_chart() {
        let doc = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Quarterly Results".to_string()),
                source_sha256: "deadbeef".to_string(),
                source_filename: Some("report.pdf".to_string()),
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
                blocks: vec![
                    Block {
                        id: "p0_b00".to_string(),
                        bbox: BBox::new(0.1, 0.1, 0.9, 0.15),
                        reading_order: 0,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::Paragraph {
                            lines: vec![TextLine {
                                bbox: BBox::new(0.1, 0.1, 0.9, 0.15),
                                spans: vec![
                                    TextSpan {
                                        text: "Let ".to_string(),
                                        bbox: BBox::new(0.1, 0.1, 0.2, 0.15),
                                        confidence: 0.99,
                                        is_bold: false,
                                        is_italic: false,
                                        inline_math_latex: None,
                                        source: TextSource::NativePdf,
                                    },
                                    TextSpan {
                                        text: "x = a + b".to_string(),
                                        bbox: BBox::new(0.2, 0.1, 0.4, 0.15),
                                        confidence: 0.96,
                                        is_bold: false,
                                        is_italic: true,
                                        inline_math_latex: Some(r"x = \alpha + \beta".to_string()),
                                        source: TextSource::FormulaModel,
                                    },
                                ],
                            }],
                        },
                    },
                    Block {
                        id: "p0_b01".to_string(),
                        bbox: BBox::new(0.1, 0.2, 0.9, 0.3),
                        reading_order: 1,
                        column_index: None,
                        confidence: 0.97,
                        kind: BlockKind::DisplayFormula {
                            latex: r"\int_0^1 x^2 \, dx = \frac{1}{3}".to_string(),
                            fallback_asset_id: Some("formula_01".to_string()),
                        },
                    },
                    Block {
                        id: "p0_b02".to_string(),
                        bbox: BBox::new(0.1, 0.35, 0.9, 0.7),
                        reading_order: 2,
                        column_index: None,
                        confidence: 0.94,
                        kind: BlockKind::Figure {
                            asset_id: "fig_01".to_string(),
                            caption: Some("Revenue by Quarter".to_string()),
                            alt_text: Some("Bar chart of quarterly revenue".to_string()),
                            is_chart: true,
                            chart_data: Some(ChartIr {
                                kind: ChartKind::Bar,
                                title: Some("Revenue".to_string()),
                                x_axis: ChartAxis {
                                    label: Some("Quarter".to_string()),
                                    is_log_scale: false,
                                    min: None,
                                    max: None,
                                    categories: vec!["Q1".to_string(), "Q2".to_string()],
                                },
                                y_axis: ChartAxis {
                                    label: Some("USD (M)".to_string()),
                                    is_log_scale: false,
                                    min: Some(0.0),
                                    max: Some(100.0),
                                    categories: Vec::new(),
                                },
                                series: vec![ChartSeries {
                                    name: Some("2026".to_string()),
                                    color_hex: Some("#1f77b4".to_string()),
                                    points: vec![
                                        ChartPoint {
                                            x: AxisValue::Category("Q1".to_string()),
                                            y: 42.5,
                                        },
                                        ChartPoint {
                                            x: AxisValue::Category("Q2".to_string()),
                                            y: 58.0,
                                        },
                                    ],
                                }],
                                has_legend: true,
                                grid: true,
                                extraction_confidence: 0.91,
                            }),
                            tikz_source: None,
                        },
                    },
                ],
                overlays: vec![OverlayAsset {
                    asset_id: "seal_01".to_string(),
                    kind: OverlayKind::SealOrStamp,
                    bbox: BBox::new(0.7, 0.75, 0.85, 0.88),
                    svg_inline: Some("<svg></svg>".to_string()),
                    aria_hidden: true,
                    extracted_text: Some("APPROVED".to_string()),
                }],
                low_confidence_flags: vec![ConfidenceWarning {
                    block_id: "p0_b02".to_string(),
                    bbox: BBox::new(0.1, 0.35, 0.9, 0.7),
                    confidence: 0.91,
                    reason: "Chart values extracted via ML; verify numbers before publication"
                        .to_string(),
                }],
            }],
        };

        let json = serde_json::to_string_pretty(&doc).expect("serialize DocumentIr");
        let parsed: DocumentIr = serde_json::from_str(&json).expect("deserialize DocumentIr");
        assert_eq!(doc, parsed);
    }
}
