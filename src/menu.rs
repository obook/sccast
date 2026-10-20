//! Launch menu: pick a device found on the network and adjust the stream settings.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::layout::Margin;
use ratatui::widgets::{
    Block, BorderType, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline,
};
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
    /// Frames dropped by the capture since the start.
    Dropped(Arc<AtomicU64>),
    /// Time to connect to the TV's Cast port, None if it did not answer.
    RoundTrip(Option<Duration>),
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
    dropped: Option<Arc<AtomicU64>>,
    /// Dropped frames total, one sample per second, last minute.
    dropped_history: VecDeque<u64>,
    /// Average download time of the last segments by the TV.
    fetch: Option<Duration>,
    /// None until measured; Some(None) when the TV did not answer.
    round_trip: Option<Option<Duration>>,
    /// Seconds sampled, to log a summary every 10.
    samples: u64,
}

// Alert thresholds. ponytail: first guesses, to adjust with tests (the school network in particular).
/// One second of video lost per minute.
const DROPPED_PER_MINUTE: u64 = 30;
/// One second of video must reach the TV well within one second.
const SLOW_FETCH: Duration = Duration::from_millis(500);
const CONGESTED_FETCH: Duration = Duration::from_millis(900);
const SLOW_ROUND_TRIP: Duration = Duration::from_millis(100);

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
        if let Some(dropped) = &self.dropped {
            self.dropped_history.push_back(dropped.load(Ordering::Relaxed));
            if self.dropped_history.len() > 61 {
                self.dropped_history.pop_front();
            }
        }
        self.fetch = crate::server::segment_fetch_time();
        self.samples += 1;
        if self.samples.is_multiple_of(10) {
            tracing::info!(
                lost_last_minute = self.dropped_last_minute(),
                fetch_ms = self.fetch.map(|f| f.as_millis() as u64),
                ping_ms = ?self.round_trip.map(|r| r.map(|r| r.as_millis() as u64)),
                cpu = self.cpu as u64,
                "live"
            );
        }
        if let Some(ticks) = cpu_ticks() {
            if let Some((before, at)) = self.cpu_ticks {
                // ponytail: assumes 100 ticks per second (the Linux default), read sysconf if it ever differs.
                self.cpu = (ticks - before) as f64 / at.elapsed().as_secs_f64();
            }
            self.cpu_ticks = Some((ticks, Instant::now()));
        }
    }

    fn dropped_last_minute(&self) -> u64 {
        match (self.dropped_history.front(), self.dropped_history.back()) {
            (Some(first), Some(last)) => last - first,
            _ => 0,
        }
    }

    /// The worst problem seen, with its color, or None when all is well.
    fn alert(&self) -> Option<(String, Color)> {
        let dropped = self.dropped_last_minute();
        if self.round_trip == Some(None) {
            return Some(("The TV does not answer on the network".into(), Color::Red));
        }
        if dropped > DROPPED_PER_MINUTE {
            return Some((format!("PC too slow: {dropped} frames lost in a minute. Lower the resolution or close programs"), Color::Red));
        }
        if let Some(fetch) = self.fetch.filter(|f| *f > SLOW_FETCH) {
            let color = if fetch > CONGESTED_FETCH { Color::Red } else { Color::Yellow };
            return Some((format!("Network slow: 1 s of video takes {:.1} s to reach the TV. Lower the bitrate", fetch.as_secs_f64()), color));
        }
        if let Some(Some(rtt)) = self.round_trip.filter(|r| r.is_some_and(|r| r > SLOW_ROUND_TRIP)) {
            return Some((format!("Network slow: {} ms to reach the TV", rtt.as_millis()), Color::Yellow));
        }
        None
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
            Status::Dropped(counter) => self.live.dropped = Some(counter),
            Status::RoundTrip(time) => self.live.round_trip = Some(time),
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
    pub fn draw_now(&mut self, terminal: &mut DefaultTerminal, s: &Settings) -> std::io::Result<()> {
        terminal.draw(|f| self.draw(f, s)).map(drop)
    }

    /// Browse in a thread; devices show up as they answer.
    fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        // Devices gone since the last scan (unplugged, new address) must not stay listed.
        self.devices.clear();
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
                // Long device lists (a school network): jump by pages or to the ends.
                KeyCode::PageDown if count > 0 => state.select(Some((state.selected().unwrap_or(0) + 10).min(count - 1))),
                KeyCode::PageUp => state.select(Some(state.selected().unwrap_or(0).saturating_sub(10))),
                KeyCode::Home => state.select(Some(0)),
                KeyCode::End if count > 0 => state.select(Some(count - 1)),
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
        let live_height = if self.live.started.is_some() { 7 } else { 0 };
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
        let mut lines = vec![title, status];
        // Alerts take the full width: they are long, and must be seen.
        if let Some((alert, color)) = self.live.started.and(self.live.alert()) {
            lines.push(Line::from(Span::styled(format!(" ⚠ {alert}"), Style::new().fg(color).add_modifier(Modifier::BOLD))));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }).block(rounded(false)),
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
        let count = items.len();
        let title = match self.devices.len() {
            0 => " Devices ".to_string(),
            n => format!(" Devices ({n}) "),
        };
        let devices = List::new(items)
            .block(rounded(!self.on_settings).title(title))
            .highlight_symbol("▶ ")
            .highlight_style(highlight(!self.on_settings));
        f.render_stateful_widget(devices, left, &mut self.device);
        // A school can show dozens of devices: show that the list scrolls.
        let visible = left.height.saturating_sub(2) as usize;
        if count > visible {
            let mut bar = ScrollbarState::new(count - visible).position(self.device.offset());
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None);
            f.render_stateful_widget(scrollbar, left.inner(Margin { vertical: 1, horizontal: 0 }), &mut bar);
        }

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
        let author = Line::from(Span::styled(" (C) Olivier Booklage ", Style::new().fg(Color::Gray))).right_aligned();
        f.render_widget(Paragraph::new(Line::from(help)).block(rounded(false).title_bottom(author)), footer);
    }

    fn draw_live(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let live = &self.live;
        let [text_area, chart_area] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
        let label = |s: &str| Span::styled(format!("{s:<11}"), Style::new().fg(Color::Gray));
        let bold = |s: String, color: Color| Span::styled(s, Style::new().fg(color).add_modifier(Modifier::BOLD));

        let elapsed = live.started.map_or(0, |t| t.elapsed().as_secs());
        let recent = live.buffering.iter().filter(|t| t.elapsed() < Duration::from_secs(60)).count();
        let dropped = live.dropped_last_minute();
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
                Span::styled("   Lost frames ", Style::new().fg(Color::Gray)),
                bold(format!("{dropped} last minute"), warn(dropped > DROPPED_PER_MINUTE)),
            ]),
            Line::from(vec![
                label("CPU"),
                bold(format!("{:.0} % of a core", live.cpu), warn(live.cpu > 150.0)),
                Span::styled(format!("   load {}", load_average().unwrap_or_default()), Style::new().fg(Color::Gray)),
            ]),
            Line::from(vec![
                label("Network"),
                // Time for the TV to download one 1-second segment.
                bold(
                    live.fetch.map_or("-".into(), |f| format!("segment {:.2} s", f.as_secs_f64())),
                    warn(live.fetch.is_some_and(|f| f > SLOW_FETCH)),
                ),
                Span::styled("   ping ", Style::new().fg(Color::Gray)),
                bold(
                    match live.round_trip {
                        None => "-".into(),
                        Some(None) => "no answer".into(),
                        Some(Some(rtt)) => format!("{} ms", rtt.as_millis()),
                    },
                    warn(live.round_trip.is_some_and(|r| r.is_none_or(|r| r > SLOW_ROUND_TRIP))),
                ),
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
        _ => st.format = cycle(&[Format::Hls, Format::Mkv, Format::Mirror], st.format, dir),
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
        _ => match st.format {
            Format::Hls => "HLS",
            Format::Mkv => "Matroska",
            Format::Mirror => "Mirror",
        }
        .to_string(),
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
    fn alerts_follow_thresholds() {
        let mut live = Live::default();
        assert!(live.alert().is_none());
        live.round_trip = Some(Some(Duration::from_millis(250)));
        assert_eq!(live.alert().unwrap().1, Color::Yellow);
        live.fetch = Some(Duration::from_millis(950));
        assert!(live.alert().unwrap().0.starts_with("Network slow: 1 s of video"));
        assert_eq!(live.alert().unwrap().1, Color::Red);
        live.dropped_history = VecDeque::from([0, 45]);
        assert!(live.alert().unwrap().0.starts_with("PC too slow"));
        live.round_trip = Some(None);
        assert!(live.alert().unwrap().0.contains("does not answer"));
    }

    #[test]
    fn long_device_list_scrolls() {
        let mut menu = Menu::idle();
        for i in 0..30 {
            let id = format!("d{i:02}");
            let device = CastDevice {
                id: id.clone(),
                name: format!("Room {i:02}"),
                model: "Chromecast".into(),
                ip: std::net::Ipv4Addr::LOCALHOST,
                port: 8009,
            };
            menu.devices.insert(id, device);
        }
        menu.device.select(Some(25));
        let s = Settings {
            stream: Stream { resolution: (1920, 1080), fps: 30, bitrate: 2500, port: 8090, no_audio: false, format: Format::Hls },
            audio_delay: 0,
            latency: None,
            receiver: SCCAST_RECEIVER.into(),
        };
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| menu.draw(f, &s)).unwrap();
        let screen: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("Devices (30)"));
        // The selected device is scrolled into view, and the scrollbar thumb is drawn.
        assert!(screen.contains("Room 25") && screen.contains('█'));
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
        let fake = [("tv", "Living Room TV", "Chromecast"), ("hub", "Kitchen", "Google Nest Hub"), ("pj", "Classroom", "Google TV")];
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
}
