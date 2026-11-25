use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use cleanocr_ir::{BlockKind, DocumentIr, TableIr, TextLine};
use rust_xlsxwriter::{Color, Format, FormatAlign, Workbook, Worksheet};

use crate::{Exporter, PageVisualAssets};

/// Parsed financial cell value for native Excel typing.
#[derive(Debug, Clone, PartialEq)]
pub enum ExcelCellValue {
    Percentage(f64),
    Number { value: f64, has_decimals: bool },
    Text(String),
}

/// Parses a cell string into a typed Excel value (percentage, number, or UTF-8 text).
pub fn parse_excel_cell(text: &str) -> ExcelCellValue {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ExcelCellValue::Text(String::new());
    }

    if let Some(without_pct) = trimmed.strip_suffix('%') {
        let cleaned = without_pct.trim().replace(',', "");
        if is_clean_numeric(&cleaned)
            && let Ok(val) = cleaned.parse::<f64>()
        {
            return ExcelCellValue::Percentage(val / 100.0);
        }
    }

    let cleaned = trimmed.replace(',', "");
    if is_clean_numeric(&cleaned)
        && let Ok(val) = cleaned.parse::<f64>()
    {
        return ExcelCellValue::Number {
            value: val,
            has_decimals: cleaned.contains('.'),
        };
    }

    ExcelCellValue::Text(trimmed.to_string())
}

fn is_clean_numeric(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let mut start = 0usize;
    if bytes[0] == b'-' || bytes[0] == b'+' {
        start = 1;
        if start >= bytes.len() {
            return false;
        }
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    for &b in &bytes[start..] {
        if b.is_ascii_digit() {
            seen_digit = true;
        } else if b == b'.' && !seen_dot {
            seen_dot = true;
        } else {
            return false;
        }
    }
    seen_digit
}

/// Exports `DocumentIr` tables (with financial cell typing and merged ranges) to `.xlsx`.
pub struct XlsxExporter;

impl Exporter for XlsxExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        _assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let bytes = build_xlsx_bytes(ir)?;

        let is_xlsx_file = output_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("xlsx"));

        let xlsx_path = if is_xlsx_file {
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            output_path.to_path_buf()
        } else {
            fs::create_dir_all(output_path)?;
            output_path.join("document.xlsx")
        };

        fs::write(&xlsx_path, bytes)
            .with_context(|| format!("failed to write XLSX file {}", xlsx_path.display()))?;
        Ok(())
    }
}

