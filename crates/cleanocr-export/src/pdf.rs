use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use cleanocr_ir::{BBox, Block, BlockKind, DocumentIr, PageIr, TextLine};
use pdf_writer::{Name, Pdf, Rect, Ref, Str};

use crate::{Exporter, PageVisualAssets};

const EMBEDDED_IR_FILENAME: &str = "cleanocr-ir.json";

/// Centralized monotonic PDF indirect reference allocator so object IDs never collide.
struct RefAllocator {
    next_id: i32,
}

impl RefAllocator {
    fn new() -> Self {
        Self { next_id: 1 }
    }

    fn alloc(&mut self) -> Ref {
        let id = self.next_id;
        self.next_id += 1;
        Ref::new(id)
    }
}

/// Converts a normalized top-left `BBox` in `[0.0, 1.0]` to bottom-left PDF coordinates
/// `(x_ll, y_ll, width_pt, height_pt)`.
///
/// This is the ONLY module in the workspace permitted to flip the Y-axis into PDF space.
pub fn bbox_to_pdf_rect(
    bbox: BBox,
    page_width_pt: f32,
    page_height_pt: f32,
) -> (f32, f32, f32, f32) {
    let x_ll = bbox.x0 * page_width_pt;
    let y_ll = page_height_pt - (bbox.y1 * page_height_pt);
    let width_pt = bbox.width() * page_width_pt;
    let height_pt = bbox.height() * page_height_pt;
    (x_ll, y_ll, width_pt, height_pt)
}

/// Exports `DocumentIr` + visual assets to a Hybrid Layered PDF/A-3 document with 3 OCG layers,
/// pixel-aligned invisible text (`3 Tr`), and embedded `cleanocr-ir.json`.
pub struct PdfExporter;

impl Exporter for PdfExporter {
    fn export(
        &self,
        ir: &DocumentIr,
        assets: &[PageVisualAssets],
        output_path: &Path,
    ) -> Result<()> {
        let bytes = build_pdf_bytes(ir, assets)?;

        let is_pdf_file = output_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));

        let pdf_path = if is_pdf_file {
            if let Some(parent) = output_path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            output_path.to_path_buf()
        } else {
            fs::create_dir_all(output_path)?;
            output_path.join("document.pdf")
        };

        fs::write(&pdf_path, bytes)
            .with_context(|| format!("failed to write PDF file {}", pdf_path.display()))?;
        Ok(())
    }
}

