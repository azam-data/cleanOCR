use std::collections::{BTreeMap, VecDeque};
use std::io::Cursor;

use anyhow::{Context, Result};
use cleanocr_ir::{BBox, Block, BlockKind};
use image::{ImageFormat, RgbImage, Rgba, RgbaImage};

use crate::assets::crop_bbox_to_png;

/// Classification of an automatically detected non-text graphic region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicKind {
    ColorLogo,
    Signature,
    PhotoOrChart,
}

/// Detected graphic bounding box + semantic class.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectedGraphic {
    pub bbox: BBox,
    pub kind: GraphicKind,
}

/// Automatically detects color logos/stamps, handwritten signatures, and embedded photos/charts
/// outside recognized OCR/table blocks and `table_grid_mask`, crops them as PNGs (with
/// transparent paper alpha for logos/signatures), and returns `BlockKind::Figure` blocks.
pub fn detect_and_crop_graphics(
    img: &RgbImage,
    existing_blocks: &[Block],
    table_grid_mask: Option<&[u8]>,
    page_index: usize,
    figures: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<Block>> {
    let detected = detect_graphics(img, existing_blocks, table_grid_mask);
    let mut out_blocks = Vec::with_capacity(detected.len());

    for (idx, item) in detected.into_iter().enumerate() {
        let asset_id = format!("p{page_index}_fig{idx:02}");
        let png_bytes = match item.kind {
            GraphicKind::ColorLogo | GraphicKind::Signature => {
                crop_transparent_png(img, item.bbox)?
            }
            GraphicKind::PhotoOrChart => crop_bbox_to_png(img, item.bbox)?,
        };
        figures.insert(asset_id.clone(), png_bytes);

        let alt_text = match item.kind {
            GraphicKind::ColorLogo => Some("Detected color logo or stamp".to_string()),
            GraphicKind::Signature => Some("Detected handwritten signature".to_string()),
            GraphicKind::PhotoOrChart => Some("Detected embedded photo or chart".to_string()),
        };

        out_blocks.push(Block {
            id: format!("p{page_index}_g{idx:03}"),
            bbox: item.bbox,
            reading_order: 0,
            column_index: None,
            confidence: 0.90,
            kind: BlockKind::Figure {
                asset_id,
                caption: None,
                alt_text,
                is_chart: matches!(item.kind, GraphicKind::PhotoOrChart),
                chart_data: None,
                tikz_source: None,
            },
        });
    }

    Ok(out_blocks)
}

/// Detects non-text graphic regions (`ColorLogo`, `Signature`, `PhotoOrChart`) outside
/// `existing_blocks` and `table_grid_mask`.
pub fn detect_graphics(
    img: &RgbImage,
    existing_blocks: &[Block],
    table_grid_mask: Option<&[u8]>,
) -> Vec<DetectedGraphic> {
    let orig_w = img.width();
    let orig_h = img.height();
    if orig_w < 16 || orig_h < 16 {
        return Vec::new();
    }

    let step = (orig_w.max(orig_h) / 600).clamp(1, 4) as usize;
    let w = (orig_w as usize).div_ceil(step);
    let h = (orig_h as usize).div_ceil(step);

    // 1. Build exclusion mask from existing OCR/table blocks and table_grid_mask.
    let mut excluded = vec![false; w * h];
    for block in existing_blocks {
        let (x0, y0, x1, y1) = block.bbox.to_pixels(w as u32, h as u32);
        for y in y0..y1 {
            let row = y as usize * w;
            for x in x0..x1 {
                excluded[row + x as usize] = true;
            }
        }
    }

    if let Some(mask) = table_grid_mask
        && mask.len() == orig_w as usize * orig_h as usize
    {
        for sy in 0..h {
            let oy = (sy * step).min(orig_h as usize - 1);
            for sx in 0..w {
                let ox = (sx * step).min(orig_w as usize - 1);
                if mask[oy * orig_w as usize + ox] != 0 {
                    excluded[sy * w + sx] = true;
                }
            }
        }
    }

    // 2. Sample RGB, luminance, and HSV saturation on the working grid.
    let mut luma = vec![255u8; w * h];
    let mut is_color_px = vec![false; w * h];
    let mut is_dark_px = vec![false; w * h];

    for sy in 0..h {
        let oy = (sy * step).min(orig_h as usize - 1) as u32;
        for sx in 0..w {
            let ox = (sx * step).min(orig_w as usize - 1) as u32;
            let idx = sy * w + sx;
            if excluded[idx] {
                continue;
            }
            let p = img.get_pixel(ox, oy).0;
            let r = p[0];
            let g = p[1];
            let b = p[2];
            let max_c = r.max(g).max(b);
            let min_c = r.min(g).min(b);
            let lum = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) as u8;
            luma[idx] = lum;

            let sat = if max_c > 25 {
                (max_c - min_c) as f32 / max_c as f32
            } else {
                0.0
            };
            if sat > 0.25 && min_c < 225 {
                is_color_px[idx] = true;
            } else if lum < 155 {
                is_dark_px[idx] = true;
            }
        }
    }

    let mut candidates: Vec<DetectedGraphic> = Vec::new();
    let min_logo_area = ((w * h) / 3000).max(20);

    // 3. Detect color logos/stamps (dilated 1px so nearby colored strokes merge).
    let color_dilated = dilate_mask(&is_color_px, w, h, 2);
    for (x0, y0, x1, y1, area) in connected_components(&color_dilated, w, h) {
        if area >= min_logo_area {
            candidates.push(DetectedGraphic {
                bbox: BBox::new(
                    x0 as f32 / w as f32,
                    y0 as f32 / h as f32,
                    (x1 + 1) as f32 / w as f32,
                    (y1 + 1) as f32 / h as f32,
                ),
                kind: GraphicKind::ColorLogo,
            });
        }
    }

    // 4. Detect handwritten signatures: dark ink components outside OCR text lines whose
    //    height exceeds standard text line height.
    let median_text_h_norm = estimate_median_text_height(existing_blocks);
    let median_text_h_px = (median_text_h_norm * h as f32).max(6.0);
    let dark_dilated = dilate_mask(&is_dark_px, w, h, 2);

    for (x0, y0, x1, y1, area) in connected_components(&dark_dilated, w, h) {
        let bh = (y1 - y0 + 1) as f32;
        let bw = (x1 - x0 + 1) as f32;
        if area >= min_logo_area && bh > 1.35 * median_text_h_px && bw > 1.2 * median_text_h_px {
            candidates.push(DetectedGraphic {
                bbox: BBox::new(
                    x0 as f32 / w as f32,
                    y0 as f32 / h as f32,
                    (x1 + 1) as f32 / w as f32,
                    (y1 + 1) as f32 / h as f32,
                ),
                kind: GraphicKind::Signature,
            });
        }
    }

    // 5. Detect continuous-tone photos/charts via tile gradient variance & multi-level luma.
    let tile = 16usize;
    let tw = w / tile;
    let th = h / tile;
    if tw >= 2 && th >= 2 {
        let mut photo_tiles = vec![false; tw * th];
        for ty in 0..th {
            for tx in 0..tw {
                let mut excl_cnt = 0usize;
                let mut grad_sum = 0u64;
                let mut buckets = [false; 16];
                let total = tile * tile;

                for dy in 0..tile {
                    let y = ty * tile + dy;
                    for dx in 0..tile {
                        let x = tx * tile + dx;
                        let idx = y * w + x;
                        if excluded[idx] {
                            excl_cnt += 1;
                        }
                        let lum = luma[idx];
                        buckets[(lum as usize) / 16] = true;
                        if dx + 1 < tile {
                            grad_sum += lum.abs_diff(luma[idx + 1]) as u64;
                        }
                        if dy + 1 < tile {
                            grad_sum += lum.abs_diff(luma[idx + w]) as u64;
                        }
                    }
                }

                let distinct_levels = buckets.iter().filter(|&&b| b).count();
                let mean_grad = grad_sum as f32 / total as f32;
                if excl_cnt * 4 < total && distinct_levels >= 6 && mean_grad > 10.0 {
                    photo_tiles[ty * tw + tx] = true;
                }
            }
        }

        for (tx0, ty0, tx1, ty1, area) in connected_components(&photo_tiles, tw, th) {
            if area >= 4 {
                candidates.push(DetectedGraphic {
                    bbox: BBox::new(
                        (tx0 * tile) as f32 / w as f32,
                        (ty0 * tile) as f32 / h as f32,
                        ((tx1 + 1) * tile).min(w) as f32 / w as f32,
                        ((ty1 + 1) * tile).min(h) as f32 / h as f32,
                    ),
                    kind: GraphicKind::PhotoOrChart,
                });
            }
        }
    }

    merge_overlapping_graphics(candidates)
}

