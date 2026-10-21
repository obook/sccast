//! commands.rs
//! The command channel of a panel cast: one TCP connection, length-prefixed messages (wire.rs).
//! Writes go out from the session's thread, a second thread reads. Every message goes to
//! the protocol trace (trace.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::{json, Value};

use super::approval::{Refused, APPROVAL_TIMEOUT};
use super::media::cause;
use super::pin::PORTS;
use super::trace;
use super::wire::{read_message, write_message, ScreenshareMessage};

/// (commandType, commandId) pairs read while waiting, from the sender's ProtocolConstant.
pub const HEARTBEAT: (i32, i32) = (0, 0);
pub const DISCONNECTED: (i32, i32) = (1, 102);
pub const CAST_STOPPED: (i32, i32) = (6, 103);
/// The Android sender sends a heartbeat after 3 s without writing.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
/// TCP connections and writes; the answers wait for a person (approval.rs).
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// Per port when looking for the panel's command port.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Without an answer by then, the trace says what it may mean.
const HINT_AFTER: Duration = Duration::from_secs(10);

/// Command channel: writes here, a thread reads into `incoming`. Dropping it closes the
/// connection, which also ends the reading thread.
pub struct Commands {
    pub socket: TcpStream,
    sequence: i32,
    last_sent: Instant,
    pub incoming: mpsc::Receiver<io::Result<ScreenshareMessage>>,
}

/// Connect to the panel at `host`; if its port does not answer, try the other command ports
/// of the code table (a code read wrong, or a panel set another way). Returns the address
/// that answered. Gives up between two ports at `stop`.
pub fn reach(host: SocketAddr, stop: &AtomicBool) -> anyhow::Result<(TcpStream, SocketAddr)> {
    let ports = std::iter::once(host.port()).chain(PORTS.into_iter().filter(|p| *p != host.port()));
    let mut failures = Vec::new();
    for port in ports {
        if stop.load(Ordering::SeqCst) {
            anyhow::bail!("stopped");
        }
        let address = SocketAddr::new(host.ip(), port);
        let e = match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(socket) => {
                trace::line(&format!("connected to {address}"));
                return Ok((socket, address));
            }
            Err(e) => e,
        };
        trace::line(&format!("cannot connect to {address}: {}", cause(&e)));
        match e.kind() {
            // No machine answers at this address: other ports will not help.
            io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => anyhow::bail!(
                "no machine answers at {}: the code is wrong, the panel is off, or it is on another network",
                host.ip()
            ),
            io::ErrorKind::ConnectionRefused => failures.push(format!("{port}: refused")),
            _ => failures.push(format!("{port}: no answer")),
        }
    }
    anyhow::bail!(
        "the machine at {} does not take a cast on ports {}. Is it the panel, and is the code right?",
        host.ip(),
        failures.join(", ")
    )
}

