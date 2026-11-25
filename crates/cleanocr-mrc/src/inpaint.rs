use image::RgbImage;

/// Reconstructs a halo-free full-resolution layer by filling all unknown pixels (`!known[i]`)
/// from known pixels (`known[i]`) using a multi-scale mipmap Pull-Push pyramid followed by
/// 3 passes of 3x3 masked box blur.
///
/// - For **Background (`BG`)**: pass `known[i] = !ink_mask[i]` so text holes are filled with
///   surrounding paper color before downsampling.
/// - For **Foreground (`FG`)**: pass `known[i] = ink_mask[i]` so ink colors dilate smoothly
///   into background areas and prevent white halos around text edges.
pub fn pull_push_inpaint(img: &RgbImage, known: &[bool]) -> RgbImage {
    let w = img.width() as usize;
    let h = img.height() as usize;
    if w == 0 || h == 0 {
        return img.clone();
    }

    let raw = img.as_raw();
    let mut base_rgb = vec![[0.0f32; 3]; w * h];
    let mut base_w = vec![0.0f32; w * h];

    let mut any_known = false;
    for i in 0..(w * h) {
        if known[i] {
            any_known = true;
            base_rgb[i] = [
                raw[i * 3] as f32,
                raw[i * 3 + 1] as f32,
                raw[i * 3 + 2] as f32,
            ];
            base_w[i] = 1.0;
        }
    }

    if !any_known {
        return img.clone();
    }

    // 1. Pull phase: build downsampled pyramid until 1x1 so every hole gets a valid ancestor.
    type PyramidLevel = (usize, usize, Vec<[f32; 3]>, Vec<f32>);
    let mut pyramid: Vec<PyramidLevel> = Vec::new();
    pyramid.push((w, h, base_rgb, base_w));

    loop {
        let (cw, ch, ref crgb, ref cwts) = *pyramid.last().expect("pyramid non-empty");
        if cw == 1 && ch == 1 {
            break;
        }
        let nw = cw.div_ceil(2);
        let nh = ch.div_ceil(2);
        let mut nrgb = vec![[0.0f32; 3]; nw * nh];
        let mut nwts = vec![0.0f32; nw * nh];

        for ny in 0..nh {
            let sy0 = ny * 2;
            let sy1 = (sy0 + 1).min(ch - 1);
            for nx in 0..nw {
                let sx0 = nx * 2;
                let sx1 = (sx0 + 1).min(cw - 1);

                let mut acc = [0.0f32; 3];
                let mut wsum = 0.0f32;

                let coords = [(sx0, sy0), (sx1, sy0), (sx0, sy1), (sx1, sy1)];
                for &(sx, sy) in &coords {
                    let idx = sy * cw + sx;
                    let wt = cwts[idx];
                    if wt > 0.0 {
                        acc[0] += crgb[idx][0] * wt;
                        acc[1] += crgb[idx][1] * wt;
                        acc[2] += crgb[idx][2] * wt;
                        wsum += wt;
                    }
                }

                let nidx = ny * nw + nx;
                if wsum > 0.0 {
                    nrgb[nidx] = [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum];
                    nwts[nidx] = 1.0;
                }
            }
        }

        pyramid.push((nw, nh, nrgb, nwts));
    }

    // 2. Push phase: propagate coarse colors back down into unknown pixels.
    for level in (0..(pyramid.len() - 1)).rev() {
        let (pw, ph, parent_rgb) = {
            let (pw, ph, ref prgb, _) = pyramid[level + 1];
            (pw, ph, prgb.clone())
        };
        let (cw, ch, ref mut crgb, ref mut cwts) = pyramid[level];

        for y in 0..ch {
            let py = (y / 2).min(ph - 1);
            for x in 0..cw {
                let idx = y * cw + x;
                if cwts[idx] == 0.0 {
                    let px = (x / 2).min(pw - 1);
                    crgb[idx] = parent_rgb[py * pw + px];
                    cwts[idx] = 1.0;
                }
            }
        }
    }

    // 3. Smooth unknown regions with 3 passes of 3x3 masked box blur (leaving known pixels intact).
    let (_, _, mut filled, _) = pyramid.swap_remove(0);
    let mut scratch = filled.clone();

    for _ in 0..3 {
        for y in 0..h {
            let y0 = y.saturating_sub(1);
            let y1 = (y + 1).min(h - 1);
            for x in 0..w {
                let idx = y * w + x;
                if known[idx] {
                    scratch[idx] = filled[idx];
                    continue;
                }
                let x0 = x.saturating_sub(1);
                let x1 = (x + 1).min(w - 1);
                let mut acc = [0.0f32; 3];
                let mut count = 0.0f32;
                for ny in y0..=y1 {
                    let row = ny * w;
                    for nx in x0..=x1 {
                        let c = filled[row + nx];
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        count += 1.0;
                    }
                }
                scratch[idx] = [acc[0] / count, acc[1] / count, acc[2] / count];
            }
        }
        std::mem::swap(&mut filled, &mut scratch);
    }

    let mut out = vec![0u8; w * h * 3];
    for i in 0..(w * h) {
        out[i * 3] = filled[i][0].round().clamp(0.0, 255.0) as u8;
        out[i * 3 + 1] = filled[i][1].round().clamp(0.0, 255.0) as u8;
        out[i * 3 + 2] = filled[i][2].round().clamp(0.0, 255.0) as u8;
    }

    RgbImage::from_raw(img.width(), img.height(), out).expect("buffer matches image dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn fills_black_text_hole_with_paper_color_without_dark_halo() {
        let mut img = RgbImage::from_pixel(32, 32, Rgb([245, 240, 230]));
        let mut known = vec![true; 32 * 32];

        // Stamp a black 6x6 text glyph in the center and mark it unknown for BG inpainting.
        for y in 13..19 {
            for x in 13..19 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
                known[(y * 32 + x) as usize] = false;
            }
        }

        let bg = pull_push_inpaint(&img, &known);
        let center = bg.get_pixel(16, 16);
        assert!(center[0] >= 240 && center[1] >= 235 && center[2] >= 225);
    }
}
