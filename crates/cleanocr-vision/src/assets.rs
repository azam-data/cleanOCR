use std::io::Cursor;

use anyhow::{Context, Result};
use cleanocr_ir::BBox;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::crop_imm;
use image::{ColorType, ImageFormat, RgbImage};

/// Crops a normalized `[0.0, 1.0]` bounding box from `img` and encodes it as PNG bytes.
pub fn crop_bbox_to_png(img: &RgbImage, bbox: BBox) -> Result<Vec<u8>> {
    let (x0, y0, x1, y1) = bbox.to_pixels(img.width(), img.height());
    let w = x1
        .saturating_sub(x0)
        .max(1)
        .min(img.width().saturating_sub(x0));
    let h = y1
        .saturating_sub(y0)
        .max(1)
        .min(img.height().saturating_sub(y0));

    let cropped = crop_imm(img, x0, y0, w, h).to_image();
    let mut buf = Cursor::new(Vec::new());
    cropped
        .write_to(&mut buf, ImageFormat::Png)
        .context("failed to encode cropped figure as PNG")?;
    Ok(buf.into_inner())
}

/// Crops a normalized `[0.0, 1.0]` bounding box from `img` and encodes it as RGB JPEG bytes.
pub fn crop_bbox_to_jpeg(img: &RgbImage, bbox: BBox, quality: u8) -> Result<Vec<u8>> {
    let (x0, y0, x1, y1) = bbox.to_pixels(img.width(), img.height());
    let w = x1
        .saturating_sub(x0)
        .max(1)
        .min(img.width().saturating_sub(x0));
    let h = y1
        .saturating_sub(y0)
        .max(1)
        .min(img.height().saturating_sub(y0));

    let cropped = crop_imm(img, x0, y0, w, h).to_image();
    let mut buf = Cursor::new(Vec::new());
    let mut encoder = JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .encode(
            cropped.as_raw(),
            cropped.width(),
            cropped.height(),
            ColorType::Rgb8.into(),
        )
        .context("failed to encode cropped figure as JPEG")?;
    Ok(buf.into_inner())
}

/// Vectorizes a cropped RGBA/RGB stamp, signature, or logo into a self-contained inline `<svg>`
/// using `vtracer`.
pub fn vectorize_overlay_svg(img: &RgbImage, bbox: BBox) -> Result<String> {
    let (x0, y0, x1, y1) = bbox.to_pixels(img.width(), img.height());
    let w = x1
        .saturating_sub(x0)
        .max(1)
        .min(img.width().saturating_sub(x0));
    let h = y1
        .saturating_sub(y0)
        .max(1)
        .min(img.height().saturating_sub(y0));

    let cropped = crop_imm(img, x0, y0, w, h).to_image();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in cropped.pixels() {
        // Make near-white paper background transparent for clean SVG tracing.
        let is_paper = px[0] > 235 && px[1] > 235 && px[2] > 235;
        rgba.push(px[0]);
        rgba.push(px[1]);
        rgba.push(px[2]);
        rgba.push(if is_paper { 0 } else { 255 });
    }

    let color_image = vtracer::ColorImage {
        pixels: rgba,
        width: w as usize,
        height: h as usize,
    };

    let config = vtracer::Config::default();
    let svg = vtracer::convert(color_image, config)
        .map_err(|e| anyhow::anyhow!("vtracer SVG conversion failed: {e}"))?;
    Ok(svg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn crops_normalized_bbox_to_valid_png_and_jpeg() {
        let img = RgbImage::from_pixel(100, 100, Rgb([200, 120, 80]));
        let bbox = BBox::new(0.1, 0.2, 0.6, 0.7);

        let png = crop_bbox_to_png(&img, bbox).expect("crop PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let jpeg = crop_bbox_to_jpeg(&img, bbox, 80).expect("crop JPEG");
        assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));
    }

    #[test]
    fn vectorizes_colored_stamp_to_svg() {
        let mut img = RgbImage::from_pixel(64, 64, Rgb([250, 250, 250]));
        for y in 16..48 {
            for x in 16..48 {
                img.put_pixel(x, y, Rgb([200, 30, 30]));
            }
        }
        let svg = vectorize_overlay_svg(&img, BBox::new(0.0, 0.0, 1.0, 1.0)).expect("trace SVG");
        assert!(svg.contains("<svg"));
    }
}
