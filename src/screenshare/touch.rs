//! touch.rs
//! Touchback: the panel asks the PC for a UDP port on the command channel, then sends each
//! touch on it as a pointer event. Read from the Windows client (touchback service) and the
//! Android sender's message table; unverified on a real panel. With the Touchback setting, the
//! touches move the desktop's pointer through the portal (capture/portal.rs).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

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

/// Touchback request in `m`, if it is one. Hypothesis for the Windows form: an unknown command
/// type with id 1 (start) or 2 (quit).
pub fn request(m: &ScreenshareMessage) -> Option<Request> {
    if (m.command_type, m.command_id) == ANDROID_REQUEST {
        let toggle = serde_json::from_str::<Value>(&m.payload).ok()?["toggle"].as_i64();
        return Some(if toggle == Some(0) { Request::Quit } else { Request::Start });
    }
    match (KNOWN_TYPES.contains(&m.command_type), m.command_id) {
        (false, 1) => Some(Request::Start),
        (false, 2) => Some(Request::Quit),
        _ => None,
    }
}

/// Answer to a start request: same type, id + 100; port 0 when the socket could not open.
/// The keys follow Android's TouchbackPortResponse; the Windows client fills the same values.
pub fn answer(m: &ScreenshareMessage, port: u16) -> ((i32, i32), Value) {
    let (result, reason) = if port == 0 { (-100, "Request failed.") } else { (1, "Request success.") };
    let payload = json!({"resultType": result, "failReason": reason, "udpServerPort": port});
    ((m.command_type, m.command_id + 100), payload)
}

/// Touchback state of one cast: the UDP port and the stop flag of its reading thread.
pub struct Touchback {
    running: Option<(u16, &'static AtomicBool)>,
    /// The desktop's pointer; None when the Touchback setting is off.
    input: Option<Arc<Input>>,
    /// Size of the video the panel shows, in pixels.
    video: (f64, f64),
}

impl Touchback {
    pub fn new(input: Option<Arc<Input>>, (width, height): (u32, u32)) -> Self {
        Self { running: None, input, video: (f64::from(width), f64::from(height)) }
    }

