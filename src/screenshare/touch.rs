//! touch.rs
//! Touchback: the panel asks the PC for a UDP port on the command channel, then sends each
//! touch on it as a pointer event. Read from the Windows client (touchback service) and the
//! Android sender's message table; unverified on a real panel. With the Touchback setting, the
//! touches move the desktop's pointer (pointer.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::pointer::{apply, Action, Button, TouchEvent};
use super::wire::ScreenshareMessage;
use crate::capture::Input;

/// Android's table: (6, 6) asks with `toggle`, (6, 106) answers with `udpServerPort`.
const ANDROID_REQUEST: (i32, i32) = (6, 6);
/// Command types sccast already knows; the Windows client's touchback type is not known.
const KNOWN_TYPES: [i32; 3] = [0, 1, 6];
/// Pointer events, as in the remote control table (type 3).
const POINTER_TYPE: i32 = 3;

/// What the panel asks on the command channel.
#[derive(Debug, PartialEq)]
pub enum Request {
    Start,
    Quit,
}

/// Touchback request in `message`, if it is one. Hypothesis for the Windows form: an unknown
/// command type with id 1 (start) or 2 (quit).
pub fn request(message: &ScreenshareMessage) -> Option<Request> {
    if (message.command_type, message.command_id) == ANDROID_REQUEST {
        let toggle = serde_json::from_str::<Value>(&message.payload).ok()?["toggle"].as_i64();
        return Some(if toggle == Some(0) { Request::Quit } else { Request::Start });
    }
    match (KNOWN_TYPES.contains(&message.command_type), message.command_id) {
        (false, 1) => Some(Request::Start),
        (false, 2) => Some(Request::Quit),
        _ => None,
    }
}

/// Answer to a start request: same type, id + 100; port 0 when the socket could not open.
/// The keys follow Android's TouchbackPortResponse; the Windows client fills the same values.
pub fn answer(request: &ScreenshareMessage, port: u16) -> ((i32, i32), Value) {
    let (result, reason) = if port == 0 { (-100, "Request failed.") } else { (1, "Request success.") };
    let payload = json!({"resultType": result, "failReason": reason, "udpServerPort": port});
    ((request.command_type, request.command_id + 100), payload)
}

/// Touchback state of one cast: the UDP port and the stop flag of its reading thread.
pub struct Touchback {
    running: Option<(u16, &'static AtomicBool)>,
    /// The desktop's pointer; None when the Touchback setting is off.
    input: Option<Arc<Input>>,
    /// Only the panel may move the pointer: datagrams from anywhere else are dropped.
    panel: IpAddr,
    /// Size of the video the panel shows, in pixels.
    video: (f64, f64),
}

impl Touchback {
    pub fn new(input: Option<Arc<Input>>, panel: IpAddr, (width, height): (u32, u32)) -> Self {
        Self { running: None, input, panel, video: (f64::from(width), f64::from(height)) }
    }

    /// Act on a touchback request in `message`; return the answer to send, if any.
    pub fn handle(&mut self, message: &ScreenshareMessage) -> Option<((i32, i32), Value)> {
        match request(message)? {
            Request::Quit => {
                self.stop();
                None
            }
            Request::Start => {
                // Refused when off: the panel says so instead of showing touches that do nothing.
                let Some(input) = self.input.clone() else {
                    tracing::info!("touchback asked by the panel, but the Touchback setting is off");
                    return Some(answer(message, 0));
                };
                if self.running.is_none() {
                    // ponytail: leaks one flag per start (a byte), the thread wants a 'static flag.
                    let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
                    let video = self.video;
                    match listen(ended, self.panel, move |touch| apply(&input, &touch, video)) {
                        Ok(port) => self.running = Some((port, ended)),
                        Err(e) => tracing::warn!("touchback socket: {e}"),
                    }
                }
                Some(answer(message, self.running.map_or(0, |(port, _)| port)))
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some((_, ended)) = self.running.take() {
            ended.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for Touchback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Event from one UDP datagram: a protobuf message without length, JSON payload (UdpPoint).
pub fn decode(datagram: &[u8]) -> Option<TouchEvent> {
    let message = ScreenshareMessage::decode(datagram).ok()?;
    if message.command_type != POINTER_TYPE {
        return None;
    }
    let action = match message.command_id {
        21 => Action::Press(Button::Left),
        22 => Action::Release(Button::Left),
        23 => Action::Press(Button::Right),
        24 => Action::Release(Button::Right),
        25 => Action::Move,
        26 => Action::Click(Button::Left),
        27 => Action::Click(Button::Right),
        28 => Action::DoubleClick,
        29 => Action::Wheel,
        _ => return None,
    };
    let point: Value = serde_json::from_str(&message.payload).ok()?;
    let get = |key: &str| point[key].as_f64().unwrap_or(0.0);
    Some(TouchEvent { action, x: get("x"), y: get("y"), point_x: get("pointX"), point_y: get("pointY") })
}

/// Open a UDP socket on any port and read the panel's touches on it until `ended`, passing
/// each to `on_touch`.
fn listen(ended: &'static AtomicBool, panel: IpAddr, mut on_touch: impl FnMut(TouchEvent) + Send + 'static) -> std::io::Result<u16> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let port = socket.local_addr()?.port();
    std::thread::spawn(move || {
        let mut buffer = [0; 2048];
        while !ended.load(Ordering::SeqCst) {
            let Ok((len, from)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            // A student's device on the same Wi-Fi must not drive the teacher's pointer.
            if from.ip() != panel {
                tracing::warn!(%from, "touchback datagram from another address, dropped");
                continue;
            }
            match decode(&buffer[..len]) {
                Some(touch) => on_touch(touch),
                None => tracing::debug!(%from, len, "touchback datagram not understood"),
            }
        }
    });
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(command_type: i32, command_id: i32, payload: &str) -> ScreenshareMessage {
        ScreenshareMessage { version: 1, command_type, command_id, payload: payload.into(), ..Default::default() }
    }

    #[test]
    fn decodes_a_touch() {
        let datagram = message(3, 21, r#"{"enable":1,"mPort":0,"pointX":0.25,"pointY":0.5,"x":480.0,"y":540.0}"#).encode();
        let touch = decode(&datagram).unwrap();
        let expected = TouchEvent { action: Action::Press(Button::Left), x: 480.0, y: 540.0, point_x: 0.25, point_y: 0.5 };
        assert_eq!(touch, expected);
        assert!(decode(&message(1, 21, "{}").encode()).is_none());
        assert!(decode(&message(3, 99, "{}").encode()).is_none());
    }

    #[test]
    fn reads_and_answers_requests() {
        assert_eq!(request(&message(6, 6, r#"{"toggle":1}"#)), Some(Request::Start));
        assert_eq!(request(&message(6, 6, r#"{"toggle":0}"#)), Some(Request::Quit));
        assert_eq!(request(&message(9, 1, "{}")), Some(Request::Start));
        assert_eq!(request(&message(9, 2, "{}")), Some(Request::Quit));
        assert_eq!(request(&message(1, 1, "{}")), None);
        let (ids, payload) = answer(&message(9, 1, "{}"), 40000);
        assert_eq!(ids, (9, 101));
        assert_eq!((payload["udpServerPort"].as_u64(), payload["resultType"].as_i64()), (Some(40000), Some(1)));
        assert_eq!(answer(&message(9, 1, "{}"), 0).1["resultType"], -100);
    }
}
