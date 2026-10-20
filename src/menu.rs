//! Launch menu: pick a device found on the network and adjust the stream settings.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Sparkline};
use ratatui::{DefaultTerminal, Frame};

use crate::cast::{DEFAULT_MEDIA_RECEIVER, SCCAST_RECEIVER};
use crate::cli::{Format, Stream};
use crate::discovery::{self, CastDevice};

/// Settings the menu edits, prefilled from the command line.
#[derive(Clone)]
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

/// What the user asked for.
pub enum Action {
    /// Cast to this device, stopping the current cast first.
    Cast(CastDevice),
    Stop,
    /// Choose the screen again in the portal dialog.
    PickScreen,
    Quit,
}

/// Progress of the running cast, sent by its thread.
pub enum Status {
    Info(String),
    Error(String),
    Ended,
    /// Player state reported by the TV (Playing, Buffering...).
    Player(String),
    Reconnecting,
    /// Directory of the HLS segments, to measure the real bitrate.
    Segments(PathBuf),
    /// Bytes sent so far by the Matroska server, for the same measure.
    Served(ByteCounter),
}

pub type ByteCounter = Box<dyn Fn() -> u64 + Send>;

/// Stats of the current cast, shown in the Live panel.
#[derive(Default)]
struct Live {
    started: Option<Instant>,
    tv: String,
    /// Start of each buffering period.
    buffering: Vec<Instant>,
    reconnects: u32,
    segments: Option<PathBuf>,
    /// Matroska byte counter, and its last reading.
    served: Option<(ByteCounter, Option<(u64, Instant)>)>,
    /// kbit/s, one sample per second, last minute.
    bitrate: Vec<u64>,
    /// sccast's CPU time in clock ticks, and when it was read.
    cpu_ticks: Option<(u64, Instant)>,
    /// Percent of one core.
    cpu: f64,
    sampled: Option<Instant>,
}

impl Live {
    /// Refresh the measures once a second.
    fn sample(&mut self) {
        if self.sampled.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.sampled = Some(Instant::now());
        if let Some(dir) = &self.segments {
            // Segments written in the last 5 s: about 5 one-second segments.
            let bytes: u64 = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() < 5))
                .map(|m| m.len())
                .sum();
            self.bitrate.push(bytes * 8 / 5 / 1000);
        }
        if let Some((counter, last)) = &mut self.served {
            let bytes = counter();
            if let Some((before, at)) = *last {
                self.bitrate.push((bytes.saturating_sub(before) as f64 * 8.0 / 1000.0 / at.elapsed().as_secs_f64()) as u64);
            }
            *last = Some((bytes, Instant::now()));
        }
        if self.bitrate.len() > 60 {
            self.bitrate.remove(0);
        }
        if let Some(ticks) = cpu_ticks() {
            if let Some((before, at)) = self.cpu_ticks {
                // ponytail: assumes 100 ticks per second (the Linux default), read sysconf if it ever differs.
                self.cpu = (ticks - before) as f64 / at.elapsed().as_secs_f64();
            }
            self.cpu_ticks = Some((ticks, Instant::now()));
        }
    }
}

/// utime + stime of this process, from /proc/self/stat (fields 14 and 15).
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name can hold spaces: count fields after its closing parenthesis.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace().skip(11);
    Some(fields.next()?.parse::<u64>().ok()? + fields.next()?.parse::<u64>().ok()?)
}

fn load_average() -> Option<String> {
    let load = std::fs::read_to_string("/proc/loadavg").ok()?;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    Some(format!("{}/{cpus}", load.split_whitespace().next()?))
}

pub struct Menu {
    devices: BTreeMap<String, CastDevice>,
    device: ListState,
    row: ListState,
    on_settings: bool,
    scanning: bool,
    tick: usize,
    tx: Sender<Found>,
    rx: Receiver<Found>,
    status: Status,
    /// Id of the device being cast to.
    casting: Option<String>,
    status_tx: Sender<Status>,
    status_rx: Receiver<Status>,
    live: Live,
}

impl Menu {
    pub fn new() -> Self {
        let mut menu = Self::idle();
        menu.scan();
        menu
    }

