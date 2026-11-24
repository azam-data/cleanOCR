use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::escl::normalize_escl_base_url;

/// A driverless eSCL (AirScan / Mopria) scanner discovered on the local network via mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredScanner {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub base_url: String,
    pub use_tls: bool,
}

/// Discovers eSCL scanners advertising `_uscan._tcp.local.` or `_uscans._tcp.local.`
/// on the local network for up to `timeout`.
pub fn discover_scanners(timeout: Duration) -> Result<Vec<DiscoveredScanner>> {
    let mdns = ServiceDaemon::new().context("failed to start mDNS daemon")?;
    let rx_http = mdns
        .browse("_uscan._tcp.local.")
        .context("failed to browse _uscan._tcp.local.")?;
    let rx_https = mdns
        .browse("_uscans._tcp.local.")
        .context("failed to browse _uscans._tcp.local.")?;

    let deadline = Instant::now() + timeout;
    let mut found: Vec<DiscoveredScanner> = Vec::new();

    while Instant::now() < deadline {
        for (rx, use_tls) in [(&rx_http, false), (&rx_https, true)] {
            if let Ok(ServiceEvent::ServiceResolved(info)) =
                rx.recv_timeout(Duration::from_millis(50))
            {
                let host = info
                    .get_addresses()
                    .iter()
                    .next()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| info.get_hostname().trim_end_matches('.').to_string());
                let port = info.get_port();
                let root_path = info.get_property_val_str("rs").unwrap_or("eSCL");
                let name = info
                    .get_property_val_str("ty")
                    .unwrap_or_else(|| info.get_fullname())
                    .to_string();

                let base_url = normalize_escl_base_url(&host, port, root_path, use_tls);
                if !found.iter().any(|s| s.base_url == base_url) {
                    found.push(DiscoveredScanner {
                        name,
                        host,
                        port,
                        base_url,
                        use_tls,
                    });
                }
            }
        }
    }

    let _ = mdns.shutdown();
    Ok(found)
}