fn estimate_median_text_height(blocks: &[Block]) -> f32 {
    let mut heights: Vec<f32> = Vec::new();
    for b in blocks {
        if let BlockKind::Paragraph { lines } | BlockKind::Heading { lines, .. } = &b.kind {
            for l in lines {
                if l.bbox.height() > 0.002 {
                    heights.push(l.bbox.height());
                }
            }
        }
    }
    if heights.is_empty() {
        return 0.025;
    }
    heights.sort_by(|a, b| a.total_cmp(b));
    heights[heights.len() / 2]
}

fn dilate_mask(mask: &[bool], w: usize, h: usize, radius: usize) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            if !mask[y * w + x] {
                continue;
            }
            let y0 = y.saturating_sub(radius);
            let y1 = (y + radius).min(h - 1);
            let x0 = x.saturating_sub(radius);
            let x1 = (x + radius).min(w - 1);
            for ny in y0..=y1 {
                for nx in x0..=x1 {
                    out[ny * w + nx] = true;
                }
            }
        }
    }
    out
}

fn connected_components(
    mask: &[bool],
    w: usize,
    h: usize,
) -> Vec<(usize, usize, usize, usize, usize)> {
    let mut visited = vec![false; w * h];
    let mut comps = Vec::new();
    let mut queue = VecDeque::new();

    for y in 0..h {
        for x in 0..w {
            let start = y * w + x;
            if !mask[start] || visited[start] {
                continue;
            }
            visited[start] = true;
            queue.push_back((x, y));

            let mut min_x = x;
            let mut max_x = x;
            let mut min_y = y;
            let mut max_y = y;
            let mut area = 0usize;

            while let Some((cx, cy)) = queue.pop_front() {
                area += 1;
                min_x = min_x.min(cx);
                max_x = max_x.max(cx);
                min_y = min_y.min(cy);
                max_y = max_y.max(cy);

                for dy in -1isize..=1 {
                    let ny = cy as isize + dy;
                    if ny < 0 || ny >= h as isize {
                        continue;
                    }
                    for dx in -1isize..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = cx as isize + dx;
                        if nx < 0 || nx >= w as isize {
                            continue;
                        }
                        let nidx = ny as usize * w + nx as usize;
                        if mask[nidx] && !visited[nidx] {
                            visited[nidx] = true;
                            queue.push_back((nx as usize, ny as usize));
                        }
                    }
                }
            }

            comps.push((min_x, min_y, max_x, max_y, area));
        }
    }

    comps
}

