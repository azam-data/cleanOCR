use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use cleanocr_ir::{Block, BlockKind, DocumentIr, PageIr, TableCell, TableIr, TextLine, TextSpan};

use crate::latex_math::validate_latex_math;
use crate::{Exporter, PageVisualAssets};

/// Exports `DocumentIr` + cropped figure/fallback assets to GitHub-Flavored Markdown.
///
/// - If `output_path` ends with `.md`, writes the Markdown file directly to `output_path` and
///   places any figure PNGs in a sibling `assets/` directory.
/// - Otherwise, treats `output_path` as a directory and writes `document.md` + `assets/`.
pub struct MarkdownExporter;

impl Exporter for MarkdownExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let md = render_markdown(ir);
        let collected_assets = collect_figure_assets(assets);

        let is_md_file = output_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"));

        let (md_path, base_dir) = if is_md_file {
            let parent = output_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent)?;
            (output_path.to_path_buf(), parent.to_path_buf())
        } else {
            fs::create_dir_all(output_path)?;
            (output_path.join("document.md"), output_path.to_path_buf())
        };

        fs::write(&md_path, md)
            .with_context(|| format!("failed to write Markdown file {}", md_path.display()))?;

        if !collected_assets.is_empty() {
            let assets_dir = base_dir.join("assets");
            fs::create_dir_all(&assets_dir)?;
            for (asset_id, bytes) in collected_assets {
                fs::write(assets_dir.join(format!("{asset_id}.png")), bytes)?;
            }
        }

        Ok(())
    }
}

/// Renders GitHub-Flavored Markdown directly from `DocumentIr`.
pub fn render_markdown(ir: &DocumentIr) -> String {
    let mut out = String::new();

    if let Some(title) = &ir.metadata.title
        && !title.trim().is_empty()
    {
        let _ = writeln!(out, "# {}", title.trim());
        let _ = writeln!(out);
    }

    for (idx, page) in ir.pages.iter().enumerate() {
        if idx > 0 {
            let _ = writeln!(out, "---");
            let _ = writeln!(out);
        }
        render_page(page, &mut out);
    }

    out
}

fn render_page(page: &PageIr, out: &mut String) {
    for block in &page.blocks {
        render_block(block, out);
    }
}

fn render_block(block: &Block, out: &mut String) {
    match &block.kind {
        BlockKind::Heading { level, lines } => {
            let hashes = "#".repeat((*level).clamp(1, 6) as usize);
            let text = render_lines(lines);
            if !text.is_empty() {
                let _ = writeln!(out, "{hashes} {text}");
                let _ = writeln!(out);
            }
        }
        BlockKind::Paragraph { lines } => {
            let text = render_lines(lines);
            if !text.is_empty() {
                let _ = writeln!(out, "{text}");
                let _ = writeln!(out);
            }
        }
        BlockKind::List { ordered, items } => {
            for (idx, item) in items.iter().enumerate() {
                let text = render_lines(&item.lines);
                if *ordered {
                    let _ = writeln!(out, "{}. {text}", idx + 1);
                } else {
                    let _ = writeln!(out, "- {text}");
                }
            }
            if !items.is_empty() {
                let _ = writeln!(out);
            }
        }
        BlockKind::Table(table) => render_table(table, out),
        BlockKind::DisplayFormula {
            latex,
            fallback_asset_id,
        } => {
            if validate_latex_math(latex).is_ok() {
                let _ = writeln!(out, "$$");
                let _ = writeln!(out, "{}", latex.trim());
                let _ = writeln!(out, "$$");
                let _ = writeln!(out);
            } else if let Some(asset_id) = fallback_asset_id {
                let _ = writeln!(out, "![Formula](assets/{asset_id}.png)");
                let _ = writeln!(out);
            } else {
                let _ = writeln!(out, "```text");
                let _ = writeln!(out, "{}", latex.trim());
                let _ = writeln!(out, "```");
                let _ = writeln!(out);
            }
        }
        BlockKind::Figure {
            asset_id,
            caption,
            alt_text,
            ..
        } => {
            let alt = alt_text
                .as_deref()
                .or(caption.as_deref())
                .unwrap_or(asset_id.as_str());
            let _ = writeln!(out, "![{}](assets/{asset_id}.png)", escape_md_alt(alt));
            if let Some(cap) = caption {
                let _ = writeln!(out);
                let _ = writeln!(out, "*{}*", cap.trim());
            }
            let _ = writeln!(out);
        }
        BlockKind::HeaderFooter { is_header, text } => {
            let label = if *is_header { "Header" } else { "Footer" };
            let safe = text.replace("-->", "--\\>");
            let _ = writeln!(out, "<!-- {label}: {} -->", safe.trim());
            let _ = writeln!(out);
        }
        BlockKind::Footnote { marker, lines } => {
            let text = render_lines(lines);
            if !text.is_empty() {
                if let Some(m) = marker {
                    let _ = writeln!(out, "> [{m}] {text}");
                } else {
                    let _ = writeln!(out, "> {text}");
                }
                let _ = writeln!(out);
            }
        }
    }
}

