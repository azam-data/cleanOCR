# cleanOCR v2 Engineering Blueprint (Agent Execution Spec)

> **Target Agent:** Antigravity  
> **Repository:** `azam-data/cleanOCR` (Rust Workspace, Edition 2024, MSRV 1.85)  
> **Methodology:** **Elephant-Fish Approach**  
> **License Guardrail:** **100% Apache-2.0 / MIT only.** Do **not** add GPL, AGPL, or revenue-capped OpenRAIL-M model weights (e.g., Datalab Chandra/Indra/Surya) to the default pipeline.  
> **Hardware Guardrail:** Must run smoothly on an **8 GB RAM, CPU-only laptop** as a **single portable binary** (zero admin rights, zero Python/C++ external runtimes).

---

## 0. The Elephant-Fish Execution Protocol

To prevent sprawling, broken refactors across 8 crates, execute this blueprint using the **Elephant-Fish Approach**:

1. **The Elephant (Macro Vertical Slicing):**  
   Never implement all 4 phases in one giant diff. Slice the "elephant" into **10 thin, end-to-end vertical slices** (Slices `S1` through `S10`). Every slice must compile cleanly, keep all 8 crates green, and deliver measurable value on its own.
2. **The Fish (Fish-Level Atomic Specs + Sensitive Test Probes):**  
   - **Fish-Level Granularity:** Each slice specifies exact target crates, modules, data structures, and algorithmic invariants—leaving zero ambiguity.
   - **Elephant-Fish Quality Probes:** Like the *Mormyridae* (elephant fish) biosensor that detects minute fluctuations in water quality, every slice **must** add targeted unit/integration test probes that immediately catch regressions (e.g., dropped digits like `10.00% -> 1.00%`, broken table columns, layer bleed, or RAM bloat).
3. **Mandatory Gate After Every Slice:**  
   Do not proceed to slice $N+1$ until slice $N$ passes:
   ```bash
   cargo fmt --all
   cargo test --workspace
   cargo clippy --workspace --all-targets
   ```

---

## 1. Current Workspace Baseline (All 8 Crates Green)

| Crate | Current Responsibility | Known Gaps to Resolve in v2 |
|---|---|---|
| `cleanocr-ir` | `DocumentIr`, `BBox`, Recursive XY-Cut reading order | XY-Cut slices tables vertically like newspaper columns; unused `serde_json` & `thiserror` warnings. |
| `cleanocr-ingest` | Deskew, illumination norm, `lopdf` router, `hayro` PDF render | Loads all pages into RAM up front instead of streaming page-by-page. |
| `cleanocr-vision` | `oar-ocr` ONNX OCR, `vtracer` SVG, ink-band fallback | No bordered/borderless table detector; table borders corrupt OCR digits (`10.00%` $\rightarrow$ `1.00%`/`0.00%`); `figure_boxes` always empty (logos & signatures unprotected). |
| `cleanocr-mrc` | Sauvola binarization, photo-mask, pull-push inpainting, lossless JBIG2 | Merges text & table rules into 1 mask; shaded headers/abstract backgrounds bleed into 1-bit mask; no high-photo-density bypass; unused `zune-jpeg` warning. |
| `cleanocr-export` | `PDF/A-3`, `HTML5`, `LaTeX`, `Markdown`, `DOCX`, `convert-ir` | PDF only writes 3 MRC layers (no separate 1-bit Table Grid or Color Graphics OCG); no Excel (`.xlsx`) exporter. |
| `cleanocr-scan` | Driverless eSCL mDNS discovery + HTTP scan client | Complete (no changes required). |
| `cleanocr-cli` | `process`, `convert-ir`, `scan`, `doctor` | Needs `xlsx` format flag + streaming pipeline orchestration. |
| `cleanocr-gui` | Portable `egui`/`eframe` desktop app + `rfd` dialogs | Needs Excel checkbox, auto-detect `./models` next to exe, Copy Markdown & Open Output Folder buttons. |

---

## 2. Phase 1 — Hybrid Table Engine & Financial OCR Accuracy

