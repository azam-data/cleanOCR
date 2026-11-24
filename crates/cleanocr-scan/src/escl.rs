use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::Event;

/// Physical paper input source on an eSCL scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanInputSource {
    /// Flatbed glass platen (single page).
    Platen,
    /// Automatic Document Feeder (multi-page batch).
    #[default]
    Adf,
}

impl ScanInputSource {
    pub fn as_escl_str(self) -> &'static str {
        match self {
            Self::Platen => "Platen",
            Self::Adf => "Feeder",
        }
    }
}

/// Parameters for submitting an eSCL `/eSCL/ScanJobs` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanJobRequest {
    pub source: ScanInputSource,
    pub dpi: u32,
    pub color_mode: &'static str,
    pub document_format: &'static str,
}

impl Default for ScanJobRequest {
    fn default() -> Self {
        Self {
            source: ScanInputSource::Adf,
            dpi: 300,
            color_mode: "RGB24",
            document_format: "image/jpeg",
        }
    }
}

/// Parsed subset of `/eSCL/ScannerCapabilities` XML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScannerCapabilities {
    pub make_and_model: Option<String>,
    pub supports_platen: bool,
    pub supports_adf: bool,
    pub supported_formats: Vec<String>,
}

/// Builds a standards-compliant eSCL `<scan:ScanSettings>` XML payload.
pub fn build_scan_settings_xml(req: &ScanJobRequest) -> String {
    let dpi = req.dpi.clamp(72, 1200);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.0</pwg:Version>
  <pwg:InputSource>{source}</pwg:InputSource>
  <scan:ColorMode>{color}</scan:ColorMode>
  <scan:XResolution>{dpi}</scan:XResolution>
  <scan:YResolution>{dpi}</scan:YResolution>
  <pwg:DocumentFormat>{fmt}</pwg:DocumentFormat>
</scan:ScanSettings>"#,
        source = req.source.as_escl_str(),
        color = req.color_mode,
        dpi = dpi,
        fmt = req.document_format,
    )
}

/// Parses an eSCL `/eSCL/ScannerCapabilities` XML response using `quick-xml`.
pub fn parse_scanner_capabilities(xml: &str) -> Result<ScannerCapabilities> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut caps = ScannerCapabilities::default();
    let mut current_tag = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = local_name_string(e.local_name().as_ref());
                if local == "Platen" {
                    caps.supports_platen = true;
                } else if local == "Adf" {
                    caps.supports_adf = true;
                }
                current_tag = local;
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .unescape()
                    .context("failed to unescape XML text in ScannerCapabilities")?
                    .into_owned();
                if text.is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "MakeAndModel" => caps.make_and_model = Some(text),
                    "DocumentFormat" | "DocumentFormatExt" => {
                        if !caps.supported_formats.contains(&text) {
                            caps.supported_formats.push(text);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(_)) => {
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("failed to parse eSCL ScannerCapabilities XML: {e}"),
            _ => {}
        }
        buf.clear();
    }

    Ok(caps)
}

fn local_name_string(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// Normalizes an eSCL base URL (e.g. `http://192.168.1.50:8080/eSCL`).
pub fn normalize_escl_base_url(host: &str, port: u16, root_path: &str, use_tls: bool) -> String {
    let scheme = if use_tls { "https" } else { "http" };
    let clean_root = root_path.trim_matches('/');
    let root = if clean_root.is_empty() {
        "eSCL"
    } else {
        clean_root
    };
    format!("{scheme}://{host}:{port}/{root}")
}

/// HTTP client for communicating with a driverless eSCL (AirScan / Mopria) scanner.
pub struct EsclClient {
    base_url: String,
    http: reqwest::blocking::Client,
}

impl EsclClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("failed to build eSCL HTTP client")?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    /// Fetches and parses `/ScannerCapabilities`.
    pub fn capabilities(&self) -> Result<ScannerCapabilities> {
        let url = format!("{}/ScannerCapabilities", self.base_url);
        let xml = self
            .http
            .get(&url)
            .send()
            .with_context(|| format!("GET {url} failed"))?
            .error_for_status()
            .with_context(|| format!("scanner returned error for {url}"))?
            .text()
            .context("failed to read ScannerCapabilities body")?;
        parse_scanner_capabilities(&xml)
    }

    /// Submits a scan job and pulls all pages from `<job_url>/NextDocument` until HTTP 404.
    pub fn scan_pages(&self, req: &ScanJobRequest) -> Result<Vec<Vec<u8>>> {
        let jobs_url = format!("{}/ScanJobs", self.base_url);
        let xml = build_scan_settings_xml(req);

        let resp = self
            .http
            .post(&jobs_url)
            .header("Content-Type", "text/xml")
            .body(xml)
            .send()
            .with_context(|| format!("POST {jobs_url} failed"))?;

        if !resp.status().is_success() {
            bail!("eSCL ScanJobs rejected with HTTP {}", resp.status());
        }

        let job_location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("eSCL scanner response missing Location header"))?
            .trim_end_matches('/')
            .to_string();

        let next_doc_url = format!("{job_location}/NextDocument");
        let mut pages = Vec::new();

        loop {
            let page_resp = self
                .http
                .get(&next_doc_url)
                .send()
                .with_context(|| format!("GET {next_doc_url} failed"))?;

            if page_resp.status() == reqwest::StatusCode::NOT_FOUND {
                break;
            }
            if !page_resp.status().is_success() {
                bail!("eSCL NextDocument failed with HTTP {}", page_resp.status());
            }

            let bytes = page_resp
                .bytes()
                .context("failed to read scanned page bytes")?
                .to_vec();
            if !bytes.is_empty() {
                pages.push(bytes);
            }

            if req.source == ScanInputSource::Platen {
                break;
            }
        }

        // Best-effort job cleanup.
        let _ = self.http.delete(&job_location).send();
        Ok(pages)
    }
}