fn render_lines(lines: &[TextLine]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let mut line_out = String::new();
        for span in &line.spans {
            line_out.push_str(&render_span(span));
        }
        let trimmed = line_out.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    parts.join(" ")
}

fn render_span(span: &TextSpan) -> String {
    if let Some(raw_math) = &span.inline_math_latex {
        if validate_latex_math(raw_math).is_ok() {
            return format!("${}$", raw_math.trim());
        }
        return span.text.clone();
    }

    if span.text.trim().is_empty() {
        return span.text.clone();
    }

    match (span.is_bold, span.is_italic) {
        (true, true) => format!("***{}***", span.text),
        (true, false) => format!("**{}**", span.text),
        (false, true) => format!("*{}*", span.text),
        (false, false) => span.text.clone(),
    }
}

fn render_table(table: &TableIr, out: &mut String) {
    if table.num_rows == 0 || table.num_cols == 0 {
        return;
    }

    let has_spans = table.cells.iter().any(|c| c.row_span > 1 || c.col_span > 1);

    if has_spans {
        render_html_table(table, out);
    } else {
        render_gfm_pipe_table(table, out);
    }

    if let Some(cap) = &table.caption {
        let _ = writeln!(out, "*{}*", cap.trim());
        let _ = writeln!(out);
    }
}

fn render_gfm_pipe_table(table: &TableIr, out: &mut String) {
    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); table.num_cols]; table.num_rows];
    for cell in &table.cells {
        if cell.row < table.num_rows && cell.col < table.num_cols {
            grid[cell.row][cell.col] = escape_pipe_cell(&cell.text);
        }
    }

    let _ = writeln!(out, "| {} |", grid[0].join(" | "));
    let sep: Vec<&str> = vec!["---"; table.num_cols];
    let _ = writeln!(out, "| {} |", sep.join(" | "));

    for row in grid.iter().skip(1) {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }
    let _ = writeln!(out);
}

fn render_html_table(table: &TableIr, out: &mut String) {
    let mut cell_map: HashMap<(usize, usize), &TableCell> = HashMap::new();
    for cell in &table.cells {
        cell_map.insert((cell.row, cell.col), cell);
    }

    let mut covered = vec![vec![false; table.num_cols]; table.num_rows];
    let _ = writeln!(out, "<table>");

    for r in 0..table.num_rows {
        let _ = writeln!(out, "  <tr>");
        let mut c = 0;
        while c < table.num_cols {
            if covered[r][c] {
                c += 1;
                continue;
            }
            if let Some(cell) = cell_map.get(&(r, c)) {
                let rs = cell.row_span.max(1).min(table.num_rows - r);
                let cs = cell.col_span.max(1).min(table.num_cols - c);
                for dr in 0..rs {
                    for dc in 0..cs {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        covered[r + dr][c + dc] = true;
                    }
                }
                let tag = if cell.is_header { "th" } else { "td" };
                let mut attrs = String::new();
                if rs > 1 {
                    let _ = write!(attrs, " rowspan=\"{rs}\"");
                }
                if cs > 1 {
                    let _ = write!(attrs, " colspan=\"{cs}\"");
                }
                let _ = writeln!(
                    out,
                    "    <{tag}{attrs}>{}</{tag}>",
                    escape_html(cell.text.trim())
                );
                c += cs;
            } else {
                let _ = writeln!(out, "    <td></td>");
                c += 1;
            }
        }
        let _ = writeln!(out, "  </tr>");
    }

    let _ = writeln!(out, "</table>");
    let _ = writeln!(out);
}

fn escape_pipe_cell(input: &str) -> String {
    input
        .trim()
        .replace('|', r"\|")
        .replace('\r', "")
        .replace('\n', "<br>")
}