    /// A menu that does not search the network yet.
    fn idle() -> Self {
        let (tx, rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::channel();
        Self {
            devices: BTreeMap::new(),
            device: ListState::default().with_selected(Some(0)),
            row: ListState::default().with_selected(Some(0)),
            on_settings: false,
            scanning: false,
            tick: 0,
            tx,
            rx,
            status: Status::Info("Choose a device, then Enter".into()),
            casting: None,
            status_tx,
            status_rx,
            live: Live::default(),
        }
    }

    /// Channel for a new cast thread to report its progress. Replaces the previous one, so a
    /// stopped cast's late messages (its "ended") are dropped instead of ending the new cast.
    pub fn status_sender(&mut self) -> Sender<Status> {
        (self.status_tx, self.status_rx) = mpsc::channel();
        self.status_tx.clone()
    }

    /// Current channel, for messages that are not from a cast (screen picker).
    pub fn current_sender(&self) -> Sender<Status> {
        self.status_tx.clone()
    }

    pub fn set_status(&mut self, status: Status) {
        match status {
            Status::Player(state) => {
                if state == "Buffering" && self.live.tv != "Buffering" {
                    self.live.buffering.push(Instant::now());
                }
                self.live.tv = state;
            }
            Status::Reconnecting => self.live.reconnects += 1,
            Status::Segments(dir) => self.live.segments = Some(dir),
            Status::Served(counter) => self.live.served = Some((counter, None)),
            Status::Info(_) => self.status = status,
            Status::Error(_) | Status::Ended => {
                self.casting = None;
                self.live.started = None;
                self.status = status;
            }
        }
    }

    /// Mark the device being cast to; a new cast starts new stats.
    pub fn set_casting(&mut self, id: Option<String>) {
        if id.is_some() {
            self.live = Live { started: Some(Instant::now()), ..Live::default() };
        }
        self.casting = id;
    }

    /// Redraw before a blocking step (portal dialog, stopping).
    pub fn draw_now(&mut self, terminal: &mut DefaultTerminal, s: &Settings) -> anyhow::Result<()> {
        terminal.draw(|f| self.draw(f, s))?;
        Ok(())
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

    /// Draw and handle keys until the user asks for something. `quit` is set by SIGTERM.
    pub fn next(&mut self, terminal: &mut DefaultTerminal, s: &mut Settings, quit: &AtomicBool) -> anyhow::Result<Action> {
        loop {
            if quit.load(Ordering::SeqCst) {
                return Ok(Action::Quit);
            }
            while let Ok(found) = self.rx.try_recv() {
                match found {
                    // Keyed by id: a device answers several times.
                    Found::Device(d) => drop(self.devices.insert(d.id.clone(), d)),
                    Found::Done => self.scanning = false,
                }
            }
            while let Ok(status) = self.status_rx.try_recv() {
                self.set_status(status);
            }
            if self.live.started.is_some() {
                self.live.sample();
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
                // Raw mode turns Ctrl+C into a key, not a signal.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(Action::Quit),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
                KeyCode::Char('s') if self.casting.is_some() => return Ok(Action::Stop),
                KeyCode::Char('p') => return Ok(Action::PickScreen),
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
                        return Ok(Action::Cast((*device).clone()));
                    }
                }
                _ => {}
            }
        }
    }

