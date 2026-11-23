//! Mixed Raster Content (MRC) 3-layer decomposition (Mask + FG + BG).

/// Compressed 3-layer Mixed Raster Content representation of a single page.
///
/// - `mask_*`: Full-resolution 1-bit stencil for sharp text, thin lines, and stamps.
/// - `fg_*`: Downsampled RGB ink color map punched out by the mask.
/// - `bg_*`: Downsampled RGB background holding paper tone and photographs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrcLayers {
    pub width: u32,
    pub height: u32,
    /// Full-resolution 1-bit lossless JBIG2 stream for PDF `/Mask` XObjects.
    pub mask_jbig2: Vec<u8>,
    /// Full-resolution 1-bit PNG for HTML CSS `mask-image`.
    pub mask_png_1bit: Vec<u8>,
    /// Downsampled RGB JPEG foreground ink layer.
    pub fg_jpeg: Vec<u8>,
    pub fg_width: u32,
    pub fg_height: u32,
    /// Downsampled RGB JPEG background layer with text inpainted out.
    pub bg_jpeg: Vec<u8>,
    pub bg_width: u32,
    pub bg_height: u32,
}
