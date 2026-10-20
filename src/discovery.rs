use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};

const SERVICE_TYPE: &str = "_googlecast._tcp.local.";

#[derive(Debug, Clone)]
pub struct CastDevice {
    pub id: String,
    pub name: String,
    pub model: String,
    pub ip: Ipv4Addr,
    pub port: u16,
}

impl CastDevice {
    fn from_service(info: &ResolvedService) -> Option<Self> {
        let txt = |key| info.get_property_val_str(key).unwrap_or_default().trim().to_string();
        Some(Self {
            id: txt("id"),
            name: txt("fn"),
            model: txt("md"),
            ip: info.get_addresses_v4().into_iter().next()?,
            port: info.get_port(),
        })
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
pub fn find(query: &str, timeout: Duration) -> anyhow::Result<Option<CastDevice>> {
    let mut hit = None;
    browse(timeout, |device| {
        let matches = device.name.eq_ignore_ascii_case(query) || device.id == query;
        if matches {
            hit = Some(device);
        }
        matches
    })?;
    Ok(hit)
}

/// Call `on_device` for each resolved device until it returns true or `timeout` expires.
fn browse(timeout: Duration, mut on_device: impl FnMut(CastDevice) -> bool) -> anyhow::Result<()> {
    let mdns = ServiceDaemon::new()?;
    let events = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(device) = CastDevice::from_service(&info) {
                    tracing::debug!(?device, "resolved");
                    if on_device(device) {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = mdns.shutdown();
    Ok(())
}
