use std::fs;
use std::io::{Cursor, Read as _};
use std::sync::Mutex;

use anyhow::Result;
use cleanocr_export::{
    Exporter, MarkdownExporter, PdfExporter, XlsxExporter, extract_ir_from_file,
};
use cleanocr_ir::{
    BBox, Block, BlockKind, DocumentIr, DocumentMetadata, IR_SCHEMA_VERSION, PageIngestMode,
    TextLine, TextSource, TextSpan,
};
use cleanocr_mrc::{MrcConfig, decompose_mrc};
use cleanocr_vision::{
    PageAnalysisInput, TextRecognizer, VisionConfig, analyze_page_with_recognizer,
};
use image::{Rgb, RgbImage};
use zip::ZipArchive;

struct MockCircularRecognizer {
    cell_idx: Mutex<usize>,
}

impl MockCircularRecognizer {
    fn new() -> Self {
        Self {
            cell_idx: Mutex::new(0),
        }
    }
}

fn make_para(id: &str, text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> Block {
    let bbox = BBox::new(x0, y0, x1, y1);
    Block {
        id: id.to_string(),
        bbox,
        reading_order: 0,
        column_index: None,
        confidence: 0.98,
        kind: BlockKind::Paragraph {
            lines: vec![TextLine {
                bbox,
                spans: vec![TextSpan {
                    text: text.to_string(),
                    bbox,
                    confidence: 0.98,
                    is_bold: false,
                    is_italic: false,
                    inline_math_latex: None,
                    source: TextSource::Ocr,
                }],
            }],
        },
    }
}

impl TextRecognizer for MockCircularRecognizer {
    fn recognize_page(&self, _img: &RgbImage, page_index: usize) -> Result<Vec<Block>> {
        if page_index == 0 {
            Ok(vec![make_para(
                "p0_hdr",
                "Rate Circular 2026",
                0.22,
                0.04,
                0.90,
                0.08,
            )])
        } else {
            // Page 1: borderless 3-column summary table (3 rows).
            Ok(vec![
                make_para("p1_r0c0", "Product", 0.10, 0.15, 0.30, 0.18),
                make_para("p1_r0c1", "Min Deposit", 0.40, 0.15, 0.62, 0.18),
                make_para("p1_r0c2", "Rate", 0.72, 0.15, 0.88, 0.18),
                make_para("p1_r1c0", "Savings", 0.10, 0.22, 0.30, 0.25),
                make_para("p1_r1c1", "1,000.00", 0.40, 0.22, 0.62, 0.25),
                make_para("p1_r1c2", "9.50%", 0.72, 0.22, 0.88, 0.25),
                make_para("p1_r2c0", "FDR", 0.10, 0.29, 0.30, 0.32),
                make_para("p1_r2c1", "50,000.00", 0.40, 0.29, 0.62, 0.32),
                make_para("p1_r2c2", "10.00%", 0.72, 0.29, 0.88, 0.32),
            ])
        }
    }

    fn recognize_crop(&self, _crop: &RgbImage) -> Result<(String, f32)> {
        let mut idx = self.cell_idx.lock().expect("lock cell_idx");
        let cur = *idx;
        *idx += 1;
        // 4 cols row 0 (0..3), 1 merged cell row 1 (4), 4 cols row 2 (5..8) = 9 cells.
        let label = match cur {
            0 => "Tenure",
            1 => "Rate",
            2 => "Min Balance",
            3 => "Status",
            4 => "STAR Savings Account",
            5 => "1 Year",
            6 => "10.00%",
            7 => "৳1,250.00",
            _ => "Active",
        };
        Ok((label.to_string(), 0.99))
    }
}

fn build_page0_image() -> RgbImage {
    let w = 600u32;
    let h = 800u32;
    let mut img = RgbImage::from_pixel(w, h, Rgb([250, 250, 250]));
    let black = Rgb([15, 15, 15]);

    // 1. Colored bank logo (top-left).
    for y in 24..72 {
        for x in 30..100 {
            img.put_pixel(x, y, Rgb([20, 90, 210]));
        }
    }

    // 2. Shaded header bar.
    for y in 28..72 {
        for x in 125..550 {
            img.put_pixel(x, y, Rgb([215, 224, 238]));
        }
    }

    // 3. Bordered 4-column x 3-row table with row 1 merged full-width.
    let xs = [60u32, 180, 300, 420, 540];
    let ys = [160u32, 220, 280, 340];
    for &y in &ys {
        for dy in 0..2 {
            for x in xs[0]..=xs[4] {
                img.put_pixel(x, y + dy, black);
            }
        }
    }
    for &x in &[xs[0], xs[4]] {
        for dx in 0..2 {
            for y in ys[0]..=ys[3] {
                img.put_pixel(x + dx, y, black);
            }
        }
    }
    for &x in &[xs[1], xs[2], xs[3]] {
        for dx in 0..2 {
            for y in ys[0]..=ys[1] {
                img.put_pixel(x + dx, y, black);
            }
            for y in ys[2]..=ys[3] {
                img.put_pixel(x + dx, y, black);
            }
        }
    }

    // 4. Tall handwritten signature graphic at bottom-right.
    for x in 380..520 {
        let y = 690 + ((x as f32 * 0.15).sin() * 22.0) as i32;
        for dy in -2..=2 {
            img.put_pixel(x, (y + dy) as u32, Rgb([15, 15, 25]));
        }
    }

    img
}

#[test]
fn financial_circular_2page_e2e_verifies_md_xlsx_and_4layer_pdf() {
    let page0_img = build_page0_image();
    let page1_img = RgbImage::from_pixel(600, 800, Rgb([250, 250, 250]));
    let images = [page0_img, page1_img];

    let recognizer = MockCircularRecognizer::new();
    let vision_cfg = VisionConfig::default();
    let mrc_cfg = MrcConfig::default();

    let mut page_irs = Vec::new();
    let mut page_assets = Vec::new();

    for (idx, img) in images.iter().enumerate() {
        let v_out = analyze_page_with_recognizer(
            &PageAnalysisInput {
                page_index: idx,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::ScannedRaster,
                image: img,
                native_text: None,
            },
            &vision_cfg,
            Some(&recognizer),
        )
        .expect("analyze page");

        let figure_boxes: Vec<BBox> = v_out
            .page_ir
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
            .map(|b| b.bbox)
            .collect();

        let mrc = decompose_mrc(
            img,
            &figure_boxes,
            v_out.table_grid_mask.as_deref(),
            &mrc_cfg,
        )
        .expect("decompose mrc");

        page_assets.push(cleanocr_export::PageVisualAssets {
            page_index: idx,
            mrc: Some(mrc),
            fallback_raster_jpeg: None,
            figures: v_out.figures,
        });
        page_irs.push(v_out.page_ir);
    }

    let doc_ir = DocumentIr {
        schema_version: IR_SCHEMA_VERSION.to_string(),
        metadata: DocumentMetadata {
            title: Some("2-Page Financial Rate Circular".to_string()),
            source_sha256: "e2e_sha256".to_string(),
            source_filename: Some("circular.pdf".to_string()),
            detected_languages: vec!["en".to_string(), "bn".to_string()],
            created_utc: "2026-11-25T00:00:00Z".to_string(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        pages: page_irs,
    };

    let out_dir = std::env::temp_dir().join(format!("cleanocr_s10_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&out_dir);
    fs::create_dir_all(&out_dir).expect("create out_dir");

    let md_path = out_dir.join("circular.md");
    let xlsx_path = out_dir.join("circular.xlsx");
    let pdf_path = out_dir.join("circular.pdf");

    MarkdownExporter
        .export(&doc_ir, &page_assets, &md_path)
        .expect("export md");
    XlsxExporter
        .export(&doc_ir, &page_assets, &xlsx_path)
        .expect("export xlsx");
    PdfExporter
        .export(&doc_ir, &page_assets, &pdf_path)
        .expect("export pdf");

    // 1. Assert Markdown contains GFM pipe tables and intact "10.00%".
    let md = fs::read_to_string(&md_path).expect("read md");
    assert!(md.contains("10.00%"), "MD must keep 10.00% intact");
    assert!(
        md.contains("| 9.50% |"),
        "MD must contain borderless summary table"
    );

    // 2. Assert XLSX unzips cleanly with numeric 0.1 and <mergeCell>.
    let xlsx_bytes = fs::read(&xlsx_path).expect("read xlsx");
    let mut archive = ZipArchive::new(Cursor::new(xlsx_bytes)).expect("valid xlsx zip");
    let mut sheet1 = String::new();
    archive
        .by_name("xl/worksheets/sheet1.xml")
        .expect("sheet1.xml")
        .read_to_string(&mut sheet1)
        .unwrap();
    assert!(
        sheet1.contains("<v>0.1</v>"),
        "10.00% must be numeric 0.1 in XLSX"
    );
    assert!(
        sheet1.contains("<mergeCell"),
        "merged section header must emit <mergeCell>"
    );
    assert!(
        archive.by_name("xl/worksheets/sheet2.xml").is_ok(),
        "borderless summary table on page 2 must produce sheet2.xml"
    );

    // 3. Assert PDF contains 4 OCGs, /SMask, /ToUnicode, and lossless IR round-trip.
    let pdf_bytes = fs::read(&pdf_path).expect("read pdf");
    let pdf_text = String::from_utf8_lossy(&pdf_bytes);
    for token in [
        "(Background)",
        "(Graphics)",
        "(Tables)",
        "(Text)",
        "/SMask",
        "/ToUnicode",
        "/JBIG2Decode",
        "<09F3>",
    ] {
        assert!(pdf_text.contains(token), "PDF missing {token}");
    }

    let roundtripped = extract_ir_from_file(&pdf_path).expect("extract IR from PDF");
    assert_eq!(roundtripped, doc_ir);

    let _ = fs::remove_dir_all(&out_dir);
}
