# cleanOCR

A Rust-native smart OCR, Mixed Raster Content (MRC) compression, and semantic document reconstruction engine.

## Core Architecture: The 3-Layer Semantic Sandwich
`cleanOCR` ingests scanned images, camera photos, and existing PDFs (both scanned and digital-born) and compiles them into a single **Canonical Document IR (`DocumentIr`)** paired with three tightly aligned layers:
1. **Visual Ground Truth Layer (MRC or Full Raster):** Preserves 100% visual fidelity of fonts, wet signatures, rubber stamps, and paper texture while shrinking scanned pages 5x–15x using Mixed Raster Content (`BG` + `FG` + lossless 1-bit JBIG2 `Mask`).
2. **Invisible Pixel-Aligned Semantic Text Layer:** Stores headings, multi-column paragraphs, tables, lists, and validated LaTeX math (display + inline) anchored to exact bounding boxes on the visual layer.
3. **Isolated Figure & Asset Layer:** Stores cropped high-clarity figures, charts, logos, vector-traced seals/signatures, and structured `ChartIr` data linked to captions.

## Supported Outputs
- **Hybrid Layered PDF/A-3 (`.pdf`)** with toggleable Optional Content Groups (`Background`, `Foreground_Ink`, `Figures_Overlays`), pixel-aligned invisible Unicode text, and embedded `cleanocr-ir.json`.
- **Self-Contained Layered HTML5 (`.html`)** with a **Replica Mode** vs. **Reader Mode** toggle, layer inspector, and embedded JSON IR.
- **Semantic LaTeX Bundle (`.tex` + `assets/`)** targeting LuaLaTeX/XeLaTeX with strict text escaping, syntax-validated math + image fallbacks, and deterministic `pgfplots` emission.
- **GitHub-Flavored Markdown (`.md` + `assets/`)**
- **Microsoft Word (`.docx`)**

## Project Documentation
- [`BLUEPRINT.md`](./BLUEPRINT.md) — Full architectural specification, IR schema, crate designs, and Milestone M1–M6 roadmap.
- [`AGENTS.md`](./AGENTS.md) — Engineering invariants, build/test commands, and guardrails for AI coding agents.