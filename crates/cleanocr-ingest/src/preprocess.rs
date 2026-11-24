use image::{Rgb, RgbImage};
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};

/// Configuration for raster page preprocessing (deskew + illumination normalization).
#[derive(Debug, Clone, Copy)]
pub struct PreprocessConfig {
    /// Maximum absolute skew angle (in degrees) to search (default: 10.0).
    pub max_skew_deg: f32,
    /// Minimum detected skew angle (in degrees) required to trigger rotation (default: 0.35).
    pub min_apply_skew_deg: f32,
    /// Enable uneven-illumination / shadow normalization (default: true).
    pub normalize_illumination: bool,
}

impl Default for PreprocessConfig {
    fn default() -> Self {
        Self {
            max_skew_deg: 10.0,
            min_apply_skew_deg: 0.35,
            normalize_illumination: true,
        }
    }
}

/// Result of preprocessing a raster page.
#[derive(Debug, Clone)]
pub struct PreprocessedPage {
    pub image: RgbImage,
    pub detected_skew_deg: f32,
    pub illumination_normalized: bool,
}

/// Runs deskew and optional illumination normalization on a raster page.
pub fn preprocess_raster(img: &RgbImage, config: &PreprocessConfig) -> PreprocessedPage {
    let skew_deg = estimate_skew_angle_deg(img, config.max_skew_deg);
    let deskewed = if skew_deg.abs() >= config.min_apply_skew_deg {
        rotate_about_center(
            img,
            -skew_deg.to_radians(),
            Interpolation::Bilinear,
            Rgb([255, 255, 255]),
        )
    } else {
        img.clone()
    };

    let final_img = if config.normalize_illumination {
        normalize_illumination(&deskewed)
    } else {
        deskewed
    };

    PreprocessedPage {
        image: final_img,
        detected_skew_deg: skew_deg,
        illumination_normalized: config.normalize_illumination,
    }
}

/// Estimates document skew angle in degrees using horizontal projection-profile variance
/// maximization on a downsampled binary ink map (coarse 1.0° sweep + fine 0.2° refinement).
pub fn estimate_skew_angle_deg(img: &RgbImage, max_angle_deg: f32) -> f32 {
    let (w, h) = img.dimensions();
    if w < 32 || h < 32 || max_angle_deg <= 0.0 {
        return 0.0;
    }

    // Downsample to at most 400px on the longest side for fast, noise-resistant skew estimation.
    let scale = (400.0 / w.max(h) as f32).min(1.0);
    let sw = ((w as f32 * scale).round() as usize).max(16);
    let sh = ((h as f32 * scale).round() as usize).max(16);

    let mut ink_points: Vec<(f32, f32)> = Vec::new();
    let cx = sw as f32 * 0.5;
    let cy = sh as f32 * 0.5;

    for sy in 0..sh {
        let oy = ((sy as f32 / sh as f32) * h as f32).min((h - 1) as f32) as u32;
        for sx in 0..sw {
            let ox = ((sx as f32 / sw as f32) * w as f32).min((w - 1) as f32) as u32;
            let p = img.get_pixel(ox, oy);
            let lum = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
            if lum < 170.0 {
                ink_points.push((sx as f32 - cx, sy as f32 - cy));
            }
        }
    }

    if ink_points.len() < 20 {
        return 0.0;
    }

    let score_angle = |angle_deg: f32| -> f64 {
        let rad = (-angle_deg).to_radians();
        let sin_a = rad.sin();
        let cos_a = rad.cos();
        let mut hist = vec![0u32; sh];

        for &(dx, dy) in &ink_points {
            let ry = (dx * sin_a + dy * cos_a + cy).round() as isize;
            if ry >= 0 && (ry as usize) < sh {
                hist[ry as usize] += 1;
            }
        }

        // Sum of squared row counts (maximizing variance of horizontal projection profile).
        hist.iter().map(|&c| (c as f64) * (c as f64)).sum()
    };

    // 1. Coarse sweep every 1.0 degree.
    let mut best_angle = 0.0f32;
    let mut best_score = score_angle(0.0);
    let steps = max_angle_deg.ceil() as i32;

    for deg in -steps..=steps {
        let angle = (deg as f32).clamp(-max_angle_deg, max_angle_deg);
        let s = score_angle(angle);
        if s > best_score {
            best_score = s;
            best_angle = angle;
        }
    }

    // 2. Fine sweep around best coarse angle in 0.2 degree increments.
    let center = best_angle;
    for step in -5..=5 {
        let angle = (center + step as f32 * 0.2).clamp(-max_angle_deg, max_angle_deg);
        let s = score_angle(angle);
        if s > best_score {
            best_score = s;
            best_angle = angle;
        }
    }

    best_angle
}

