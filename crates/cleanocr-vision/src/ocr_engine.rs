use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cleanocr_ir::{BBox, Block, BlockKind, TextLine, TextSource, TextSpan};
use image::RgbImage;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};

/// Checks `./models` next to the current executable (and in the current working directory),
/// returning `Some(dir)` if valid ONNX OCR model files are discovered inside it.
pub fn discover_default_models_dir() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let candidate = exe_dir.join("models");
        if OcrModelPaths::discover_in_dir(&candidate).is_ok() {
            return Some(candidate);
        }
    }

    let cwd_candidate = PathBuf::from("models");
    if OcrModelPaths::discover_in_dir(&cwd_candidate).is_ok() {
        return Some(cwd_candidate);
    }

    None
}

/// Resolved local file paths for ONNX text detection, text recognition, and character dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrModelPaths {
    pub det_model: PathBuf,
    pub rec_model: PathBuf,
    pub dict_path: PathBuf,
}

impl OcrModelPaths {
    /// Resolves ONNX model and dictionary files inside `dir`.
    ///
    /// Accepts either canonical names (`det.onnx`, `rec.onnx`, `dict.txt`) or standard
    /// PaddleOCR / PP-OCRv5 / PP-OCRv6 filenames (e.g. `pp-ocrv5_mobile_det.onnx`).
    pub fn discover_in_dir(dir: &Path) -> Result<Self> {
        if !dir.is_dir() {
            bail!("OCR model directory does not exist: {}", dir.display());
        }

        let det_model = find_first_matching(
            dir,
            &[
                "det.onnx",
                "pp-ocrv6_small_det.onnx",
                "pp-ocrv6_tiny_det.onnx",
                "pp-ocrv5_mobile_det.onnx",
                "pp-ocrv5_server_det.onnx",
                "text_detection.onnx",
            ],
            "det",
            "onnx",
        )
        .with_context(|| format!("missing text detection ONNX model in {}", dir.display()))?;

        let rec_model = find_first_matching(
            dir,
            &[
                "rec.onnx",
                "pp-ocrv6_small_rec.onnx",
                "pp-ocrv6_tiny_rec.onnx",
                "pp-ocrv5_mobile_rec.onnx",
                "pp-ocrv5_server_rec.onnx",
                "text_recognition.onnx",
            ],
            "rec",
            "onnx",
        )
        .with_context(|| format!("missing text recognition ONNX model in {}", dir.display()))?;

        let dict_path = find_first_matching(
            dir,
            &[
                "dict.txt",
                "ppocrv6_dict.txt",
                "ppocrv6_tiny_dict.txt",
                "ppocrv5_dict.txt",
                "character_dict.txt",
            ],
            "dict",
            "txt",
        )
        .with_context(|| {
            format!(
                "missing character dictionary .txt file in {}",
                dir.display()
            )
        })?;

        Ok(Self {
            det_model,
            rec_model,
            dict_path,
        })
    }
}

/// Abstraction over OCR inference so `pipeline` can be tested offline with a mock recognizer.
pub trait TextRecognizer {
    fn recognize_page(&self, img: &RgbImage, page_index: usize) -> Result<Vec<Block>>;
    fn recognize_crop(&self, crop: &RgbImage) -> Result<(String, f32)>;
}

/// ONNX-backed OCR engine wrapping `oar-ocr`'s `OAROCR` pipeline.
pub struct OcrEngine {
    inner: OAROCR,
}

impl OcrEngine {
    /// Loads ONNX detection + recognition models from `paths`.
    pub fn from_paths(paths: &OcrModelPaths) -> Result<Self> {
        let inner = OAROCRBuilder::new(&paths.det_model, &paths.rec_model, &paths.dict_path)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to initialize oar-ocr pipeline: {e}"))?;

        Ok(Self { inner })
    }

    /// Discovers models inside `model_dir` and initializes the ONNX OCR engine.
    pub fn from_model_dir(model_dir: &Path) -> Result<Self> {
        let paths = OcrModelPaths::discover_in_dir(model_dir)?;
        Self::from_paths(&paths)
    }

