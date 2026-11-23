use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use cleanocr_ir::{
    BBox, Block, BlockKind, DocumentIr, OverlayAsset, PageIr, TableCell, TableIr, TextLine,
    TextSpan,
};

use crate::latex_math::validate_latex_math;
use crate::{Exporter, PageVisualAssets};

const IR_SCRIPT_OPEN: &str = r#"<script type="application/json" id="cleanocr-ir">"#;
const IR_SCRIPT_CLOSE: &str = "</script>";

/// Exports `DocumentIr` + visual assets to a self-contained dual-mode HTML5 document.
///
/// - If `output_path` ends with `.html` or `.htm`, writes directly to `output_path`.
/// - Otherwise, treats `output_path` as a directory and writes `document.html` inside it.
pub struct HtmlExporter;

impl Exporter for HtmlExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let html = render_html(ir, assets)?;

        let is_html_file = output_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"));

        let html_path = if is_html_file {
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            output_path.to_path_buf()
        } else {
            fs::create_dir_all(output_path)?;
            output_path.join("document.html")
        };

        fs::write(&html_path, html)
            .with_context(|| format!("failed to write HTML file {}", html_path.display()))?;
        Ok(())
    }
}

/// Renders a complete, offline self-contained HTML5 document with Replica/Reader modes,
/// layer inspector toggles, and embedded `DocumentIr` JSON.
pub fn render_html(ir: &DocumentIr, assets: &[PageVisualAssets]) -> Result<String> {
    let raw_json =
        serde_json::to_string(ir).context("failed to serialize DocumentIr for HTML embedding")?;
    // Guard against premature </script> closing tags; `\/` is valid JSON for `/`.
    let safe_json = raw_json.replace("</", r"<\/");

    let mut page_assets_map: HashMap<usize, &PageVisualAssets> = HashMap::new();
    let mut all_figures: BTreeMap<&str, &[u8]> = BTreeMap::new();
    for asset in assets {
        page_assets_map.insert(asset.page_index, asset);
        for (id, bytes) in &asset.figures {
            all_figures.insert(id.as_str(), bytes.as_slice());
        }
    }

    let doc_title = ir
        .metadata
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("cleanOCR Document");

    let mut out = String::with_capacity(16 * 1024);
    let _ = writeln!(out, "<!DOCTYPE html>");
    let _ = writeln!(out, "<html lang=\"en\">");
    let _ = writeln!(out, "<head>");
    let _ = writeln!(out, "  <meta charset=\"utf-8\">");
    let _ = writeln!(
        out,
        "  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
    );
    let _ = writeln!(out, "  <title>{}</title>", escape_html(doc_title));
    let _ = writeln!(out, "  <style>");
    out.push_str(HTML_STYLES);
    let _ = writeln!(out, "  </style>");
    let _ = writeln!(out, "</head>");
    let _ = writeln!(out, "<body data-mode=\"replica\">");

    // Top control bar
    let _ = writeln!(out, "  <header class=\"toolbar\">");
    let _ = writeln!(
        out,
        "    <strong class=\"brand\">{}</strong>",
        escape_html(doc_title)
    );
    let _ = writeln!(out, "    <div class=\"controls\">");
    let _ = writeln!(
        out,
        "      <button type=\"button\" id=\"btn-replica\" class=\"active\">Replica Mode</button>"
    );
    let _ = writeln!(
        out,
        "      <button type=\"button\" id=\"btn-reader\">Reader Mode</button>"
    );
    let _ = writeln!(
        out,
        "      <label><input type=\"checkbox\" id=\"toggle-bg\" checked> Background</label>"
    );
    let _ = writeln!(
        out,
        "      <label><input type=\"checkbox\" id=\"toggle-fg\" checked> Foreground Ink</label>"
    );
    let _ = writeln!(
        out,
        "      <label><input type=\"checkbox\" id=\"toggle-fig\" checked> Figures & Overlays</label>"
    );
    let _ = writeln!(
        out,
        "      <label><input type=\"checkbox\" id=\"toggle-debug\"> Debug Text Boxes</label>"
    );
    let _ = writeln!(
        out,
        "      <label><input type=\"checkbox\" id=\"toggle-flags\" checked> Confidence Flags</label>"
    );
    let _ = writeln!(out, "    </div>");
    let _ = writeln!(out, "  </header>");

    // Replica Mode container
    let _ = writeln!(out, "  <main id=\"replica-view\" class=\"view-replica\">");
    for page in &ir.pages {
        let page_assets = page_assets_map.get(&page.page_index).copied();
        render_replica_page(page, page_assets, &all_figures, &mut out);
    }
    let _ = writeln!(out, "  </main>");

    // Reader Mode container
    let _ = writeln!(out, "  <article id=\"reader-view\" class=\"view-reader\">");
    if let Some(title) = &ir.metadata.title
        && !title.trim().is_empty()
    {
        let _ = writeln!(
            out,
            "    <h1 class=\"doc-title\">{}</h1>",
            escape_html(title)
        );
    }
    for (idx, page) in ir.pages.iter().enumerate() {
        if idx > 0 {
            let _ = writeln!(out, "    <hr class=\"page-break\">");
        }
        render_reader_page(page, &all_figures, &mut out);
    }
    let _ = writeln!(out, "  </article>");

    // Embedded canonical JSON IR
    let _ = writeln!(out, "  {IR_SCRIPT_OPEN}{safe_json}{IR_SCRIPT_CLOSE}");

    // Inline UI controller script
    let _ = writeln!(out, "  <script>");
    out.push_str(HTML_SCRIPT);
    let _ = writeln!(out, "  </script>");
    let _ = writeln!(out, "</body>");
    let _ = writeln!(out, "</html>");

    Ok(out)
}