/// Builds an in-memory `.xlsx` workbook from all `BlockKind::Table` blocks in `ir`.
pub fn build_xlsx_bytes(ir: &DocumentIr) -> Result<Vec<u8>> {
    let mut workbook = Workbook::new();

    let header_text_fmt = Format::new()
        .set_bold()
        .set_text_wrap()
        .set_background_color(Color::RGB(0xE8EEF5))
        .set_align(FormatAlign::VerticalCenter);
    let header_pct_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE8EEF5))
        .set_num_format("0.00%");
    let header_num_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE8EEF5))
        .set_num_format("#,##0.00");
    let header_int_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE8EEF5))
        .set_num_format("#,##0");

    let body_text_fmt = Format::new()
        .set_text_wrap()
        .set_align(FormatAlign::VerticalCenter);
    let body_pct_fmt = Format::new().set_num_format("0.00%");
    let body_num_fmt = Format::new().set_num_format("#,##0.00");
    let body_int_fmt = Format::new().set_num_format("#,##0");

    struct ActiveSheet<'a> {
        ws: &'a mut Worksheet,
        row_offset: u32,
        col_widths: Vec<usize>,
        num_cols: usize,
        last_page_idx: usize,
    }

    let mut active: Option<ActiveSheet<'_>> = None;
    let mut can_continue = false;
    let mut total_tables = 0usize;

    for (p_idx, page) in ir.pages.iter().enumerate() {
        let mut page_table_idx = 0usize;
        for block in &page.blocks {
            match &block.kind {
                BlockKind::Table(table) => {
                    total_tables += 1;
                    page_table_idx += 1;

                    let should_merge_into_prev = can_continue
                        && active.as_ref().is_some_and(|a| {
                            a.num_cols == table.num_cols && a.last_page_idx + 1 == p_idx
                        });

                    if !should_merge_into_prev {
                        if let Some(prev) = active.take() {
                            apply_col_widths(prev.ws, &prev.col_widths)?;
                        }
                        let raw_name = format!("Page{}_Table{}", p_idx + 1, page_table_idx);
                        let sheet_name: String = raw_name.chars().take(31).collect();
                        let ws = workbook.add_worksheet();
                        ws.set_name(&sheet_name)
                            .context("failed to set worksheet name")?;
                        active = Some(ActiveSheet {
                            ws,
                            row_offset: 0,
                            col_widths: vec![10usize; table.num_cols],
                            num_cols: table.num_cols,
                            last_page_idx: p_idx,
                        });
                    }

                    if let Some(sheet) = active.as_mut() {
                        write_table_to_worksheet(
                            sheet.ws,
                            table,
                            sheet.row_offset,
                            &mut sheet.col_widths,
                            &header_text_fmt,
                            &header_pct_fmt,
                            &header_num_fmt,
                            &header_int_fmt,
                            &body_text_fmt,
                            &body_pct_fmt,
                            &body_num_fmt,
                            &body_int_fmt,
                        )?;
                        sheet.row_offset += table.num_rows as u32;
                        sheet.last_page_idx = p_idx;
                    }
                    can_continue = true;
                }
                BlockKind::Heading { .. }
                | BlockKind::Paragraph { .. }
                | BlockKind::List { .. }
                | BlockKind::DisplayFormula { .. }
                | BlockKind::Figure { .. }
                | BlockKind::Footnote { .. } => {
                    can_continue = false;
                }
                BlockKind::HeaderFooter { .. } => {}
            }
        }
    }

    if let Some(prev) = active.take() {
        apply_col_widths(prev.ws, &prev.col_widths)?;
    }

    // Fallback when zero tables exist: write extracted paragraphs into `Extracted_Text`.
    if total_tables == 0 {
        let ws = workbook.add_worksheet();
        ws.set_name("Extracted_Text")
            .context("failed to name fallback Extracted_Text sheet")?;
        let mut row = 0u32;
        let mut max_w = 12usize;
        for page in &ir.pages {
            for block in &page.blocks {
                let text = match &block.kind {
                    BlockKind::Heading { lines, .. }
                    | BlockKind::Paragraph { lines }
                    | BlockKind::Footnote { lines, .. } => join_lines_text(lines),
                    BlockKind::List { items, .. } => items
                        .iter()
                        .map(|it| join_lines_text(&it.lines))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    BlockKind::DisplayFormula { latex, .. } => latex.clone(),
                    BlockKind::HeaderFooter { text, .. } => text.clone(),
                    BlockKind::Table(_) | BlockKind::Figure { .. } => continue,
                };
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    ws.write_string_with_format(row, 0, trimmed, &body_text_fmt)?;
                    max_w = max_w.max(trimmed.chars().count());
                    row += 1;
                }
            }
        }
        ws.set_column_width(0, (max_w.clamp(10, 60) + 2) as f64)?;
    }

    workbook
        .save_to_buffer()
        .context("failed to serialize XLSX workbook to buffer")
}

#[allow(clippy::too_many_arguments)]
fn write_table_to_worksheet(
    ws: &mut Worksheet,
    table: &TableIr,
    row_offset: u32,
    col_widths: &mut Vec<usize>,
    header_text_fmt: &Format,
    header_pct_fmt: &Format,
    header_num_fmt: &Format,
    header_int_fmt: &Format,
    body_text_fmt: &Format,
    body_pct_fmt: &Format,
    body_num_fmt: &Format,
    body_int_fmt: &Format,
) -> Result<()> {
    if col_widths.len() < table.num_cols {
        col_widths.resize(table.num_cols, 10);
    }

    for cell in &table.cells {
        let r0 = row_offset + cell.row as u32;
        let c0 = cell.col as u16;
        let is_hdr = cell.is_header || (row_offset == 0 && cell.row == 0);

        let text_len = cell.text.trim().chars().count();
        if cell.col_span <= 1
            && let Some(w) = col_widths.get_mut(cell.col)
        {
            *w = (*w).max(text_len);
        }

        if cell.row_span > 1 || cell.col_span > 1 {
            let r1 = r0 + (cell.row_span.saturating_sub(1)) as u32;
            let c1 = c0 + (cell.col_span.saturating_sub(1)) as u16;
            let fmt = if is_hdr {
                header_text_fmt
            } else {
                body_text_fmt
            };
            ws.merge_range(r0, c0, r1, c1, "", fmt)?;
        }

        match parse_excel_cell(&cell.text) {
            ExcelCellValue::Percentage(val) => {
                let fmt = if is_hdr { header_pct_fmt } else { body_pct_fmt };
                ws.write_number_with_format(r0, c0, val, fmt)?;
            }
            ExcelCellValue::Number {
                value,
                has_decimals,
            } => {
                let fmt = match (is_hdr, has_decimals) {
                    (true, true) => header_num_fmt,
                    (true, false) => header_int_fmt,
                    (false, true) => body_num_fmt,
                    (false, false) => body_int_fmt,
                };
                ws.write_number_with_format(r0, c0, value, fmt)?;
            }
            ExcelCellValue::Text(s) => {
                let fmt = if is_hdr {
                    header_text_fmt
                } else {
                    body_text_fmt
                };
                ws.write_string_with_format(r0, c0, &s, fmt)?;
            }
        }
    }

    Ok(())
}