    /// Act on a touchback request in `m`; return the answer to send, if any.
    pub fn handle(&mut self, m: &ScreenshareMessage) -> Option<((i32, i32), Value)> {
        match request(m)? {
            Request::Quit => {
                self.stop();
                None
            }
            Request::Start => {
                // Refused when off: the panel says so instead of showing touches that do nothing.
                let Some(input) = self.input.clone() else {
                    tracing::info!("touchback asked by the panel, but the Touchback setting is off");
                    return Some(answer(m, 0));
                };
                if self.running.is_none() {
                    // ponytail: leaks one flag per start (a byte), the thread wants a 'static flag.
                    let ended: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
                    let video = self.video;
                    match listen(ended, move |touch| apply(&input, &touch, video)) {
                        Ok(port) => self.running = Some((port, ended)),
                        Err(e) => tracing::warn!("touchback socket: {e}"),
                    }
                }
                Some(answer(m, self.running.map_or(0, |(port, _)| port)))
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

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Button { Left, Right }

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Action { Press(Button), Release(Button), Move, Click(Button), DoubleClick, Wheel }

/// One touch. Which of the two coordinate pairs the Windows client uses, and in which unit
/// (screen pixels or 0 to 1), is unverified: both are kept.
#[derive(Debug, PartialEq)]
pub struct TouchEvent {
    pub action: Action,
    pub x: f64,
    pub y: f64,
    pub point_x: f64,
    pub point_y: f64,
}

/// Event from one UDP datagram: a protobuf message without length, JSON payload (UdpPoint).
pub fn decode(datagram: &[u8]) -> Option<TouchEvent> {
    let m = ScreenshareMessage::decode(datagram).ok()?;
    if m.command_type != POINTER_TYPE {
        return None;
    }
    let action = match m.command_id {
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
    let point: Value = serde_json::from_str(&m.payload).ok()?;
    let get = |key: &str| point[key].as_f64().unwrap_or(0.0);
    Some(TouchEvent { action, x: get("x"), y: get("y"), point_x: get("pointX"), point_y: get("pointY") })
}

/// Point of the shared screen under (`x`, `y`) of the video, which shows the screen
/// letterboxed (capture/encode.rs). None in the black bands.
pub fn to_screen((x, y): (f64, f64), video: (f64, f64), screen: (f64, f64)) -> Option<(f64, f64)> {
    let scale = (video.0 / screen.0).min(video.1 / screen.1);
    let (left, top) = ((video.0 - screen.0 * scale) / 2.0, (video.1 - screen.1 * scale) / 2.0);
    let (sx, sy) = ((x - left) / scale, (y - top) / scale);
    (sx >= 0.0 && sy >= 0.0 && sx < screen.0 && sy < screen.1).then_some((sx, sy))
}

/// Touch position in video pixels. Hypothesis, from the Windows client: `x, y` are pixels of
/// the video size sent in the cast request; `pointX, pointY` are read as 0 to 1 when only they
/// are filled.
fn video_point(touch: &TouchEvent, video: (f64, f64)) -> (f64, f64) {
    let unit = 0.0..=1.0;
    if touch.x == 0.0 && touch.y == 0.0 && unit.contains(&touch.point_x) && unit.contains(&touch.point_y) {
        (touch.point_x * video.0, touch.point_y * video.1)
    } else {
        (touch.x, touch.y)
    }
}

/// Move the desktop's pointer as `touch` says.
fn apply(input: &Input, touch: &TouchEvent, video: (f64, f64)) {
    tracing::debug!(?touch, "touch");
    if touch.action == Action::Wheel {
        // Hypothesis: the sign of y gives the direction.
        if touch.y != 0.0 {
            input.wheel(touch.y.signum() as i32);
        }
        return;
    }
    let Some((x, y)) = to_screen(video_point(touch, video), video, input.size) else { return };
    input.move_to(x, y);
    let click = |right: bool| {
        input.button(right, true);
        input.button(right, false);
    };
    match touch.action {
        Action::Press(button) => input.button(button == Button::Right, true),
        Action::Release(button) => input.button(button == Button::Right, false),
        Action::Click(button) => click(button == Button::Right),
        Action::DoubleClick => (0..2).for_each(|_| click(false)),
        Action::Move | Action::Wheel => {}
    }
}

/// Open a UDP socket on any port and read touches on it until `ended`, passing each to `on_touch`.
fn listen(ended: &'static AtomicBool, mut on_touch: impl FnMut(TouchEvent) + Send + 'static) -> std::io::Result<u16> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let port = socket.local_addr()?.port();
    std::thread::spawn(move || {
        let mut buffer = [0; 2048];
        while !ended.load(Ordering::SeqCst) {
            let Ok((len, from)) = socket.recv_from(&mut buffer) else { continue };
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
        assert_eq!(touch, TouchEvent { action: Action::Press(Button::Left), x: 480.0, y: 540.0, point_x: 0.25, point_y: 0.5 });
        assert!(decode(&message(1, 21, "{}").encode()).is_none());
        assert!(decode(&message(3, 99, "{}").encode()).is_none());
    }

    #[test]
    fn maps_touches_to_the_letterboxed_screen() {
        // A 1920x1200 screen in 1920x1080 video: 1728x1080 image, bands of 96 px left and right.
        let (video, screen) = ((1920.0, 1080.0), (1920.0, 1200.0));
        assert_eq!(to_screen((96.0, 0.0), video, screen), Some((0.0, 0.0)));
        assert_eq!(to_screen((960.0, 540.0), video, screen), Some((960.0, 600.0)));
        assert_eq!(to_screen((50.0, 540.0), video, screen), None);
        // Same shape: pixels scale straight.
        assert_eq!(to_screen((640.0, 360.0), (1280.0, 720.0), video), Some((960.0, 540.0)));
        let only_unit = TouchEvent { action: Action::Move, x: 0.0, y: 0.0, point_x: 0.5, point_y: 0.25 };
        assert_eq!(video_point(&only_unit, video), (960.0, 270.0));
    }

    #[test]
    fn reads_and_answers_requests() {
        assert_eq!(request(&message(6, 6, r#"{"toggle":1}"#)), Some(Request::Start));
        assert_eq!(request(&message(6, 6, r#"{"toggle":0}"#)), Some(Request::Quit));
        assert_eq!(request(&message(9, 1, "{}")), Some(Request::Start));
        assert_eq!(request(&message(9, 2, "{}")), Some(Request::Quit));
        assert_eq!(request(&message(1, 1, "{}")), None);
        let (ids, payload) = answer(&message(9, 1, "{}"), 40000);
        assert_eq!((ids, payload["udpServerPort"].as_u64(), payload["resultType"].as_i64()), ((9, 101), Some(40000), Some(1)));
        assert_eq!(answer(&message(9, 1, "{}"), 0).1["resultType"], -100);
    }
}
