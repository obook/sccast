//! Launch menu: pick a device found on the network and adjust the stream settings.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::cast::{DEFAULT_MEDIA_RECEIVER, SCCAST_RECEIVER};
use crate::cli::{Format, Stream};
use crate::discovery::{self, CastDevice};

/// Settings the menu edits, prefilled from the command line.
pub struct Settings {
    pub stream: Stream,
    pub audio_delay: u64,
    pub latency: Option<f64>,
    pub receiver: String,
}

/// Name, resolution, bitrate. "Entry-level TV" comes from the Sharp tests: 4000 kbit/s froze on moving content.
const PRESETS: [(&str, (u32, u32), u32); 3] = [
    ("Entry-level TV", (1920, 1080), 2500),
    ("Full HD", (1920, 1080), 4000),
    ("720p", (1280, 720), 2500),
];
const RESOLUTIONS: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (854, 480)];
const BITRATES: [u32; 6] = [1500, 2500, 4000, 6000, 8000, 10000];
const FPS: [u32; 4] = [25, 30, 50, 60];
/// None: the receiver page's default (1 s).
const LATENCIES: [Option<f64>; 5] = [None, Some(1.5), Some(2.0), Some(3.0), Some(5.0)];
const AUDIO_DELAYS: [u64; 9] = [0, 100, 200, 300, 400, 500, 750, 1000, 1500];

const ROWS: [&str; 9] = [
    "Preset", "Resolution", "Bitrate", "Frame rate", "Latency", "Sound", "Audio delay", "Receiver", "Format",
];
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Cyan;

enum Found {
    Device(CastDevice),
    Done,
}

/// Show the menu until the user casts (Some(device)) or quits (None).
pub fn run(settings: &mut Settings) -> anyhow::Result<Option<CastDevice>> {
    let mut terminal = ratatui::init();
    let result = Menu::new().run(&mut terminal, settings);
    ratatui::restore();
    result
}

struct Menu {
    devices: BTreeMap<String, CastDevice>,
    device: ListState,
    row: ListState,
    on_settings: bool,
    scanning: bool,
    tick: usize,
    tx: Sender<Found>,
    rx: Receiver<Found>,
}

impl Menu {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut menu = Self {
            devices: BTreeMap::new(),
            device: ListState::default().with_selected(Some(0)),
            row: ListState::default().with_selected(Some(0)),
            on_settings: false,
            scanning: false,
            tick: 0,
            tx,
            rx,
        };
        menu.scan();
        menu
    }

    /// Browse in a thread; devices show up as they answer.
    fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Stop early once the menu is gone (send fails).
            let result = discovery::browse(Duration::from_secs(8), |d| tx.send(Found::Device(d)).is_err());
            if let Err(e) = result {
                tracing::warn!("discovery failed: {e:#}");
            }
            let _ = tx.send(Found::Done);
        });
    }

    /// Devices sorted by name, as listed.
    fn list(&self) -> Vec<&CastDevice> {
        let mut list: Vec<_> = self.devices.values().collect();
        list.sort_by_key(|d| d.name.to_lowercase());
        list
    }

    fn run(&mut self, terminal: &mut DefaultTerminal, s: &mut Settings) -> anyhow::Result<Option<CastDevice>> {
        loop {
            while let Ok(found) = self.rx.try_recv() {
                match found {
                    // Keyed by id: a device answers several times.
                    Found::Device(d) => drop(self.devices.insert(d.id.clone(), d)),
                    Found::Done => self.scanning = false,
                }
            }
            terminal.draw(|f| self.draw(f, s))?;
            self.tick += 1;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let count = if self.on_settings { ROWS.len() } else { self.devices.len() };
            let state = if self.on_settings { &mut self.row } else { &mut self.device };
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                    state.select(Some(state.selected().unwrap_or(0).checked_sub(1).unwrap_or(count - 1)));
                }
                KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                    state.select(Some((state.selected().unwrap_or(0) + 1) % count));
                }
                KeyCode::Tab | KeyCode::BackTab => self.on_settings = !self.on_settings,
                KeyCode::Left | KeyCode::Char('h') if self.on_settings => change(s, self.row.selected().unwrap_or(0), -1),
                KeyCode::Right | KeyCode::Char('l') if self.on_settings => change(s, self.row.selected().unwrap_or(0), 1),
                KeyCode::Char('r') => self.scan(),
                KeyCode::Enter => {
                    let list = self.list();
                    if let Some(device) = self.device.selected().and_then(|i| list.get(i)) {
                        return Ok(Some((*device).clone()));
                    }
                }
                _ => {}
            }
        }
    }

    fn draw(&mut self, f: &mut Frame, s: &Settings) {
        let [header, body, footer] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(ROWS.len() as u16 + 2), Constraint::Length(3)])
                .areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body);

        let title = Line::from(vec![
            Span::styled(" sccast ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled("screen & sound to Chromecast", Style::new().fg(Color::Gray).italic()),
        ]);
        f.render_widget(Paragraph::new(title).block(rounded(false)), header);

        // Devices.
        let mut items: Vec<ListItem> = self
            .list()
            .iter()
            .map(|d| {
                ListItem::new(Line::from(vec![
                    Span::styled(d.name.clone(), Style::new().bold()),
                    Span::styled(format!("  {}", d.model), Style::new().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        if self.scanning {
            let frame = SPINNER[self.tick % SPINNER.len()];
            items.push(ListItem::new(Span::styled(format!("{frame} searching..."), Style::new().fg(ACCENT))));
        } else if items.is_empty() {
            items.push(ListItem::new(Span::styled("No device found. r: search again", Style::new().fg(Color::Yellow))));
        }
        let devices = List::new(items)
            .block(rounded(!self.on_settings).title(" Devices "))
            .highlight_symbol("▶ ")
            .highlight_style(highlight(!self.on_settings));
        f.render_stateful_widget(devices, left, &mut self.device);

        // Settings.
        let items: Vec<ListItem> = ROWS
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let arrows = Style::new().fg(if self.on_settings { ACCENT } else { Color::DarkGray });
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{label:<12}"), Style::new().fg(Color::Gray)),
                    Span::styled("◀ ", arrows),
                    Span::styled(value(s, i), Style::new().bold()),
                    Span::styled(" ▶", arrows),
                ]))
            })
            .collect();
        let rows = List::new(items)
            .block(rounded(self.on_settings).title(" Settings "))
            .highlight_symbol("▶ ")
            .highlight_style(highlight(self.on_settings));
        f.render_stateful_widget(rows, right, &mut self.row);

        let keys = [("↑↓", "move"), ("←→", "change"), ("Tab", "switch"), ("r", "rescan"), ("Enter", "cast"), ("q", "quit")];
        let help: Vec<Span> = keys
            .iter()
            .flat_map(|(key, what)| {
                [Span::styled(format!(" {key} "), Style::new().fg(Color::Yellow).bold()), Span::raw(format!("{what}  "))]
            })
            .collect();
        f.render_widget(Paragraph::new(Line::from(help)).block(rounded(false)), footer);
    }
}

