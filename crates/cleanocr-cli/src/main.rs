use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use cleanocr_ir::IR_SCHEMA_VERSION;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "cleanocr",
    version,
    about = "Rust-native smart OCR, MRC compression, and semantic document reconstruction engine"
)]
struct Cli {
    /// Enable debug logging (overrides RUST_LOG).
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Process an input PDF or raster image into one or more output formats.
    Process(ProcessArgs),
    /// Losslessly re-export an archived cleanOCR HTML or PDF/A-3 file via its embedded JSON IR.
    ConvertIr(ConvertIrArgs),
    /// Inspect environment health, IR schema version, and compiled feature flags.
    Doctor,
    /// [Phase 2] Discover or scan directly from an eSCL (AirScan / Mopria) network scanner.
    Scan(ScanArgs),
}

#[derive(Debug, Clone, Args)]
struct ProcessArgs {
    /// Input PDF, PNG, JPEG, or TIFF document.
    input: PathBuf,

    /// Directory where output files and assets will be written.
    #[arg(long, default_value = "./output")]
    out_dir: PathBuf,

    /// Comma-separated list of output formats (e.g. pdf,html,tex,md,docx).
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_values_t = [OutputFormat::Pdf, OutputFormat::Html]
    )]
    formats: Vec<OutputFormat>,

    /// Mixed Raster Content (MRC) compression profile (default: lossless JBIG2).
    #[arg(long, alias = "compression-profile", value_enum, default_value_t = MrcProfile::Lossless)]
    mrc: MrcProfile,

    /// Vision and OCR accuracy profile.
    #[arg(long, value_enum, default_value_t = AccuracyProfile::Standard)]
    accuracy: AccuracyProfile,

    /// Chart handling mode for LaTeX export.
    #[arg(long, value_enum, default_value_t = ChartMode::Raster)]
    charts: ChartMode,

    /// Target raster processing resolution in DPI (72..=1200).
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(72..=1200))]
    dpi: u32,
}

#[derive(Debug, Clone, Args)]
struct ConvertIrArgs {
    /// Archived cleanOCR `.pdf` or `.html` file containing an embedded DocumentIr JSON payload.
    input: PathBuf,

    /// Target output format.
    #[arg(long = "to", value_enum)]
    format: OutputFormat,

    /// Destination file or directory path.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Clone, Args)]
struct ScanArgs {
    /// Discover eSCL scanners on the local network via mDNS and exit.
    #[arg(long)]
    discover: bool,

    /// Target scanner device name from `--discover`.
    #[arg(long)]
    device: Option<String>,

    /// Scanner paper source.
    #[arg(long, value_enum, default_value_t = ScanSource::Adf)]
    source: ScanSource,