/// Flattens uneven lighting and camera shadows by dividing each pixel by a smooth,
/// downsampled local illumination estimate while preserving crisp dark ink.
pub fn normalize_illumination(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    if w < 8 || h < 8 {
        return img.clone();
    }

    // Estimate low-frequency background illumination on a coarse 32x32 grid using the 85th
    // percentile luminance/color per cell so dark text strokes don't pull the background down.
    let gx = 32usize.min(w as usize);
    let gy = 32usize.min(h as usize);
    let mut bg_grid = vec![[245.0f32; 3]; gx * gy];

    for cy in 0..gy {
        let y0 = (cy * h as usize) / gy;
        let y1 = (((cy + 1) * h as usize) / gy).max(y0 + 1).min(h as usize);
        for cx in 0..gx {
            let x0 = (cx * w as usize) / gx;
            let x1 = (((cx + 1) * w as usize) / gx).max(x0 + 1).min(w as usize);

            let mut samples: Vec<(u8, [u8; 3])> = Vec::new();
            let step_y = ((y1 - y0) / 8).max(1);
            let step_x = ((x1 - x0) / 8).max(1);

            for y in (y0..y1).step_by(step_y) {
                for x in (x0..x1).step_by(step_x) {
                    let p = img.get_pixel(x as u32, y as u32).0;
                    let lum = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
                        .round() as u8;
                    samples.push((lum, p));
                }
            }

            if !samples.is_empty() {
                samples.sort_unstable_by_key(|&(lum, _)| lum);
                let idx = ((samples.len() - 1) * 85) / 100;
                let p = samples[idx].1;
                bg_grid[cy * gx + cx] = [
                    (p[0] as f32).max(60.0),
                    (p[1] as f32).max(60.0),
                    (p[2] as f32).max(60.0),
                ];
            }
        }
    }

    let mut out = RgbImage::new(w, h);
    let raw = img.as_raw();
    let out_raw = out.as_mut();

    for y in 0..h as usize {
        let fy = (y as f32 / h as f32) * (gy - 1) as f32;
        let y0 = (fy.floor() as usize).min(gy - 1);
        let y1 = (y0 + 1).min(gy - 1);
        let wy = fy - y0 as f32;

        for x in 0..w as usize {
            let fx = (x as f32 / w as f32) * (gx - 1) as f32;
            let x0 = (fx.floor() as usize).min(gx - 1);
            let x1 = (x0 + 1).min(gx - 1);
            let wx = fx - x0 as f32;

            let c00 = bg_grid[y0 * gx + x0];
            let c10 = bg_grid[y0 * gx + x1];
            let c01 = bg_grid[y1 * gx + x0];
            let c11 = bg_grid[y1 * gx + x1];

            let idx = (y * w as usize + x) * 3;
            for ch in 0..3 {
                let top = c00[ch] * (1.0 - wx) + c10[ch] * wx;
                let bot = c01[ch] * (1.0 - wx) + c11[ch] * wx;
                let bg = (top * (1.0 - wy) + bot * wy).max(60.0);
                let normalized = (raw[idx + ch] as f32 / bg) * 250.0;
                out_raw[idx + ch] = normalized.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_skew_angle_on_synthetic_text_lines() {
        let mut base = RgbImage::from_pixel(300, 300, Rgb([250, 250, 250]));
        for row in 0..10u32 {
            let y = 40 + row * 22;
            for dy in 0..4u32 {
                for x in 35..265u32 {
                    base.put_pixel(x, y + dy, Rgb([20, 20, 20]));
                }
            }
        }

        let skewed = rotate_about_center(
            &base,
            4.0f32.to_radians(),
            Interpolation::Bilinear,
            Rgb([250, 250, 250]),
        );

        let est = estimate_skew_angle_deg(&skewed, 10.0);
        assert!(
            (est - 4.0).abs() <= 0.8,
            "expected detected skew near 4.0 deg, got {est:.2} deg"
        );
    }

    #[test]
    fn illumination_normalization_flattens_shadow_gradient() {
        let mut img = RgbImage::new(160, 160);
        for y in 0..160u32 {
            // Left side dark shadow (140), right side bright paper (245).
            for x in 0..160u32 {
                let bg = (140.0 + (x as f32 / 159.0) * 105.0) as u8;
                img.put_pixel(x, y, Rgb([bg, bg, bg]));
            }
        }

        let norm = normalize_illumination(&img);
        let left = norm.get_pixel(10, 80)[0];
        let right = norm.get_pixel(150, 80)[0];
        assert!(
            left.abs_diff(right) <= 12,
            "expected shadow gradient to flatten, got left={left}, right={right}"
        );
    }
}
