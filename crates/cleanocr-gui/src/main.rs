#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use cleanocr_export::{
    DocxExporter, Exporter, HtmlExporter, LatexExporter, MarkdownExporter, PageVisualAssets,
    PdfExporter,
};
use cleanocr_ingest::{IngestConfig, ingest_document};
use cleanocr_ir::{BBox, BlockKind, DocumentIr, DocumentMetadata, IR_SCHEMA_VERSION};
use cleanocr_mrc::{MrcCompressionProfile, MrcConfig, decompose_mrc};
use cleanocr_scan::{ScanInputSource, ScanJobRequest, discover_scanners, scan_and_ingest};
use cleanocr_vision::{
    PageAnalysisInput, VisionConfig, VisionProfile, analyze_page, crop_bbox_to_jpeg,
};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cleanOCR")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([840.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "cleanOCR",
        options,
        Box::new(|cc| Ok(Box::new(CleanOcrApp::new(cc)))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    Process,
    Scanner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MrcProfile {
    Lossless,
    None,
    BookMax,
}

impl MrcProfile {
    fn label(&self) -> &'static str {
        match self {
            Self::Lossless => "Lossless (JBIG2 Default)",
            Self::None => "None (Full Raster)",
            Self::BookMax => "BookMax (Lossy Archival)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccuracyProfile {
    Standard,
    High,
}

impl AccuracyProfile {
    fn label(&self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::High => "High",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanSourceUi {
    Adf,
    Platen,
}

impl ScanSourceUi {
    fn label(&self) -> &'static str {
        match self {
            Self::Adf => "ADF (Feeder)",
            Self::Platen => "Platen (Flatbed)",
        }
    }
}

#[derive(Debug, Clone)]
struct ExportFormats {
    pdf_a3: bool,
    html5: bool,
    latex: bool,
    markdown: bool,
    docx: bool,
}

impl Default for ExportFormats {
    fn default() -> Self {
        Self {
            pdf_a3: true,
            html5: false,
            latex: false,
            markdown: true,
            docx: false,
        }
    }
}

impl ExportFormats {
    fn any_selected(&self) -> bool {
        self.pdf_a3 || self.html5 || self.latex || self.markdown || self.docx
    }
}

#[derive(Debug, Clone)]
struct DiscoveredScannerUi {
    name: String,
    base_url: String,
}

enum WorkerMessage {
    Progress(f32, String),
    ScannersFound(Result<Vec<DiscoveredScannerUi>, String>),
    Finished(Result<(String, PathBuf), String>),
}

struct CleanOcrApp {
    dark_mode: bool,
    active_tab: ActiveTab,

    // Inputs & Config
    input_file: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    model_dir: Option<PathBuf>,
    formats: ExportFormats,
    mrc_profile: MrcProfile,
    accuracy: AccuracyProfile,
    dpi: u32,

    // Scanner State
    scanners: Vec<DiscoveredScannerUi>,
    selected_scanner_url: String,
    scan_source: ScanSourceUi,

    // Worker & Progress State
    is_busy: bool,
    progress: f32,
    status_text: String,
    worker_rx: Option<Receiver<WorkerMessage>>,

    // Preview
    preview_markdown: String,
}

impl CleanOcrApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        Self {
            dark_mode: true,
            active_tab: ActiveTab::Process,
            input_file: None,
            output_dir: None,
            model_dir: None,
            formats: ExportFormats::default(),
            mrc_profile: MrcProfile::Lossless,
            accuracy: AccuracyProfile::Standard,
            dpi: 300,
            scanners: Vec::new(),
            selected_scanner_url: String::new(),
            scan_source: ScanSourceUi::Adf,
            is_busy: false,
            progress: 0.0,
            status_text: "Ready. Drop a PDF or image to begin.".to_string(),
            worker_rx: None,
            preview_markdown: String::new(),
        }
    }

    fn resolve_output_dir(&self) -> PathBuf {
        if let Some(dir) = &self.output_dir {
            return dir.clone();
        }
        if let Some(input) = &self.input_file {
            if let Some(parent) = input.parent() {
                return parent.join("cleanocr_output");
            }
        }
        PathBuf::from("./output")
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        if let Some(rx) = &self.worker_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    WorkerMessage::Progress(p, status) => {
                        self.progress = p;
                        self.status_text = status;
                    }
                    WorkerMessage::ScannersFound(result) => {
                        finished = true;
                        self.is_busy = false;
                        self.progress = 1.0;
                        match result {
                            Ok(list) => {
                                if list.is_empty() {
                                    self.status_text =
                                        "No eSCL scanners found on the local network.".to_string();
                                } else {
                                    self.status_text =
                                        format!("Found {} eSCL scanner(s).", list.len());
                                    if self.selected_scanner_url.is_empty() {
                                        self.selected_scanner_url = list[0].base_url.clone();
                                    }
                                }
                                self.scanners = list;
                            }
                            Err(err) => {
                                self.status_text = format!("Scanner discovery failed: {err}");
                            }
                        }
                    }
                    WorkerMessage::Finished(result) => {
                        finished = true;
                        self.is_busy = false;
                        match result {
                            Ok((md, out_dir)) => {
                                self.progress = 1.0;
                                self.status_text =
                                    format!("Done. Exported to {}", out_dir.display());
                                self.preview_markdown = md;
                            }
                            Err(err) => {
                                self.status_text = format!("Error: {err}");
                            }
                        }
                    }
                }
            }
        }
        if finished {
            self.worker_rx = None;
        }
        if self.is_busy {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn start_processing(&mut self, ctx: egui::Context) {
        let Some(input) = self.input_file.clone() else {
            self.status_text = "Select an input file first.".to_string();
            return;
        };
        if !self.formats.any_selected() {
            self.status_text = "Select at least one output format.".to_string();
            return;
        }

        let out_dir = self.resolve_output_dir();
        let formats = self.formats.clone();
        let mrc_profile = self.mrc_profile;
        let accuracy = self.accuracy;
        let dpi = self.dpi;
        let model_dir = self.model_dir.clone();

        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        self.is_busy = true;
        self.progress = 0.05;
        self.status_text = "Starting cleanOCR pipeline...".to_string();

        thread::spawn(move || {
            let res = run_file_pipeline(
                &input,
                &out_dir,
                &formats,
                mrc_profile,
                accuracy,
                dpi,
                model_dir,
                &tx,
                &ctx,
            );
            let _ = tx.send(WorkerMessage::Finished(res.map(|md| (md, out_dir))));
            ctx.request_repaint();
        });
    }

    fn start_scanner_discovery(&mut self, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        self.is_busy = true;
        self.progress = 0.2;
        self.status_text = "Discovering eSCL scanners via mDNS (3s)...".to_string();

        thread::spawn(move || {
            let res = discover_scanners(Duration::from_secs(3))
                .map(|list| {
                    list.into_iter()
                        .map(|s| DiscoveredScannerUi {
                            name: s.name,
                            base_url: s.base_url,
                        })
                        .collect()
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(WorkerMessage::ScannersFound(res));
            ctx.request_repaint();
        });
    }

    fn start_scan_and_process(&mut self, ctx: egui::Context) {
        let base_url = self.selected_scanner_url.trim().to_string();
        if base_url.is_empty() {
            self.status_text = "Select a discovered scanner or enter an eSCL URL.".to_string();
            return;
        }
        if !self.formats.any_selected() {
            self.status_text = "Select at least one output format.".to_string();
            return;
        }

        let out_dir = self.resolve_output_dir();
        let formats = self.formats.clone();
        let mrc_profile = self.mrc_profile;
        let accuracy = self.accuracy;
        let dpi = self.dpi;
        let model_dir = self.model_dir.clone();
        let source = self.scan_source;

        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        self.is_busy = true;
        self.progress = 0.1;
        self.status_text = format!("Scanning from {base_url}...");

        thread::spawn(move || {
            let res = run_scan_pipeline(
                &base_url,
                source,
                &out_dir,
                &formats,
                mrc_profile,
                accuracy,
                dpi,
                model_dir,
                &tx,
                &ctx,
            );
            let _ = tx.send(WorkerMessage::Finished(res.map(|md| (md, out_dir))));
            ctx.request_repaint();
        });
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        ctx.input(|i| {
            if let Some(file) = i.raw.dropped_files.first() {
                if let Some(path) = &file.path {
                    self.input_file = Some(path.clone());
                    self.status_text = format!("Loaded: {}", path.display());
                }
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn run_file_pipeline(
    input: &PathBuf,
    out_dir: &PathBuf,
    formats: &ExportFormats,
    mrc_profile: MrcProfile,
    accuracy: AccuracyProfile,
    dpi: u32,
    model_dir: Option<PathBuf>,
    tx: &mpsc::Sender<WorkerMessage>,
    ctx: &egui::Context,
) -> Result<String, String> {
    let send_progress = |p: f32, msg: String| {
        let _ = tx.send(WorkerMessage::Progress(p, msg));
        ctx.request_repaint();
    };

    send_progress(0.10, format!("Ingesting {} at {} DPI...", input.display(), dpi));
    let ingest_cfg = IngestConfig {
        dpi,
        ..IngestConfig::default()
    };
    let ingested = ingest_document(input, &ingest_cfg).map_err(|e| e.to_string())?;

    let vision_cfg = VisionConfig {
        profile: match accuracy {
            AccuracyProfile::Standard => VisionProfile::Standard,
            AccuracyProfile::High => VisionProfile::High,
        },
        model_dir,
        ..VisionConfig::default()
    };

    let total_pages = ingested.pages.len().max(1);
    let mut page_irs = Vec::with_capacity(ingested.pages.len());
    let mut page_assets = Vec::with_capacity(ingested.pages.len());

    for (idx, page) in ingested.pages.iter().enumerate() {
        let frac = 0.15 + 0.65 * (idx as f32 / total_pages as f32);
        send_progress(
            frac,
            format!("Analyzing & compressing page {}/{}...", idx + 1, total_pages),
        );

        let vision_out = analyze_page(
            &PageAnalysisInput {
                page_index: page.page_index,
                width_pt: page.width_pt,
                height_pt: page.height_pt,
                dpi: page.dpi,
                ingest_mode: page.ingest_mode,
                image: &page.image,
                native_text: page.native_text.as_deref(),
            },
            &vision_cfg,
        )
        .map_err(|e| e.to_string())?;

        let figure_boxes: Vec<BBox> = vision_out
            .page_ir
            .blocks
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
            .map(|b| b.bbox)
            .collect();

        let (mrc, fallback_raster_jpeg) = build_mrc(&page.image, &figure_boxes, mrc_profile)?;

        page_assets.push(PageVisualAssets {
            page_index: page.page_index,
            mrc,
            fallback_raster_jpeg,
            figures: vision_out.figures,
        });
        page_irs.push(vision_out.page_ir);
    }

    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("document");

    let doc_ir = DocumentIr {
        schema_version: IR_SCHEMA_VERSION.to_string(),
        metadata: DocumentMetadata {
            title: Some(stem.to_string()),
            source_sha256: ingested.source_sha256,
            source_filename: ingested.source_filename,
            detected_languages: vec!["en".to_string()],
            created_utc: "2026-11-24T00:00:00Z".to_string(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        pages: page_irs,
    };

    send_progress(0.85, "Exporting selected formats...".to_string());
    export_all(&doc_ir, &page_assets, out_dir, stem, formats)
}

#[allow(clippy::too_many_arguments)]
fn run_scan_pipeline(
    base_url: &str,
    source: ScanSourceUi,
    out_dir: &PathBuf,
    formats: &ExportFormats,
    mrc_profile: MrcProfile,
    accuracy: AccuracyProfile,
    dpi: u32,
    model_dir: Option<PathBuf>,
    tx: &mpsc::Sender<WorkerMessage>,
    ctx: &egui::Context,
) -> Result<String, String> {
    let send_progress = |p: f32, msg: String| {
        let _ = tx.send(WorkerMessage::Progress(p, msg));
        ctx.request_repaint();
    };

    let job = ScanJobRequest {
        source: match source {
            ScanSourceUi::Platen => ScanInputSource::Platen,
            ScanSourceUi::Adf => ScanInputSource::Adf,
        },
        ..ScanJobRequest::default()
    };

    let ingest_cfg = IngestConfig {
        dpi,
        ..IngestConfig::default()
    };
    let scanned_docs = scan_and_ingest(base_url, &job, &ingest_cfg).map_err(|e| e.to_string())?;
    if scanned_docs.is_empty() {
        return Err("Scanner returned 0 pages.".to_string());
    }

    let vision_cfg = VisionConfig {
        profile: match accuracy {
            AccuracyProfile::Standard => VisionProfile::Standard,
            AccuracyProfile::High => VisionProfile::High,
        },
        model_dir,
        ..VisionConfig::default()
    };

    let total_pages = scanned_docs.len().max(1);
    let mut page_irs = Vec::with_capacity(scanned_docs.len());
    let mut page_assets = Vec::with_capacity(scanned_docs.len());

    for (idx, doc) in scanned_docs.iter().enumerate() {
        let frac = 0.25 + 0.55 * (idx as f32 / total_pages as f32);
        send_progress(
            frac,
            format!("Processing scanned page {}/{}...", idx + 1, total_pages),
        );

        for page in &doc.pages {
            let vision_out = analyze_page(
                &PageAnalysisInput {
                    page_index: idx,
                    width_pt: page.width_pt,
                    height_pt: page.height_pt,
                    dpi: page.dpi,
                    ingest_mode: page.ingest_mode,
                    image: &page.image,
                    native_text: page.native_text.as_deref(),
                },
                &vision_cfg,
            )
            .map_err(|e| e.to_string())?;

            let figure_boxes: Vec<BBox> = vision_out
                .page_ir
                .blocks
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Figure { .. }))
                .map(|b| b.bbox)
                .collect();

            let (mrc, fallback_raster_jpeg) = build_mrc(&page.image, &figure_boxes, mrc_profile)?;
            page_assets.push(PageVisualAssets {
                page_index: idx,
                mrc,
                fallback_raster_jpeg,
                figures: vision_out.figures,
            });
            page_irs.push(vision_out.page_ir);
        }
    }

    let doc_ir = DocumentIr {
        schema_version: IR_SCHEMA_VERSION.to_string(),
        metadata: DocumentMetadata {
            title: Some("Scanned Document".to_string()),
            source_sha256: scanned_docs[0].source_sha256.clone(),
            source_filename: Some("scan.jpg".to_string()),
            detected_languages: vec!["en".to_string()],
            created_utc: "2026-11-24T00:00:00Z".to_string(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        },
        pages: page_irs,
    };

    send_progress(0.85, "Exporting scanned outputs...".to_string());
    export_all(&doc_ir, &page_assets, out_dir, "scan", formats)
}

fn build_mrc(
    image: &image::DynamicImage,
    figure_boxes: &[BBox],
    profile: MrcProfile,
) -> Result<(Option<cleanocr_mrc::MrcDecomposition>, Option<Vec<u8>>), String> {
    match profile {
        MrcProfile::None => {
            let full_jpg =
                crop_bbox_to_jpeg(image, BBox::new(0.0, 0.0, 1.0, 1.0), 85).map_err(|e| e.to_string())?;
            Ok((None, Some(full_jpg)))
        }
        MrcProfile::Lossless => {
            let mrc_cfg = MrcConfig {
                profile: MrcCompressionProfile::Lossless,
                ..MrcConfig::default()
            };
            let mrc = decompose_mrc(image, figure_boxes, &mrc_cfg).map_err(|e| e.to_string())?;
            Ok((Some(mrc), None))
        }
        MrcProfile::BookMax => {
            let mrc_cfg = MrcConfig {
                profile: MrcCompressionProfile::BookMax,
                ..MrcConfig::default()
            };
            let mrc = decompose_mrc(image, figure_boxes, &mrc_cfg).map_err(|e| e.to_string())?;
            Ok((Some(mrc), None))
        }
    }
}

fn export_all(
    doc_ir: &DocumentIr,
    page_assets: &[PageVisualAssets],
    out_dir: &PathBuf,
    stem: &str,
    formats: &ExportFormats,
) -> Result<String, String> {
    fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;

    if formats.pdf_a3 {
        PdfExporter
            .export(doc_ir, page_assets, &out_dir.join(format!("{stem}.pdf")))
            .map_err(|e| e.to_string())?;
    }
    if formats.html5 {
        HtmlExporter
            .export(doc_ir, page_assets, &out_dir.join(format!("{stem}.html")))
            .map_err(|e| e.to_string())?;
    }
    if formats.latex {
        LatexExporter
            .export(doc_ir, page_assets, &out_dir.join(format!("{stem}_tex")))
            .map_err(|e| e.to_string())?;
    }
    if formats.docx {
        DocxExporter
            .export(doc_ir, page_assets, &out_dir.join(format!("{stem}.docx")))
            .map_err(|e| e.to_string())?;
    }

    let md_preview = if formats.markdown {
        let md_path = out_dir.join(format!("{stem}.md"));
        MarkdownExporter
            .export(doc_ir, page_assets, &md_path)
            .map_err(|e| e.to_string())?;
        fs::read_to_string(&md_path).map_err(|e| e.to_string())?
    } else {
        let tmp_md = std::env::temp_dir().join(format!("cleanocr_preview_{}.md", std::process::id()));
        MarkdownExporter
            .export(doc_ir, page_assets, &tmp_md)
            .map_err(|e| e.to_string())?;
        let text = fs::read_to_string(&tmp_md).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&tmp_md);
        text
    };

    Ok(md_preview)
}

impl eframe::App for CleanOcrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker(ctx);
        self.handle_dropped_files(ctx);

        // Top Bar: Brand, Tabs, Theme Toggle
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("cleanOCR");
                ui.separator();

                ui.selectable_value(&mut self.active_tab, ActiveTab::Process, "Process Document");
                ui.selectable_value(&mut self.active_tab, ActiveTab::Scanner, "eSCL Scanner");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let theme_label = if self.dark_mode { "Light Mode" } else { "Dark Mode" };
                    if ui.button(theme_label).clicked() {
                        self.dark_mode = !self.dark_mode;
                        ctx.set_visuals(if self.dark_mode {
                            egui::Visuals::dark()
                        } else {
                            egui::Visuals::light()
                        });
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Bottom Bar: Live Progress & Status
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::ProgressBar::new(self.progress)
                        .desired_width(220.0)
                        .show_percentage()
                        .animate(self.is_busy),
                );
                ui.label(&self.status_text);
            });
            ui.add_space(4.0);
        });

        // Left Sidebar: Pipeline Settings
        egui::SidePanel::left("settings_panel")
            .resizable(true)
            .default_width(275.0)
            .min_width(230.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Pipeline Settings");
                ui.separator();

                ui.label(egui::RichText::new("Output Formats").strong());
                ui.checkbox(&mut self.formats.pdf_a3, "PDF/A-3 (MRC)");
                ui.checkbox(&mut self.formats.markdown, "Markdown (.md)");
                ui.checkbox(&mut self.formats.html5, "HTML5");
                ui.checkbox(&mut self.formats.latex, "LaTeX + pgfplots");
                ui.checkbox(&mut self.formats.docx, "DOCX");

                ui.add_space(10.0);
                ui.label(egui::RichText::new("MRC Profile").strong());
                egui::ComboBox::from_id_salt("mrc_profile")
                    .selected_text(self.mrc_profile.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.mrc_profile,
                            MrcProfile::Lossless,
                            MrcProfile::Lossless.label(),
                        );
                        ui.selectable_value(
                            &mut self.mrc_profile,
                            MrcProfile::None,
                            MrcProfile::None.label(),
                        );
                        ui.selectable_value(
                            &mut self.mrc_profile,
                            MrcProfile::BookMax,
                            MrcProfile::BookMax.label(),
                        );
                    });
                if self.mrc_profile == MrcProfile::BookMax {
                    ui.small("⚠ Lossy JBIG2: avoid on financial/legal docs.");
                }

                ui.add_space(10.0);
                ui.label(egui::RichText::new("OCR Accuracy").strong());
                egui::ComboBox::from_id_salt("accuracy_profile")
                    .selected_text(self.accuracy.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.accuracy,
                            AccuracyProfile::Standard,
                            AccuracyProfile::Standard.label(),
                        );
                        ui.selectable_value(
                            &mut self.accuracy,
                            AccuracyProfile::High,
                            AccuracyProfile::High.label(),
                        );
                    });

                ui.add_space(10.0);
                ui.label(egui::RichText::new("Render DPI").strong());
                ui.add(egui::Slider::new(&mut self.dpi, 72..=600).suffix(" DPI"));

                ui.add_space(10.0);
                ui.label(egui::RichText::new("ONNX Model Directory (Optional)").strong());
                ui.horizontal(|ui| {
                    if ui.button("Browse...").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.model_dir = Some(dir);
                        }
                    }
                    if self.model_dir.is_some() && ui.button("Clear").clicked() {
                        self.model_dir = None;
                    }
                });
                if let Some(dir) = &self.model_dir {
                    ui.small(dir.display().to_string());
                } else {
                    ui.small("Using offline ink-band fallback");
                }
            });

        // Main Content Area
        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            ActiveTab::Process => self.render_process_tab(ui, ctx),
            ActiveTab::Scanner => self.render_scanner_tab(ui, ctx),
        });
    }
}

