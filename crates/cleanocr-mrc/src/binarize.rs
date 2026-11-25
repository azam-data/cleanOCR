use cleanocr_ir::BBox;
use image::RgbImage;
use rayon::prelude::*;

/// Configuration for MRC foreground/background mask extraction.
#[derive(Debug, Clone, Copy)]
pub struct BinarizeConfig {
    /// Odd Sauvola local window size in pixels (default: 31).
    pub window_size: usize,
    /// Sauvola sensitivity parameter `k` (default: 0.25).
    pub sauvola_k: f64,
    /// Sauvola dynamic range `R` (default: 128.0).
    pub sauvola_r: f64,
    /// RGB Euclidean distance from estimated paper-white to capture colored ink/stamps.
    pub color_ink_threshold: f32,
    /// Inward padding in pixels applied to figure boxes so borders stay clean.
    pub photo_inset_px: u32,
}

impl Default for BinarizeConfig {
    fn default() -> Self {
        Self {
            window_size: 31,
            sauvola_k: 0.25,
            sauvola_r: 128.0,
            color_ink_threshold: 28.0,
            photo_inset_px: 2,
        }
    }
}

/// Rasterizes normalized figure bounding boxes (inset by `inset_px`) into a boolean photo mask.
/// Pixels where `photo_mask[i] == true` are protected from being binarized into the 1-bit ink mask.
pub fn build_photo_mask(
    width: u32,
    height: u32,
    figure_boxes: &[BBox],
    inset_px: u32,
) -> Vec<bool> {
    let w = width as usize;
    let h = height as usize;
    let mut mask = vec![false; w * h];

    for bbox in figure_boxes {
        let (x0, y0, x1, y1) = bbox.to_pixels(width, height);
        let ix0 = x0.saturating_add(inset_px).min(width);
        let iy0 = y0.saturating_add(inset_px).min(height);
        let ix1 = x1.saturating_sub(inset_px).min(width);
        let iy1 = y1.saturating_sub(inset_px).min(height);

        if ix0 >= ix1 || iy0 >= iy1 {
            continue;
        }

        for y in iy0..iy1 {
            let row_offset = (y as usize) * w;
            for x in ix0..ix1 {
                mask[row_offset + x as usize] = true;
            }
        }
    }

    mask
}

/// Extracts a 1-bit foreground ink mask (`true` = ink, `false` = background) using:
/// 1. O(1) per-pixel 64-bit integral-image Sauvola thresholding on luminance.
/// 2. Color-distance ink capture (for colored headers, red/blue stamps, and signatures).
/// 3. Photo protection mask (`photo_mask[i] == true` forces `mask[i] = false`).
/// 4. Isolated 1–2 px dust despeckling.
pub fn extract_ink_mask(
    img: &RgbImage,
    figure_boxes: &[BBox],
    config: &BinarizeConfig,
) -> Vec<bool> {
    let width = img.width();
    let height = img.height();
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return Vec::new();
    }

    let photo_mask = build_photo_mask(width, height, figure_boxes, config.photo_inset_px);
    let raw = img.as_raw();

    // 1. Compute luminance buffer and estimate coarse paper-white RGB from bright pixels.
    let mut luma = vec![0u8; w * h];
    let mut sum_r: u64 = 0;
    let mut sum_g: u64 = 0;
    let mut sum_b: u64 = 0;
    let mut bright_count: u64 = 0;

    for i in 0..(w * h) {
        let r = raw[i * 3] as f32;
        let g = raw[i * 3 + 1] as f32;
        let b = raw[i * 3 + 2] as f32;
        let y = (0.299 * r + 0.587 * g + 0.114 * b)
            .round()
            .clamp(0.0, 255.0) as u8;
        luma[i] = y;
        if y >= 200 && !photo_mask[i] {
            sum_r += raw[i * 3] as u64;
            sum_g += raw[i * 3 + 1] as u64;
            sum_b += raw[i * 3 + 2] as u64;
            bright_count += 1;
        }
    }

    let (paper_r, paper_g, paper_b) = match (
        sum_r.checked_div(bright_count),
        sum_g.checked_div(bright_count),
        sum_b.checked_div(bright_count),
    ) {
        (Some(r), Some(g), Some(b)) => (r as f32, g as f32, b as f32),
        _ => (250.0, 250.0, 250.0),
    };

    // 2. Build (w + 1) x (h + 1) 64-bit summed-area tables for O(1) Sauvola mean & variance.
    let stride = w + 1;
    let mut integral = vec![0u64; stride * (h + 1)];
    let mut integral_sq = vec![0u64; stride * (h + 1)];

    for y in 0..h {
        let mut row_sum: u64 = 0;
        let mut row_sq_sum: u64 = 0;
        let src_row = y * w;
        let cur_row = (y + 1) * stride;
        let prev_row = y * stride;

        for x in 0..w {
            let v = luma[src_row + x] as u64;
            row_sum += v;
            row_sq_sum += v * v;
            integral[cur_row + x + 1] = integral[prev_row + x + 1] + row_sum;
            integral_sq[cur_row + x + 1] = integral_sq[prev_row + x + 1] + row_sq_sum;
        }
    }

    // 3. Parallel per-row Sauvola + colored-ink classification.
    let half_w = (config.window_size / 2).max(1);
    let k = config.sauvola_k;
    let r_dyn = config.sauvola_r.max(1.0);
    let color_thresh_sq = config.color_ink_threshold * config.color_ink_threshold;

    let mut mask = vec![false; w * h];
    mask.par_chunks_mut(w).enumerate().for_each(|(y, row_out)| {
        let y0 = y.saturating_sub(half_w);
        let y1 = (y + half_w).min(h - 1);
        let row_top = y0 * stride;
        let row_bot = (y1 + 1) * stride;

        for (x, out_px) in row_out.iter_mut().enumerate().take(w) {
            let idx = y * w + x;
            if photo_mask[idx] {
                *out_px = false;
                continue;
            }

            let x0 = x.saturating_sub(half_w);
            let x1 = (x + half_w).min(w - 1);
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64;

            let sum = (integral[row_bot + x1 + 1] + integral[row_top + x0]
                - integral[row_bot + x0]
                - integral[row_top + x1 + 1]) as f64;
            let sum_sq = (integral_sq[row_bot + x1 + 1] + integral_sq[row_top + x0]
                - integral_sq[row_bot + x0]
                - integral_sq[row_top + x1 + 1]) as f64;

            let mean = sum / area;
            let var = (sum_sq / area - mean * mean).max(0.0);
            let std_dev = var.sqrt();
            let threshold = mean * (1.0 + k * (std_dev / r_dyn - 1.0));

            let lum = luma[idx] as f64;
            let is_sauvola_ink = lum < threshold && (mean - lum) > 8.0;

            let dr = raw[idx * 3] as f32 - paper_r;
            let dg = raw[idx * 3 + 1] as f32 - paper_g;
            let db = raw[idx * 3 + 2] as f32 - paper_b;
            let max_ch = raw[idx * 3].max(raw[idx * 3 + 1]).max(raw[idx * 3 + 2]);
            let min_ch = raw[idx * 3].min(raw[idx * 3 + 1]).min(raw[idx * 3 + 2]);
            let chroma = (max_ch - min_ch) as f32;
            let dist_sq = dr * dr + dg * dg + db * db;
            let is_colored_ink = chroma > 24.0 && dist_sq > color_thresh_sq;

            *out_px = is_sauvola_ink || is_colored_ink;
        }
    });

    // 4. Despeckle isolated 1-2 px dust noise.
    despeckle_mask(&mut mask, w, h);
    mask
}

