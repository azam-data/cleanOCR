use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::{Cursor, Write as _};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use cleanocr_ir::{BBox, Block, BlockKind, DocumentIr, PageIr, TextLine};
use flate2::Compression;
use flate2::write::ZlibEncoder;
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

/// Exports `DocumentIr` + visual assets to a Hybrid Layered PDF/A-3 document with 4 OCG layers
/// (`Background`, `Graphics`, `Tables`, `Text`), pixel-aligned Unicode invisible text (`3 Tr`),
/// and embedded `cleanocr-ir.json`.
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
    let cid_font_id = refs.alloc();
    let font_desc_id = refs.alloc();
    let to_unicode_id = refs.alloc();

    // 4 toggleable Optional Content Groups (layers)
    let ocg_bg_id = refs.alloc();
    let ocg_gfx_id = refs.alloc();
    let ocg_tbl_id = refs.alloc();
    let ocg_txt_id = refs.alloc();

    // Embedded canonical JSON IR (PDF/A-3 Associated File)
    let ir_stream_id = refs.alloc();
    let ir_filespec_id = refs.alloc();
    let ir_json_bytes =
        serde_json::to_vec(ir).context("failed to serialize DocumentIr for PDF embedding")?;

    // Collect all unique Unicode scalar values for Type0 Identity-H + ToUnicode CMap.
    let unicode_cmap = build_unicode_cid_map(ir);
    write_unicode_type0_font(
        &mut pdf,
        font_id,
        cid_font_id,
        font_desc_id,
        to_unicode_id,
        &unicode_cmap,
    );

    // Map page_index -> PageVisualAssets
    let mut assets_by_page: HashMap<usize, &PageVisualAssets> = HashMap::new();
    for asset in assets {
        assets_by_page.insert(asset.page_index, asset);
    }

    // Write OCG dictionaries
    write_ocg(&mut pdf, ocg_bg_id, b"Background");
    write_ocg(&mut pdf, ocg_gfx_id, b"Graphics");
    write_ocg(&mut pdf, ocg_tbl_id, b"Tables");
    write_ocg(&mut pdf, ocg_txt_id, b"Text");

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

    // Emit each page in strict z-order: Background -> Graphics -> Tables -> Text -> Invisible Text
    let mut page_ids = Vec::with_capacity(ir.pages.len());
    for page_ir in &ir.pages {
        let page_id = refs.alloc();
        let content_id = refs.alloc();
        page_ids.push(page_id);

        let page_assets = assets_by_page.get(&page_ir.page_index).copied();
        let mut xobjects: Vec<(String, Ref)> = Vec::new();
        let mut stream_ops = String::with_capacity(2048);

        if let Some(pa) = page_assets {
            // Layer 1: Background (MRC BG JPEG or full-page fallback raster JPEG)
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

                let _ = writeln!(stream_ops, "/OC /OCBg BDC");
                let _ = writeln!(
                    stream_ops,
                    "q {:.3} 0 0 {:.3} 0 0 cm /BgImg Do Q",
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

            // Layer 2: Graphics (JPEG DCTDecode or PNG FlateDecode RGB + optional /SMask alpha)
            let mut fig_counter = 0usize;
            let mut fig_ops = String::new();
            for block in &page_ir.blocks {
                if let BlockKind::Figure { asset_id, .. } = &block.kind
                    && let Some(bytes) = pa.figures.get(asset_id)
                {
                    let (x_ll, y_ll, w_pt, h_pt) =
                        bbox_to_pdf_rect(block.bbox, page_ir.width_pt, page_ir.height_pt);
                    let name = format!("Fig{fig_counter}");

                    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
                        let (jw, jh) = parse_jpeg_dimensions(bytes).unwrap_or((
                            w_pt.round().max(1.0) as i32,
                            h_pt.round().max(1.0) as i32,
                        ));
                        let fig_ref = refs.alloc();
                        write_jpeg_xobject(&mut pdf, fig_ref, bytes, jw, jh, None);
                        xobjects.push((name.clone(), fig_ref));
                        fig_counter += 1;
                        let _ = writeln!(
                            fig_ops,
                            "q {:.3} 0 0 {:.3} {:.3} {:.3} cm /{name} Do Q",
                            w_pt, h_pt, x_ll, y_ll
                        );
                    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                        && let Some(fig_ref) = write_png_xobject(&mut pdf, &mut refs, bytes)?
                    {
                        xobjects.push((name.clone(), fig_ref));
                        fig_counter += 1;
                        let _ = writeln!(
                            fig_ops,
                            "q {:.3} 0 0 {:.3} {:.3} {:.3} cm /{name} Do Q",
                            w_pt, h_pt, x_ll, y_ll
                        );
                    }
                }
            }
            if !fig_ops.is_empty() {
                let _ = writeln!(stream_ops, "/OC /OCGfx BDC");
                stream_ops.push_str(&fig_ops);
                let _ = writeln!(stream_ops, "EMC");
            }

            // Layer 3 & Layer 4: Tables (1-bit JBIG2 ImageMask) and Text (FG JPEG + Text JBIG2 Mask)
            if let Some(mrc) = &pa.mrc {
                if let Some(table_jbig2) = &mrc.table_jbig2 {
                    let tbl_mask_id = refs.alloc();
                    write_jbig2_mask_xobject(
                        &mut pdf,
                        tbl_mask_id,
                        table_jbig2,
                        mrc.width.max(1) as i32,
                        mrc.height.max(1) as i32,
                    );
                    xobjects.push(("TblMask".to_string(), tbl_mask_id));

                    let _ = writeln!(stream_ops, "/OC /OCTbl BDC");
                    let _ = writeln!(
                        stream_ops,
                        "q 0 0 0 rg 0 g {:.3} 0 0 {:.3} 0 0 cm /TblMask Do Q",
                        page_ir.width_pt, page_ir.height_pt
                    );
                    let _ = writeln!(stream_ops, "EMC");
                }

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

                let _ = writeln!(stream_ops, "/OC /OCTxt BDC");
                let _ = writeln!(
                    stream_ops,
                    "q {:.3} 0 0 {:.3} 0 0 cm /FgImg Do Q",
                    page_ir.width_pt, page_ir.height_pt
                );
                let _ = writeln!(stream_ops, "EMC");
            }
        }

        // Invisible Pixel-Aligned Semantic Unicode Text Layer (3 Tr)
        append_invisible_text_ops(page_ir, &unicode_cmap, &mut stream_ops);

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
                props.pair(Name(b"OCGfx"), ocg_gfx_id);
                props.pair(Name(b"OCTbl"), ocg_tbl_id);
                props.pair(Name(b"OCTxt"), ocg_txt_id);
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

    // Write Catalog with 4 OCGs, EmbeddedFiles name tree, and PDF/A-3 AF array
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
                .item(ocg_gfx_id)
                .item(ocg_tbl_id)
                .item(ocg_txt_id);

            let mut default_cfg = oc_props.insert(Name(b"D")).dict();
            default_cfg.pair(Name(b"BaseState"), Name(b"ON"));
            default_cfg
                .insert(Name(b"Order"))
                .array()
                .item(ocg_bg_id)
                .item(ocg_gfx_id)
                .item(ocg_tbl_id)
                .item(ocg_txt_id);
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

fn build_unicode_cid_map(ir: &DocumentIr) -> BTreeMap<char, u16> {
    let mut map = BTreeMap::new();
    let mut next_cid: u16 = 1;
    let mut add_str = |s: &str| {
        for ch in s.chars() {
            if !map.contains_key(&ch) && next_cid < u16::MAX {
                map.insert(ch, next_cid);
                next_cid += 1;
            }
        }
    };

    for page in &ir.pages {
        for block in &page.blocks {
            match &block.kind {
                BlockKind::Heading { lines, .. }
                | BlockKind::Paragraph { lines }
                | BlockKind::Footnote { lines, .. } => {
                    for l in lines {
                        for sp in &l.spans {
                            add_str(&sp.text);
                        }
                    }
                }
                BlockKind::List { items, .. } => {
                    for it in items {
                        for l in &it.lines {
                            for sp in &l.spans {
                                add_str(&sp.text);
                            }
                        }
                    }
                }
                BlockKind::Table(tbl) => {
                    for cell in &tbl.cells {
                        add_str(&cell.text);
                    }
                }
                BlockKind::DisplayFormula { latex, .. } => add_str(latex),
                BlockKind::HeaderFooter { text, .. } => add_str(text),
                BlockKind::Figure { .. } => {}
            }
        }
        for overlay in &page.overlays {
            if let Some(t) = &overlay.extracted_text {
                add_str(t);
            }
        }
    }
    map
}

fn write_unicode_type0_font(
    pdf: &mut Pdf,
    font_id: Ref,
    cid_font_id: Ref,
    font_desc_id: Ref,
    to_unicode_id: Ref,
    unicode_cmap: &BTreeMap<char, u16>,
) {
    {
        let mut f = pdf.indirect(font_id).dict();
        f.pair(Name(b"Type"), Name(b"Font"));
        f.pair(Name(b"Subtype"), Name(b"Type0"));
        f.pair(Name(b"BaseFont"), Name(b"Helvetica"));
        f.pair(Name(b"Encoding"), Name(b"Identity-H"));
        f.insert(Name(b"DescendantFonts")).array().item(cid_font_id);
        f.pair(Name(b"ToUnicode"), to_unicode_id);
    }
    {
        let mut cf = pdf.indirect(cid_font_id).dict();
        cf.pair(Name(b"Type"), Name(b"Font"));
        cf.pair(Name(b"Subtype"), Name(b"CIDFontType2"));
        cf.pair(Name(b"BaseFont"), Name(b"Helvetica"));
        let mut sys = cf.insert(Name(b"CIDSystemInfo")).dict();
        sys.pair(Name(b"Registry"), Str(b"Adobe"));
        sys.pair(Name(b"Ordering"), Str(b"Identity"));
        sys.pair(Name(b"Supplement"), 0);
        drop(sys);
        cf.pair(Name(b"FontDescriptor"), font_desc_id);
        cf.pair(Name(b"DW"), 500);
        cf.pair(Name(b"CIDToGIDMap"), Name(b"Identity"));
    }
    {
        let mut fd = pdf.indirect(font_desc_id).dict();
        fd.pair(Name(b"Type"), Name(b"FontDescriptor"));
        fd.pair(Name(b"FontName"), Name(b"Helvetica"));
        fd.pair(Name(b"Flags"), 32);
        fd.pair(Name(b"FontBBox"), Rect::new(0.0, -200.0, 1000.0, 900.0));
        fd.pair(Name(b"ItalicAngle"), 0);
        fd.pair(Name(b"Ascent"), 800);
        fd.pair(Name(b"Descent"), -200);
        fd.pair(Name(b"CapHeight"), 700);
        fd.pair(Name(b"StemV"), 80);
    }

    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n\
         12 dict begin\n\
         begincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n\
         /CMapType 2 def\n\
         1 begincodespacerange\n\
         <0000> <FFFF>\n\
         endcodespacerange\n",
    );

    let entries: Vec<(u16, char)> = unicode_cmap.iter().map(|(&ch, &cid)| (cid, ch)).collect();
    for chunk in entries.chunks(100) {
        let _ = writeln!(cmap, "{} beginbfchar", chunk.len());
        for &(cid, ch) in chunk {
            let mut u16_buf = [0u16; 2];
            let units = ch.encode_utf16(&mut u16_buf);
            let mut hex = String::new();
            for u in units {
                let _ = write!(hex, "{u:04X}");
            }
            let _ = writeln!(cmap, "<{cid:04X}> <{hex}>");
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str(
        "endcmap\n\
         CMapName currentdict /CMap defineresource pop\n\
         end\n\
         end\n",
    );

    pdf.stream(to_unicode_id, cmap.as_bytes());
}

fn write_png_xobject(
    pdf: &mut Pdf,
    refs: &mut RefAllocator,
    png_bytes: &[u8],
) -> Result<Option<Ref>> {
    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let buf_size = reader.output_buffer_size();
    let mut raw = vec![0u8; buf_size];
    let info = match reader.next_frame(&mut raw) {
        Ok(info) => info,
        Err(_) => return Ok(None),
    };
    let raw = &raw[..info.buffer_size()];
    let w = info.width as i32;
    let h = info.height as i32;

    let (rgb_pixels, alpha_pixels) = match info.color_type {
        png::ColorType::Rgb => (raw.to_vec(), None),
        png::ColorType::Rgba => {
            let mut rgb = Vec::with_capacity((raw.len() / 4) * 3);
            let mut alpha = Vec::with_capacity(raw.len() / 4);
            for px in raw.chunks_exact(4) {
                rgb.push(px[0]);
                rgb.push(px[1]);
                rgb.push(px[2]);
                alpha.push(px[3]);
            }
            (rgb, Some(alpha))
        }
        png::ColorType::Grayscale => {
            let mut rgb = Vec::with_capacity(raw.len() * 3);
            for &g in raw {
                rgb.extend_from_slice(&[g, g, g]);
            }
            (rgb, None)
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgb = Vec::with_capacity((raw.len() / 2) * 3);
            let mut alpha = Vec::with_capacity(raw.len() / 2);
            for px in raw.chunks_exact(2) {
                rgb.extend_from_slice(&[px[0], px[0], px[0]]);
                alpha.push(px[1]);
            }
            (rgb, Some(alpha))
        }
        png::ColorType::Indexed => return Ok(None),
    };

    let compressed_rgb = zlib_compress(&rgb_pixels)?;
    let smask_ref = if let Some(alpha) = alpha_pixels {
        let compressed_alpha = zlib_compress(&alpha)?;
        let smask_id = refs.alloc();
        let mut smask = pdf.stream(smask_id, &compressed_alpha);
        smask.pair(Name(b"Type"), Name(b"XObject"));
        smask.pair(Name(b"Subtype"), Name(b"Image"));
        smask.pair(Name(b"Width"), w);
        smask.pair(Name(b"Height"), h);
        smask.pair(Name(b"ColorSpace"), Name(b"DeviceGray"));
        smask.pair(Name(b"BitsPerComponent"), 8);
        smask.pair(Name(b"Filter"), Name(b"FlateDecode"));
        Some(smask_id)
    } else {
        None
    };

    let img_id = refs.alloc();
    let mut stream = pdf.stream(img_id, &compressed_rgb);
    stream.pair(Name(b"Type"), Name(b"XObject"));
    stream.pair(Name(b"Subtype"), Name(b"Image"));
    stream.pair(Name(b"Width"), w);
    stream.pair(Name(b"Height"), h);
    stream.pair(Name(b"ColorSpace"), Name(b"DeviceRGB"));
    stream.pair(Name(b"BitsPerComponent"), 8);
    stream.pair(Name(b"Filter"), Name(b"FlateDecode"));
    if let Some(smask) = smask_ref {
        stream.pair(Name(b"SMask"), smask);
    }

    Ok(Some(img_id))
}

fn zlib_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)
        .context("failed to zlib-compress image stream")?;
    enc.finish().context("failed to finish zlib stream")
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

fn append_invisible_text_ops(page: &PageIr, unicode_cmap: &BTreeMap<char, u16>, ops: &mut String) {
    for block in &page.blocks {
        append_block_text_ops(block, page.width_pt, page.height_pt, unicode_cmap, ops);
    }
    for overlay in &page.overlays {
        if let Some(text) = &overlay.extracted_text {
            emit_text_span_op(
                text,
                overlay.bbox,
                page.width_pt,
                page.height_pt,
                unicode_cmap,
                ops,
            );
        }
    }
}

fn append_block_text_ops(
    block: &Block,
    page_w: f32,
    page_h: f32,
    unicode_cmap: &BTreeMap<char, u16>,
    ops: &mut String,
) {
    match &block.kind {
        BlockKind::Heading { lines, .. }
        | BlockKind::Paragraph { lines }
        | BlockKind::Footnote { lines, .. } => {
            append_lines_text_ops(lines, page_w, page_h, unicode_cmap, ops);
        }
        BlockKind::List { items, .. } => {
            for item in items {
                append_lines_text_ops(&item.lines, page_w, page_h, unicode_cmap, ops);
            }
        }
        BlockKind::Table(table) => {
            for cell in &table.cells {
                emit_text_span_op(&cell.text, cell.bbox, page_w, page_h, unicode_cmap, ops);
            }
        }
        BlockKind::DisplayFormula { latex, .. } => {
            emit_text_span_op(latex, block.bbox, page_w, page_h, unicode_cmap, ops);
        }
        BlockKind::HeaderFooter { text, .. } => {
            emit_text_span_op(text, block.bbox, page_w, page_h, unicode_cmap, ops);
        }
        BlockKind::Figure { .. } => {}
    }
}

fn append_lines_text_ops(
    lines: &[TextLine],
    page_w: f32,
    page_h: f32,
    unicode_cmap: &BTreeMap<char, u16>,
    ops: &mut String,
) {
    for line in lines {
        for span in &line.spans {
            emit_text_span_op(&span.text, span.bbox, page_w, page_h, unicode_cmap, ops);
        }
    }
}

fn emit_text_span_op(
    text: &str,
    bbox: BBox,
    page_w: f32,
    page_h: f32,
    unicode_cmap: &BTreeMap<char, u16>,
    ops: &mut String,
) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

    let (x_ll, y_ll, _w_pt, h_pt) = bbox_to_pdf_rect(bbox, page_w, page_h);
    let font_size = (h_pt * 0.85).clamp(4.0, 96.0);
    let mut hex_cids = String::with_capacity(trimmed.chars().count() * 4);
    for ch in trimmed.chars() {
        let cid = unicode_cmap.get(&ch).copied().unwrap_or(0);
        let _ = write!(hex_cids, "{cid:04X}");
    }

    let _ = writeln!(
        ops,
        "BT /F1 {font_size:.2} Tf 3 Tr 1 0 0 1 {x_ll:.2} {y_ll:.2} Tm <{hex_cids}> Tj ET"
    );
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
                table_jbig2: None,
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
        assert!(pdf_text.contains("(Graphics)"));
        assert!(pdf_text.contains("(Tables)"));
        assert!(pdf_text.contains("(Text)"));
        assert!(pdf_text.contains("/JBIG2Decode"));
        assert!(pdf_text.contains("/ToUnicode"));
        assert!(pdf_text.contains("3 Tr"));
        assert!(pdf_text.contains("> Tj"));
        assert!(pdf_text.contains("/AFRelationship /Data"));

        let roundtripped = extract_ir_from_pdf(&pdf_bytes).expect("extract IR from PDF");
        assert_eq!(roundtripped, ir);
    }

    #[test]
    fn pdf_4_layer_ocgs_png_smask_table_jbig2_and_unicode_tounicode() {
        // Encode a 2x2 RGBA PNG with transparency.
        let mut rgba_png = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut rgba_png, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().expect("png header");
            writer
                .write_image_data(&[
                    20, 90, 210, 255, 255, 255, 255, 0, 20, 90, 210, 255, 255, 255, 255, 0,
                ])
                .expect("png data");
        }

        let b = BBox::new(0.10, 0.12, 0.60, 0.18);
        let ir = DocumentIr {
            schema_version: IR_SCHEMA_VERSION.to_string(),
            metadata: DocumentMetadata {
                title: Some("Bank Circular".to_string()),
                source_sha256: "deadbeef".to_string(),
                source_filename: Some("bank.pdf".to_string()),
                detected_languages: vec!["en".to_string(), "bn".to_string()],
                created_utc: "2026-11-25T00:00:00Z".to_string(),
                engine_version: "0.1.0".to_string(),
            },
            pages: vec![PageIr {
                page_index: 0,
                width_pt: 595.0,
                height_pt: 842.0,
                dpi: 300,
                ingest_mode: PageIngestMode::ScannedRaster,
                blocks: vec![
                    Block {
                        id: "p0_p0".to_string(),
                        bbox: b,
                        reading_order: 0,
                        column_index: None,
                        confidence: 0.99,
                        kind: BlockKind::Paragraph {
                            lines: vec![TextLine {
                                bbox: b,
                                spans: vec![TextSpan {
                                    text: "৳1,250.00".to_string(),
                                    bbox: b,
                                    confidence: 0.99,
                                    is_bold: false,
                                    is_italic: false,
                                    inline_math_latex: None,
                                    source: TextSource::Ocr,
                                }],
                            }],
                        },
                    },
                    Block {
                        id: "p0_fig0".to_string(),
                        bbox: BBox::new(0.05, 0.04, 0.20, 0.10),
                        reading_order: 1,
                        column_index: None,
                        confidence: 0.95,
                        kind: BlockKind::Figure {
                            asset_id: "logo_rgba".to_string(),
                            caption: None,
                            alt_text: Some("Bank Logo".to_string()),
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
        figures.insert("logo_rgba".to_string(), rgba_png);

        let assets = vec![PageVisualAssets {
            page_index: 0,
            mrc: Some(MrcLayers {
                width: 600,
                height: 800,
                mask_jbig2: vec![0x97, 0x4A, 0x42, 0x32],
                table_jbig2: Some(vec![0x97, 0x4A, 0x42, 0x33]),
                mask_png_1bit: vec![137, 80, 78, 71],
                fg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                fg_width: 200,
                fg_height: 267,
                bg_jpeg: vec![0xFF, 0xD8, 0xFF, 0xD9],
                bg_width: 300,
                bg_height: 400,
            }),
            fallback_raster_jpeg: None,
            figures,
        }];

        let pdf_bytes = build_pdf_bytes(&ir, &assets).expect("build 4-layer PDF bytes");
        let pdf_text = String::from_utf8_lossy(&pdf_bytes);

        for required in [
            "(Background)",
            "(Graphics)",
            "(Tables)",
            "(Text)",
            "/SMask",
            "/JBIG2Decode",
            "/ToUnicode",
            "/TblMask Do",
            "<09F3>", // UTF-16BE code point for '৳' in ToUnicode CMap
        ] {
            assert!(
                pdf_text.contains(required),
                "PDF missing required token: {required}"
            );
        }

        let roundtripped = extract_ir_from_pdf(&pdf_bytes).expect("extract IR from PDF");
        assert_eq!(roundtripped, ir);
    }
}