/// Builds a complete Hybrid Layered PDF/A-3 byte buffer from `DocumentIr` and visual assets.
pub fn build_pdf_bytes(ir: &DocumentIr, assets: &[PageVisualAssets]) -> Result<Vec<u8>> {
    let mut pdf = Pdf::new();
    let mut refs = RefAllocator::new();

    let catalog_id = refs.alloc();
    let page_tree_id = refs.alloc();
    let font_id = refs.alloc();

    // 3 toggleable Optional Content Groups (layers)
    let ocg_bg_id = refs.alloc();
    let ocg_fg_id = refs.alloc();
    let ocg_fig_id = refs.alloc();

    // Embedded canonical JSON IR (PDF/A-3 Associated File)
    let ir_stream_id = refs.alloc();
    let ir_filespec_id = refs.alloc();
    let ir_json_bytes =
        serde_json::to_vec(ir).context("failed to serialize DocumentIr for PDF embedding")?;

    // Map page_index -> PageVisualAssets
    let mut assets_by_page: HashMap<usize, &PageVisualAssets> = HashMap::new();
    for asset in assets {
        assets_by_page.insert(asset.page_index, asset);
    }

    // Write font object (used strictly for invisible `3 Tr` text selection layer)
    {
        let mut font = pdf.indirect(font_id).dict();
        font.pair(Name(b"Type"), Name(b"Font"));
        font.pair(Name(b"Subtype"), Name(b"Type1"));
        font.pair(Name(b"BaseFont"), Name(b"Helvetica"));
        font.pair(Name(b"Encoding"), Name(b"WinAnsiEncoding"));
    }

    // Write OCG dictionaries
    write_ocg(&mut pdf, ocg_bg_id, b"Background");
    write_ocg(&mut pdf, ocg_fg_id, b"Foreground_Ink");
    write_ocg(&mut pdf, ocg_fig_id, b"Figures_Overlays");

    // Write embedded cleanocr-ir.json stream and filespec
    {
        let mut stream = pdf.stream(ir_stream_id, &ir_json_bytes);
        stream.pair(Name(b"Type"), Name(b"EmbeddedFile"));
        stream.pair(Name(b"Subtype"), Name(b"application#2Fjson"));
        stream
            .insert(Name(b"Params"))
            .dict()
            .pair(Name(b"Size"), ir_json_bytes.len() as i32);
    }
    {
        let mut fs_dict = pdf.indirect(ir_filespec_id).dict();
        fs_dict.pair(Name(b"Type"), Name(b"Filespec"));
        fs_dict.pair(Name(b"F"), Str(EMBEDDED_IR_FILENAME.as_bytes()));
        fs_dict.pair(Name(b"UF"), Str(EMBEDDED_IR_FILENAME.as_bytes()));
        fs_dict.pair(Name(b"Desc"), Str(b"cleanOCR Canonical Document IR"));
        fs_dict.pair(Name(b"AFRelationship"), Name(b"Data"));
        let mut ef = fs_dict.insert(Name(b"EF")).dict();
        ef.pair(Name(b"F"), ir_stream_id);
        ef.pair(Name(b"UF"), ir_stream_id);
    }

    // Emit each page
    let mut page_ids = Vec::with_capacity(ir.pages.len());
    for page_ir in &ir.pages {
        let page_id = refs.alloc();
        let content_id = refs.alloc();
        page_ids.push(page_id);

        let page_assets = assets_by_page.get(&page_ir.page_index).copied();
        let mut xobjects: Vec<(String, Ref)> = Vec::new();
        let mut stream_ops = String::with_capacity(2048);

        // Layer 1: Background + Foreground Ink (MRC or fallback raster)
        if let Some(pa) = page_assets {
            if let Some(mrc) = &pa.mrc {
                let bg_id = refs.alloc();
                write_jpeg_xobject(
                    &mut pdf,
                    bg_id,
                    &mrc.bg_jpeg,
                    mrc.bg_width.max(1) as i32,
                    mrc.bg_height.max(1) as i32,
                    None,
                );
                xobjects.push(("BgImg".to_string(), bg_id));

                let mask_id = refs.alloc();
                write_jbig2_mask_xobject(
                    &mut pdf,
                    mask_id,
                    &mrc.mask_jbig2,
                    mrc.width.max(1) as i32,
                    mrc.height.max(1) as i32,
                );

                let fg_id = refs.alloc();
                write_jpeg_xobject(
                    &mut pdf,
                    fg_id,
                    &mrc.fg_jpeg,
                    mrc.fg_width.max(1) as i32,
                    mrc.fg_height.max(1) as i32,
                    Some(mask_id),
                );
                xobjects.push(("FgImg".to_string(), fg_id));

                let _ = writeln!(stream_ops, "/OC /OCBg BDC");
                let _ = writeln!(
                    stream_ops,
                    "q {:.3} 0 0 {:.3} 0 0 cm /BgImg Do Q",
                    page_ir.width_pt, page_ir.height_pt
                );
                let _ = writeln!(stream_ops, "EMC");

                let _ = writeln!(stream_ops, "/OC /OCFg BDC");
                let _ = writeln!(
                    stream_ops,
                    "q {:.3} 0 0 {:.3} 0 0 cm /FgImg Do Q",
                    page_ir.width_pt, page_ir.height_pt
                );
                let _ = writeln!(stream_ops, "EMC");
            } else if let Some(jpeg) = &pa.fallback_raster_jpeg {
                let (jw, jh) = parse_jpeg_dimensions(jpeg).unwrap_or((
                    page_ir.width_pt.round().max(1.0) as i32,
                    page_ir.height_pt.round().max(1.0) as i32,
                ));
                let bg_id = refs.alloc();
                write_jpeg_xobject(&mut pdf, bg_id, jpeg, jw, jh, None);
                xobjects.push(("BgImg".to_string(), bg_id));

                let _ = writeln!(stream_ops, "/OC /OCBg BDC");
                let _ = writeln!(
                    stream_ops,
                    "q {:.3} 0 0 {:.3} 0 0 cm /BgImg Do Q",
                    page_ir.width_pt, page_ir.height_pt
                );
                let _ = writeln!(stream_ops, "EMC");
            }

            // Layer 3: Isolated JPEG Figure Overlays
            let mut fig_counter = 0usize;
            let mut fig_ops = String::new();
            for block in &page_ir.blocks {
                if let BlockKind::Figure { asset_id, .. } = &block.kind
                    && let Some(bytes) = pa.figures.get(asset_id)
                    && bytes.starts_with(&[0xFF, 0xD8, 0xFF])
                {
                    let (x_ll, y_ll, w_pt, h_pt) =
                        bbox_to_pdf_rect(block.bbox, page_ir.width_pt, page_ir.height_pt);
                    let (jw, jh) = parse_jpeg_dimensions(bytes)
                        .unwrap_or((w_pt.round().max(1.0) as i32, h_pt.round().max(1.0) as i32));
                    let fig_ref = refs.alloc();
                    let name = format!("Fig{fig_counter}");
                    fig_counter += 1;
                    write_jpeg_xobject(&mut pdf, fig_ref, bytes, jw, jh, None);
                    xobjects.push((name.clone(), fig_ref));
                    let _ = writeln!(
                        fig_ops,
                        "q {:.3} 0 0 {:.3} {:.3} {:.3} cm /{name} Do Q",
                        w_pt, h_pt, x_ll, y_ll
                    );
                }
            }
            if !fig_ops.is_empty() {
                let _ = writeln!(stream_ops, "/OC /OCFig BDC");
                stream_ops.push_str(&fig_ops);
                let _ = writeln!(stream_ops, "EMC");
            }
        }

        // Layer 2: Invisible Pixel-Aligned Semantic Text Layer (3 Tr)
        append_invisible_text_ops(page_ir, &mut stream_ops);

        // Write content stream
        pdf.stream(content_id, stream_ops.as_bytes());

        // Write Page dictionary
        {
            let mut page = pdf.page(page_id);
            page.parent(page_tree_id);
            page.media_box(Rect::new(0.0, 0.0, page_ir.width_pt, page_ir.height_pt));
            page.contents(content_id);

            let mut resources = page.resources();
            resources
                .insert(Name(b"Font"))
                .dict()
                .pair(Name(b"F1"), font_id);

            {
                let mut props = resources.insert(Name(b"Properties")).dict();
                props.pair(Name(b"OCBg"), ocg_bg_id);
                props.pair(Name(b"OCFg"), ocg_fg_id);
                props.pair(Name(b"OCFig"), ocg_fig_id);
            }

            if !xobjects.is_empty() {
                let mut xobj_dict = resources.insert(Name(b"XObject")).dict();
                for (name, xref) in &xobjects {
                    xobj_dict.pair(Name(name.as_bytes()), *xref);
                }
            }
        }
    }

    // Write Page Tree
    {
        let mut pages = pdf.pages(page_tree_id);
        pages.kids(page_ids.iter().copied());
        pages.count(page_ids.len() as i32);
    }

    // Write Catalog with OCG properties, EmbeddedFiles name tree, and PDF/A-3 AF array
    {
        let mut catalog = pdf.catalog(catalog_id);
        catalog.pages(page_tree_id);
        catalog.insert(Name(b"AF")).array().item(ir_filespec_id);

        {
            let mut names = catalog.insert(Name(b"Names")).dict();
            let mut embedded = names.insert(Name(b"EmbeddedFiles")).dict();
            let mut name_arr = embedded.insert(Name(b"Names")).array();
            name_arr
                .item(Str(EMBEDDED_IR_FILENAME.as_bytes()))
                .item(ir_filespec_id);
        }

        {
            let mut oc_props = catalog.insert(Name(b"OCProperties")).dict();
            oc_props
                .insert(Name(b"OCGs"))
                .array()
                .item(ocg_bg_id)
                .item(ocg_fg_id)
                .item(ocg_fig_id);

            let mut default_cfg = oc_props.insert(Name(b"D")).dict();
            default_cfg.pair(Name(b"BaseState"), Name(b"ON"));
            default_cfg
                .insert(Name(b"Order"))
                .array()
                .item(ocg_bg_id)
                .item(ocg_fg_id)
                .item(ocg_fig_id);
        }
    }

    Ok(pdf.finish())
}

