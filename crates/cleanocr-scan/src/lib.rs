//! Driverless eSCL (AirScan / Mopria) network scanner discovery and acquisition.

pub mod discovery;
pub mod escl;

use anyhow::{Context, Result};
use cleanocr_ingest::{IngestConfig, IngestedDocument, ingest_bytes};

pub use discovery::{DiscoveredScanner, discover_scanners};
pub use escl::{
    EsclClient, ScanInputSource, ScanJobRequest, ScannerCapabilities, build_scan_settings_xml,
    normalize_escl_base_url, parse_scanner_capabilities,
};

/// Scans one or more pages from an eSCL scanner and runs them through `cleanocr-ingest`
/// (deskew + illumination normalization).
pub fn scan_and_ingest(
    base_url: &str,
    job: &ScanJobRequest,
    ingest_cfg: &IngestConfig,
) -> Result<Vec<IngestedDocument>> {
    let client = EsclClient::new(base_url)?;
    let raw_pages = client.scan_pages(job)?;
    let mut docs = Vec::with_capacity(raw_pages.len());

    for (idx, bytes) in raw_pages.into_iter().enumerate() {
        let filename = Some(format!("scan_page_{:03}.jpg", idx + 1));
        let doc = ingest_bytes(&bytes, filename, ingest_cfg)
            .with_context(|| format!("failed to ingest scanned page {}", idx + 1))?;
        docs.push(doc);
    }

    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_escl_scan_settings_xml() {
        let req = ScanJobRequest {
            source: ScanInputSource::Adf,
            dpi: 300,
            color_mode: "RGB24",
            document_format: "image/jpeg",
        };
        let xml = build_scan_settings_xml(&req);
        assert!(xml.contains("<pwg:InputSource>Feeder</pwg:InputSource>"));
        assert!(xml.contains("<scan:XResolution>300</scan:XResolution>"));
        assert!(xml.contains("<pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>"));
    }

    #[test]
    fn parses_scanner_capabilities_xml() {
        let sample = r#"<?xml version="1.0" encoding="UTF-8"?>
        <scan:ScannerCapabilities xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
            <pwg:MakeAndModel>Brother ADS-2700W</pwg:MakeAndModel>
            <scan:Platen>
                <scan:PlatenInputCaps>
                    <scan:SettingProfiles>
                        <scan:SettingProfile>
                            <scan:DocumentFormats>
                                <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>
                                <pwg:DocumentFormat>application/pdf</pwg:DocumentFormat>
                            </scan:DocumentFormats>
                        </scan:SettingProfile>
                    </scan:SettingProfiles>
                </scan:PlatenInputCaps>
            </scan:Platen>
            <scan:Adf />
        </scan:ScannerCapabilities>"#;

        let caps = parse_scanner_capabilities(sample).expect("parse capabilities");
        assert_eq!(caps.make_and_model.as_deref(), Some("Brother ADS-2700W"));
        assert!(caps.supports_platen);
        assert!(caps.supports_adf);
        assert!(caps.supported_formats.contains(&"image/jpeg".to_string()));
        assert!(
            caps.supported_formats
                .contains(&"application/pdf".to_string())
        );
    }

    #[test]
    fn normalizes_escl_base_url() {
        assert_eq!(
            normalize_escl_base_url("192.168.1.42", 8080, "/eSCL/", false),
            "http://192.168.1.42:8080/eSCL"
        );
        assert_eq!(
            normalize_escl_base_url("scanner.local", 443, "", true),
            "https://scanner.local:443/eSCL"
        );
    }
}
