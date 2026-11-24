#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

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
    LosslessJbig2,
    Balanced,
    Aggressive,
}

impl MrcProfile {
    fn label(&self) -> &'static str {
        match self {
            Self::LosslessJbig2 => "Lossless (JBIG2 Default)",
            Self::Balanced => "Balanced",
            Self::Aggressive => "Aggressive (Smallest)",
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

enum WorkerMessage {
    Progress(f32, String),
    Finished(Result<String, String>),
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
    dpi: u32,

    // Worker & Progress State
    is_processing: bool,
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
            mrc_profile: MrcProfile::LosslessJbig2,
            dpi: 300,
            is_processing: false,
            progress: 0.0,
            status_text: "Ready. Drop a PDF or image to begin.".to_string(),
            worker_rx: None,
            preview_markdown: String::new(),
        }
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
                    WorkerMessage::Finished(result) => {
                        finished = true;
                        self.is_processing = false;
                        match result {
                            Ok(md) => {
                                self.progress = 1.0;
                                self.status_text = "Completed successfully.".to_string();
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
        if self.is_processing {
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

        let (tx, rx) = mpsc::channel();
        self.worker_rx = Some(rx);
        self.is_processing = true;
        self.progress = 0.0;
        self.status_text = "Starting pipeline...".to_string();

        let dpi = self.dpi;
        let profile = self.mrc_profile;

        // Background worker thread keeps UI responsive on 8 GB RAM laptops
        thread::spawn(move || {
            let steps = [
                (0.20, format!("Ingesting {} at {} DPI...", input.display(), dpi)),
                (0.50, "Running OCR & layout reconstruction...".to_string()),
                (0.75, format!("Applying MRC ({})...", profile.label())),
                (0.95, "Exporting selected formats...".to_string()),
            ];

            for (p, msg) in steps {
                let _ = tx.send(WorkerMessage::Progress(p, msg));
                ctx.request_repaint();
                thread::sleep(Duration::from_millis(300));
            }

            let preview = format!(
                "# Extracted Preview\n\n- **Source:** `{}`\n- **DPI:** {}\n- **MRC Profile:** {}\n\n*(Pipeline stub — wire `cleanocr-ingest` → `cleanocr-vision` → `cleanocr-mrc` → `cleanocr-export` next.)*",
                input.display(),
                dpi,
                profile.label()
            );

            let _ = tx.send(WorkerMessage::Finished(Ok(preview)));
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
                        .animate(self.is_processing),
                );
                ui.label(&self.status_text);
            });
            ui.add_space(4.0);
        });

        // Left Sidebar: Pipeline Settings
        egui::SidePanel::left("settings_panel")
            .resizable(true)
            .default_width(270.0)
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
                            MrcProfile::LosslessJbig2,
                            MrcProfile::LosslessJbig2.label(),
                        );
                        ui.selectable_value(
                            &mut self.mrc_profile,
                            MrcProfile::Balanced,
                            MrcProfile::Balanced.label(),
                        );
                        ui.selectable_value(
                            &mut self.mrc_profile,
                            MrcProfile::Aggressive,
                            MrcProfile::Aggressive.label(),
                        );
                    });

                ui.add_space(10.0);
                ui.label(egui::RichText::new("Render DPI").strong());
                ui.add(egui::Slider::new(&mut self.dpi, 150..=600).suffix(" DPI"));

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
            ActiveTab::Scanner => self.render_scanner_tab(ui),
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
                    .add_filter("Documents & Images", &["pdf", "png", "jpg", "jpeg", "tif", "tiff"])
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

            let can_run = !self.is_processing && self.input_file.is_some();
            if ui.add_enabled(can_run, egui::Button::new("Run cleanOCR")).clicked() {
                self.start_processing(ctx.clone());
            }
        });

        ui.add_space(6.0);
        if let Some(path) = &self.input_file {
            ui.label(format!("Input: {}", path.display()));
        } else {
            ui.label("Drop a PDF or image anywhere in this window, or click 'Select File...'.");
        }
        if let Some(out) = &self.output_dir {
            ui.small(format!("Output dir: {}", out.display()));
        }

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

    fn render_scanner_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Driverless eSCL Scanner (AirScan / Mopria)");
        ui.add_space(6.0);
        ui.label("Discover network scanners via mDNS and scan directly into cleanOCR.");
        ui.add_space(8.0);
        if ui.button("Discover Scanners").clicked() {
            self.status_text = "eSCL discovery wiring coming next...".to_string();
        }
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