/// Extracts and deserializes the embedded `DocumentIr` JSON payload from an archived cleanOCR
/// HTML5 file.
pub fn extract_ir_from_html(html: &str) -> Result<DocumentIr> {
    let start_tag_pos = html
        .find(IR_SCRIPT_OPEN)
        .ok_or_else(|| anyhow!("embedded cleanocr-ir script block not found in HTML"))?;
    let json_start = start_tag_pos + IR_SCRIPT_OPEN.len();
    let rest = &html[json_start..];
    let json_end = rest
        .find(IR_SCRIPT_CLOSE)
        .ok_or_else(|| anyhow!("unterminated cleanocr-ir script block in HTML"))?;
    let json_str = rest[..json_end].trim();

    serde_json::from_str(json_str)
        .context("failed to deserialize embedded DocumentIr JSON from HTML")
}

fn render_replica_page(
    page: &PageIr,
    page_assets: Option<&PageVisualAssets>,
    all_figures: &BTreeMap<&str, &[u8]>,
    out: &mut String,
) {
    let w = page.width_pt.max(1.0);
    let h = page.height_pt.max(1.0);
    let _ = writeln!(
        out,
        "    <section class=\"page-replica\" data-page=\"{}\" style=\"aspect-ratio: {:.3} / {:.3};\">",
        page.page_index, w, h
    );

    // Layer 1: Visual Ground Truth (MRC BG + FG/Mask or fallback JPEG)
    if let Some(assets) = page_assets {
        if let Some(mrc) = &assets.mrc {
            let bg_b64 = BASE64_STANDARD.encode(&mrc.bg_jpeg);
            let fg_b64 = BASE64_STANDARD.encode(&mrc.fg_jpeg);
            let mask_b64 = BASE64_STANDARD.encode(&mrc.mask_png_1bit);
            let _ = writeln!(
                out,
                "      <img class=\"layer layer-bg\" alt=\"Page {} background\" src=\"data:image/jpeg;base64,{bg_b64}\">",
                page.page_index + 1
            );
            let _ = writeln!(
                out,
                "      <div class=\"layer layer-fg\" style=\"background-image:url('data:image/jpeg;base64,{fg_b64}');mask-image:url('data:image/png;base64,{mask_b64}');-webkit-mask-image:url('data:image/png;base64,{mask_b64}');\"></div>"
            );
        } else if let Some(jpeg) = &assets.fallback_raster_jpeg {
            let b64 = BASE64_STANDARD.encode(jpeg);
            let _ = writeln!(
                out,
                "      <img class=\"layer layer-bg\" alt=\"Page {} raster\" src=\"data:image/jpeg;base64,{b64}\">",
                page.page_index + 1
            );
        }
    }

    // Layer 3: Isolated Figures & Overlays
    let _ = writeln!(out, "      <div class=\"layer layer-fig\">");
    for block in &page.blocks {
        if let BlockKind::Figure {
            asset_id,
            alt_text,
            caption,
            ..
        } = &block.kind
            && let Some(bytes) = all_figures.get(asset_id.as_str())
        {
            let mime = detect_image_mime(bytes);
            let b64 = BASE64_STANDARD.encode(bytes);
            let alt = alt_text
                .as_deref()
                .or(caption.as_deref())
                .unwrap_or(asset_id.as_str());
            let _ = writeln!(
                out,
                "        <img class=\"positioned-figure\" style=\"{}\" alt=\"{}\" src=\"data:{mime};base64,{b64}\">",
                bbox_css(block.bbox),
                escape_html(alt)
            );
        }
    }
    for overlay in &page.overlays {
        render_replica_overlay(overlay, all_figures, out);
    }
    let _ = writeln!(out, "      </div>");

    // Layer 2: Invisible Pixel-Aligned Selectable Text Layer
    let _ = writeln!(out, "      <div class=\"layer layer-text\">");
    for block in &page.blocks {
        render_replica_text_block(block, out);
    }
    let _ = writeln!(out, "      </div>");

    // Low-confidence warning overlay
    if !page.low_confidence_flags.is_empty() {
        let _ = writeln!(out, "      <div class=\"layer layer-flags\">");
        for flag in &page.low_confidence_flags {
            let _ = writeln!(
                out,
                "        <div class=\"confidence-flag\" style=\"{}\" title=\"{} (conf {:.2})\"></div>",
                bbox_css(flag.bbox),
                escape_html(&flag.reason),
                flag.confidence
            );
        }
        let _ = writeln!(out, "      </div>");
    }

    let _ = writeln!(out, "    </section>");
}