/// Extracts the embedded `cleanocr-ir.json` stream from an archived cleanOCR PDF/A-3 file.
pub fn extract_ir_from_pdf(pdf_bytes: &[u8]) -> Result<DocumentIr> {
    let stream_obj_num = find_embedded_ir_stream_obj(pdf_bytes)
        .ok_or_else(|| anyhow!("embedded '{EMBEDDED_IR_FILENAME}' attachment not found in PDF"))?;
    let payload = extract_stream_bytes(pdf_bytes, stream_obj_num)?;
    serde_json::from_slice(payload)
        .context("failed to deserialize embedded DocumentIr JSON from PDF")
}

fn write_ocg(pdf: &mut Pdf, id: Ref, name: &[u8]) {
    let mut ocg = pdf.indirect(id).dict();
    ocg.pair(Name(b"Type"), Name(b"OCG"));
    ocg.pair(Name(b"Name"), Str(name));
}

fn write_jpeg_xobject(
    pdf: &mut Pdf,
    id: Ref,
    jpeg_bytes: &[u8],
    width: i32,
    height: i32,
    mask_ref: Option<Ref>,
) {
    let mut stream = pdf.stream(id, jpeg_bytes);
    stream.pair(Name(b"Type"), Name(b"XObject"));
    stream.pair(Name(b"Subtype"), Name(b"Image"));
    stream.pair(Name(b"Width"), width);
    stream.pair(Name(b"Height"), height);
    stream.pair(Name(b"ColorSpace"), Name(b"DeviceRGB"));
    stream.pair(Name(b"BitsPerComponent"), 8);
    stream.pair(Name(b"Filter"), Name(b"DCTDecode"));
    if let Some(mask) = mask_ref {
        stream.pair(Name(b"Mask"), mask);
    }
}