**Goal:** Turn dense financial circulars (bordered and borderless tables) into accurate `BlockKind::Table` structures with zero dropped digits (`10.00%` stays `10.00%`).

### Slice S1: Workspace Hygiene & Regression Test Fixture
- **Target Crates:** `cleanocr-ir`, `cleanocr-mrc`
- **Fish-Level Tasks:**
  1. Remove unused `thiserror` from `crates/cleanocr-ir/Cargo.toml`, move `serde_json` to `[dev-dependencies]` if only used in tests, and remove unused `zune-jpeg` from `crates/cleanocr-mrc/Cargo.toml`.
  2. Verify zero manifest warnings across the workspace.
- **Elephant-Fish Probe:** `cargo test --workspace` compiles with 0 unused-dependency warnings.

### Slice S2: Morphological Lattice Detector & 1-Bit Table Grid Mask
- **Target Crate:** `cleanocr-vision` (new module `crates/cleanocr-vision/src/table_lattice.rs`)
- **Fish-Level Tasks:**
  1. Implement fast 1D morphological opening (horizontal kernel width $\approx 2.5\%$ of page width; vertical kernel height $\approx 2.0\%$ of page height) on a binarized luminance buffer of the page.
  2. Extract:
     - `horizontal_lines: Vec<LineSegment>`
     - `vertical_lines: Vec<LineSegment>`
     - `table_grid_mask: Vec<u8>` (1-bit packed or `GrayImage` mask of pure H/V table rulings, saved for Phase 4 Layer 3).
  3. Cluster intersecting H/V segments into rectangular `DetectedTableGrid` structures containing exact normalized cell `BBox` coordinates `(row, col, row_span, col_span)`.
  4. Handle merged header/section rows (e.g., a full-width subheader like `STAR Savings Account` or `Fixed Deposit Receipt (FDR) for COMPANY` spanning all columns).
- **Elephant-Fish Probe:** Synthetic unit test drawing a 4-column $\times$ 5-row table with 1 merged section-header row and verifying exact cell count, `col_span`, and non-empty `table_grid_mask`.

### Slice S3: Pre-OCR Line Erasure & Padded Cell Recognition
- **Target Crate:** `cleanocr-vision` (`src/ocr_engine.rs`, `src/lib.rs`)
- **Fish-Level Tasks:**
  1. **Pre-OCR Line Erasure:** Before running `oar-ocr`, create a clean working copy of the page where pixels belonging to `table_grid_mask` are replaced with the local background luminance (median of neighboring non-line pixels). Ensure character strokes that cross a thin grid line are preserved by checking stroke continuity or dilating only strictly along straight ruling pixels.
  2. **Crop Padding:** Ensure OCR recognition crops include 3–4 px horizontal and 2–3 px vertical whitespace padding clamped to image bounds so leading/trailing digits (`1` in `10.00%`) are never clipped.
  3. **Cell Assignment:** Map each recognized OCR text line into its containing `DetectedTableGrid` cell by maximum IoU / center-point containment. Sort multiple spans inside a single cell top-to-bottom, left-to-right.
  4. Emit matched regions as `BlockKind::Table` in `PageIr`, and remove those spans from the flat paragraph list so no text is duplicated.
- **Elephant-Fish Probe:** Synthetic test rendering crisp text `"10.00%"` positioned 1 px from a black vertical border; verify line erasure removes the border pixel column while leaving the `"10.00%"` ink intact, and verify `MarkdownExporter` outputs a valid GFM pipe table (`| ... | 10.00% |`).

