use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use cleanocr_ir::{Block, BlockKind, DocumentIr, PageIr, TableCell, TableIr, TextLine, TextSpan};
use zip::write::SimpleFileOptions;

use crate::latex_math::{escape_latex, sanitize_tex_comment, validate_latex_math};
use crate::latex_pgfplots::{emit_pgfplots, is_supported_pgfplots_chart};
use crate::{Exporter, PageVisualAssets};

/// Exports `DocumentIr` + cropped figure/fallback assets to a semantic LuaLaTeX/XeLaTeX bundle.
///
/// - If `output_path` ends with `.zip`, writes a zip archive containing `main.tex` and `assets/*`.
/// - Otherwise, treats `output_path` as a directory and writes `main.tex` + `assets/*` into it.
pub struct LatexExporter;

impl Exporter for LatexExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let tex = render_latex(ir);
        let collected_assets = collect_figure_assets(assets);

        let is_zip = output_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"));

        if is_zip {
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            let file = fs::File::create(output_path).with_context(|| {
                format!(
                    "failed to create LaTeX zip bundle {}",
                    output_path.display()
                )
            })?;
            let mut zip = zip::ZipWriter::new(file);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

            zip.start_file("main.tex", opts)?;
            zip.write_all(tex.as_bytes())?;

            for (asset_id, bytes) in collected_assets {
                let path = format!("assets/{asset_id}.png");
                zip.start_file(path, opts)?;
                zip.write_all(&bytes)?;
            }
            zip.finish()?;
        } else {
            fs::create_dir_all(output_path)?;
            fs::write(output_path.join("main.tex"), tex)?;

            if !collected_assets.is_empty() {
                let assets_dir = output_path.join("assets");
                fs::create_dir_all(&assets_dir)?;
                for (asset_id, bytes) in collected_assets {
                    fs::write(assets_dir.join(format!("{asset_id}.png")), bytes)?;
                }
            }
        }

        Ok(())
    }
}

/// Renders the full semantic LuaLaTeX/XeLaTeX source directly from `DocumentIr`.
pub fn render_latex(ir: &DocumentIr) -> String {
    let mut needs_pgfplots = false;
    let mut needs_tikz = false;

    for page in &ir.pages {
        for block in &page.blocks {
            if let BlockKind::Figure {
                chart_data,
                tikz_source,
                ..
            } = &block.kind
            {
                if chart_data.as_ref().is_some_and(is_supported_pgfplots_chart) {
                    needs_pgfplots = true;
                } else if tikz_source.is_some() {
                    needs_tikz = true;
                }
            }
        }
    }

    let mut tex = String::new();
    let _ = writeln!(tex, r"\documentclass[11pt]{{article}}");
    let _ = writeln!(tex, r"\usepackage{{fontspec}}");
    let _ = writeln!(
        tex,
        r"\usepackage{{amsmath,amssymb,graphicx,booktabs,multirow,multicol,hyperref}}"
    );
    if needs_pgfplots {
        let _ = writeln!(tex, r"\usepackage{{pgfplots}}");
        let _ = writeln!(tex, r"\pgfplotsset{{compat=1.18}}");
    } else if needs_tikz {
        let _ = writeln!(tex, r"\usepackage{{tikz}}");
    }

    if let Some(title) = &ir.metadata.title {
        let _ = writeln!(tex, r"\title{{{}}}", escape_latex(title));
        let _ = writeln!(tex, r"\date{{}}");
    }

    let _ = writeln!(tex, r"\begin{{document}}");
    if ir.metadata.title.is_some() {
        let _ = writeln!(tex, r"\maketitle");
    }
    let _ = writeln!(tex);

    for (idx, page) in ir.pages.iter().enumerate() {
        if idx > 0 {
            let _ = writeln!(tex, r"\newpage");
            let _ = writeln!(tex);
        }
        render_page(page, &mut tex);
    }

    let _ = writeln!(tex, r"\end{{document}}");
    tex
}

