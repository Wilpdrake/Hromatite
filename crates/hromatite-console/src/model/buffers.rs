//! Log buffers, multi-source management, filtering, and scrolling.

use super::{ConsoleModel, Mode, field_manual};
use crate::input::Key;
use crate::parser::{LineKind, LogLine};
use std::collections::VecDeque;

pub(super) const MAX_LINES: usize = 20_000;
/// Rows a single mouse-wheel notch moves, matching the usual terminal default.
const WHEEL_ROWS: usize = 3;

/// One followed log: its own history, scroll offset and follow state, so
/// switching views never moves the other log's position.
pub(super) struct LogBuffer {
    pub(super) lines: VecDeque<LogLine>,
    pub(super) clear_before: usize,
    pub(super) follow: bool,
    pub(super) scroll_back: usize,
    /// Lines that arrived while another view was active.
    pub(super) unread: usize,
}

impl LogBuffer {
    pub(super) fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            clear_before: 0,
            follow: true,
            scroll_back: 0,
            unread: 0,
        }
    }
}

impl ConsoleModel {
    /// Declare the followed logs. Existing buffers keep their history so a
    /// re-plan never throws away what was already read.
    pub fn configure_sources(&mut self, labels: &[&'static str]) {
        if labels.is_empty() {
            return;
        }
        for (index, label) in labels.iter().enumerate() {
            match self.buffers.get_mut(index) {
                Some(_) => self.labels[index] = label,
                None => {
                    self.labels.push(label);
                    self.buffers.push(LogBuffer::new());
                }
            }
        }
        self.active_source = self.active_source.min(self.buffers.len() - 1);
    }

    pub fn source_labels(&self) -> &[&'static str] {
        &self.labels
    }

    pub fn source_count(&self) -> usize {
        self.buffers.len()
    }

    pub fn active_source(&self) -> usize {
        self.active_source
    }

    /// Lines that arrived in this view since it was last shown.
    pub fn source_unread(&self, index: usize) -> usize {
        self.buffers.get(index).map_or(0, |buffer| buffer.unread)
    }

    /// Switch by index, label, `next`/`prev` or `+1`/`-1`. Returns the label.
    pub fn select_source(&mut self, target: &str) -> Option<&'static str> {
        let target = target.trim().to_ascii_lowercase();
        if target.is_empty() {
            return None;
        }
        let index = match target.as_str() {
            "next" => (self.active_source + 1) % self.buffers.len(),
            "prev" | "previous" => {
                (self.active_source + self.buffers.len() - 1) % self.buffers.len()
            }
            _ => {
                let position = self
                    .labels
                    .iter()
                    .position(|label| *label == target)
                    .or_else(|| {
                        self.labels
                            .iter()
                            .position(|label| label.starts_with(&target))
                    });
                match position {
                    Some(index) => index,
                    None => target.parse::<usize>().ok()?.checked_sub(1)?,
                }
            }
        };
        if index >= self.buffers.len() {
            return None;
        }
        self.show_source(index);
        Some(self.labels[index])
    }

    pub(super) fn show_source(&mut self, index: usize) {
        self.active_source = index;
        if let Some(buffer) = self.buffers.get_mut(index) {
            buffer.unread = 0;
        }
        self.notice = format!(
            "log view {}/{}: {}",
            index + 1,
            self.buffers.len(),
            self.labels[index]
        );
    }

    pub(super) fn step_source(&mut self, forward: bool) {
        let count = self.buffers.len();
        if count < 2 {
            self.notice = "only one log is followed".into();
            return;
        }
        let index = if forward {
            (self.active_source + 1) % count
        } else {
            (self.active_source + count - 1) % count
        };
        self.show_source(index);
    }

    /// The `logs` line: every followed log with its position and unread count.
    pub fn source_summary(&self) -> String {
        let mut text = String::from("logs:");
        for (index, label) in self.labels.iter().enumerate() {
            let unread = self.source_unread(index);
            let name = if index == self.active_source {
                format!("[{label}]")
            } else {
                (*label).to_string()
            };
            text.push_str(&format!(" {}){name}", index + 1));
            if unread > 0 {
                text.push_str(&format!("+{unread}"));
            }
        }
        text
    }