impl Commands {
    pub fn open(host: SocketAddr, stop: &AtomicBool) -> anyhow::Result<Self> {
        let (socket, _) = reach(host, stop)?;
        socket.set_nodelay(true)?;
        // A heartbeat must not block on a dead link.
        socket.set_write_timeout(Some(ANSWER_TIMEOUT))?;
        let mut reader = socket.try_clone()?;
        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || loop {
            let message = read_message(&mut reader);
            match &message {
                Ok(read) => trace::command("<-", read.command_type, read.command_id, read.sequence, &read.payload),
                Err(e) => trace::line(&format!("<- {}", cause(e))),
            }
            let failed = message.is_err();
            if tx.send(message).is_err() || failed {
                break;
            }
        });
        Ok(Self { socket, sequence: 0, last_sent: Instant::now(), incoming })
    }

    pub fn send(&mut self, (command_type, command_id): (i32, i32), payload: &Value) -> io::Result<()> {
        self.sequence += 1;
        self.reply((command_type, command_id), self.sequence, payload)
    }

    /// Send with a given sequence: an answer repeats its request's.
    pub fn reply(&mut self, (command_type, command_id): (i32, i32), sequence: i32, payload: &Value) -> io::Result<()> {
        let (timestamp, payload) = (now_ms(), payload.to_string());
        trace::command("->", command_type, command_id, sequence, &payload);
        self.last_sent = Instant::now();
        write_message(&mut self.socket, &ScreenshareMessage { version: 1, command_type, command_id, sequence, timestamp, payload })
    }

    /// A heartbeat when nothing was sent for a while, so the panel keeps the session.
    pub fn keep_alive(&mut self) -> io::Result<()> {
        if self.last_sent.elapsed() < HEARTBEAT_INTERVAL {
            return Ok(());
        }
        self.send(HEARTBEAT, &json!({"sentTimeMs": now_ms()}))
    }

    /// Wait for `wanted`, as long as someone may take to allow it on the panel, with
    /// heartbeats meanwhile; return its version and JSON payload. None at `stop`.
    /// `countdown` gets the seconds left, once a second.
    pub fn wait(&mut self, wanted: (i32, i32), stop: &AtomicBool, countdown: &dyn Fn(u64)) -> anyhow::Result<Option<(i32, Value)>> {
        let started = Instant::now();
        let (mut shown, mut hinted) = (None, false);
        while !stop.load(Ordering::SeqCst) {
            let Some(left) = (started + APPROVAL_TIMEOUT).checked_duration_since(Instant::now()) else {
                return Err(Refused::new("no answer from the panel in 90 s (nobody allowed the cast?)").into());
            };
            if shown != Some(left.as_secs()) {
                shown = Some(left.as_secs());
                countdown(left.as_secs() + 1);
            }
            if !hinted && started.elapsed() > HINT_AFTER {
                hinted = true;
                trace::line(&format!("no answer to {wanted:?} after 10 s: waiting for Allow on the panel, or it does not speak this protocol"));
            }
            self.keep_alive().context("the panel closed the connection")?;
            let message = match self.incoming.recv_timeout(Duration::from_millis(200)) {
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                other => other.context("the panel closed the connection")??,
            };
            match (message.command_type, message.command_id) {
                ids if ids == wanted => {
                    let payload = serde_json::from_str(&message.payload).unwrap_or(Value::Null);
                    return Ok(Some((message.version, payload)));
                }
                DISCONNECTED | CAST_STOPPED => {
                    return Err(Refused::new(format!("the panel ended the session: {}", message.payload)).into());
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// Connect request, with the fields the Windows client fills (keys from its builder; both
/// spellings of the code appear there). Which ones the panel needs is unverified. Hypothesis:
/// deviceType 5, written by the Windows client, stands for a PC (the Android sender writes 1).
/// The device id stays the same across reconnections and runs: derived from the machine-id,
/// which must not leave the PC as is (systemd says so), so hashed with sccast's name.
pub fn connect_request(pin: Option<&str>, local_ip: IpAddr) -> Value {
    let read = |path| std::fs::read_to_string(path).map(|text| text.trim().to_string()).ok().filter(|text| !text.is_empty());
    let name = read("/etc/hostname").unwrap_or_else(|| "sccast".to_string());
    let machine_id = read("/etc/machine-id").unwrap_or_else(|| name.clone());
    let uuid = format!("{:016x}", fnv1a(&format!("sccast {machine_id}")));
    let pin = pin.unwrap_or("");
    json!({"pincode": pin, "pinCode": pin, "deviceName": name, "deviceType": 5, "connectType": 0,
        "deviceVersion": env!("CARGO_PKG_VERSION"), "deviceUuid": uuid, "originIp": local_ip.to_string(),
        "supportMultiTouch": false, "supportPen": false, "functions": [], "systemName": "Linux"})
}

/// FNV-1a, 64 bits: a hash that stays the same across Rust versions, unlike the std one.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