fn render_page(page: &PageIr, tex: &mut String) {
    let blocks = &page.blocks;
    let mut i = 0;

    while i < blocks.len() {
        if blocks[i].column_index.is_some() {
            let start = i;
            let mut max_col: u8 = 0;
            while i < blocks.len() {
                if let Some(col) = blocks[i].column_index {
                    max_col = max_col.max(col);
                    i += 1;
                } else {
                    break;
                }
            }
            let num_cols = (max_col as usize) + 1;
            if num_cols >= 2 {
                let _ = writeln!(tex, r"\begin{{multicols}}{{{num_cols}}}");
                for block in &blocks[start..i] {
                    render_block(block, tex);
                }
                let _ = writeln!(tex, r"\end{{multicols}}");
                let _ = writeln!(tex);
            } else {
                for block in &blocks[start..i] {
                    render_block(block, tex);
                }
            }
        } else {
            render_block(&blocks[i], tex);
            i += 1;
        }
    }
}

fn render_block(block: &Block, tex: &mut String) {
    match &block.kind {
        BlockKind::Heading { level, lines } => {
            let cmd = match level {
                1 => "section",
                2 => "subsection",
                3 => "subsubsection",
                4 => "paragraph",
                _ => "subparagraph",
            };
            let text = render_lines(lines);
            let _ = writeln!(tex, r"\{cmd}{{{text}}}");
            let _ = writeln!(tex);
        }
        BlockKind::Paragraph { lines } => {
            let text = render_lines(lines);
            if !text.is_empty() {
                let _ = writeln!(tex, "{text}");
                let _ = writeln!(tex);
            }
        }
        BlockKind::List { ordered, items } => {
            let env = if *ordered { "enumerate" } else { "itemize" };
            let _ = writeln!(tex, r"\begin{{{env}}}");
            for item in items {
                let text = render_lines(&item.lines);
                let _ = writeln!(tex, r"  \item {text}");
            }
            let _ = writeln!(tex, r"\end{{{env}}}");
            let _ = writeln!(tex);
        }
        BlockKind::Table(table) => render_table(table, tex),
        BlockKind::DisplayFormula {
            latex,
            fallback_asset_id,
        } => render_display_formula(latex, fallback_asset_id.as_deref(), tex),
        BlockKind::Figure {
            asset_id,
            caption,
            chart_data,
            tikz_source,
            ..
        } => {
            let _ = writeln!(tex, r"\begin{{figure}}[htbp]");
            let _ = writeln!(tex, r"  \centering");
            if let Some(pgf) = chart_data.as_ref().and_then(emit_pgfplots) {
                for line in pgf.lines() {
                    let _ = writeln!(tex, "  {line}");
                }
            } else if let Some(tikz) = tikz_source {
                for line in tikz.lines() {
                    let _ = writeln!(tex, "  {line}");
                }
            } else {
                let _ = writeln!(
                    tex,
                    r"  \includegraphics[width=0.85\linewidth]{{assets/{asset_id}.png}}"
                );
            }
            if let Some(cap) = caption {
                let _ = writeln!(tex, r"  \caption{{{}}}", escape_latex(cap));
            }
            let _ = writeln!(tex, r"\end{{figure}}");
            let _ = writeln!(tex);
        }
        BlockKind::HeaderFooter { is_header, text } => {
            let label = if *is_header { "Header" } else { "Footer" };
            let _ = writeln!(tex, "% {label}: {}", sanitize_tex_comment(text));
        }
        BlockKind::Footnote { lines, .. } => {
            let text = render_lines(lines);
            if !text.is_empty() {
                let _ = writeln!(tex, r"\footnote{{{text}}}");
                let _ = writeln!(tex);
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
        match validate_latex_math(raw_math) {
            Ok(()) => return format!("${}$", raw_math.trim()),
            Err(err) => {
                let escaped = escape_latex(&span.text);
                let note = sanitize_tex_comment(&err.to_string());
                return format!("{escaped}% CLEANOCR: invalid inline math ({note})\n");
            }
        }
    }

    let mut rendered = escape_latex(&span.text);
    if span.is_italic {
        rendered = format!(r"\textit{{{rendered}}}");
    }
    if span.is_bold {
        rendered = format!(r"\textbf{{{rendered}}}");
    }
    rendered
}

fn render_display_formula(latex: &str, fallback_asset_id: Option<&str>, tex: &mut String) {
    match validate_latex_math(latex) {
        Ok(()) => {
            let _ = writeln!(tex, r"\[");
            let _ = writeln!(tex, "{}", latex.trim());
            let _ = writeln!(tex, r"\]");
            let _ = writeln!(tex);
        }
        Err(err) => {
            let _ = writeln!(
                tex,
                "% CLEANOCR WARNING: Invalid formula ({}) -- raw: {}",
                sanitize_tex_comment(&err.to_string()),
                sanitize_tex_comment(latex)
            );
            if let Some(asset_id) = fallback_asset_id {
                let _ = writeln!(tex, r"\begin{{figure}}[htbp]");
                let _ = writeln!(tex, r"  \centering");
                let _ = writeln!(
                    tex,
                    r"  \includegraphics[width=0.80\linewidth]{{assets/{asset_id}.png}}"
                );
                let _ = writeln!(tex, r"\end{{figure}}");
                let _ = writeln!(tex);
            } else {
                let _ = writeln!(tex, r"\[");
                let _ = writeln!(tex, r"\text{{{}}}", escape_latex(latex.trim()));
                let _ = writeln!(tex, r"\]");
                let _ = writeln!(tex);
            }
        }
    }
}

fn render_table(table: &TableIr, tex: &mut String) {
    if table.num_rows == 0 || table.num_cols == 0 {
        return;
    }

    let mut cell_map: HashMap<(usize, usize), &TableCell> = HashMap::new();
    for cell in &table.cells {
        cell_map.insert((cell.row, cell.col), cell);
    }

    let mut covered = vec![vec![false; table.num_cols]; table.num_rows];
    let has_caption = table.caption.is_some();

    if has_caption {
        let _ = writeln!(tex, r"\begin{{table}}[htbp]");
        let _ = writeln!(tex, r"  \centering");
    }

    let col_spec = "l".repeat(table.num_cols);
    let _ = writeln!(tex, r"\begin{{tabular}}{{{col_spec}}}");
    let _ = writeln!(tex, r"\toprule");

    for r in 0..table.num_rows {
        let mut row_entries: Vec<String> = Vec::new();
        let mut c = 0;
        let mut row_has_cells = false;
        let mut row_all_headers = true;

        while c < table.num_cols {
            if covered[r][c] {
                row_entries.push(String::new());
                c += 1;
                continue;
            }

            if let Some(cell) = cell_map.get(&(r, c)) {
                row_has_cells = true;
                if !cell.is_header {
                    row_all_headers = false;
                }

                let cs = cell.col_span.max(1).min(table.num_cols - c);
                let rs = cell.row_span.max(1).min(table.num_rows - r);

                for dr in 0..rs {
                    for dc in 0..cs {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        covered[r + dr][c + dc] = true;
                    }
                }

                let content = escape_latex(cell.text.trim());
                let formatted = match (rs > 1, cs > 1) {
                    (true, true) => {
                        format!(r"\multicolumn{{{cs}}}{{l}}{{\multirow{{{rs}}}{{*}}{{{content}}}}}")
                    }
                    (false, true) => format!(r"\multicolumn{{{cs}}}{{l}}{{{content}}}"),
                    (true, false) => format!(r"\multirow{{{rs}}}{{*}}{{{content}}}"),
                    (false, false) => content,
                };
                row_entries.push(formatted);
                c += cs;
            } else {
                row_entries.push(String::new());
                c += 1;
            }
        }

        let _ = writeln!(tex, r"{} \\", row_entries.join(" & "));

        if row_has_cells && row_all_headers && r + 1 < table.num_rows {
            let next_is_header = (0..table.num_cols)
                .filter_map(|col| cell_map.get(&(r + 1, col)))
                .any(|cell| cell.is_header);
            if !next_is_header {
                let _ = writeln!(tex, r"\midrule");
            }
        }
    }

    let _ = writeln!(tex, r"\bottomrule");
    let _ = writeln!(tex, r"\end{{tabular}}");

    if let Some(cap) = &table.caption {
        let _ = writeln!(tex, r"  \caption{{{}}}", escape_latex(cap));
        let _ = writeln!(tex, r"\end{{table}}");
    }
    let _ = writeln!(tex);
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