    /// Comma-separated output formats for scanned pages.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_values_t = [OutputFormat::Pdf, OutputFormat::Html]
    )]
    formats: Vec<OutputFormat>,

    /// Directory where scanned outputs will be written.
    #[arg(long, default_value = "./output")]
    out_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Pdf,
    Html,
    Tex,
    Md,
    Docx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MrcProfile {
    /// Keep full raster page without MRC layer splitting.
    None,
    /// Lossless JBIG2 1-bit mask + JPEG foreground/background (safe default).
    Lossless,
    /// Opt-in lossy JBIG2 symbol-dictionary compression for archival books only.
    BookMax,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AccuracyProfile {
    Standard,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ChartMode {
    /// Tier 1 default: embed clean cropped figure assets via `\includegraphics`.
    Raster,
    /// Tier 2 opt-in: extract structured `ChartIr` via VLM and emit deterministic `pgfplots`.
    Pgfplots,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScanSource {
    Platen,
    Adf,
}

fn init_tracing(verbose: bool) {
    let default_level = if verbose { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn validate_process_args(args: &ProcessArgs) -> Result<()> {
    if args.formats.is_empty() {
        bail!("at least one output format must be provided via --formats");
    }

    if args.charts == ChartMode::Pgfplots && !cfg!(feature = "chart-vlm") {
        bail!(
            "--charts pgfplots requires the optional Tier 2 'chart-vlm' feature. \
             Rebuild with: cargo build -p cleanocr-cli --features chart-vlm"
        );
    }

    if args.mrc == MrcProfile::BookMax {
        tracing::warn!(
            "Using lossy JBIG2 symbol-dictionary mode (--mrc book-max). \
             Do not use on legal or financial documents."
        );
    }

    Ok(())
}

fn validate_scan_args(args: &ScanArgs) -> Result<()> {
    if !cfg!(feature = "scanner") {
        bail!(
            "eSCL network scanning requires the optional 'scanner' feature. \
             Rebuild with: cargo build -p cleanocr-cli --features scanner"
        );
    }

    if !args.discover && args.device.is_none() {
        bail!("specify --discover or pass a target scanner via --device <NAME>");
    }

    Ok(())
}

fn run_doctor() {
    println!("cleanOCR Doctor");
    println!("  Engine version    : {}", env!("CARGO_PKG_VERSION"));
    println!("  IR schema version : {IR_SCHEMA_VERSION}");
    println!("  Feature chart-vlm : {}", cfg!(feature = "chart-vlm"));
    println!("  Feature scanner   : {}", cfg!(feature = "scanner"));
    println!("  Default JBIG2     : lossless (Jbig2Config::lossless)");
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Process(args) => {
            validate_process_args(&args)?;
            tracing::info!(
                input = %args.input.display(),
                out_dir = %args.out_dir.display(),
                formats = ?args.formats,
                mrc = ?args.mrc,
                accuracy = ?args.accuracy,
                charts = ?args.charts,
                dpi = args.dpi,
                "Validated process arguments"
            );
            bail!(
                "End-to-end `process` pipeline is not yet wired \
                 (scheduled for Milestones 2-5)."
            );
        }
        Command::ConvertIr(args) => {
            tracing::info!(
                input = %args.input.display(),
                format = ?args.format,
                out = %args.out.display(),
                "Validated convert-ir arguments"
            );
            bail!("`convert-ir` command is not yet wired (scheduled for Milestone 2).");
        }
        Command::Doctor => {
            run_doctor();
            Ok(())
        }
        Command::Scan(args) => {
            validate_scan_args(&args)?;
            tracing::info!(
                discover = args.discover,
                device = ?args.device,
                source = ?args.source,
                formats = ?args.formats,
                out_dir = %args.out_dir.display(),
                "Validated scan arguments"
            );
            bail!("eSCL network scanner is not yet wired (scheduled for Milestone 6).");
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    run(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_process_command_with_defaults_and_flags() {
        let cli = Cli::try_parse_from([
            "cleanocr",
            "process",
            "invoice.pdf",
            "--out-dir",
            "./out",
            "--formats",
            "pdf,html,tex,md,docx",
            "--mrc",
            "lossless",
            "--accuracy",
            "high",
            "--charts",
            "raster",
            "--dpi",
            "300",
        ])
        .expect("parse process command");

        match cli.command {
            Command::Process(args) => {
                assert_eq!(args.input, PathBuf::from("invoice.pdf"));
                assert_eq!(args.out_dir, PathBuf::from("./out"));
                assert_eq!(
                    args.formats,
                    vec![
                        OutputFormat::Pdf,
                        OutputFormat::Html,
                        OutputFormat::Tex,
                        OutputFormat::Md,
                        OutputFormat::Docx,
                    ]
                );
                assert_eq!(args.mrc, MrcProfile::Lossless);
                assert_eq!(args.accuracy, AccuracyProfile::High);
                assert_eq!(args.charts, ChartMode::Raster);
                assert_eq!(args.dpi, 300);
                validate_process_args(&args).expect("valid args");
            }
            other => panic!("expected Process command, got {other:?}"),
        }
    }

    #[test]
    fn default_mrc_profile_is_lossless_and_alias_works() {
        let default_cli = Cli::try_parse_from(["cleanocr", "process", "doc.png"]).expect("parse");
        match default_cli.command {
            Command::Process(args) => assert_eq!(args.mrc, MrcProfile::Lossless),
            other => panic!("expected Process command, got {other:?}"),
        }

        let alias_cli = Cli::try_parse_from([
            "cleanocr",
            "process",
            "book.pdf",
            "--compression-profile",
            "book-max",
        ])
        .expect("parse compression-profile alias");
        match alias_cli.command {
            Command::Process(args) => assert_eq!(args.mrc, MrcProfile::BookMax),
            other => panic!("expected Process command, got {other:?}"),
        }
    }

    #[cfg(not(feature = "chart-vlm"))]
    #[test]
    fn charts_pgfplots_fails_fast_without_chart_vlm_feature() {
        let cli = Cli::try_parse_from([
            "cleanocr",
            "process",
            "paper.pdf",
            "--charts",
            "pgfplots",
        ])
        .expect("parse process with pgfplots");

        match cli.command {
            Command::Process(args) => {
                let err = validate_process_args(&args)
                    .expect_err("must fail fast without chart-vlm feature");
                assert!(err.to_string().contains("chart-vlm"));
            }
            other => panic!("expected Process command, got {other:?}"),
        }
    }

    #[cfg(feature = "chart-vlm")]
    #[test]
    fn charts_pgfplots_succeeds_with_chart_vlm_feature() {
        let cli = Cli::try_parse_from([
            "cleanocr",
            "process",
            "paper.pdf",
            "--charts",
            "pgfplots",
        ])
        .expect("parse process with pgfplots");

        match cli.command {
            Command::Process(args) => validate_process_args(&args).expect("valid with feature"),
            other => panic!("expected Process command, got {other:?}"),
        }
    }

    #[test]
    fn parses_convert_ir_and_doctor_commands() {
        let convert_cli = Cli::try_parse_from([
            "cleanocr",
            "convert-ir",
            "archived.pdf",
            "--to",
            "tex",
            "--out",
            "./paper_tex",
        ])
        .expect("parse convert-ir");

        match convert_cli.command {
            Command::ConvertIr(args) => {
                assert_eq!(args.input, PathBuf::from("archived.pdf"));
                assert_eq!(args.format, OutputFormat::Tex);
                assert_eq!(args.out, PathBuf::from("./paper_tex"));
            }
            other => panic!("expected ConvertIr command, got {other:?}"),
        }

        let doctor_cli = Cli::try_parse_from(["cleanocr", "doctor"]).expect("parse doctor");
        run(doctor_cli).expect("doctor command succeeds");
    }

    #[test]
    fn rejects_out_of_range_dpi() {
        let err = Cli::try_parse_from(["cleanocr", "process", "doc.png", "--dpi", "30"]);
        assert!(err.is_err());
    }
}
