//! draw.rs
//! Draw the menu screen: header with the status and the alert, device list, settings,
//! key help. The Live panel of a running cast is drawn by panel.rs.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use super::settings::{value, Settings, ROWS};
use super::{Menu, Status};

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(super) const ACCENT: Color = Color::Cyan;

impl Menu {
    pub(super) fn draw(&mut self, f: &mut Frame, s: &Settings) {
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
        self.draw_header(f, header);
        self.draw_devices(f, left);
        self.draw_settings(f, right, s);
        self.draw_keys(f, footer);
    }

    fn draw_header(&self, f: &mut Frame, area: ratatui::layout::Rect) {
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
            area,
        );
    }

    fn draw_devices(&mut self, f: &mut Frame, area: ratatui::layout::Rect) {
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
        f.render_stateful_widget(devices, area, &mut self.device);
        // A school can show dozens of devices: show that the list scrolls.
        let visible = area.height.saturating_sub(2) as usize;
        if count > visible {
            let mut bar = ScrollbarState::new(count - visible).position(self.device.offset());
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None);
            f.render_stateful_widget(scrollbar, area.inner(Margin { vertical: 1, horizontal: 0 }), &mut bar);
        }
    }

    fn draw_settings(&mut self, f: &mut Frame, area: ratatui::layout::Rect, s: &Settings) {
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
        f.render_stateful_widget(rows, area, &mut self.row);
    }

    fn draw_keys(&self, f: &mut Frame, area: ratatui::layout::Rect) {
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
        f.render_widget(Paragraph::new(Line::from(help)).block(rounded(false).title_bottom(author)), area);
    }
}

/// Box with rounded corners, in the accent color when it has the focus.
pub(super) fn rounded(focused: bool) -> Block<'static> {
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