fn write_jbig2_mask_xobject(pdf: &mut Pdf, id: Ref, jbig2_bytes: &[u8], width: i32, height: i32) {
    let mut stream = pdf.stream(id, jbig2_bytes);
    stream.pair(Name(b"Type"), Name(b"XObject"));
    stream.pair(Name(b"Subtype"), Name(b"Image"));
    stream.pair(Name(b"Width"), width);
    stream.pair(Name(b"Height"), height);
    stream.pair(Name(b"ImageMask"), true);
    stream.pair(Name(b"BitsPerComponent"), 1);
    stream.pair(Name(b"Filter"), Name(b"JBIG2Decode"));
}

fn append_invisible_text_ops(page: &PageIr, ops: &mut String) {
    for block in &page.blocks {
        append_block_text_ops(block, page.width_pt, page.height_pt, ops);
    }
    for overlay in &page.overlays {
        if let Some(text) = &overlay.extracted_text {
            emit_text_span_op(text, overlay.bbox, page.width_pt, page.height_pt, ops);
        }
    }
}

fn append_block_text_ops(block: &Block, page_w: f32, page_h: f32, ops: &mut String) {
    match &block.kind {
        BlockKind::Heading { lines, .. }
        | BlockKind::Paragraph { lines }
        | BlockKind::Footnote { lines, .. } => {
            append_lines_text_ops(lines, page_w, page_h, ops);
        }
        BlockKind::List { items, .. } => {
            for item in items {
                append_lines_text_ops(&item.lines, page_w, page_h, ops);
            }
        }
        BlockKind::Table(table) => {
            for cell in &table.cells {
                emit_text_span_op(&cell.text, cell.bbox, page_w, page_h, ops);
            }
        }
        BlockKind::DisplayFormula { latex, .. } => {
            emit_text_span_op(latex, block.bbox, page_w, page_h, ops);
        }
        BlockKind::HeaderFooter { text, .. } => {
            emit_text_span_op(text, block.bbox, page_w, page_h, ops);
        }
        BlockKind::Figure { .. } => {}
    }
}