fn escape_md_alt(input: &str) -> String {
    input.replace('[', r"[").replace(']', r"]")
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("\x26amp;"),
            '<' => out.push_str("\x26lt;"),
            '>' => out.push_str("\x26gt;"),
            '"' => out.push_str("\x26quot;"),
            _ => out.push(ch),
        }
    }
    out
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
    use super::*;
    use cleanocr_ir::{
        BBox, DocumentMetadata, IR_SCHEMA_VERSION, ListItem, PageIngestMode, TextSource,
    };

    fn dummy_meta() -> DocumentMetadata {
        DocumentMetadata {
            title: Some("Sample Report".to_string()),
            source_sha256: "abc123".to_string(),
            source_filename: Some("sample.pdf".to_string()),
            detected_languages: vec!["en".to_string()],
            created_utc: "2026-11-23T00:00:00Z".to_string(),
            engine_version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn renders_gfm_pipe_table_and_html_fallback_for_merged_cells() {
        let bbox = BBox::new(0.1, 0.1, 0.9, 0.3);
        let simple = TableIr {
            num_rows: 2,
            num_cols: 2,
            caption: None,
            cells: vec![
                TableCell {
                    row: 0,
                    col: 0,
                    row_span: 1,
                    col_span: 1,
                    is_header: true,
                    bbox,
                    text: "Metric".to_string(),
                },
                TableCell {
                    row: 0,
                    col: 1,
                    row_span: 1,
                    col_span: 1,
                    is_header: true,
                    bbox,
                    text: "Value | Unit".to_string(),
                },
                TableCell {
                    row: 1,
                    col: 0,
                    row_span: 1,
                    col_span: 1,
                    is_header: false,
                    bbox,
                    text: "Speed".to_string(),
                },
                TableCell {
                    row: 1,
                    col: 1,
                    row_span: 1,
                    col_span: 1,
                    is_header: false,
                    bbox,
                    text: "10x".to_string(),
                },
            ],
        };

        let merged = TableIr {
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
                    bbox,
                    text: "Combined Header".to_string(),
                },
                TableCell {
                    row: 1,
                    col: 0,
                    row_span: 1,
                    col_span: 1,
                    is_header: false,
                    bbox,
                    text: "A".to_string(),
                },
                TableCell {
                    row: 1,
                    col: 1,
                    row_span: 1,
                    col_span: 1,
                    is_header: false,
                    bbox,
                    text: "B".to_string(),
                },
            ],
        };

        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: dummy_meta(),
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::DigitalNativeText,
                blocks: vec![
                    Block {
                        id: "b0".to_string(),
                        bbox,
                        reading_order: 0,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::Table(simple),
                    },
                    Block {
                        id: "b1".to_string(),
                        bbox,
                        reading_order: 1,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::Table(merged),
                    },
                ],
                overlays: Vec::new(),
                low_confidence_flags: Vec::new(),
            }],
        };

        let md = render_markdown(&ir);
        assert!(md.contains(r"| Metric | Value \| Unit |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("<table>"));
        assert!(md.contains("<th colspan=\"2\">Combined Header</th>"));
        assert!(md.contains("*Merged Table*"));
    }

    #[test]
    fn validates_math_and_falls_back_on_broken_formulas() {
        let bbox = BBox::new(0.1, 0.1, 0.9, 0.2);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: dummy_meta(),
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::ScannedRaster,
                blocks: vec![
                    Block {
                        id: "p0".to_string(),
                        bbox,
                        reading_order: 0,
                        column_index: None,
                        confidence: 0.98,
                        kind: BlockKind::Paragraph {
                            lines: vec![TextLine {
                                bbox,
                                spans: vec![
                                    TextSpan {
                                        text: "Valid ".to_string(),
                                        bbox,
                                        confidence: 0.99,
                                        is_bold: false,
                                        is_italic: false,
                                        inline_math_latex: None,
                                        source: TextSource::Ocr,
                                    },
                                    TextSpan {
                                        text: "a+b".to_string(),
                                        bbox,
                                        confidence: 0.95,
                                        is_bold: false,
                                        is_italic: true,
                                        inline_math_latex: Some(r"\alpha + \beta".to_string()),
                                        source: TextSource::FormulaModel,
                                    },
                                    TextSpan {
                                        text: " and broken ".to_string(),
                                        bbox,
                                        confidence: 0.99,
                                        is_bold: false,
                                        is_italic: false,
                                        inline_math_latex: None,
                                        source: TextSource::Ocr,
                                    },
                                    TextSpan {
                                        text: "x/y".to_string(),
                                        bbox,
                                        confidence: 0.70,
                                        is_bold: false,
                                        is_italic: false,
                                        inline_math_latex: Some(r"\frac{x}{y".to_string()),
                                        source: TextSource::FormulaModel,
                                    },
                                ],
                            }],
                        },
                    },
                    Block {
                        id: "f0".to_string(),
                        bbox,
                        reading_order: 1,
                        column_index: None,
                        confidence: 0.95,
                        kind: BlockKind::DisplayFormula {
                            latex: r"E = mc^2".to_string(),
                            fallback_asset_id: Some("eq_ok".to_string()),
                        },
                    },
                    Block {
                        id: "f1".to_string(),
                        bbox,
                        reading_order: 2,
                        column_index: None,
                        confidence: 0.60,
                        kind: BlockKind::DisplayFormula {
                            latex: r"\left( x + 1".to_string(),
                            fallback_asset_id: Some("eq_broken".to_string()),
                        },
                    },
                    Block {
                        id: "l0".to_string(),
                        bbox,
                        reading_order: 3,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::List {
                            ordered: true,
                            items: vec![ListItem {
                                marker: "1.".to_string(),
                                lines: vec![TextLine {
                                    bbox,
                                    spans: vec![TextSpan {
                                        text: "First item".to_string(),
                                        bbox,
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
                ],
                overlays: Vec::new(),
                low_confidence_flags: Vec::new(),
            }],
        };

        let md = render_markdown(&ir);
        assert!(md.contains(r"Valid $\alpha + \beta$ and broken x/y"));
        assert!(md.contains("$$\nE = mc^2\n$$"));
        assert!(md.contains("![Formula](assets/eq_broken.png)"));
        assert!(md.contains("1. **First item**"));
    }
}
