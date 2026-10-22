//! tests.rs
//! Tests of the menu: settings that cycle, alert thresholds, and the screen drawn with
//! made-up devices (also the source of media/menu.png).
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier};

use super::live::Live;
use super::settings::{change, cycle, value};
use super::*;
use crate::cli::{Format, Stream};
use crate::device::SCCAST_RECEIVER;

#[test]
fn cycles_and_wraps() {
    assert_eq!(cycle(&[1, 2, 3], 3, 1), 1);
    assert_eq!(cycle(&[1, 2, 3], 1, -1), 3);
    assert_eq!(cycle(&[1, 2, 3], 7, 1), 1);
}

#[test]
fn audio_delay_moves_by_50_ms_and_stops_at_the_ends() {
    let mut s = settings(2500);
    change(&mut s, AUDIO_DELAY_ROW, -1);
    assert_eq!(s.audio_delay, -50);
    s.audio_delay = -500;
    change(&mut s, AUDIO_DELAY_ROW, -1);
    assert_eq!(s.audio_delay, -500);
    s.audio_delay = 1500;
    change(&mut s, AUDIO_DELAY_ROW, 1);
    assert_eq!(s.audio_delay, 1500);
}

#[test]
fn alerts_follow_thresholds() {
    let mut live = Live::default();
    assert!(live.alert().is_none());
    live.round_trip = Some(Some(Duration::from_millis(250)));
    assert_eq!(live.alert().unwrap().1, Color::Yellow);
    live.fetch = Some(Duration::from_millis(950));
    assert!(live.alert().unwrap().0.starts_with("Network slow: 1 s of video"));
    assert_eq!(live.alert().unwrap().1, Color::Red);
    live.buffering = vec![Instant::now(); 3];
    assert!(live.alert().unwrap().0.starts_with("The TV buffered 3 times"));
    live.dropped_history = VecDeque::from([0, 45]);
    assert!(live.alert().unwrap().0.starts_with("PC too slow"));
    live.round_trip = Some(None);
    assert!(live.alert().unwrap().0.contains("does not answer"));
}

fn device(id: &str, name: &str, model: &str) -> CastDevice {
    CastDevice { id: id.into(), name: name.into(), model: model.into(), ip: std::net::Ipv4Addr::LOCALHOST, port: 8009 }
}

fn settings(bitrate: u32) -> Settings {
    let stream = Stream { resolution: (1920, 1080), fps: 30, bitrate, port: 8090, no_audio: false, format: Format::Hls };
    Settings { stream, audio_delay: 0, latency: 1.5, receiver: SCCAST_RECEIVER.into() }
}

#[test]
fn user_resolution_comes_back_after_a_device_lowered_it() {
    let mut menu = Menu::idle();
    let mut s = settings(2500);
    // The Nest Hub took 1280x720: the menu shows it.
    menu.device_resolution(&mut s, (1280, 720));
    assert_eq!(s.stream.resolution, (1280, 720));
    // The next cast (the Sharp) starts from the user's 1920x1080.
    menu.restore_resolution(&mut s);
    assert_eq!(s.stream.resolution, (1920, 1080));
    // A resolution set by hand stays.
    menu.device_resolution(&mut s, (1280, 720));
    menu.chosen_resolution = None;
    s.stream.resolution = (854, 480);
    menu.restore_resolution(&mut s);
    assert_eq!(s.stream.resolution, (854, 480));
}

#[test]
fn long_device_list_scrolls() {
    let mut menu = Menu::idle();
    for i in 0..30 {
        let id = format!("d{i:02}");
        menu.devices.insert(id.clone(), device(&id, &format!("Room {i:02}"), "Chromecast"));
    }
    menu.device.select(Some(25));
    let s = settings(2500);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
    terminal.draw(|f| menu.draw(f, &s)).unwrap();
    let screen: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    assert!(screen.contains("Devices (30)"));
    // The selected device is scrolled into view, and the scrollbar thumb is drawn.
    assert!(screen.contains("Room 25") && screen.contains('█'));
}

#[test]
fn draws_and_applies_presets() {
    let mut s = settings(4000);
    change(&mut s, 0, -1);
    assert_eq!(value(&s, 0), "Entry-level TV");
    assert_eq!(s.stream.bitrate, 2500);

    // Made-up devices: no network search, so no real device shows up.
    let mut menu = Menu::idle();
    let fake = [("tv", "Living Room TV", "Chromecast"), ("hub", "Kitchen", "Google Nest Hub"), ("pj", "Classroom", "Google TV")];
    for (id, name, model) in fake {
        menu.devices.insert(id.into(), device(id, name, model));
    }
    menu.device.select(Some(1));
    menu.set_casting(Some("tv".into()));
    menu.set_status(Status::Player("Playing".into()));
    menu.set_status(Status::Info("Casting to Living Room TV. Ctrl+C stops.".into()));
    menu.live.started = Instant::now().checked_sub(Duration::from_secs(754));
    menu.live.buffering = vec![Instant::now().checked_sub(Duration::from_secs(400)).unwrap()];
    menu.live.cpu = 62.0;
    menu.live.fetch = Some(Duration::from_millis(120));
    menu.live.round_trip = Some(Some(Duration::from_millis(3)));
    menu.live.bitrate = (0..60).map(|i| 2500 + (i * 37 % 11) * 40 - 200).collect();

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 26)).unwrap();
    terminal.draw(|f| menu.draw(f, &s)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen: Vec<&str> = buffer.content().iter().map(|c| c.symbol()).collect();
    for line in screen.chunks(80) {
        println!("{}", line.concat());
    }
    let text = screen.concat();
    assert!(text.contains("Living Room TV") && text.contains("Entry-level TV") && text.contains("HLS"));
    assert!(text.contains("00:12:34") && text.contains("Bitrate"));

    // SCCAST_SCREENSHOT=file.html cargo test draws: the same screen as a colored page, for media/.
    if let Ok(path) = std::env::var("SCCAST_SCREENSHOT") {
        std::fs::write(path, html(buffer)).unwrap();
    }
}

fn html(buffer: &ratatui::buffer::Buffer) -> String {
    let css = |c: Color, default: &str| match c {
        Color::Reset => default.to_string(),
        Color::Black => "#1e1e2e".into(),
        Color::Gray => "#a6adc8".into(),
        Color::DarkGray => "#585b70".into(),
        Color::Green => "#a6e3a1".into(),
        Color::Yellow => "#f9e2af".into(),
        Color::Red => "#f38ba8".into(),
        Color::Cyan => "#89dceb".into(),
        Color::White => "#ffffff".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => default.to_string(),
    };
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><body style=\"margin:0;background:#1e1e2e\">\
         <pre style=\"margin:16px;font:16px/1.15 'DejaVu Sans Mono',monospace;color:#cdd6f4\">",
    );
    for (i, cell) in buffer.content().iter().enumerate() {
        if i > 0 && i % buffer.area.width as usize == 0 {
            out.push('\n');
        }
        let weight = if cell.modifier.contains(Modifier::BOLD) { "bold" } else { "normal" };
        let symbol = match cell.symbol() {
            "<" => "&lt;",
            ">" => "&gt;",
            "&" => "&amp;",
            s => s,
        };
        out.push_str(&format!(
            "<span style=\"color:{};background:{};font-weight:{weight}\">{symbol}</span>",
            css(cell.fg, "#cdd6f4"),
            css(cell.bg, "transparent"),
        ));
    }
    out + "</pre>"
}