    /// Runs ONNX text detection + recognition on `img` and converts detected text regions
    /// into normalized `[0.0, 1.0]` `Block`s.
    pub fn recognize_page(&self, img: &RgbImage, page_index: usize) -> Result<Vec<Block>> {
        <Self as TextRecognizer>::recognize_page(self, img, page_index)
    }

    /// Runs ONNX text recognition on a cropped region (such as a padded table cell).
    pub fn recognize_crop(&self, crop: &RgbImage) -> Result<(String, f32)> {
        <Self as TextRecognizer>::recognize_crop(self, crop)
    }
}

impl TextRecognizer for OcrEngine {
    fn recognize_page(&self, img: &RgbImage, page_index: usize) -> Result<Vec<Block>> {
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }

        let results = self
            .inner
            .predict(vec![img.clone()])
            .map_err(|e| anyhow::anyhow!("oar-ocr inference failed on page {page_index}: {e}"))?;

        let Some(page_result) = results.into_iter().next() else {
            return Ok(Vec::new());
        };

        let mut blocks = Vec::with_capacity(page_result.text_regions.len());
        for (idx, region) in page_result.text_regions.iter().enumerate() {
            let Some((text, confidence)) = region.text_with_confidence() else {
                continue;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }

            let bbox = BBox::new(
                (region.bounding_box.x_min() / w as f32).clamp(0.0, 1.0),
                (region.bounding_box.y_min() / h as f32).clamp(0.0, 1.0),
                (region.bounding_box.x_max() / w as f32).clamp(0.0, 1.0),
                (region.bounding_box.y_max() / h as f32).clamp(0.0, 1.0),
            );

            let span = TextSpan {
                text: trimmed.to_string(),
                bbox,
                confidence,
                is_bold: false,
                is_italic: false,
                inline_math_latex: None,
                source: TextSource::Ocr,
            };

            let line = TextLine {
                bbox,
                spans: vec![span],
            };

            blocks.push(Block {
                id: format!("p{page_index}_ocr{idx:03}"),
                bbox,
                reading_order: idx,
                column_index: None,
                confidence,
                kind: BlockKind::Paragraph { lines: vec![line] },
            });
        }

        Ok(blocks)
    }

    fn recognize_crop(&self, crop: &RgbImage) -> Result<(String, f32)> {
        let (w, h) = crop.dimensions();
        if w == 0 || h == 0 {
            return Ok((String::new(), 1.0));
        }

        let results = self
            .inner
            .predict(vec![crop.clone()])
            .map_err(|e| anyhow::anyhow!("oar-ocr crop inference failed: {e}"))?;

        let Some(page_result) = results.into_iter().next() else {
            return Ok((String::new(), 1.0));
        };

        let mut parts = Vec::new();
        let mut conf_sum = 0.0f32;
        let mut count = 0usize;

        for region in &page_result.text_regions {
            if let Some((text, conf)) = region.text_with_confidence() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                    conf_sum += conf;
                    count += 1;
                }
            }
        }

        if count == 0 {
            Ok((String::new(), 1.0))
        } else {
            Ok((parts.join(" "), conf_sum / count as f32))
        }
    }
}

fn find_first_matching(
    dir: &Path,
    preferred: &[&str],
    keyword: &str,
    ext: &str,
) -> Option<PathBuf> {
    for name in preferred {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let has_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext));
        let has_kw = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().contains(keyword));
        if has_ext && has_kw {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn discovers_standard_and_ppocr_model_filenames_in_directory() {
        let dir =
            std::env::temp_dir().join(format!("cleanocr_model_discovery_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp model dir");

        fs::write(dir.join("pp-ocrv6_small_det.onnx"), b"det").unwrap();
        fs::write(dir.join("pp-ocrv6_small_rec.onnx"), b"rec").unwrap();
        fs::write(dir.join("ppocrv6_dict.txt"), b"a\nb\n").unwrap();

        let paths = OcrModelPaths::discover_in_dir(&dir).expect("discover models");
        assert!(paths.det_model.ends_with("pp-ocrv6_small_det.onnx"));
        assert!(paths.rec_model.ends_with("pp-ocrv6_small_rec.onnx"));
        assert!(paths.dict_path.ends_with("ppocrv6_dict.txt"));

        let _ = fs::remove_dir_all(&dir);
    }
}
