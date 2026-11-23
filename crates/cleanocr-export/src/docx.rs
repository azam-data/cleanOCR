use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use cleanocr_ir::{
    Block, BlockKind, DocumentIr, PageIr, TableCell as IrTableCell, TableIr, TextLine, TextSpan,
};
use docx_rs::{BreakType, Docx, Paragraph, Pic, Run, Table, TableCell, TableRow, VMergeType};

use crate::latex_math::validate_latex_math;
use crate::{Exporter, PageVisualAssets};

/// Exports `DocumentIr` + cropped figure/fallback assets to a `.docx` file.
pub struct DocxExporter;

impl Exporter for DocxExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let bytes = build_docx_bytes(ir, assets)?;

        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        fs::write(output_path, bytes)
            .with_context(|| format!("failed to write DOCX file {}", output_path.display()))?;
        Ok(())
    }
}

/// Builds a complete `.docx` OOXML archive in memory directly from `DocumentIr`.
pub fn build_docx_bytes(ir: &DocumentIr, assets: &[PageVisualAssets]) -> Result<Vec<u8>> {
    let figure_map = collect_figure_assets(assets);
    let mut docx = Docx::new();

    if let Some(title) = &ir.metadata.title
        && !title.trim().is_empty()
    {
        let p = Paragraph::new().add_run(Run::new().add_text(title.trim()).bold().size(36));
        docx = docx.add_paragraph(p);
    }

    for (idx, page) in ir.pages.iter().enumerate() {
        if idx > 0 {
            docx =
                docx.add_paragraph(Paragraph::new().add_run(Run::new().add_break(BreakType::Page)));
        }
        docx = append_page(docx, page, &figure_map);
    }

    let mut cursor = Cursor::new(Vec::new());
    docx.build()
        .pack(&mut cursor)
        .context("failed to pack DOCX OOXML archive")?;
    Ok(cursor.into_inner())
}

fn append_page(mut docx: Docx, page: &PageIr, figures: &BTreeMap<String, Vec<u8>>) -> Docx {
    for block in &page.blocks {
        docx = append_block(docx, block, figures);
    }
    docx
}

fn append_block(mut docx: Docx, block: &Block, figures: &BTreeMap<String, Vec<u8>>) -> Docx {
    match &block.kind {
        BlockKind::Heading { level, lines } => {
            let size = match level {
                1 => 32,
                2 => 28,
                3 => 24,
                _ => 22,
            };
            let mut para = Paragraph::new();
            for run in build_line_runs(lines, Some(size), true) {
                para = para.add_run(run);
            }
            docx = docx.add_paragraph(para);
        }
        BlockKind::Paragraph { lines } => {
            let mut para = Paragraph::new();
            for run in build_line_runs(lines, None, false) {
                para = para.add_run(run);
            }
            docx = docx.add_paragraph(para);
        }
        BlockKind::List { ordered, items } => {
            for (idx, item) in items.iter().enumerate() {
                let prefix = if *ordered {
                    format!("{}. ", idx + 1)
                } else {
                    "• ".to_string()
                };
                let mut para = Paragraph::new().add_run(Run::new().add_text(prefix));
                for run in build_line_runs(&item.lines, None, false) {
                    para = para.add_run(run);
                }
                docx = docx.add_paragraph(para);
            }
        }
        BlockKind::Table(table) => {
            if let Some(docx_table) = build_table(table) {
                docx = docx.add_table(docx_table);
            }
            if let Some(cap) = &table.caption {
                docx = docx.add_paragraph(
                    Paragraph::new().add_run(Run::new().add_text(cap.trim()).italic().size(20)),
                );
            }
        }
        BlockKind::DisplayFormula {
            latex,
            fallback_asset_id,
        } => {
            if validate_latex_math(latex).is_ok() {
                docx = docx.add_paragraph(
                    Paragraph::new().add_run(Run::new().add_text(latex.trim()).italic()),
                );
            } else if let Some(asset_id) = fallback_asset_id
                && let Some(img_bytes) = figures.get(asset_id)
                && is_supported_raster(img_bytes)
            {
                let pic = Pic::new(img_bytes.as_slice());
                docx = docx.add_paragraph(Paragraph::new().add_run(Run::new().add_image(pic)));
            } else {
                docx =
                    docx.add_paragraph(Paragraph::new().add_run(Run::new().add_text(latex.trim())));
            }
        }
        BlockKind::Figure {
            asset_id,
            caption,
            alt_text,
            ..
        } => {
            if let Some(img_bytes) = figures.get(asset_id)
                && is_supported_raster(img_bytes)
            {
                let pic = Pic::new(img_bytes.as_slice());
                docx = docx.add_paragraph(Paragraph::new().add_run(Run::new().add_image(pic)));
            } else {
                let label = alt_text
                    .as_deref()
                    .or(caption.as_deref())
                    .unwrap_or(asset_id.as_str());
                docx = docx.add_paragraph(
                    Paragraph::new()
                        .add_run(Run::new().add_text(format!("[Figure: {label}]")).italic()),
                );
            }
            if let Some(cap) = caption {
                docx = docx.add_paragraph(
                    Paragraph::new().add_run(Run::new().add_text(cap.trim()).italic().size(20)),
                );
            }
        }
        BlockKind::HeaderFooter { .. } => {
            // Running headers/footers are excluded from body flow.
        }
        BlockKind::Footnote { marker, lines } => {
            let mut para = Paragraph::new();
            if let Some(m) = marker {
                para = para.add_run(Run::new().add_text(format!("[{m}] ")).size(18));
            }
            for run in build_line_runs(lines, Some(18), false) {
                para = para.add_run(run);
            }
            docx = docx.add_paragraph(para);
        }
    }
    docx
}