fn render_replica_overlay(
    overlay: &OverlayAsset,
    all_figures: &BTreeMap<&str, &[u8]>,
    out: &mut String,
) {
    let aria = if overlay.aria_hidden {
        " aria-hidden=\"true\""
    } else {
        ""
    };
    if let Some(svg) = &overlay.svg_inline {
        let _ = writeln!(
            out,
            "        <div class=\"positioned-overlay\" style=\"{}\"{aria}>{svg}</div>",
            bbox_css(overlay.bbox)
        );
    } else if let Some(bytes) = all_figures.get(overlay.asset_id.as_str()) {
        let mime = detect_image_mime(bytes);
        let b64 = BASE64_STANDARD.encode(bytes);
        let _ = writeln!(
            out,
            "        <img class=\"positioned-overlay\" style=\"{}\"{aria} alt=\"{}\" src=\"data:{mime};base64,{b64}\">",
            bbox_css(overlay.bbox),
            escape_html(&overlay.asset_id)
        );
    }
}

fn render_replica_text_block(block: &Block, out: &mut String) {
    match &block.kind {
        BlockKind::Heading { lines, .. }
        | BlockKind::Paragraph { lines }
        | BlockKind::Footnote { lines, .. } => {
            render_replica_lines(lines, out);
        }
        BlockKind::List { items, .. } => {
            for item in items {
                render_replica_lines(&item.lines, out);
            }
        }
        BlockKind::Table(table) => {
            for cell in &table.cells {
                if !cell.text.trim().is_empty() {
                    let _ = writeln!(
                        out,
                        "        <span class=\"ocr-span\" style=\"{}\">{}</span>",
                        bbox_css(cell.bbox),
                        escape_html(cell.text.trim())
                    );
                }
            }
        }
        BlockKind::DisplayFormula { latex, .. } => {
            if !latex.trim().is_empty() {
                let _ = writeln!(
                    out,
                    "        <span class=\"ocr-span\" style=\"{}\">{}</span>",
                    bbox_css(block.bbox),
                    escape_html(latex.trim())
                );
            }
        }
        BlockKind::HeaderFooter { text, .. } => {
            if !text.trim().is_empty() {
                let _ = writeln!(
                    out,
                    "        <span class=\"ocr-span\" style=\"{}\">{}</span>",
                    bbox_css(block.bbox),
                    escape_html(text.trim())
                );
            }
        }
        BlockKind::Figure { .. } => {}
    }
}

fn render_replica_lines(lines: &[TextLine], out: &mut String) {
    for line in lines {
        for span in &line.spans {
            if !span.text.is_empty() {
                let _ = writeln!(
                    out,
                    "        <span class=\"ocr-span\" style=\"{}\">{}</span>",
                    bbox_css(span.bbox),
                    escape_html(&span.text)
                );
            }
        }
    }
}

