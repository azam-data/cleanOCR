use std::sync::Arc;

use anyhow::{Result, anyhow};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use image::RgbImage;

/// Lazy, page-by-page `hayro` PDF rasterizer so 100+ page PDFs never hold all
/// uncompressed `RgbImage`s in RAM simultaneously.
pub struct PdfPageStream {
    pdf: Pdf,
    interpreter_settings: InterpreterSettings,
    render_settings: RenderSettings,
    pixmap_settings: PixmapSettings,
    next_idx: usize,
    total_pages: usize,
}

impl PdfPageStream {
    pub fn new(pdf_bytes: Vec<u8>, dpi: u32) -> Result<Self> {
        let data = Arc::new(pdf_bytes);
        let pdf = Pdf::new(data).map_err(|e| anyhow!("hayro failed to parse PDF: {e:?}"))?;
        let total_pages = pdf.pages().len();

        let scale = (dpi.max(72) as f32) / 72.0;
        let pixmap_settings = PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            ..PixmapSettings::default()
        };

        Ok(Self {
            pdf,
            interpreter_settings: InterpreterSettings::default(),
            render_settings: RenderSettings::default(),
            pixmap_settings,
            next_idx: 0,
            total_pages,
        })
    }

    pub fn len(&self) -> usize {
        self.total_pages
    }

    pub fn is_empty(&self) -> bool {
        self.total_pages == 0
    }
}

impl Iterator for PdfPageStream {
    type Item = Result<RgbImage>;

    fn next(&mut self) -> Option<Self::Item> {
        let page = self.pdf.pages().get(self.next_idx)?;
        self.next_idx += 1;

        let cache = RenderCache::new();
        let pixmap = render(
            page,
            &cache,
            &self.interpreter_settings,
            &self.render_settings,
            &self.pixmap_settings,
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

        Some(
            RgbImage::from_raw(w, h, rgb)
                .ok_or_else(|| anyhow!("failed to construct RgbImage from hayro pixmap")),
        )
    }
}

/// Renders all pages of a PDF byte buffer into `RgbImage`s at the requested `dpi` (e.g. 300)
/// using the pure-Rust `hayro` CPU rasterizer.
pub fn render_pdf_pages(pdf_bytes: Vec<u8>, dpi: u32) -> Result<Vec<RgbImage>> {
    PdfPageStream::new(pdf_bytes, dpi)?.collect()
}
