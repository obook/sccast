//! discovery.rs
//! Find Chromecast devices (service `_googlecast._tcp`) on the local network with mDNS.
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
        let txt = |key: &str| info.get_property_val_str(key).map(|v| v.trim().to_string()).unwrap_or_default();
        let ip = info.get_addresses_v4().into_iter().next()?;
        Some(Self { id: txt("id"), name: txt("fn"), model: txt("md"), ip, port: info.get_port() })
    }

    /// One line of the devices file: id, name, model, ip, port, date of the last answer
    /// (Unix seconds), tab-separated.
    fn to_line(&self, seen: u64) -> String {
        format!("{}\t{}\t{}\t{}\t{}\t{seen}", self.id, self.name, self.model, self.ip, self.port)
    }

    fn from_line(line: &str, now: u64) -> Option<(Self, u64)> {
        let cols: Vec<_> = line.split('\t').collect();
        let (cols, seen) = match cols.as_slice() {
            [cols @ .., seen] if cols.len() == 5 => (cols, seen.parse().ok()?),
            // A line written before the dates: counts as seen now, to start its 3 weeks.
            cols => (cols, now),
        };
        let [id, name, model, ip, port]: [&str; 5] = cols.try_into().ok()?;
        let device = Self { id: id.into(), name: name.into(), model: model.into(), ip: ip.parse().ok()?, port: port.parse().ok()? };
        Some((device, seen))
    }
}

/// A device silent this long is forgotten: removed, or on a network not visited lately.
const FORGET_AFTER: u64 = 21 * 24 * 3600;
/// A date moves at most once a day, so answers every 12 s do not rewrite the file.
const DATE_STEP: u64 = 24 * 3600;

/// Remembered devices by id, with the date of their last answer.
type Memory = BTreeMap<String, (CastDevice, u64)>;

/// Devices seen before, kept because a busy network (a school at 10 am) may not let their
/// answers through: ~/.local/state/sccast/devices.tsv.
fn devices_file() -> std::path::PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state")));
    state.unwrap_or_else(std::env::temp_dir).join("sccast/devices.tsv")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The file's devices, minus those silent for `FORGET_AFTER`.
fn parse(text: &str, now: u64) -> Memory {
    text.lines()
        .filter_map(|line| CastDevice::from_line(line, now))
        .filter(|(_, seen)| now.saturating_sub(*seen) < FORGET_AFTER)
        .map(|(d, seen)| (d.id.clone(), (d, seen)))
        .collect()
}

fn load(now: u64) -> Memory {
    parse(&std::fs::read_to_string(devices_file()).unwrap_or_default(), now)
}

/// Devices seen on earlier runs; none when the file is missing or unreadable.
pub fn remembered() -> Vec<CastDevice> {
    load(now()).into_values().map(|(d, _)| d).collect()
}

/// Date `device`'s answer in `memory`. True when its line changed: new device, new address
/// or name, or a date a day old.
fn update(memory: &mut Memory, device: &CastDevice, now: u64) -> bool {
    let fresh = |(old, seen): &(CastDevice, u64)| old.to_line(*seen) == device.to_line(*seen) && now.saturating_sub(*seen) < DATE_STEP;
    if memory.get(&device.id).is_some_and(fresh) {
        return false;
    }
    memory.insert(device.id.clone(), (device.clone(), now));
    true
}

/// `device` answered (mDNS or ping): remember it, and drop the devices silent for 3 weeks.
/// A failure only costs the memory, so it is logged and skipped.
pub fn seen(device: &CastDevice) {
    let now = now();
    let mut memory = load(now);
    if !update(&mut memory, device, now) {
        return;
    }
    let path = devices_file();
    let text: String = memory.values().map(|(d, seen)| d.to_line(*seen) + "\n").collect();
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text));
    if let Err(e) = written {
        tracing::warn!(path = %path.display(), "cannot remember the devices: {e}");
    }
}

/// Ids of the `devices` that answer one ping within 1 s, all pinged at once.
// shortcut: runs the system ping (iputils, in every Ubuntu); none answer if it is missing.
pub fn ping_all(devices: &[CastDevice]) -> Vec<String> {
    use std::process::{Command, Stdio};
    let children: Vec<_> = devices
        .iter()
        .filter_map(|d| {
            let child = Command::new("ping").args(["-c1", "-W1", &d.ip.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            child.ok().map(|c| (&d.id, c))
        })
        .collect();
    children.into_iter().filter_map(|(id, mut c)| c.wait().ok().filter(|s| s.success()).map(|_| id.clone())).collect()
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

pub const NONE_FOUND: &str = "no Chromecast found (check that mDNS/UDP 5353 is not blocked)";

/// Call `on_device` for each resolved device until it returns true or `timeout` expires.
pub fn browse(timeout: Duration, mut on_device: impl FnMut(CastDevice) -> bool) -> anyhow::Result<()> {
    let mdns = ServiceDaemon::new()?;
    let browser = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match browser.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                tracing::debug!(?info, "resolved");
                if CastDevice::from_service(&info).is_some_and(&mut on_device) {
                    break;
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
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
        };
        assert!(is_named(&device, "télé salle 12"));
        assert!(is_named(&device, "abc123"));
        assert!(!is_named(&device, "Salle 12"));
    }

    #[test]
    fn forgets_devices_silent_for_three_weeks() {
        const DAY: u64 = 24 * 3600;
        let now = 1_800_000_000;
        let device = CastDevice { id: "abc123".into(), name: "TÉLÉ Salle 12".into(), model: "Chromecast".into(), ip: Ipv4Addr::new(10, 0, 0, 7), port: 8009 };
        let line = device.to_line(now - 20 * DAY);
        let old = CastDevice { id: "old".into(), ..device.clone() }.to_line(now - 21 * DAY);
        let before_dates = "five\tSalon\tChromecast\t10.0.0.8\t8009";
        let memory = parse(&[line.as_str(), &old, before_dates, "abc123\tbroken"].join("\n"), now);
        assert_eq!(memory.keys().collect::<Vec<_>>(), ["abc123", "five"]);
        assert_eq!(memory["abc123"].0.to_line(now - 20 * DAY), line);
        assert_eq!(memory["five"].1, now);
    }

    #[test]
    fn rewrites_a_date_once_a_day_and_any_change_at_once() {
        let now = 1_800_000_000;
        let device = CastDevice { id: "abc123".into(), name: "Salon".into(), model: "Chromecast".into(), ip: Ipv4Addr::new(10, 0, 0, 7), port: 8009 };
        let mut memory = Memory::new();
        assert!(update(&mut memory, &device, now));
        assert!(!update(&mut memory, &device, now + 3600));
        let moved = CastDevice { ip: Ipv4Addr::new(10, 0, 0, 9), ..device.clone() };
        assert!(update(&mut memory, &moved, now + 3600));
        assert!(update(&mut memory, &moved, now + 3600 + 24 * 3600));
    }

    #[test]
    fn ping_finds_only_hosts_that_answer() {
        let at = |id: &str, ip| CastDevice { id: id.into(), name: String::new(), model: String::new(), ip, port: 8009 };
        // 192.0.2.1: TEST-NET-1, never routed.
        let devices = [at("here", Ipv4Addr::LOCALHOST), at("nowhere", Ipv4Addr::new(192, 0, 2, 1))];
        assert_eq!(ping_all(&devices), vec!["here".to_string()]);
    }
}