    fn draw(&mut self, f: &mut Frame, s: &Settings) {
        let live_height = if self.live.started.is_some() { 6 } else { 0 };
        let [header, body, live, footer] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Min(ROWS.len() as u16 + 2),
            Constraint::Length(live_height),
            Constraint::Length(3),
        ])
        .areas(f.area());
        if live_height > 0 {
            self.draw_live(f, live);
        }
        let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(body);

        let title = Line::from(vec![
            Span::styled(" sccast ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  "),
            Span::styled("screen & sound to Chromecast", Style::new().fg(Color::Gray).italic()),
        ]);
        let (text, color) = match &self.status {
            Status::Info(m) => (m.as_str(), if self.casting.is_some() { Color::Green } else { Color::Yellow }),
            Status::Error(m) => (m.as_str(), Color::Red),
            // Stats never become the status line (see set_status).
            _ => ("Stopped", Color::Gray),
        };
        // A message ending with "..." is a step in progress: animate it.
        let text = if text.ends_with("...") {
            format!("{} {text}", SPINNER[self.tick % SPINNER.len()])
        } else {
            text.to_string()
        };
        // Status on its own line: errors are long.
        let status = Line::from(Span::styled(format!(" {text}"), Style::new().fg(color).add_modifier(Modifier::BOLD)));
        f.render_widget(
            Paragraph::new(vec![title, status]).wrap(ratatui::widgets::Wrap { trim: false }).block(rounded(false)),
            header,
        );

        // Devices.
        let mut items: Vec<ListItem> = self
            .list()
            .iter()
            .map(|d| {
                let live = self.casting.as_deref() == Some(d.id.as_str());
                ListItem::new(Line::from(vec![
                    Span::styled(if live { "● " } else { "" }, Style::new().fg(Color::Green)),
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

        let mut keys =
            vec![("↑↓", "move"), ("←→", "change"), ("Tab", "switch"), ("r", "rescan"), ("p", "screen"), ("Enter", "cast")];
        if self.casting.is_some() {
            keys.push(("s", "stop"));
        }
        keys.push(("q", "quit"));
        let help: Vec<Span> = keys
            .iter()
            .flat_map(|(key, what)| {
                [Span::styled(format!("{key} "), Style::new().fg(Color::Yellow).bold()), Span::raw(format!("{what}  "))]
            })
            .collect();
        f.render_widget(Paragraph::new(Line::from(help)).block(rounded(false)), footer);
    }
}

impl Menu {
    fn draw_live(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let live = &self.live;
        let [text_area, chart_area] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
        let label = |s: &str| Span::styled(format!("{s:<11}"), Style::new().fg(Color::Gray));
        let bold = |s: String, color: Color| Span::styled(s, Style::new().fg(color).add_modifier(Modifier::BOLD));

        let elapsed = live.started.map_or(0, |t| t.elapsed().as_secs());
        let recent = live.buffering.iter().filter(|t| t.elapsed() < Duration::from_secs(60)).count();
        let tv_color = match live.tv.as_str() {
            "Playing" => Color::Green,
            "Buffering" => Color::Yellow,
            _ => Color::Gray,
        };
        let warn = |bad: bool| if bad { Color::Yellow } else { Color::Green };
        let lines = vec![
            Line::from(vec![
                label("Duration"),
                bold(format!("{:02}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60), Color::White),
                Span::raw("   "),
                label("TV"),
                bold(if live.tv.is_empty() { "-".into() } else { live.tv.clone() }, tv_color),
            ]),
            Line::from(vec![
                label("Buffering"),
                bold(format!("{recent} last minute"), warn(recent > 2)),
                Span::styled(format!(", {} in all", live.buffering.len()), Style::new().fg(Color::Gray)),
            ]),
            Line::from(vec![
                label("Reconnects"),
                bold(live.reconnects.to_string(), warn(live.reconnects > 0)),
            ]),
            Line::from(vec![
                label("CPU"),
                bold(format!("{:.0} % of a core", live.cpu), warn(live.cpu > 150.0)),
                Span::styled(format!("   load {}", load_average().unwrap_or_default()), Style::new().fg(Color::Gray)),
            ]),
        ];
        f.render_widget(Paragraph::new(lines).block(rounded(false).title(" Live ")), text_area);

        let title = match live.bitrate.last() {
            Some(rate) => format!(" Bitrate {rate} kbit/s "),
            None => " Bitrate ".to_string(),
        };
        let chart = Sparkline::default()
            .block(rounded(false).title(title))
            .data(&live.bitrate)
            .style(Style::new().fg(ACCENT));
        f.render_widget(chart, chart_area);
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

        // Made-up devices: no network search, so no real device shows up.
        let mut menu = Menu::idle();
        let fake = [("tv", "Living Room TV", "Chromecast"), ("hub", "Kitchen", "Google Nest Hub"), ("pj", "Classroom", "Chromecast with Google TV")];
        for (id, name, model) in fake {
            let device = CastDevice {
                id: id.into(),
                name: name.into(),
                model: model.into(),
                ip: std::net::Ipv4Addr::LOCALHOST,
                port: 8009,
            };
            menu.devices.insert(id.into(), device);
        }
        menu.device.select(Some(1));
        menu.set_casting(Some("tv".into()));
        menu.set_status(Status::Player("Playing".into()));
        menu.set_status(Status::Info("Casting to Living Room TV. Ctrl+C stops.".into()));
        menu.live.started = Instant::now().checked_sub(Duration::from_secs(754));
        menu.live.buffering = vec![Instant::now().checked_sub(Duration::from_secs(400)).unwrap()];
        menu.live.cpu = 62.0;
        menu.live.bitrate = (0..60).map(|i| 2500 + (i * 37 % 11) * 40 - 200).collect();

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 25)).unwrap();
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
             <pre style=\"margin:16px;font:16px/1.25 'DejaVu Sans Mono',monospace;color:#cdd6f4\">",
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
}