### Slice S4: Borderless (Stream) Table Detector & XY-Cut Guard
- **Target Crates:** `cleanocr-vision` (new module `crates/cleanocr-vision/src/table_stream.rs`), `cleanocr-ir` (`src/reading_order.rs`)
- **Fish-Level Tasks:**
  1. For OCR spans **not** inside a lattice table, cluster spans into horizontal visual rows using Y-axis overlap ($\ge 50\%$ vertical overlap of the shorter box).
  2. Within contiguous multi-span rows ($\ge 3$ consecutive rows), compute a 1D X-axis occupancy projection.
  3. Detect consistent vertical whitespace gutters (column separators) shared across $\ge 75\%$ of those rows.
  4. When $\ge 2$ consistent columns and $\ge 3$ aligned rows are detected, construct a `BlockKind::Table` (mapping each span to its column bin and preserving empty cells).
  5. **XY-Cut Guard (`cleanocr-ir`):** Because both lattice and stream tables are now atomic `BlockKind::Table` blocks, XY-Cut will treat the entire table as a single indivisible block and will **never** slice tabular rows into vertical magazine columns. For remaining non-table lines on the same horizontal band, add a tie-breaker sorting left-to-right when Y-centers are within $0.5 \times \text{line\_height}$.
- **Elephant-Fish Probe:**
  - Unit test with a borderless 4-column financial rate table (no grid lines) verifying `table_stream` emits a single `BlockKind::Table` with all rows aligned left-to-right.
  - Unit test verifying `reading_order` preserves table atomicity and never interleaves columns.

---

## 3. Phase 2 — Native Excel Exporter (`.xlsx`)

**Goal:** Export all detected tables (bordered and borderless) into clean, formula-ready Excel workbooks with zero C dependencies.

