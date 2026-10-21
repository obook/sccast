//! discovery.rs
//! Find Chromecast devices (service `_googlecast._tcp`) and interactive panels
//! (`_sspcast._tcp`, see docs/screenshare.md) on the local network with mDNS.
//! Used by `sccast list`, by `--device` and by the menu's device list.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use mdns_sd::{RecvTimeoutError, ResolvedService, ServiceDaemon, ServiceEvent};

const SERVICE_TYPE: &str = "_googlecast._tcp.local.";
/// Panels, read from their Android sender. Unverified on a real panel.
const SCREENSHARE_TYPE: &str = "_sspcast._tcp.local.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chromecast,
    /// An interactive panel: cast with the screenshare format, on `port`.
    Screenshare,
}

#[derive(Debug, Clone)]
pub struct CastDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub kind: Kind,
}

impl CastDevice {
    fn from_service(info: &ResolvedService) -> Option<Self> {
        let txt = |key: &str| info.get_property_val_str(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let kind = if info.ty_domain == SCREENSHARE_TYPE { Kind::Screenshare } else { Kind::Chromecast };
        Self::from_txt(kind, txt, info.get_addresses_v4().into_iter().next(), info.get_port())
    }

    /// Device from its TXT record, its address (A record) and its SRV port.
    fn from_txt(kind: Kind, txt: impl Fn(&str) -> Option<String>, ip: Option<Ipv4Addr>, port: u16) -> Option<Self> {
        if kind == Kind::Chromecast {
            let txt = |key| txt(key).unwrap_or_default();
            return Some(Self { id: txt("id"), name: txt("fn"), model: txt("md"), ip: ip?, port, kind });
        }
        // The panel's sender also reads `host` and `port` from the TXT record: they win, as there.
        // Other keys it reads: mac, pv, dv (versions, hypothesis), useHost.
        let ip = txt("host").and_then(|h| h.parse().ok()).or(ip)?;
        let port = txt("port").and_then(|p| p.parse().ok()).unwrap_or(port);
        let name = txt("name").unwrap_or_else(|| ip.to_string());
        let id = txt("id").unwrap_or_else(|| format!("{ip}:{port}"));
        Some(Self { id, name, model: "Screenshare".into(), ip, port, kind })
    }
}

/// Browse the network for `timeout` and return the devices found, sorted by name.
pub fn discover(timeout: Duration) -> anyhow::Result<Vec<CastDevice>> {
    let mut found = BTreeMap::new();
    browse(timeout, |device| {
        // Keyed by id: a device answers several times during the browse.
        found.insert(device.id.clone(), device);
        false
    })?;
    let mut devices: Vec<_> = found.into_values().collect();
    devices.sort_by_key(|d| d.name.to_lowercase());
    Ok(devices)
}

/// Find the device whose name (case-insensitive) or id is `query`.
/// Returns as soon as it answers, instead of waiting for the full timeout.
/// When it does not answer, the error lists the devices seen during the same browse.
pub fn find(query: &str, timeout: Duration) -> anyhow::Result<CastDevice> {
    let mut hit = None;
    let mut seen = BTreeMap::new();
    browse(timeout, |device| {
        if is_named(&device, query) {
            hit = Some(device);
            return true;
        }
        seen.insert(device.id.clone(), device.name);
        false
    })?;
    if let Some(device) = hit {
        return Ok(device);
    }
    if seen.is_empty() {
        anyhow::bail!("{NONE_FOUND}");
    }
    let names: Vec<_> = seen.into_values().collect();
    anyhow::bail!("device {query:?} not found; available: {}", names.join(", "))
}

/// True when `query` is the device's id, or its name in any case (accented letters included).
fn is_named(device: &CastDevice, query: &str) -> bool {
    device.id == query || device.name.to_lowercase() == query.to_lowercase()
}

pub const NONE_FOUND: &str = "no Chromecast or panel found (check that mDNS/UDP 5353 is not blocked)";

/// Call `on_device` for each resolved device until it returns true or `timeout` expires.
pub fn browse(timeout: Duration, mut on_device: impl FnMut(CastDevice) -> bool) -> anyhow::Result<()> {
    let mdns = ServiceDaemon::new()?;
    let mut browsers = vec![mdns.browse(SERVICE_TYPE)?, mdns.browse(SCREENSHARE_TYPE)?];
    let deadline = Instant::now() + timeout;

    'browse: while Instant::now() < deadline && !browsers.is_empty() {
        // Short waits in turn: one browse must not hold up the other.
        for i in (0..browsers.len()).rev() {
            match browsers[i].recv_timeout(Duration::from_millis(50)) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    tracing::debug!(?info, "resolved");
                    if let Some(device) = CastDevice::from_service(&info) {
                        if on_device(device) {
                            break 'browse;
                        }
                    }
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => drop(browsers.swap_remove(i)),
            }
        }
    }
    let _ = mdns.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_names_in_any_case() {
        let device = CastDevice {
            id: "abc123".into(),
            name: "TÉLÉ Salle 12".into(),
            model: "Chromecast".into(),
            ip: Ipv4Addr::LOCALHOST,
            port: 8009,
            kind: Kind::Chromecast,
        };
        assert!(is_named(&device, "télé salle 12"));
        assert!(is_named(&device, "abc123"));
        assert!(!is_named(&device, "Salle 12"));
    }

    #[test]
    fn reads_a_panel_txt_record() {
        let record = [("id", "0a1b2c"), ("name", "Salle 104"), ("port", "15000"), ("pv", "76"), ("useHost", "false")];
        let txt = |key: &str| record.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string());
        let device = CastDevice::from_txt(Kind::Screenshare, txt, Some(Ipv4Addr::new(10, 0, 0, 7)), 9000).unwrap();
        assert_eq!((device.id.as_str(), device.name.as_str(), device.port), ("0a1b2c", "Salle 104", 15000));
        assert_eq!((device.ip, device.kind, device.model.as_str()), (Ipv4Addr::new(10, 0, 0, 7), Kind::Screenshare, "Screenshare"));
        // Without TXT keys: the SRV port, and the address as name and id.
        let bare = CastDevice::from_txt(Kind::Screenshare, |_| None, Some(Ipv4Addr::new(10, 0, 0, 8)), 9000).unwrap();
        assert_eq!((bare.name.as_str(), bare.id.as_str(), bare.port), ("10.0.0.8", "10.0.0.8:9000", 9000));
    }
}
