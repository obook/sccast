//! panel.rs
//! Live panel of a running cast: duration, TV state, buffering, reconnections, lost
//! frames, CPU and network on the left, the bitrate chart on the right.
//! The measures come from live.rs.
//!
//! Author: O. Booklage
//! Date: October 2026
//! Licence: MIT

use std::time::Duration;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline};
use ratatui::Frame;

use super::draw::{rounded, ACCENT};
use super::live::{load_average, DROPPED_PER_MINUTE, SLOW_FETCH, SLOW_ROUND_TRIP};
use super::Menu;

impl Menu {
    pub(super) fn draw_live(&self, f: &mut Frame, area: Rect) {
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
                // HLS: time for the TV to download one 1-second segment. Screenshare: frames dropped.
                match (&live.network_dropped, live.fetch) {
                    (Some(n), _) => {
                        let n = n.load(std::sync::atomic::Ordering::Relaxed);
                        bold(format!("{n} frames dropped"), warn(n > 0))
                    }
                    (None, fetch) => bold(
                        fetch.map_or("-".into(), |f| format!("segment {:.2} s", f.as_secs_f64())),
                        warn(fetch.is_some_and(|f| f > SLOW_FETCH)),
                    ),
                },
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