fn append_lines_text_ops(lines: &[TextLine], page_w: f32, page_h: f32, ops: &mut String) {
    for line in lines {
        for span in &line.spans {
            emit_text_span_op(&span.text, span.bbox, page_w, page_h, ops);
        }
    }
}

fn emit_text_span_op(text: &str, bbox: BBox, page_w: f32, page_h: f32, ops: &mut String) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

    let (x_ll, y_ll, _w_pt, h_pt) = bbox_to_pdf_rect(bbox, page_w, page_h);
    let font_size = (h_pt * 0.85).clamp(4.0, 96.0);
    let escaped = escape_pdf_literal(trimmed);

    let _ = writeln!(
        ops,
        "BT /F1 {font_size:.2} Tf 3 Tr 1 0 0 1 {x_ll:.2} {y_ll:.2} Tm ({escaped}) Tj ET"
    );
}

fn escape_pdf_literal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '(' => out.push_str(r"\("),
            ')' => out.push_str(r"\)"),
            '\r' | '\n' => out.push(' '),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

fn parse_jpeg_dimensions(data: &[u8]) -> Option<(i32, i32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let marker = data[i];
        i += 1;
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 2 > data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > data.len() {
            break;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) && seg_len >= 7 {
            let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as i32;
            let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as i32;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        i += seg_len;
    }
    None
}

fn find_embedded_ir_stream_obj(pdf: &[u8]) -> Option<u32> {
    let needle = format!("({EMBEDDED_IR_FILENAME})");
    let needle_bytes = needle.as_bytes();
    let mut search_from = 0;

    while let Some(rel) = find_subslice(&pdf[search_from..], needle_bytes) {
        let pos = search_from + rel;
        let window_end = (pos + 512).min(pdf.len());
        let window = &pdf[pos..window_end];

        if let Some(endobj_rel) = find_subslice(window, b"endobj") {
            let obj_tail = &window[..endobj_rel];
            if let Some(ef_rel) = find_subslice(obj_tail, b"/EF") {
                let ef_slice = &obj_tail[ef_rel + 3..];
                if let Some(f_rel) = find_subslice(ef_slice, b"/F")
                    && let Some(num) = parse_first_uint(&ef_slice[f_rel + 2..])
                {
                    return Some(num);
                }
            }
        }

        search_from = pos + needle_bytes.len();
    }
    None
}