fn render_reader_page(page: &PageIr, all_figures: &BTreeMap<&str, &[u8]>, out: &mut String) {
    let _ = writeln!(
        out,
        "    <section class=\"page-reader\" data-page=\"{}\">",
        page.page_index
    );

    for block in &page.blocks {
        render_reader_block(block, all_figures, out);
    }

    if !page.low_confidence_flags.is_empty() {
        let _ = writeln!(out, "      <aside class=\"warnings\">");
        let _ = writeln!(out, "        <strong>Verification Notices</strong>");
        let _ = writeln!(out, "        <ul>");
        for flag in &page.low_confidence_flags {
            let _ = writeln!(
                out,
                "          <li><code>{}</code> ({:.0}%): {}</li>",
                escape_html(&flag.block_id),
                flag.confidence * 100.0,
                escape_html(&flag.reason)
            );
        }
        let _ = writeln!(out, "        </ul>");
        let _ = writeln!(out, "      </aside>");
    }

    let _ = writeln!(out, "    </section>");
}

fn render_reader_block(block: &Block, all_figures: &BTreeMap<&str, &[u8]>, out: &mut String) {
    match &block.kind {
        BlockKind::Heading { level, lines } => {
            let tag = (*level).clamp(1, 6);
            let _ = writeln!(out, "      <h{tag}>{}</h{tag}>", render_reader_lines(lines));
        }
        BlockKind::Paragraph { lines } => {
            let body = render_reader_lines(lines);
            if !body.is_empty() {
                let _ = writeln!(out, "      <p>{body}</p>");
            }
        }
        BlockKind::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let _ = writeln!(out, "      <{tag}>");
            for item in items {
                let _ = writeln!(out, "        <li>{}</li>", render_reader_lines(&item.lines));
            }
            let _ = writeln!(out, "      </{tag}>");
        }
        BlockKind::Table(table) => render_reader_table(table, out),
        BlockKind::DisplayFormula {
            latex,
            fallback_asset_id,
        } => {
            if validate_latex_math(latex).is_ok() {
                let _ = writeln!(
                    out,
                    "      <div class=\"math-display\" data-latex=\"{}\">\\[{}\\]</div>",
                    escape_html(latex.trim()),
                    escape_html(latex.trim())
                );
            } else if let Some(asset_id) = fallback_asset_id
                && let Some(bytes) = all_figures.get(asset_id.as_str())
            {
                let mime = detect_image_mime(bytes);
                let b64 = BASE64_STANDARD.encode(bytes);
                let _ = writeln!(
                    out,
                    "      <figure class=\"formula-fallback\"><img alt=\"Formula\" src=\"data:{mime};base64,{b64}\"></figure>"
                );
            } else {
                let _ = writeln!(
                    out,
                    "      <pre class=\"formula-raw\">{}</pre>",
                    escape_html(latex.trim())
                );
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
            let _ = writeln!(out, "      <figure>");
            if let Some(bytes) = all_figures.get(asset_id.as_str()) {
                let mime = detect_image_mime(bytes);
                let b64 = BASE64_STANDARD.encode(bytes);
                let _ = writeln!(
                    out,
                    "        <img alt=\"{}\" src=\"data:{mime};base64,{b64}\">",
                    escape_html(alt)
                );
            } else {
                let _ = writeln!(
                    out,
                    "        <div class=\"figure-placeholder\">[Figure: {}]</div>",
                    escape_html(alt)
                );
            }
            if let Some(cap) = caption {
                let _ = writeln!(out, "        <figcaption>{}</figcaption>", escape_html(cap));
            }
            let _ = writeln!(out, "      </figure>");
        }
        BlockKind::HeaderFooter { .. } => {}
        BlockKind::Footnote { marker, lines } => {
            let prefix = marker
                .as_deref()
                .map(|m| format!("<sup>{}</sup> ", escape_html(m)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "      <p class=\"footnote\">{prefix}{}</p>",
                render_reader_lines(lines)
            );
        }
    }
}

fn render_reader_lines(lines: &[TextLine]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let mut line_html = String::new();
        for span in &line.spans {
            line_html.push_str(&render_reader_span(span));
        }
        let trimmed = line_html.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    parts.join(" ")
}

fn render_reader_span(span: &TextSpan) -> String {
    if let Some(raw_math) = &span.inline_math_latex
        && validate_latex_math(raw_math).is_ok()
    {
        let escaped = escape_html(raw_math.trim());
        return format!(
            "<span class=\"math-inline\" data-latex=\"{escaped}\">\\({escaped}\\)</span>"
        );
    }

    let mut html = escape_html(&span.text);
    if span.is_italic {
        html = format!("<em>{html}</em>");
    }
    if span.is_bold {
        html = format!("<strong>{html}</strong>");
    }
    html
}

fn render_reader_table(table: &TableIr, out: &mut String) {
    if table.num_rows == 0 || table.num_cols == 0 {
        return;
    }

    let mut cell_map: HashMap<(usize, usize), &TableCell> = HashMap::new();
    for cell in &table.cells {
        cell_map.insert((cell.row, cell.col), cell);
    }

    let mut covered = vec![vec![false; table.num_cols]; table.num_rows];
    let _ = writeln!(out, "      <table>");
    if let Some(cap) = &table.caption {
        let _ = writeln!(out, "        <caption>{}</caption>", escape_html(cap));
    }

    for r in 0..table.num_rows {
        let _ = writeln!(out, "        <tr>");
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
                    "          <{tag}{attrs}>{}</{tag}>",
                    escape_html(cell.text.trim())
                );
                c += cs;
            } else {
                let _ = writeln!(out, "          <td></td>");
                c += 1;
            }
        }
        let _ = writeln!(out, "        </tr>");
    }

    let _ = writeln!(out, "      </table>");
}

