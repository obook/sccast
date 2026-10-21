//! commands.rs
//! The command channel of a panel cast: one TCP connection, length-prefixed messages (wire.rs).
//! Writes go out from the session's thread, a second thread reads. Every message goes to
//! the protocol trace (trace.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde_json::Value;

use super::approval::{Refused, APPROVAL_TIMEOUT};
use super::media::cause;
use super::pin::PORTS;
use super::trace;
use super::wire::{read_message, write_message, ScreenshareMessage};

/// TCP connections and writes; the answers wait for a person (approval.rs).
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// Per port when looking for the panel's command port.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Without an answer by then, the trace says what it may mean.
const HINT_AFTER: Duration = Duration::from_secs(10);

/// Command channel: writes here, a thread reads into `incoming`.
pub struct Commands {
    pub socket: TcpStream,
    sequence: i32,
    pub incoming: mpsc::Receiver<io::Result<ScreenshareMessage>>,
}

/// Connect to the panel at `host`; if its port does not answer, try the other command ports
/// of the code table (a code read wrong, or a panel set another way).
fn reach(host: SocketAddr) -> anyhow::Result<TcpStream> {
    let ports = std::iter::once(host.port()).chain(PORTS.into_iter().filter(|p| *p != host.port()));
    let mut failures = Vec::new();
    for port in ports {
        let address = SocketAddr::new(host.ip(), port);
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(socket) => {
                trace::line(&format!("connected to {address}"));
                return Ok(socket);
            }
            Err(e) => {
                trace::line(&format!("cannot connect to {address}: {}", cause(&e)));
                failures.push(format!("{port}: {}", if e.kind() == io::ErrorKind::TimedOut { "no answer" } else { "refused" }));
            }
        }
    }
    anyhow::bail!(
        "cannot reach the panel at {} (ports {}). Is it on, on the same network, and is the code right?",
        host.ip(),
        failures.join(", ")
    )
}

impl Commands {
    pub fn open(host: SocketAddr) -> anyhow::Result<Self> {
        let socket = reach(host)?;
        socket.set_nodelay(true)?;
        // A heartbeat must not block on a dead link.
        socket.set_write_timeout(Some(ANSWER_TIMEOUT))?;
        let mut reader = socket.try_clone()?;
        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || loop {
            let message = read_message(&mut reader);
            match &message {
                Ok(m) => trace::command("<-", m.command_type, m.command_id, m.sequence, &m.payload),
                Err(e) => trace::line(&format!("<- {}", cause(e))),
            }
            let failed = message.is_err();
            if tx.send(message).is_err() || failed {
                break;
            }
        });
        Ok(Self { socket, sequence: 0, incoming })
    }

    pub fn send(&mut self, (command_type, command_id): (i32, i32), payload: &Value) -> io::Result<()> {
        self.sequence += 1;
        self.reply((command_type, command_id), self.sequence, payload)
    }

    /// Send with a given sequence: an answer repeats its request's.
    pub fn reply(&mut self, (command_type, command_id): (i32, i32), sequence: i32, payload: &Value) -> io::Result<()> {
        let (timestamp, payload) = (now_ms(), payload.to_string());
        trace::command("->", command_type, command_id, sequence, &payload);
        write_message(&mut self.socket, &ScreenshareMessage { version: 1, command_type, command_id, sequence, timestamp, payload })
    }

    /// Wait for `wanted`, as long as someone may take to allow it on the panel; return its
    /// version and JSON payload. None at `stop`. `countdown` gets the seconds left, once a second.
    pub fn wait(&self, wanted: (i32, i32), stop: &AtomicBool, countdown: &dyn Fn(u64)) -> anyhow::Result<Option<(i32, Value)>> {
        let started = Instant::now();
        let (mut shown, mut hinted) = (None, false);
        while !stop.load(Ordering::SeqCst) {
            let Some(left) = (started + APPROVAL_TIMEOUT).checked_duration_since(Instant::now()) else {
                return Err(Refused("no answer from the panel in 90 s (nobody allowed the cast?)".into()).into());
            };
            if shown != Some(left.as_secs()) {
                shown = Some(left.as_secs());
                countdown(left.as_secs() + 1);
            }
            if !hinted && started.elapsed() > HINT_AFTER {
                hinted = true;
                trace::line(&format!("no answer to {wanted:?} after 10 s: waiting for Allow on the panel, or it does not speak this protocol"));
            }
            let m = match self.incoming.recv_timeout(Duration::from_millis(200)) {
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                other => other.context("the panel closed the connection")??,
            };
            if (m.command_type, m.command_id) == wanted {
                return Ok(Some((m.version, serde_json::from_str(&m.payload).unwrap_or(Value::Null))));
            }
        }
        Ok(None)
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
