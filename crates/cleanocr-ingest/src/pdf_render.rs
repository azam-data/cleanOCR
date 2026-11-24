use std::sync::Arc;

use anyhow::{Result, anyhow};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use image::RgbImage;

/// Renders all pages of a PDF byte buffer into `RgbImage`s at the requested `dpi` (e.g. 300)
/// using the pure-Rust `hayro` CPU rasterizer.
pub fn render_pdf_pages(pdf_bytes: Vec<u8>, dpi: u32) -> Result<Vec<RgbImage>> {
    let data = Arc::new(pdf_bytes);
    let pdf = Pdf::new(data).map_err(|e| anyhow!("hayro failed to parse PDF: {e:?}"))?;

    let scale = (dpi.max(72) as f32) / 72.0;
    let interpreter_settings = InterpreterSettings::default();
    let render_settings = RenderSettings::default();
    let pixmap_settings = PixmapSettings {
        x_scale: scale,
        y_scale: scale,
        ..PixmapSettings::default()
    };

    let cache = RenderCache::new();
    let mut images = Vec::with_capacity(pdf.pages().len());

    for page in pdf.pages().iter() {
        let pixmap = render(
            page,
            &cache,
            &interpreter_settings,
            &render_settings,
            &pixmap_settings,
        );
        let w = pixmap.width() as u32;
        let h = pixmap.height() as u32;
        let rgba = pixmap.data_as_u8_slice();

        // Composite premultiplied RGBA over opaque white paper.
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for px in rgba.chunks_exact(4) {
            let a = px[3] as u16;
            let inv_a = 255u16.saturating_sub(a);
            let r = (px[0] as u16 + inv_a).min(255) as u8;
            let g = (px[1] as u16 + inv_a).min(255) as u8;
            let b = (px[2] as u16 + inv_a).min(255) as u8;
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }

        let img = RgbImage::from_raw(w, h, rgb)
            .ok_or_else(|| anyhow!("failed to construct RgbImage from hayro pixmap"))?;
        images.push(img);
    }

    Ok(images)
}