impl CleanOcrApp {
    fn render_process_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Document Input");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            if ui.button("Select File...").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(
                        "Documents & Images",
                        &["pdf", "png", "jpg", "jpeg", "tif", "tiff"],
                    )
                    .pick_file()
                {
                    self.input_file = Some(path);
                }
            }
            if ui.button("Output Folder...").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.output_dir = Some(dir);
                }
            }

            let can_run = !self.is_busy && self.input_file.is_some();
            if ui
                .add_enabled(can_run, egui::Button::new("Run cleanOCR"))
                .clicked()
            {
                self.start_processing(ctx.clone());
            }
        });

        ui.add_space(6.0);
        if let Some(path) = &self.input_file {
            ui.label(format!("Input: {}", path.display()));
        } else {
            ui.label("Drop a PDF or image anywhere in this window, or click 'Select File...'.");
        }
        ui.small(format!("Output dir: {}", self.resolve_output_dir().display()));

        ui.separator();
        ui.heading("Extracted Text / Markdown Preview");
        ui.add_space(4.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_sized(
                ui.available_size(),
                egui::TextEdit::multiline(&mut self.preview_markdown)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("Extracted Markdown will appear here after processing..."),
            );
        });
    }

    fn render_scanner_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Driverless eSCL Scanner (AirScan / Mopria)");
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.is_busy, egui::Button::new("Discover Scanners"))
                .clicked()
            {
                self.start_scanner_discovery(ctx.clone());
            }

            egui::ComboBox::from_id_salt("scan_source")
                .selected_text(self.scan_source.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.scan_source,
                        ScanSourceUi::Adf,
                        ScanSourceUi::Adf.label(),
                    );
                    ui.selectable_value(
                        &mut self.scan_source,
                        ScanSourceUi::Platen,
                        ScanSourceUi::Platen.label(),
                    );
                });

            let can_scan = !self.is_busy && !self.selected_scanner_url.trim().is_empty();
            if ui
                .add_enabled(can_scan, egui::Button::new("Scan & Process"))
                .clicked()
            {
                self.start_scan_and_process(ctx.clone());
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Scanner eSCL URL:");
            ui.add(
                egui::TextEdit::singleline(&mut self.selected_scanner_url)
                    .hint_text("http://192.168.1.50:80/eSCL")
                    .desired_width(320.0),
            );
        });

        if !self.scanners.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Discovered Scanners").strong());
            for s in &self.scanners {
                let selected = self.selected_scanner_url == s.base_url;
                if ui
                    .selectable_label(selected, format!("{}  ({})", s.name, s.base_url))
                    .clicked()
                {
                    self.selected_scanner_url = s.base_url.clone();
                }
            }
        }

        ui.separator();
        ui.heading("Scanned Markdown Preview");
        ui.add_space(4.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_sized(
                ui.available_size(),
                egui::TextEdit::multiline(&mut self.preview_markdown)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("Scanned document text will appear here..."),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_formats_include_pdf_and_markdown() {
        let formats = ExportFormats::default();
        assert!(formats.pdf_a3);
        assert!(formats.markdown);
        assert!(formats.any_selected());
    }
}