fn build_line_runs(lines: &[TextLine], force_size: Option<usize>, force_bold: bool) -> Vec<Run> {
    let mut runs = Vec::new();
    for (line_idx, line) in lines.iter().enumerate() {
        if line_idx > 0 {
            runs.push(Run::new().add_text(" "));
        }
        for span in &line.spans {
            runs.push(build_span_run(span, force_size, force_bold));
        }
    }
    runs
}

fn build_span_run(span: &TextSpan, force_size: Option<usize>, force_bold: bool) -> Run {
    let (content, is_math) = if let Some(raw_math) = &span.inline_math_latex {
        if validate_latex_math(raw_math).is_ok() {
            (format!("${}$", raw_math.trim()), true)
        } else {
            (span.text.clone(), false)
        }
    } else {
        (span.text.clone(), false)
    };

    let mut run = Run::new().add_text(content);
    if force_bold || span.is_bold {
        run = run.bold();
    }
    if is_math || span.is_italic {
        run = run.italic();
    }
    if let Some(size) = force_size {
        run = run.size(size);
    }
    run
}

#[derive(Clone, Copy)]
enum CellSlot<'a> {
    Empty,
    Origin(&'a IrTableCell, usize, usize),
    HorizCovered,
    VertContinuation(usize),
}

fn build_table(table: &TableIr) -> Option<Table> {
    if table.num_rows == 0 || table.num_cols == 0 {
        return None;
    }

    let mut cell_map: HashMap<(usize, usize), &IrTableCell> = HashMap::new();
    for cell in &table.cells {
        cell_map.insert((cell.row, cell.col), cell);
    }

    let mut grid = vec![vec![CellSlot::Empty; table.num_cols]; table.num_rows];

    for r in 0..table.num_rows {
        for c in 0..table.num_cols {
            if !matches!(grid[r][c], CellSlot::Empty) {
                continue;
            }
            if let Some(&cell) = cell_map.get(&(r, c)) {
                let rs = cell.row_span.max(1).min(table.num_rows - r);
                let cs = cell.col_span.max(1).min(table.num_cols - c);
                grid[r][c] = CellSlot::Origin(cell, rs, cs);

                for dc in 1..cs {
                    grid[r][c + dc] = CellSlot::HorizCovered;
                }
                for dr in 1..rs {
                    grid[r + dr][c] = CellSlot::VertContinuation(cs);
                    for dc in 1..cs {
                        grid[r + dr][c + dc] = CellSlot::HorizCovered;
                    }
                }
            }
        }
    }

    let mut rows = Vec::with_capacity(table.num_rows);
    for row_slots in grid {
        let mut docx_cells = Vec::new();
        for slot in row_slots {
            match slot {
                CellSlot::HorizCovered => {}
                CellSlot::Empty => {
                    docx_cells.push(TableCell::new().add_paragraph(Paragraph::new()));
                }
                CellSlot::VertContinuation(cs) => {
                    let mut tc = TableCell::new()
                        .add_paragraph(Paragraph::new())
                        .vertical_merge(VMergeType::Continue);
                    if cs > 1 {
                        tc = tc.grid_span(cs);
                    }
                    docx_cells.push(tc);
                }
                CellSlot::Origin(cell, rs, cs) => {
                    let mut run = Run::new().add_text(cell.text.trim());
                    if cell.is_header {
                        run = run.bold();
                    }
                    let mut tc = TableCell::new().add_paragraph(Paragraph::new().add_run(run));
                    if cs > 1 {
                        tc = tc.grid_span(cs);
                    }
                    if rs > 1 {
                        tc = tc.vertical_merge(VMergeType::Restart);
                    }
                    docx_cells.push(tc);
                }
            }
        }
        rows.push(TableRow::new(docx_cells));
    }

    Some(Table::new(rows))
}

