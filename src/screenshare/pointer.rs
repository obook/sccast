//! pointer.rs
//! A panel's touch, and how it moves the desktop's pointer: from the video the panel shows
//! back to the shared screen, then through the portal (capture/portal.rs). The touches
//! arrive in touch.rs. Unverified on a real panel.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use crate::capture::Input;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Button {
    Left,
    Right,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Action {
    Press(Button),
    Release(Button),
    Move,
    Click(Button),
    DoubleClick,
    Wheel,
}

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

/// Point of the shared screen under (`x`, `y`) of the video, which shows the screen
/// letterboxed (capture/encode.rs). None in the black bands.
pub fn to_screen((x, y): (f64, f64), video: (f64, f64), screen: (f64, f64)) -> Option<(f64, f64)> {
    let scale = (video.0 / screen.0).min(video.1 / screen.1);
    let left_band = (video.0 - screen.0 * scale) / 2.0;
    let top_band = (video.1 - screen.1 * scale) / 2.0;
    let screen_x = (x - left_band) / scale;
    let screen_y = (y - top_band) / scale;
    let inside = screen_x >= 0.0 && screen_y >= 0.0 && screen_x < screen.0 && screen_y < screen.1;
    inside.then_some((screen_x, screen_y))
}

/// Touch position in video pixels. Hypothesis, from the Windows client: `x, y` are pixels of
/// the video size sent in the cast request; `pointX, pointY` are read as 0 to 1 when only they
/// are filled.
fn video_point(touch: &TouchEvent, video: (f64, f64)) -> (f64, f64) {
    let unit = 0.0..=1.0;
    let only_unit_pair = touch.x == 0.0 && touch.y == 0.0 && unit.contains(&touch.point_x) && unit.contains(&touch.point_y);
    if only_unit_pair {
        (touch.point_x * video.0, touch.point_y * video.1)
    } else {
        (touch.x, touch.y)
    }
}

/// Move the desktop's pointer as `touch` says, `video` being the size the panel shows.
pub fn apply(input: &Input, touch: &TouchEvent, video: (f64, f64)) {
    tracing::debug!(?touch, "touch");
    if touch.action == Action::Wheel {
        // Hypothesis: the sign of y gives the direction.
        if touch.y != 0.0 {
            input.wheel(touch.y.signum() as i32);
        }
        return;
    }
    let Some((x, y)) = to_screen(video_point(touch, video), video, input.size) else {
        return;
    };
    input.move_to(x, y);
    let click = |right: bool| {
        input.button(right, true);
        input.button(right, false);
    };
    match touch.action {
        Action::Press(button) => input.button(button == Button::Right, true),
        Action::Release(button) => input.button(button == Button::Right, false),
        Action::Click(button) => click(button == Button::Right),
        Action::DoubleClick => {
            click(false);
            click(false);
        }
        Action::Move | Action::Wheel => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