### Slice S5: Pure-Rust `XlsxExporter` with Financial Cell Typing
- **Target Crates:** Root `Cargo.toml`, `crates/cleanocr-export` (new module `src/xlsx.rs`)
- **Fish-Level Tasks:**
  1. Add `rust_xlsxwriter` to `[workspace.dependencies]` and `crates/cleanocr-export/Cargo.toml`.
  2. Implement `XlsxExporter` implementing the `Exporter` trait:
     - Iterate through all `BlockKind::Table` blocks across `doc_ir.pages`.
     - Create one worksheet per table (named `Page{p}_Table{t}`, truncated to Excel's 31-char limit).
     - **Optional Smart Merging:** If a table on Page $p+1$ has the exact same column count and no intervening heading/paragraph, append it to the previous sheet (or keep separate sheets if headers differ).
     - **Financial Cell Typing Helper (`parse_excel_cell`):**
       - Percentage strings (e.g., `"9.50%"`, `"10.00%"`, `"0%"`) $\rightarrow$ write `f64` (`0.095`, `0.10`, `0.0`) with `0.00%` number format.
       - Plain/comma numbers (e.g., `"1,250.50"`, `"200"`) $\rightarrow$ write `f64` with numeric format.
       - All other strings $\rightarrow$ write UTF-8 text with text-wrap enabled.
     - Preserve `row_span > 1` or `col_span > 1` using `worksheet.merge_range(...)`.
     - Bold row 0 (header row) with light subtle shading and auto-fit column widths based on max string length (clamped between 10 and 60 chars).
     - If a document has **zero** tables, create a single `Extracted_Text` sheet writing one paragraph per row so export never fails silently.
- **Elephant-Fish Probe:** Unit test exporting a `DocumentIr` fixture containing merged cells and `"10.00%"` / `"1,250.00"` values to `.xlsx`; unzip the generated `.xlsx` archive and assert valid `xl/workbook.xml`, `xl/worksheets/sheet1.xml`, numeric `<v>0.1</v>` (not string), and `<mergeCell>` tags.

### Slice S6: Wire Excel Export into CLI & GUI + GUI Usability Polish
- **Target Crates:** `crates/cleanocr-cli`, `crates/cleanocr-gui`
- **Fish-Level Tasks:**
  1. Add `OutputFormat::Xlsx` (`xlsx`) to `cleanocr-cli` (`process`, `convert-ir`, `scan`).
  2. Add `xlsx: bool` checkbox (`Excel (.xlsx)`) to `cleanocr-gui` left sidebar and `export_all`.
  3. **GUI Auto-Model Detection:** On startup in `CleanOcrApp::new`, check if `./models` exists next to `std::env::current_exe()`. If it contains `.onnx` + `.txt` files, pre-select `self.model_dir = Some(models_dir)` automatically.
  4. **GUI Quick Actions:** Add `"Copy Markdown"` (via `ctx.copy_text(...)`) and `"Open Output Folder"` buttons next to the preview header.
- **Elephant-Fish Probe:** CLI test verifying `cleanocr process ... --formats md,xlsx` produces a valid `.xlsx` file; GUI unit test verifying `ExportFormats` includes `xlsx`.

---

## 4. Phase 3 — Complex Backgrounds, Auto-Graphics Protection & Image-Heavy Streaming

**Goal:** Protect logos/signatures automatically, preserve abstract/shaded backgrounds without dirtying the 1-bit text mask, and handle image-heavy 100+ page PDFs flatly within 8 GB RAM.

### Slice S7: Automatic Graphics, Logo & Signature Detector
- **Target Crate:** `cleanocr-vision` (new module `crates/cleanocr-vision/src/graphics.rs`)
- **Fish-Level Tasks:**
  1. Automatically populate `figure_boxes` and `vision_out.figures` *before* MRC runs by detecting three non-text classes outside recognized OCR text lines and table grid lines:
     - **Color Logos & Stamps:** Regions with high HSV saturation ($S > 0.25$) and area above noise threshold (e.g., Bank Asia color logo).
     - **Handwritten Signatures:** Connected dark ink components outside OCR text boxes whose aspect ratio/bounding box is significantly taller than standard text stroke height (e.g., signatures above `Chief Financial Officer` / `Deputy Managing Director`).
     - **Embedded Photos / Charts:** Continuous-tone rectangular regions with high local gradient variance and multi-level luminance histograms (not bimodal black/white).
  2. Merge overlapping graphic boxes, crop them as crisp full-color PNG/JPEG assets into `vision_out.figures`, and emit `BlockKind::Figure` blocks so `cleanocr-mrc` automatically protects them from binarization.
- **Elephant-Fish Probe:**
  - Synthetic test with a colored logo block + dark signature scribble next to standard text lines; verify both are detected into `figure_boxes` while standard text lines are excluded.

### Slice S8: Abstract/Shaded Background Preservation & Image-Heavy Bypass
- **Target Crates:** `cleanocr-mrc` (`src/binarize.rs`, `src/lib.rs`), `cleanocr-ingest`
- **Fish-Level Tasks:**
  1. **Shaded Header & Abstract Background Protection (`cleanocr-mrc`):**
     - Add a **local contrast gate** to Sauvola binarization: a pixel only enters the 1-bit foreground text mask if it is significantly darker than its local background window ($\text{local\_mean} - \text{pixel} > \Delta_{\min}$, e.g. $\Delta_{\min} = 28$) AND not part of a smooth low-gradient background shade/watermark.
     - Result: Light blue/gray shaded table headers, subtle watermarks, and abstract page backgrounds remain intact on the **Background JPEG** layer while only crisp dark text strokes are pulled into the 1-bit mask and inpainted out of the background.
  2. **Table Grid Subtraction (`cleanocr-mrc`):**
     - Accept optional `table_grid_mask: Option<&[u8]>` in `decompose_mrc(...)`.
     - Subtract `table_grid_mask` pixels from the 1-bit **Text Mask** and encode `table_grid_mask` as its own separate lossless 1-bit JBIG2 stream (`table_jbig2: Option<Vec<u8>>`) on the MRC output struct.
     - Inpaint **both** text pixels and table-line pixels out of the Background JPEG so Layer 1 is a completely clean paper + design background.
  3. **Image-Heavy Page Bypass:**
     - Calculate `photo_coverage_ratio = union_area(photo_boxes) / page_area`.
     - If `photo_coverage_ratio > 0.65`, automatically bypass 4-layer MRC decomposition for that page and store a single high-quality compressed JPEG (`fallback_raster_jpeg`) + invisible OCR text layer.
  4. **Page-by-Page RAM Streaming:**
     - Refactor `cleanocr-cli` and `cleanocr-gui` page loops so each page's uncompressed `RgbImage` is dropped immediately after vision + MRC compression completes (never holding all pages' raw `RgbImage` buffers in memory simultaneously).