fn is_supported_raster(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

fn collect_figure_assets(assets: &[PageVisualAssets]) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    for page in assets {
        for (id, bytes) in &page.figures {
            map.insert(id.clone(), bytes.clone());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;
    use cleanocr_ir::{BBox, DocumentMetadata, IR_SCHEMA_VERSION, PageIngestMode, TextSource};

    /// Valid 1x1 white RGB PNG image for offline OOXML media embedding tests.
    const PNG_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2,
        0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 255, 255, 63,
        0, 5, 254, 2, 254, 13, 239, 70, 184, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn builds_valid_docx_zip_with_headings_merged_table_and_image() {
        let bbox = BBox::new(0.1, 0.1, 0.9, 0.2);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("CleanOCR Word Test".to_string()),
                source_sha256: "001122".to_string(),
                source_filename: Some("test.pdf".to_string()),
                detected_languages: vec!["en".to_string()],
                created_utc: "2026-11-23T00:00:00Z".to_string(),
                engine_version: "0.1.0".to_string(),
            },
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::DigitalNativeText,
                blocks: vec![
                    Block {
                        id: "h1".to_string(),
                        bbox,
                        reading_order: 0,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::Heading {
                            level: 1,
                            lines: vec![TextLine {
                                bbox,
                                spans: vec![TextSpan {
                                    text: "Introduction".to_string(),
                                    bbox,
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
                        id: "t1".to_string(),
                        bbox,
                        reading_order: 1,
                        column_index: None,
                        confidence: 0.98,
                        kind: BlockKind::Table(TableIr {
                            num_rows: 2,
                            num_cols: 2,
                            caption: Some("Summary".to_string()),
                            cells: vec![
                                IrTableCell {
                                    row: 0,
                                    col: 0,
                                    row_span: 2,
                                    col_span: 1,
                                    is_header: true,
                                    bbox,
                                    text: "Group".to_string(),
                                },
                                IrTableCell {
                                    row: 0,
                                    col: 1,
                                    row_span: 1,
                                    col_span: 1,
                                    is_header: true,
                                    bbox,
                                    text: "Q1".to_string(),
                                },
                                IrTableCell {
                                    row: 1,
                                    col: 1,
                                    row_span: 1,
                                    col_span: 1,
                                    is_header: false,
                                    bbox,
                                    text: "99".to_string(),
                                },
                            ],
                        }),
                    },
                    Block {
                        id: "fig1".to_string(),
                        bbox,
                        reading_order: 2,
                        column_index: None,
                        confidence: 0.95,
                        kind: BlockKind::Figure {
                            asset_id: "fig_01".to_string(),
                            caption: Some("Figure 1".to_string()),
                            alt_text: None,
                            is_chart: false,
                            chart_data: None,
                            tikz_source: None,
                        },
                    },
                ],
                overlays: Vec::new(),
                low_confidence_flags: Vec::new(),
            }],
        };

        let mut figures = BTreeMap::new();
        figures.insert("fig_01".to_string(), PNG_1X1.to_vec());
        let assets = vec![PageVisualAssets {
            page_index: 0,
            mrc: None,
            fallback_raster_jpeg: None,
            figures,
        }];

        let bytes = build_docx_bytes(&ir, &assets).expect("build docx bytes");
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("valid docx zip archive");
        let mut doc_xml = String::new();
        zip.by_name("word/document.xml")
            .expect("word/document.xml exists")
            .read_to_string(&mut doc_xml)
            .expect("read document.xml");

        assert!(doc_xml.contains("CleanOCR Word Test"));
        assert!(doc_xml.contains("Introduction"));
        assert!(doc_xml.contains("vMerge"));
        assert!(doc_xml.contains("Figure 1"));
    }
}