fn rounded(focused: bool) -> Block<'static> {
    let color = if focused { ACCENT } else { Color::DarkGray };
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(color))
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().bg(Color::Rgb(0, 60, 80)).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    }
}

/// Next (dir 1) or previous (dir -1) entry of `list`, wrapping. A value not in the list jumps to its first entry.
fn cycle<T: PartialEq + Copy>(list: &[T], current: T, dir: isize) -> T {
    let len = list.len() as isize;
    let next = list.iter().position(|v| *v == current).map_or(0, |i| (i as isize + dir).rem_euclid(len));
    list[next as usize]
}

/// Index of the preset matching the current settings.
fn preset(s: &Settings) -> Option<usize> {
    PRESETS.iter().position(|(_, res, rate)| *res == s.stream.resolution && *rate == s.stream.bitrate)
}

fn change(s: &mut Settings, row: usize, dir: isize) {
    let st = &mut s.stream;
    match row {
        0 => {
            let i = preset(s).map_or(0, |i| (i as isize + dir).rem_euclid(PRESETS.len() as isize) as usize);
            (_, s.stream.resolution, s.stream.bitrate) = PRESETS[i];
        }
        1 => st.resolution = cycle(&RESOLUTIONS, st.resolution, dir),
        2 => st.bitrate = cycle(&BITRATES, st.bitrate, dir),
        3 => st.fps = cycle(&FPS, st.fps, dir),
        4 => s.latency = cycle(&LATENCIES, s.latency, dir),
        5 => st.no_audio = !st.no_audio,
        6 => s.audio_delay = cycle(&AUDIO_DELAYS, s.audio_delay, dir),
        7 => s.receiver = cycle(&[SCCAST_RECEIVER, DEFAULT_MEDIA_RECEIVER], s.receiver.as_str(), dir).to_string(),
        _ => st.format = if st.format == Format::Hls { Format::Mkv } else { Format::Hls },
    }
}

fn value(s: &Settings, row: usize) -> String {
    let st = &s.stream;
    match row {
        0 => preset(s).map_or("Custom", |i| PRESETS[i].0).to_string(),
        1 => format!("{}x{}", st.resolution.0, st.resolution.1),
        2 => format!("{} kbit/s", st.bitrate),
        3 => format!("{} fps", st.fps),
        4 => s.latency.map_or("1 s (default)".to_string(), |l| format!("{l} s")),
        5 => if st.no_audio { "off" } else { "on" }.to_string(),
        6 => format!("{} ms", s.audio_delay),
        7 => match s.receiver.as_str() {
            SCCAST_RECEIVER => "sccast".to_string(),
            DEFAULT_MEDIA_RECEIVER => "Google".to_string(),
            other => other.to_string(),
        },
        _ => if st.format == Format::Hls { "HLS" } else { "Matroska" }.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_and_wraps() {
        assert_eq!(cycle(&[1, 2, 3], 3, 1), 1);
        assert_eq!(cycle(&[1, 2, 3], 1, -1), 3);
        assert_eq!(cycle(&[1, 2, 3], 7, 1), 1);
    }

    #[test]
    fn draws_and_applies_presets() {
        let stream = Stream {
            resolution: (1920, 1080),
            fps: 30,
            bitrate: 4000,
            port: 8090,
            no_audio: false,
            format: Format::Hls,
        };
        let mut s = Settings { stream, audio_delay: 0, latency: None, receiver: SCCAST_RECEIVER.into() };
        change(&mut s, 0, -1);
        assert_eq!(value(&s, 0), "Entry-level TV");
        assert_eq!(s.stream.bitrate, 2500);

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(64, 17)).unwrap();
        let mut menu = Menu::new();
        menu.devices.insert("x".into(), CastDevice {
            id: "x".into(),
            name: "Living Room TV".into(),
            model: "Chromecast".into(),
            ip: std::net::Ipv4Addr::LOCALHOST,
            port: 8009,
        });
        terminal.draw(|f| menu.draw(f, &s)).unwrap();
        let screen: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        for line in screen.chars().collect::<Vec<_>>().chunks(64) {
            println!("{}", line.iter().collect::<String>());
        }
        assert!(screen.contains("Living Room TV") && screen.contains("Entry-level TV") && screen.contains("HLS"));
    }
}