    pub fn record_sent(&mut self, command: &str) {
        self.ingest_from(0, [format!("> {}", command.trim())]);
        if let Some(line) = self.buffers[0].lines.back_mut() {
            line.tag = "tx".into();
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.buffers[self.active_source].lines.iter()
    }

    pub fn visible_lines(&self) -> Vec<&LogLine> {
        let buffer = &self.buffers[self.active_source];
        buffer
            .lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= buffer.clear_before)
            .map(|(_, line)| line)
            .filter(|line| self.matches_filters(line))
            .collect()
    }

    pub(super) fn matches_filters(&self, line: &LogLine) -> bool {
        let error_match = !self.errors_only || matches!(line.kind, LineKind::Error | LineKind::Gc);
        let needle = self.search.to_lowercase();
        error_match && (needle.is_empty() || line.raw.to_lowercase().contains(&needle))
    }

    pub fn window(&self, height: usize) -> Vec<&LogLine> {
        let visible = self.visible_lines();
        if height == 0 {
            return Vec::new();
        }
        let max_back = visible.len().saturating_sub(height);
        let back = self.buffers[self.active_source].scroll_back.min(max_back);
        let end = visible.len().saturating_sub(back);
        let start = end.saturating_sub(height);
        visible[start..end].to_vec()
    }

    /// Rows the log body shows; the terminal driver reports it once per frame so
    /// scrolling can clamp instead of banking invisible offsets.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        let maxima: Vec<usize> = (0..self.buffers.len())
            .map(|index| self.buffer_max_scroll_back(index))
            .collect();
        for (buffer, max) in self.buffers.iter_mut().zip(maxima) {
            buffer.scroll_back = buffer.scroll_back.min(max);
        }
        self.help_scroll = self.help_scroll.min(self.max_help_scroll());
    }

    /// Visible rows of one buffer, minus the body height.
    pub(super) fn buffer_max_scroll_back(&self, index: usize) -> usize {
        let buffer = &self.buffers[index];
        buffer
            .lines
            .iter()
            .enumerate()
            .filter(|(position, _)| *position >= buffer.clear_before)
            .filter(|(_, line)| self.matches_filters(line))
            .count()
            .saturating_sub(self.viewport)
    }

    pub(super) fn max_scroll_back(&self) -> usize {
        self.buffer_max_scroll_back(self.active_source)
    }

    pub(super) fn max_help_scroll(&self) -> usize {
        field_manual("", "--", "")
            .len()
            .saturating_sub(self.viewport)
    }

    /// One wheel notch, one page, or nothing at all, applied to whatever list is
    /// currently on screen.
    pub(super) fn scroll_view(&mut self, key: Key) {
        if self.show_help {
            let by = if matches!(key, Key::PgUp | Key::PgDn) {
                self.viewport.max(2) - 1
            } else {
                WHEEL_ROWS
            };
            let up = matches!(key, Key::ScrollUp | Key::PgUp);
            let max = self.max_help_scroll();
            self.help_scroll = if up {
                (self.help_scroll + by).min(max)
            } else {
                self.help_scroll.saturating_sub(by)
            };
            self.notice = if max == 0 {
                "field manual fits on screen".into()
            } else {
                format!("field manual {}/{max} rows", self.help_scroll)
            };
            return;
        }
        if self.mode == Mode::Palette {
            let count = self.palette_entries().len();
            if count > 0 {
                self.palette_selection = if matches!(key, Key::ScrollUp | Key::PgUp) {
                    (self.palette_selection + count - 1) % count
                } else {
                    (self.palette_selection + 1) % count
                };
            }
            return;
        }
        let by = if matches!(key, Key::PgUp | Key::PgDn) {
            self.viewport.max(2) - 1
        } else {
            WHEEL_ROWS
        };
        let max = self.max_scroll_back();
        let buffer = &mut self.buffers[self.active_source];
        if matches!(key, Key::ScrollUp | Key::PgUp) {
            buffer.scroll_back = (buffer.scroll_back + by).min(max);
        } else {
            buffer.scroll_back = buffer.scroll_back.saturating_sub(by);
        }
        buffer.follow = buffer.scroll_back == 0;
        if max == 0 {
            self.notice = "log fits on screen — nothing to scroll".into();
        }
    }

    pub fn set_search(&mut self, query: &str) {
        self.search = query.to_string();
        self.buffers[self.active_source].scroll_back = 0;
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn follow(&self) -> bool {
        self.buffers[self.active_source].follow
    }

    pub fn scroll_back(&self) -> usize {
        self.buffers[self.active_source].scroll_back
    }

    pub fn errors_only(&self) -> bool {
        self.errors_only
    }
}
