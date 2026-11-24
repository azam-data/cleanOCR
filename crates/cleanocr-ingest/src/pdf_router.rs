use anyhow::{Context, Result};
use cleanocr_ir::PageIngestMode;
use lopdf::{Document, Object};

/// Per-page structural inspection result from `lopdf` used to route between
/// native PDF extraction, full raster OCR, or hybrid processing.
#[derive(Debug, Clone)]
pub struct PdfPageClassification {
    pub page_index: usize,
    pub page_number: u32,
    pub width_pt: f32,
    pub height_pt: f32,
    pub mode: PageIngestMode,
    pub native_text: String,
    pub image_xobject_count: usize,
}

/// Inspects every page of a PDF buffer via `lopdf` and classifies each page into:
/// - `DigitalNativeText`: Clean embedded text layer, no full-page scan images.
/// - `ScannedRaster`: Little or no usable text layer (needs full OCR).
/// - `HybridNativePlusOcr`: Contains both substantial native text and raster image XObjects.
pub fn classify_pdf_pages(pdf_bytes: &[u8]) -> Result<Vec<PdfPageClassification>> {
    let doc = Document::load_mem(pdf_bytes).context("lopdf failed to parse PDF document")?;
    let pages = doc.get_pages();
    let mut results = Vec::with_capacity(pages.len());

    for (page_index, (&page_num, &page_id)) in pages.iter().enumerate() {
        let (width_pt, height_pt) = extract_media_box(&doc, page_id).unwrap_or((595.0, 842.0));
        let native_text = doc
            .extract_text(&[page_num])
            .unwrap_or_default()
            .trim()
            .to_string();

        let image_xobject_count = count_image_xobjects(&doc, page_id);
        let char_count = native_text.chars().filter(|c| !c.is_whitespace()).count();

        let mode = if char_count >= 25 && image_xobject_count == 0 {
            PageIngestMode::DigitalNativeText
        } else if char_count >= 25 && image_xobject_count > 0 {
            PageIngestMode::HybridNativePlusOcr
        } else {
            PageIngestMode::ScannedRaster
        };

        results.push(PdfPageClassification {
            page_index,
            page_number: page_num,
            width_pt,
            height_pt,
            mode,
            native_text,
            image_xobject_count,
        });
    }

    Ok(results)
}

fn extract_media_box(doc: &Document, page_id: lopdf::ObjectId) -> Option<(f32, f32)> {
    let page_dict = doc.get_dictionary(page_id).ok()?;
    let media_box = page_dict.get(b"MediaBox").ok()?;
    let arr = match media_box {
        Object::Array(a) => a,
        Object::Reference(r) => doc.get_object(*r).ok()?.as_array().ok()?,
        _ => return None,
    };
    if arr.len() < 4 {
        return None;
    }
    let x0 = obj_to_f32(&arr[0])?;
    let y0 = obj_to_f32(&arr[1])?;
    let x1 = obj_to_f32(&arr[2])?;
    let y1 = obj_to_f32(&arr[3])?;
    let w = (x1 - x0).abs();
    let h = (y1 - y0).abs();
    if w > 1.0 && h > 1.0 {
        Some((w, h))
    } else {
        None
    }
}

fn obj_to_f32(obj: &Object) -> Option<f32> {
    match obj {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(f) => Some(*f),
        _ => None,
    }
}

fn count_image_xobjects(doc: &Document, page_id: lopdf::ObjectId) -> usize {
    let Ok(page_dict) = doc.get_dictionary(page_id) else {
        return 0;
    };
    let Ok(res_obj) = page_dict.get(b"Resources") else {
        return 0;
    };
    let res_dict = match res_obj {
        Object::Dictionary(d) => d,
        Object::Reference(r) => match doc.get_dictionary(*r) {
            Ok(d) => d,
            Err(_) => return 0,
        },
        _ => return 0,
    };

    let Ok(xobj_obj) = res_dict.get(b"XObject") else {
        return 0;
    };
    let xobj_dict = match xobj_obj {
        Object::Dictionary(d) => d,
        Object::Reference(r) => match doc.get_dictionary(*r) {
            Ok(d) => d,
            Err(_) => return 0,
        },
        _ => return 0,
    };

    let mut count = 0;
    for (_, val) in xobj_dict.iter() {
        let stream_dict = match val {
            Object::Reference(r) => doc
                .get_object(*r)
                .ok()
                .and_then(|o| o.as_stream().ok())
                .map(|s| &s.dict),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        };
        if let Some(d) = stream_dict
            && let Ok(Object::Name(subtype)) = d.get(b"Subtype")
            && subtype == b"Image"
        {
            count += 1;
        }
    }
    count
}