/// Removes isolated 1-2 pixel foreground specks with fewer than 2 ink neighbors in their 3x3 window.
fn despeckle_mask(mask: &mut [bool], w: usize, h: usize) {
    if w < 3 || h < 3 {
        return;
    }
    let snapshot = mask.to_vec();
    for y in 1..(h - 1) {
        let row = y * w;
        for x in 1..(w - 1) {
            if !snapshot[row + x] {
                continue;
            }
            let mut neighbors = 0u8;
            for dy in -1isize..=1 {
                let nr = ((y as isize + dy) as usize) * w;
                for dx in -1isize..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    if snapshot[nr + (x as isize + dx) as usize] {
                        neighbors += 1;
                    }
                }
            }
            if neighbors < 2 {
                mask[row + x] = false;
            }
        }
    }
}

/// Packs a boolean ink mask (`true` = ink = `1`, `false` = background = `0`) into MSB-first,
/// byte-aligned rows as required by `jbig2enc-rust` and 1-bit PNG encoders.
pub fn pack_mask_msb_first(mask: &[bool], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let row_bytes = w.div_ceil(8);
    let mut packed = vec![0u8; row_bytes * h];

    for y in 0..h {
        let src_row = y * w;
        let dst_row = y * row_bytes;
        for x in 0..w {
            if mask[src_row + x] {
                packed[dst_row + (x / 8)] |= 1 << (7 - (x % 8));
            }
        }
    }

    packed
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn photo_mask_protects_figure_region_from_binarization() {
        let mut img = RgbImage::from_pixel(100, 100, Rgb([252, 252, 250]));

        // Draw dark text bar in top area (outside figure box).
        for y in 10..20 {
            for x in 10..40 {
                img.put_pixel(x, y, Rgb([15, 15, 15]));
            }
        }

        // Draw dark photo block inside figure box [0.5..0.9, 0.5..0.9].
        for y in 55..85 {
            for x in 55..85 {
                img.put_pixel(x, y, Rgb([20, 20, 20]));
            }
        }

        let fig = BBox::new(0.5, 0.5, 0.9, 0.9);
        let mask = extract_ink_mask(&img, &[fig], &BinarizeConfig::default());

        assert!(mask[15 * 100 + 25], "text pixel outside photo must be ink");
        assert!(
            !mask[70 * 100 + 70],
            "dark pixel inside photo box must stay in background"
        );
    }

    #[test]
    fn captures_colored_stamp_and_despeckles_single_pixel_dust() {
        let mut img = RgbImage::from_pixel(80, 80, Rgb([250, 250, 250]));

        // Red rubber stamp patch (high chroma, moderate luminance).
        for y in 20..30 {
            for x in 20..35 {
                img.put_pixel(x, y, Rgb([210, 40, 40]));
            }
        }

        // Single-pixel dust speck.
        img.put_pixel(60, 60, Rgb([0, 0, 0]));

        let mask = extract_ink_mask(&img, &[], &BinarizeConfig::default());
        assert!(mask[25 * 80 + 25], "colored stamp must be captured as ink");
        assert!(!mask[60 * 80 + 60], "isolated 1px dust must be despeckled");
    }

    #[test]
    fn packs_bits_msb_first_byte_aligned_per_row() {
        let mask = vec![
            true, false, false, false, false, false, false, true, true, // row 0 (9 px)
            false, true, false, false, false, false, false, false, false, // row 1 (9 px)
        ];
        let packed = pack_mask_msb_first(&mask, 9, 2);
        assert_eq!(
            packed,
            vec![0b1000_0001, 0b1000_0000, 0b0100_0000, 0b0000_0000]
        );
    }
}