fn merge_overlapping_graphics(mut items: Vec<DetectedGraphic>) -> Vec<DetectedGraphic> {
    let mut merged: Vec<DetectedGraphic> = Vec::new();
    let pad = 0.006f32;

    for item in items.drain(..) {
        let expanded = BBox::new(
            (item.bbox.x0 - pad).max(0.0),
            (item.bbox.y0 - pad).max(0.0),
            (item.bbox.x1 + pad).min(1.0),
            (item.bbox.y1 + pad).min(1.0),
        );
        if let Some(existing) = merged
            .iter_mut()
            .find(|m| m.bbox.intersection_area(&expanded) > 0.0)
        {
            existing.bbox = BBox::new(
                existing.bbox.x0.min(item.bbox.x0),
                existing.bbox.y0.min(item.bbox.y0),
                existing.bbox.x1.max(item.bbox.x1),
                existing.bbox.y1.max(item.bbox.y1),
            );
            if item.kind == GraphicKind::PhotoOrChart {
                existing.kind = GraphicKind::PhotoOrChart;
            } else if item.kind == GraphicKind::ColorLogo && existing.kind == GraphicKind::Signature
            {
                existing.kind = GraphicKind::ColorLogo;
            }
        } else {
            merged.push(item);
        }
    }
    merged
}