fn bbox_css(bbox: BBox) -> String {
    format!(
        "left:{:.3}%;top:{:.3}%;width:{:.3}%;height:{:.3}%;",
        bbox.x0 * 100.0,
        bbox.y0 * 100.0,
        bbox.width() * 100.0,
        bbox.height() * 100.0
    )
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "image/svg+xml"
    } else {
        "image/png"
    }
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("\x26amp;"),
            '<' => out.push_str("\x26lt;"),
            '>' => out.push_str("\x26gt;"),
            '"' => out.push_str("\x26quot;"),
            '\'' => out.push_str("\x26#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const HTML_STYLES: &str = r#"
    :root { --bg: #f3f4f6; --card: #ffffff; --ink: #111827; --accent: #2563eb; }
    * { box-sizing: border-box; }
    body { margin: 0; font-family: system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--ink); }
    .toolbar { position: sticky; top: 0; z-index: 50; display: flex; flex-wrap: wrap; align-items: center; gap: 0.75rem; padding: 0.6rem 1rem; background: #1f2937; color: #f9fafb; }
    .toolbar .brand { margin-right: auto; font-size: 0.95rem; }
    .toolbar .controls { display: flex; flex-wrap: wrap; align-items: center; gap: 0.65rem; font-size: 0.85rem; }
    .toolbar button { border: 1px solid #4b5563; background: #374151; color: #fff; padding: 0.3rem 0.65rem; border-radius: 4px; cursor: pointer; }
    .toolbar button.active { background: var(--accent); border-color: var(--accent); }
    body[data-mode="replica"] .view-reader { display: none; }
    body[data-mode="reader"] .view-replica { display: none; }
    .view-replica { display: flex; flex-direction: column; align-items: center; gap: 1.5rem; padding: 1.5rem; }
    .page-replica { position: relative; width: min(92vw, 860px); background: var(--card); box-shadow: 0 2px 12px rgba(0,0,0,0.12); overflow: hidden; }
    .layer { position: absolute; inset: 0; width: 100%; height: 100%; }
    .layer-bg { object-fit: fill; pointer-events: none; }
    .layer-fg { background-size: 100% 100%; mask-size: 100% 100%; -webkit-mask-size: 100% 100%; pointer-events: none; }
    .layer-fig { pointer-events: none; }
    .positioned-figure, .positioned-overlay { position: absolute; object-fit: contain; }
    .layer-text { z-index: 10; }
    .ocr-span { position: absolute; color: transparent; white-space: pre; line-height: 1; overflow: hidden; user-select: text; }
    body.debug-text .ocr-span { color: rgba(220,38,38,0.85); outline: 1px dashed rgba(220,38,38,0.6); background: rgba(254,226,226,0.2); }
    .layer-flags { z-index: 20; pointer-events: none; }
    .confidence-flag { position: absolute; border: 2px solid #f59e0b; background: rgba(245,158,11,0.12); }
    body.hide-bg .layer-bg { display: none; }
    body.hide-fg .layer-fg { display: none; }
    body.hide-fig .layer-fig { display: none; }
    body.hide-flags .layer-flags { display: none; }
    .view-reader { max-width: 780px; margin: 1.5rem auto; padding: 2rem; background: var(--card); box-shadow: 0 1px 6px rgba(0,0,0,0.08); line-height: 1.65; }
    .view-reader table { border-collapse: collapse; width: 100%; margin: 1rem 0; }
    .view-reader th, .view-reader td { border: 1px solid #d1d5db; padding: 0.45rem 0.6rem; text-align: left; }
    .view-reader th { background: #f9fafb; }
    .view-reader figure { margin: 1.25rem 0; text-align: center; }
    .view-reader figure img { max-width: 100%; height: auto; }
    .view-reader .warnings { margin-top: 1.5rem; padding: 0.75rem 1rem; border-left: 4px solid #f59e0b; background: #fffbeb; font-size: 0.9rem; }
    .math-display { margin: 1rem 0; text-align: center; font-family: ui-monospace, monospace; }
    .math-inline { font-family: ui-monospace, monospace; }
    .footnote { font-size: 0.85rem; color: #4b5563; }
"#;

const HTML_SCRIPT: &str = r#"
    (function () {
      const body = document.body;
      const btnReplica = document.getElementById('btn-replica');
      const btnReader = document.getElementById('btn-reader');
      function setMode(mode) {
        body.setAttribute('data-mode', mode);
        btnReplica.classList.toggle('active', mode === 'replica');
        btnReader.classList.toggle('active', mode === 'reader');
      }
      btnReplica.addEventListener('click', () => setMode('replica'));
      btnReader.addEventListener('click', () => setMode('reader'));
      document.getElementById('toggle-bg').addEventListener('change', (e) => body.classList.toggle('hide-bg', !e.target.checked));
      document.getElementById('toggle-fg').addEventListener('change', (e) => body.classList.toggle('hide-fg', !e.target.checked));
      document.getElementById('toggle-fig').addEventListener('change', (e) => body.classList.toggle('hide-fig', !e.target.checked));
      document.getElementById('toggle-debug').addEventListener('change', (e) => body.classList.toggle('debug-text', e.target.checked));
      document.getElementById('toggle-flags').addEventListener('change', (e) => body.classList.toggle('hide-flags', !e.target.checked));
    })();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use cleanocr_ir::{
        ConfidenceWarning, DocumentMetadata, IR_SCHEMA_VERSION, PageIngestMode, TextSource,
    };
    use cleanocr_mrc::MrcLayers;

    #[test]
    fn html_escapes_script_tag_and_roundtrips_document_ir_losslessly() {
        let bbox = BBox::new(0.1, 0.1, 0.9, 0.16);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Security & </script> Test".to_string()),
                source_sha256: "feedface".to_string(),
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
                ingest_mode: PageIngestMode::ScannedRaster,
                blocks: vec![Block {
                    id: "p0_b00".to_string(),
                    bbox,
                    reading_order: 0,
                    column_index: None,
                    confidence: 0.92,
                    kind: BlockKind::Paragraph {
                        lines: vec![TextLine {
                            bbox,
                            spans: vec![TextSpan {
                                text: "Payload </script><script>alert(1)</script> and <\\/tag>"
                                    .to_string(),
                                bbox,
                                confidence: 0.92,
                                is_bold: false,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            }],
                        }],
                    },
                }],
                overlays: Vec::new(),
                low_confidence_flags: vec![ConfidenceWarning {
                    block_id: "p0_b00".to_string(),
                    bbox,
                    confidence: 0.92,
                    reason: "Verify OCR".to_string(),
                }],
            }],
        };

        let assets = vec![PageVisualAssets {
            page_index: 0,
            mrc: Some(MrcLayers {
                width: 100,
                height: 100,
                mask_jbig2: vec![1, 2, 3],
                mask_png_1bit: vec![137, 80, 78, 71],
                fg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                fg_width: 34,
                fg_height: 34,
                bg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                bg_width: 50,
                bg_height: 50,
            }),
            fallback_raster_jpeg: None,
            figures: BTreeMap::new(),
        }];

        let html = render_html(&ir, &assets).expect("render HTML");
        assert!(html.contains(r"<\/script>"));
        assert!(html.contains("layer-bg"));
        assert!(html.contains("layer-fg"));
        assert!(html.contains("confidence-flag"));

        let extracted = extract_ir_from_html(&html).expect("extract DocumentIr from HTML");
        assert_eq!(extracted, ir);
    }
}