fn extract_stream_bytes(pdf: &[u8], obj_num: u32) -> Result<&[u8]> {
    let header_lf = format!("\n{obj_num} 0 obj");
    let header_cr = format!("\r{obj_num} 0 obj");
    let header_start = format!("{obj_num} 0 obj");

    let obj_pos = if pdf.starts_with(header_start.as_bytes()) {
        0
    } else if let Some(p) = find_subslice(pdf, header_lf.as_bytes()) {
        p + 1
    } else if let Some(p) = find_subslice(pdf, header_cr.as_bytes()) {
        p + 1
    } else {
        return Err(anyhow!("PDF object {obj_num} 0 obj not found"));
    };

    let rest = &pdf[obj_pos..];
    let stream_kw_pos = find_subslice(rest, b"stream")
        .ok_or_else(|| anyhow!("PDF object {obj_num} has no stream keyword"))?;
    let dict_header = &rest[..stream_kw_pos];

    let len_kw_pos = find_subslice(dict_header, b"/Length")
        .ok_or_else(|| anyhow!("PDF stream object {obj_num} missing /Length"))?;
    let stream_len = parse_first_uint(&dict_header[len_kw_pos + 7..])
        .ok_or_else(|| anyhow!("invalid /Length value in PDF object {obj_num}"))?
        as usize;

    let mut data_start = stream_kw_pos + b"stream".len();
    if rest.get(data_start) == Some(&b'\r') && rest.get(data_start + 1) == Some(&b'\n') {
        data_start += 2;
    } else if rest.get(data_start) == Some(&b'\n') || rest.get(data_start) == Some(&b'\r') {
        data_start += 1;
    }

    let data_end = data_start
        .checked_add(stream_len)
        .ok_or_else(|| anyhow!("stream length overflow in PDF object {obj_num}"))?;
    if data_end > rest.len() {
        return Err(anyhow!("truncated stream in PDF object {obj_num}"));
    }

    Ok(&rest[data_start..data_end])
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_first_uint(bytes: &[u8]) -> Option<u32> {
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..i]).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use cleanocr_ir::{DocumentMetadata, IR_SCHEMA_VERSION, PageIngestMode, TextSource, TextSpan};
    use cleanocr_mrc::MrcLayers;

    #[test]
    fn top_heading_flips_to_upper_pdf_y_axis() {
        let page_w = 595.0;
        let page_h = 842.0;
        let top_heading = BBox::new(0.10, 0.05, 0.90, 0.10);

        let (x_ll, y_pdf, w_pt, h_pt) = bbox_to_pdf_rect(top_heading, page_w, page_h);
        assert!((x_ll - 59.5).abs() < 1e-3);
        assert!(
            y_pdf > 0.8 * page_h,
            "top-of-page heading must land above 80% page height, got y_pdf={y_pdf}"
        );
        assert!((w_pt - 0.8 * page_w).abs() < 1e-3);
        assert!((h_pt - 0.05 * page_h).abs() < 1e-3);
    }

    #[test]
    fn pdf_contains_ocg_layers_invisible_text_and_roundtrips_embedded_ir() {
        let heading_box = BBox::new(0.10, 0.06, 0.90, 0.11);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Layered PDF Test".to_string()),
                source_sha256: "cafebabe".to_string(),
                source_filename: Some("layered.pdf".to_string()),
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
                    id: "p0_h0".to_string(),
                    bbox: heading_box,
                    reading_order: 0,
                    column_index: None,
                    confidence: 0.99,
                    kind: BlockKind::Heading {
                        level: 1,
                        lines: vec![TextLine {
                            bbox: heading_box,
                            spans: vec![TextSpan {
                                text: "Executive Summary".to_string(),
                                bbox: heading_box,
                                confidence: 0.99,
                                is_bold: true,
                                is_italic: false,
                                inline_math_latex: None,
                                source: TextSource::Ocr,
                            }],
                        }],
                    },
                }],
                overlays: Vec::new(),
                low_confidence_flags: Vec::new(),
            }],
        };

        let assets = vec![PageVisualAssets {
            page_index: 0,
            mrc: Some(MrcLayers {
                width: 600,
                height: 800,
                mask_jbig2: vec![0x97, 0x4A, 0x42, 0x32],
                mask_png_1bit: vec![137, 80, 78, 71],
                fg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                fg_width: 200,
                fg_height: 267,
                bg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                bg_width: 300,
                bg_height: 400,
            }),
            fallback_raster_jpeg: None,
            figures: BTreeMap::new(),
        }];

        let pdf_bytes = build_pdf_bytes(&ir, &assets).expect("build PDF bytes");
        assert!(pdf_bytes.starts_with(b"%PDF-"));

        let pdf_text = String::from_utf8_lossy(&pdf_bytes);
        assert!(pdf_text.contains("(Background)"));
        assert!(pdf_text.contains("(Foreground_Ink)"));
        assert!(pdf_text.contains("(Figures_Overlays)"));
        assert!(pdf_text.contains("/JBIG2Decode"));
        assert!(pdf_text.contains("3 Tr"));
        assert!(pdf_text.contains("(Executive Summary) Tj"));
        assert!(pdf_text.contains("/AFRelationship /Data"));

        let roundtripped = extract_ir_from_pdf(&pdf_bytes).expect("extract IR from PDF");
        assert_eq!(roundtripped, ir);
    }
}