fn crop_transparent_png(img: &RgbImage, bbox: BBox) -> Result<Vec<u8>> {
    let (x0, y0, x1, y1) = bbox.to_pixels(img.width(), img.height());
    let cw = x1.saturating_sub(x0).max(1);
    let ch = y1.saturating_sub(y0).max(1);

    let mut rgba = RgbaImage::new(cw, ch);
    for dy in 0..ch {
        for dx in 0..cw {
            let p = img
                .get_pixel(
                    (x0 + dx).min(img.width() - 1),
                    (y0 + dy).min(img.height() - 1),
                )
                .0;
            let max_c = p[0].max(p[1]).max(p[2]);
            let min_c = p[0].min(p[1]).min(p[2]);
            let is_paper = p[0] > 225 && p[1] > 225 && p[2] > 225 && (max_c - min_c) < 18;
            let alpha = if is_paper { 0 } else { 255 };
            rgba.put_pixel(dx, dy, Rgba([p[0], p[1], p[2], alpha]));
        }
    }

    let mut buf = Cursor::new(Vec::new());
    rgba.write_to(&mut buf, ImageFormat::Png)
        .context("failed to encode transparent graphic PNG")?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use cleanocr_ir::{TextLine, TextSource, TextSpan};
    use image::Rgb;

    use super::*;

    #[test]
    fn detects_color_logo_and_signature_while_excluding_text_lines() {
        let w = 400u32;
        let h = 400u32;
        let mut img = RgbImage::from_pixel(w, h, Rgb([250, 250, 250]));

        // 1. Colored bank logo (top-left, saturated blue/orange).
        for y in 20..65 {
            for x in 20..90 {
                img.put_pixel(x, y, Rgb([20, 90, 210]));
            }
        }

        // 2. Standard text line in the middle (already recognized as OCR Paragraph).
        let text_bbox = BBox::new(0.10, 0.35, 0.80, 0.40);
        for y in 140..160 {
            for x in 40..320 {
                img.put_pixel(x, y, Rgb([20, 20, 20]));
            }
        }
        let text_block = Block {
            id: "p0_ocr000".to_string(),
            bbox: text_bbox,
            reading_order: 0,
            column_index: None,
            confidence: 0.98,
            kind: BlockKind::Paragraph {
                lines: vec![TextLine {
                    bbox: text_bbox,
                    spans: vec![TextSpan {
                        text: "Standard body text line".to_string(),
                        bbox: text_bbox,
                        confidence: 0.98,
                        is_bold: false,
                        is_italic: false,
                        inline_math_latex: None,
                        source: TextSource::Ocr,
                    }],
                }],
            },
        };

        // 3. Tall handwritten signature scribble at bottom-right (height 45px > 1.35 * 20px).
        for x in 240..340 {
            let y = 300 + ((x as f32 * 0.2).sin() * 18.0) as i32;
            for dy in -2..=2 {
                img.put_pixel(x, (y + dy) as u32, Rgb([15, 15, 25]));
            }
        }

        let mut figures = BTreeMap::new();
        let fig_blocks = detect_and_crop_graphics(&img, &[text_block], None, 0, &mut figures)
            .expect("detect graphics");

        assert_eq!(fig_blocks.len(), 2, "expected logo + signature figures");
        assert_eq!(figures.len(), 2);

        // Verify neither detected figure overlaps the standard text line.
        for b in &fig_blocks {
            assert!(matches!(b.kind, BlockKind::Figure { .. }));
            assert_eq!(b.bbox.intersection_area(&text_bbox), 0.0);
        }

        // Verify PNGs have transparent paper alpha (starts with PNG magic).
        for bytes in figures.values() {
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        }
    }
}