fn apply_col_widths(ws: &mut Worksheet, col_widths: &[usize]) -> Result<()> {
    for (c, &len) in col_widths.iter().enumerate() {
        let clamped = len.clamp(10, 60) as f64 + 2.0;
        ws.set_column_width(c as u16, clamped)?;
    }
    Ok(())
}

fn join_lines_text(lines: &[TextLine]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read as _};

    use cleanocr_ir::{
        BBox, Block, DocumentMetadata, IR_SCHEMA_VERSION, PageIngestMode, PageIr, TableCell,
    };
    use zip::ZipArchive;

    use super::*;

    #[test]
    fn exports_financial_tables_with_merged_cells_and_numeric_percentages() {
        let b = BBox::new(0.1, 0.1, 0.9, 0.4);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Financial Circular".to_string()),
                source_sha256: "abc".to_string(),
                source_filename: Some("circular.pdf".to_string()),
                detected_languages: vec!["en".to_string()],
                created_utc: "2026-11-25T00:00:00Z".to_string(),
                engine_version: "0.1.0".to_string(),
            },
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::ScannedRaster,
                blocks: vec![Block {
                    id: "p0_t0".to_string(),
                    bbox: b,
                    reading_order: 0,
                    column_index: None,
                    confidence: 0.98,
                    kind: BlockKind::Table(TableIr {
                        num_rows: 3,
                        num_cols: 2,
                        caption: None,
                        cells: vec![
                            TableCell {
                                row: 0,
                                col: 0,
                                row_span: 1,
                                col_span: 2,
                                is_header: true,
                                bbox: b,
                                text: "STAR Savings Account".to_string(),
                            },
                            TableCell {
                                row: 1,
                                col: 0,
                                row_span: 1,
                                col_span: 1,
                                is_header: false,
                                bbox: b,
                                text: "Interest Rate".to_string(),
                            },
                            TableCell {
                                row: 1,
                                col: 1,
                                row_span: 1,
                                col_span: 1,
                                is_header: false,
                                bbox: b,
                                text: "10.00%".to_string(),
                            },
                            TableCell {
                                row: 2,
                                col: 0,
                                row_span: 1,
                                col_span: 1,
                                is_header: false,
                                bbox: b,
                                text: "Minimum Balance".to_string(),
                            },
                            TableCell {
                                row: 2,
                                col: 1,
                                row_span: 1,
                                col_span: 1,
                                is_header: false,
                                bbox: b,
                                text: "1,250.50".to_string(),
                            },
                        ],
                    }),
                }],
                overlays: Vec::new(),
                low_confidence_flags: Vec::new(),
            }],
        };

        let bytes = build_xlsx_bytes(&ir).expect("build xlsx bytes");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("valid xlsx zip");

        let mut workbook_xml = String::new();
        archive
            .by_name("xl/workbook.xml")
            .expect("xl/workbook.xml")
            .read_to_string(&mut workbook_xml)
            .unwrap();
        assert!(workbook_xml.contains("Page1_Table1"));

        let mut sheet1_xml = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .expect("xl/worksheets/sheet1.xml")
            .read_to_string(&mut sheet1_xml)
            .unwrap();

        assert!(
            sheet1_xml.contains("<v>0.1</v>"),
            "10.00% must be stored as numeric 0.1"
        );
        assert!(
            sheet1_xml.contains("<v>1250.5</v>"),
            "1,250.50 must be stored as numeric 1250.5"
        );
        assert!(
            sheet1_xml.contains("<mergeCell"),
            "merged header must emit <mergeCell>"
        );
    }
}