- **Elephant-Fish Probe:**
  - Unit test with a light-gray shaded header bar containing black text: verify the black text enters the 1-bit text mask while the gray shade stays in the background JPEG without noise speckles.
  - Unit test with a 75%-photo synthetic page: verify MRC bypass triggers automatically (`mrc.is_none()`, `fallback_raster_jpeg.is_some()`).

---

## 5. Phase 4 — 4-Layer PDF/A-3 OCG Export

**Goal:** Deliver the exact 4 toggleable PDF layers requested for financial/archival quality.

### Slice S9: 4-Layer OCG Compositing in `PdfExporter`
- **Target Crate:** `cleanocr-export` (`src/pdf.rs`)
- **Fish-Level Tasks:**
  1. Define 4 PDF Optional Content Groups (OCGs) in the PDF Catalog `/OCProperties`:
     - **`Background`** — Layer 1: Inpainted clean paper + abstract designs + shaded table headers (JPEG).
     - **`Tables`** — Layer 2: Crisp 1-bit lossless JBIG2 mask containing *only* horizontal & vertical table grid lines (`table_jbig2`).
     - **`Text`** — Layer 3: Crisp 1-bit lossless JBIG2 mask containing *only* B&W text strokes (table rules & graphics subtracted).
     - **`Graphics`** — Layer 4: Full-color cropped logos, signatures, photos, and charts drawn at their exact normalized `BBox` coordinates.
  2. Draw layers in strict bottom-to-top z-order: `Background` $\rightarrow$ `Graphics` $\rightarrow$ `Tables` $\rightarrow$ `Text` $\rightarrow$ Invisible Searchable Text (`3 Tr`) mapped to `DocumentIr` spans.
  3. Ensure backward compatibility: if a page used Image-Heavy Bypass (`mrc.is_none()`), draw the single raster image under `Background` + invisible searchable text.
  4. Preserve embedded `DocumentIr` JSON payload for lossless `convert-ir` round-tripping.
- **Elephant-Fish Probe:**
  - Unit test inspecting generated PDF bytes for all 4 OCG names (`/Background`, `/Tables`, `/Text`, `/Graphics`), JBIG2 filter streams for both Text and Tables, and lossless `extract_ir_from_file` round-trip.

### Slice S10: End-to-End Financial Circular Verification Gate
- **Target Crates:** Workspace-wide (`cleanocr-cli`, `cleanocr-export`, `cleanocr-vision`)
- **Fish-Level Tasks:**
  1. Add an end-to-end integration test simulating a 2-page financial rate circular containing:
     - A colored bank logo (top left)
     - A bordered 4-column table with a merged section header and `"10.00%"` / `"9.50%"` values
     - A borderless 3-column summary table
     - A shaded background header bar
     - A handwritten signature graphic at the bottom
  2. Run the full pipeline emitting `PDF/A-3`, `Markdown`, and `XLSX`, and assert:
     - `.md` contains clean GFM pipe tables with `"10.00%"` intact (no column interleaving).
     - `.xlsx` opens as a valid zip with numeric percentage cells (`0.1`, `0.095`).
     - `.pdf` contains all 4 OCG layers (`Background`, `Tables`, `Text`, `Graphics`).
- **Elephant-Fish Probe:** Full `cargo fmt --all && cargo test --workspace && cargo clippy --workspace --all-targets` passes 100% green.

---

## 6. Agent Handoff Checklist (For Antigravity)

When executing this blueprint:
- [ ] Start at **Slice S1** and proceed sequentially through **Slice S10**.
- [ ] Commit after each completed slice with a conventional commit message (e.g., `feat(vision): add morphological lattice table detector (S2)`).
- [ ] Never introduce C/C++ native build dependencies that break `windows-latest` or `macos-latest` GitHub Actions release builds.
- [ ] Do not alter the lossless JBIG2 default (`MrcCompressionProfile::Lossless`).