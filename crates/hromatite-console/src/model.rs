//! Testable UI state: filtering, scrolling, telemetry and key handling.

use crate::commands::{Risk, SPECS, Spec, command};
use crate::hot::{HotStatus, parse_hot_status};
use crate::input::{History, Key, complete};
use crate::parser::{LineKind, LogLine, parse_line};
use std::collections::{BTreeSet, VecDeque};

const MAX_LINES: usize = 20_000;
/// Rows a single mouse-wheel notch moves, matching the usual terminal default.
const WHEEL_ROWS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Command,
    Search,
    Palette,
}

/// One followed log: its own history, scroll offset and follow state, so
/// switching views never moves the other log's position.
struct LogBuffer {
    lines: VecDeque<LogLine>,
    clear_before: usize,
    follow: bool,
    scroll_back: usize,
    /// Lines that arrived while another view was active.
    unread: usize,
}

impl LogBuffer {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            clear_before: 0,
            follow: true,
            scroll_back: 0,
            unread: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    Refresh,
    ToggleAuto,
    Exit,
}

pub struct ConsoleModel {
    buffers: Vec<LogBuffer>,
    labels: Vec<&'static str>,
    active_source: usize,
    errors_only: bool,
    search: String,
    mode: Mode,
    active: String,
    cursor: usize,
    saved_command: String,
    palette_selection: usize,
    pending_confirmation: Option<String>,
    history: History,
    completions: Vec<&'static str>,
    notice: String,
    demo: bool,
    auto_refresh: bool,
    auto_suspended: bool,
    show_help: bool,
    help_scroll: usize,
    /// Rows the last frame could show in the log body; scrolling needs it to
    /// clamp, so a wheel notch never piles up an invisible backlog.
    viewport: usize,
    player_ids: BTreeSet<u64>,
    reported_players: Option<usize>,
    esp_seen: bool,
    tick_hz: Option<String>,
    hot_status: Option<HotStatus>,
    feature_generation: Option<u64>,
    feature_abi: Option<u64>,
    pub camera: Option<String>,
    pub player: Option<String>,
    pub last_late: Option<String>,
    pub last_gc: Option<String>,
}

impl ConsoleModel {
    pub fn new(demo: bool) -> Self {
        Self {
            buffers: vec![LogBuffer::new()],
            labels: vec!["payload"],
            active_source: 0,
            errors_only: false,
            search: String::new(),
            mode: Mode::Command,
            active: String::new(),
            cursor: 0,
            saved_command: String::new(),
            palette_selection: 0,
            pending_confirmation: None,
            history: History::default(),
            completions: Vec::new(),
            notice: "type help + Enter; Ctrl-R searches logs".into(),
            demo,
            auto_refresh: false,
            auto_suspended: false,
            show_help: false,
            help_scroll: 0,
            viewport: 20,
            player_ids: BTreeSet::new(),
            reported_players: None,
            esp_seen: false,
            tick_hz: None,
            hot_status: None,
            feature_generation: None,
            feature_abi: None,
            camera: None,
            player: None,
            last_late: None,
            last_gc: None,
        }
    }

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

    fn show_source(&mut self, index: usize) {
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

    fn step_source(&mut self, forward: bool) {
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

    pub fn ingest(&mut self, inputs: impl IntoIterator<Item = impl AsRef<str>>) {
        self.ingest_from(0, inputs);
    }

    /// Append lines that came from `source`; telemetry is read from every
    /// source, the view state only from the buffer that received them.
    pub fn ingest_from(
        &mut self,
        source: usize,
        inputs: impl IntoIterator<Item = impl AsRef<str>>,
    ) {
        self.ingest_lines(source, inputs, true);
    }

    /// Backfill (startup replay, or the tail of a log the game just rotated
    /// to): the same ingest path, but these lines are not "new" for the reader,
    /// so they must not inflate the unread counter of a hidden view.
    pub fn ingest_replay(
        &mut self,
        source: usize,
        inputs: impl IntoIterator<Item = impl AsRef<str>>,
    ) {
        self.ingest_lines(source, inputs, false);
    }

    fn ingest_lines(
        &mut self,
        source: usize,
        inputs: impl IntoIterator<Item = impl AsRef<str>>,
        count_unread: bool,
    ) {
        let source = source.min(self.buffers.len() - 1);
        let mut prepared = Vec::new();
        for input in inputs {
            let line = parse_line(input.as_ref());
            self.observe(&line);
            let visible = self.matches_filters(&line);
            prepared.push((line, visible));
        }
        let mut added_visible = 0;
        let active = self.active_source;
        let viewport_rows = self.viewport;
        {
            let buffer = &mut self.buffers[source];
            for (line, visible) in prepared {
                buffer.lines.push_back(line);
                if visible {
                    added_visible += 1;
                }
            }
            if !buffer.follow {
                buffer.scroll_back += added_visible;
            }
            if source != active && count_unread && added_visible > 0 {
                buffer.unread += added_visible;
            }
            while buffer.lines.len() > MAX_LINES {
                buffer.lines.pop_front();
                buffer.clear_before = buffer.clear_before.saturating_sub(1);
            }
        }
        let max_back = self.buffers[source]
            .lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= self.buffers[source].clear_before)
            .filter(|(_, line)| self.matches_filters(line))
            .count()
            .saturating_sub(viewport_rows);
        let buffer = &mut self.buffers[source];
        buffer.scroll_back = buffer.scroll_back.min(max_back);
    }

    fn observe(&mut self, line: &LogLine) {
        if let Some(status) = parse_hot_status(&line.raw) {
            self.hot_status = Some(status);
        }
        if line.tag == "con"
            && let Some((generation, abi)) = parse_feature_log(&line.message)
        {
            self.feature_generation = Some(generation);
            self.feature_abi = Some(abi);
        }
        if line.tag == "late" {
            self.last_late = Some(line.message.clone());
        }
        if line.tag == "gct" {
            self.last_gc = Some(line.message.clone());
        }
        update_telemetry(self, line);
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

    fn matches_filters(&self, line: &LogLine) -> bool {
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

    pub fn handle(&mut self, key: Key) -> Action {
        if key == Key::CtrlC {
            return Action::Exit;
        }
        let cancelled_confirmation =
            key != Key::Enter && self.pending_confirmation.take().is_some();
        if key == Key::Esc {
            if cancelled_confirmation {
                self.notice = "dangerous command cancelled".into();
                return Action::None;
            }
            if self.show_help {
                self.show_help = false;
                self.help_scroll = 0;
                return Action::None;
            }
            return match self.mode {
                Mode::Command => Action::Exit,
                Mode::Search | Mode::Palette => {
                    self.mode = Mode::Command;
                    let saved = self.saved_command.clone();
                    self.set_active_text(&saved);
                    self.completions.clear();
                    Action::None
                }
            };
        }
        if cancelled_confirmation {
            self.notice = "dangerous command cancelled".into();
        }
        // Wheel and page keys scroll whatever list is on screen (log, palette or
        // field manual), so they run before the help gate swallows keys.
        if matches!(key, Key::ScrollUp | Key::ScrollDown | Key::PgUp | Key::PgDn) {
            self.scroll_view(key);
            return Action::None;
        }
        if self.show_help && key != Key::F(1) {
            return Action::None;
        }
        match key {
            Key::F(1) => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
            }
            Key::F(2) => {
                let buffer = &mut self.buffers[self.active_source];
                buffer.follow = !buffer.follow;
                if buffer.follow {
                    buffer.scroll_back = 0;
                }
                let follow = buffer.follow;
                self.notice = if follow {
                    "follow resumed".into()
                } else {
                    "follow paused".into()
                };
            }
            Key::F(3) => {
                self.errors_only = !self.errors_only;
                self.buffers[self.active_source].scroll_back = 0;
                self.notice = if self.errors_only {
                    "filter: errors + GC".into()
                } else {
                    "filter: all".into()
                };
            }
            Key::F(4) => self.begin_palette(),
            Key::F(5) => {
                self.notice = "telemetry requested: pos, who, late, gct".into();
                return Action::Refresh;
            }
            Key::F(6) => {
                self.auto_refresh = !self.auto_refresh;
                self.auto_suspended = false;
                self.notice = if self.auto_refresh {
                    "auto telemetry: ON (pos 1s, summary 5s)".into()
                } else {
                    "auto telemetry: OFF".into()
                };
                return Action::ToggleAuto;
            }
            Key::F(7) => {
                if self.mode != Mode::Command {
                    self.mode = Mode::Command;
                }
                self.set_active_text("reload");
                return self.prepare_submission("reload".into());
            }
            Key::F(8) => {
                let buffer = &mut self.buffers[self.active_source];
                buffer.clear_before = buffer.lines.len();
                buffer.scroll_back = 0;
                buffer.follow = true;
                self.notice = "view cleared — log file unchanged".into();
            }
            Key::F(9) => self.step_source(true),
            Key::F(10) => self.step_source(false),
            Key::CtrlR => self.begin_search(),
            Key::Char('/') if self.mode == Mode::Command && self.active.is_empty() => {
                self.begin_search()
            }
            Key::Char(':') if self.mode == Mode::Command && self.active.is_empty() => {
                self.begin_palette()
            }
            Key::Tab if self.mode == Mode::Command => {
                let completed = complete(&self.active);
                self.completions = completed.matches;
                self.set_active_text(&completed.text);
            }
            Key::Up | Key::Down if self.mode == Mode::Palette => {
                let count = self.palette_entries().len();
                if count > 0 {
                    self.palette_selection = if key == Key::Up {
                        (self.palette_selection + count - 1) % count
                    } else {
                        (self.palette_selection + 1) % count
                    };
                }
            }
            Key::Up | Key::Down if self.mode == Mode::Command => {
                let value = if key == Key::Up {
                    self.history.previous(&self.active)
                } else {
                    self.history.next_newer()
                };
                if let Some(value) = value {
                    self.set_active_text(&value);
                }
            }
            Key::Enter => {
                return match self.mode {
                    Mode::Command => {
                        let command = self.active.trim().to_string();
                        if command.is_empty() {
                            Action::None
                        } else {
                            self.prepare_submission(command)
                        }
                    }
                    Mode::Search => {
                        self.mode = Mode::Command;
                        let saved = self.saved_command.clone();
                        self.set_active_text(&saved);
                        self.notice = if self.search.is_empty() {
                            "search cleared".into()
                        } else {
                            format!("log filter: {}", self.search)
                        };
                        Action::None
                    }
                    Mode::Palette => {
                        let Some(selected) = self.selected_palette_command().copied() else {
                            return Action::None;
                        };
                        if selected.takes_arguments() {
                            self.mode = Mode::Command;
                            self.set_active_text(&format!("{} ", selected.name));
                            self.completions.clear();
                            self.notice = format!("{} — {}", selected.usage, selected.summary);
                            Action::None
                        } else {
                            self.prepare_submission(selected.name.to_string())
                        }
                    }
                };
            }
            Key::Char(ch) => {
                self.active.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.after_edit();
            }
            Key::Backspace => {
                if let Some((start, _)) = previous_char(&self.active, self.cursor) {
                    self.active.drain(start..self.cursor);
                    self.cursor = start;
                    self.after_edit();
                }
            }
            Key::Delete => {
                if self.cursor < self.active.len() {
                    let end = next_char(&self.active, self.cursor);
                    self.active.drain(self.cursor..end);
                    self.after_edit();
                }
            }
            Key::Left => {
                if let Some((start, _)) = previous_char(&self.active, self.cursor) {
                    self.cursor = start;
                }
            }
            Key::Right => {
                self.cursor = next_char(&self.active, self.cursor);
            }
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.active.len(),
            _ => {}
        }
        Action::None
    }

    fn begin_palette(&mut self) {
        if self.mode == Mode::Command {
            self.saved_command = self.active.clone();
        }
        self.mode = Mode::Palette;
        self.palette_selection = 0;
        self.set_active_text("");
        self.completions.clear();
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
    fn buffer_max_scroll_back(&self, index: usize) -> usize {
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

    fn max_scroll_back(&self) -> usize {
        self.buffer_max_scroll_back(self.active_source)
    }

    fn max_help_scroll(&self) -> usize {
        field_manual("", "--", "")
            .len()
            .saturating_sub(self.viewport)
    }

    /// One wheel notch, one page, or nothing at all, applied to whatever list is
    /// currently on screen.
    fn scroll_view(&mut self, key: Key) {
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

    pub fn prepare_submission(&mut self, submitted: String) -> Action {
        if matches!(submitted.as_str(), "help" | "?") {
            self.pending_confirmation = None;
            self.mode = Mode::Command;
            self.history.push(&submitted);
            self.set_active_text("");
            self.completions.clear();
            self.notice = "local field manual — Esc closes".into();
            self.show_help = true;
            self.help_scroll = 0;
            return Action::None;
        }
        if let Some(action) = self.local_view_command(&submitted) {
            return action;
        }
        let name = submitted.split_whitespace().next().unwrap_or("");
        let read_only_hot_status =
            name == "reload" && submitted.split_whitespace().nth(1) == Some("status");
        let dangerous = !read_only_hot_status
            && (command(name).is_some_and(|item| item.risk == Risk::Dangerous)
                || (name == "config" && submitted.split_whitespace().nth(1) == Some("set")));
        if dangerous && self.pending_confirmation.as_deref() != Some(submitted.as_str()) {
            self.pending_confirmation = Some(submitted);
            self.notice = "DANGER: press Enter again to execute; Esc cancels".into();
            return Action::None;
        }
        self.pending_confirmation = None;
        self.mode = Mode::Command;
        self.history.push(&submitted);
        self.set_active_text("");
        self.completions.clear();
        self.notice = format!("sent: {submitted}");
        Action::Submit(submitted)
    }

    /// `log` and `logs` are host-side: they never touch the command channel,
    /// because the payload does not declare such a command.
    fn local_view_command(&mut self, submitted: &str) -> Option<Action> {
        let mut words = submitted.split_whitespace();
        let name = words.next()?;
        if !matches!(name, "log" | "logs") {
            return None;
        }
        let argument = words.next();
        self.pending_confirmation = None;
        self.mode = Mode::Command;
        self.history.push(submitted);
        self.set_active_text("");
        self.completions.clear();
        let notice = match argument {
            None => self.source_summary(),
            Some(target) => match self.select_source(target) {
                Some(label) => format!("log view: {label}"),
                None => format!("no such log: {target} — logs: {}", self.labels.join(" ")),
            },
        };
        self.notice = notice;
        Some(Action::None)
    }

    fn begin_search(&mut self) {
        self.saved_command = if self.mode == Mode::Command {
            self.active.clone()
        } else {
            self.saved_command.clone()
        };
        self.mode = Mode::Search;
        self.completions.clear();
        self.set_active_text(&self.search.clone());
    }

    fn after_edit(&mut self) {
        self.history.reset_navigation();
        if self.mode == Mode::Search {
            self.search = self.active.clone();
            let buffer = &mut self.buffers[self.active_source];
            buffer.scroll_back = 0;
            buffer.follow = true;
        }
        if self.mode == Mode::Palette {
            self.palette_selection = 0;
        }
        self.completions.clear();
    }

    pub fn set_active_text(&mut self, text: &str) {
        self.active = text.to_string();
        self.cursor = self.active.len();
        if self.mode == Mode::Search {
            self.search = text.to_string();
        }
    }

    pub fn active_text(&self) -> &str {
        &self.active
    }

    pub fn active_cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_search(&mut self, query: &str) {
        self.search = query.to_string();
        self.buffers[self.active_source].scroll_back = 0;
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn mode(&self) -> Mode {
        self.mode
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

    pub fn palette_selection(&self) -> usize {
        self.palette_selection
    }

    pub fn palette_entries(&self) -> Vec<&'static Spec> {
        if self.mode != Mode::Palette {
            return Vec::new();
        }
        let query = self.active.to_lowercase();
        SPECS
            .iter()
            .filter(|item| {
                query.is_empty()
                    || item.name.contains(&query)
                    || item.aliases.iter().any(|alias| alias.contains(&query))
                    || item.usage.to_lowercase().contains(&query)
                    || item.summary.to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn selected_palette_command(&self) -> Option<&'static Spec> {
        self.palette_entries().get(self.palette_selection).copied()
    }

    pub fn pending_confirmation(&self) -> Option<&str> {
        self.pending_confirmation.as_deref()
    }

    pub fn completions(&self) -> &[&'static str] {
        &self.completions
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn demo(&self) -> bool {
        self.demo
    }

    pub fn auto_refresh(&self) -> bool {
        self.auto_refresh
    }

    pub fn help_visible(&self) -> bool {
        self.show_help
    }

    /// First manual row currently shown; the renderer clamps it by its own body
    /// height, the model clamps it by the last reported viewport.
    pub fn help_scroll(&self) -> usize {
        self.help_scroll
    }

    pub fn auto_suspended(&self) -> bool {
        self.auto_suspended
    }

    pub fn player_count(&self) -> Option<usize> {
        self.reported_players
            .or((self.esp_seen || !self.player_ids.is_empty()).then_some(self.player_ids.len()))
    }

    pub fn tick_hz(&self) -> Option<&str> {
        self.tick_hz.as_deref()
    }

    pub fn hot_status(&self) -> Option<&HotStatus> {
        self.hot_status.as_ref()
    }

    pub fn feature_generation(&self) -> Option<u64> {
        self.feature_generation
    }

    pub fn feature_abi(&self) -> Option<u64> {
        self.feature_abi
    }

    pub fn gc_healthy(&self) -> Option<bool> {
        self.last_gc.as_ref().map(|message| {
            let lower = message.to_lowercase();
            !["fatal", "failed", "error"]
                .iter()
                .any(|word| lower.contains(word))
        })
    }

    pub fn set_auto_suspended(&mut self, suspended: bool) {
        self.auto_suspended = suspended;
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = notice.into();
    }
}

fn previous_char(text: &str, cursor: usize) -> Option<(usize, char)> {
    text[..cursor].char_indices().next_back()
}

fn next_char(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |ch| cursor + ch.len_utf8())
}

/// The local field manual. It lives in the model (not the renderer) because
/// scrolling needs to know how many rows it has.
///
/// The manual describes the host UI only; the payload command list is generated
/// from `hromatite_commands` and shown by the palette (F4).
pub fn field_manual(log_path: &str, log_size: &str, command_path: &str) -> Vec<String> {
    vec![
        " FIELD MANUAL // help / ? / F1; Esc closes".to_string(),
        " local view: works without a live payload answer".into(),
        " enter send | tab complete | up/down command history".into(),
        " F4 or : payload commands | / or Ctrl-R log filter".into(),
        " F9 / F10 switch log view | log <name> | logs lists".into(),
        " F2 follow | F3 errors+GC | PgUp/PgDn and wheel scroll".into(),
        " F5 refresh once | F6 auto: pos 1s, summary 5s".into(),
        " F7 reload (second Enter confirms) | F8 clear view only".into(),
        " Ctrl-C quit | Esc back".into(),
        " RISK commands require a second Enter; edits cancel it".into(),
        " channel append-only; no new game threads or sockets".into(),
        format!(" log {log_path} ({log_size})"),
        format!(" cmd {command_path}"),
    ]
}

fn parse_feature_log(message: &str) -> Option<(u64, u64)> {
    let mut fields = message.split_whitespace();
    if fields.next()? != "feature:" {
        return None;
    }
    let generation = fields.next()?.strip_prefix("generation=")?.parse().ok()?;
    let abi = fields.next()?.strip_prefix("abi=")?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((generation, abi))
}

fn update_telemetry(model: &mut ConsoleModel, line: &LogLine) {
    if line.tag == "late"
        && let Some(hz) = keyed_value(&line.message, "hz=")
    {
        model.tick_hz = Some(hz.to_string());
    }
    if let Some(rest) = line.message.strip_prefix("who: list ") {
        let size = rest
            .strip_prefix("size ")
            .or_else(|| rest.split_once(", size ").map(|(_, tail)| tail));
        if let Some(count) = size
            .and_then(|tail| tail.split([',', ' ']).next())
            .and_then(|value| value.parse().ok())
        {
            model.reported_players = Some(count);
        }
    }
    if line.message.starts_with("esp: head") {
        model.player_ids.clear();
        model.reported_players = None;
        model.esp_seen = true;
    }
    if line.message.starts_with("esp: id=")
        && let Some(id) = keyed_value(&line.message, "id=").and_then(|v| v.parse().ok())
    {
        model.player_ids.insert(id);
    }
    if let Some(rest) = line.message.split("pos: get_position = ").nth(1) {
        let vector = rest.split("  [raw").next().unwrap_or(rest).trim();
        if vector.starts_with('(') && vector.ends_with(')') {
            model.camera = Some(vector.to_string());
        }
    }
    if let Some(rest) = line.message.split("esp: head").nth(1)
        && let Some(start) = rest.find("cam=(")
    {
        let rest = &rest[start + 4..];
        if let Some(end) = rest.find(')') {
            model.camera = Some(rest[..=end].to_string());
        }
    }
    if line.message.contains("player #") {
        model.player = Some(line.message.trim().to_string());
    }
}

fn keyed_value<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|token| token.strip_prefix(prefix))
}

#[cfg(test)]
mod tests {
    use super::{Action, ConsoleModel, Mode};
    use crate::input::Key;

    fn model() -> ConsoleModel {
        let mut model = ConsoleModel::new(false);
        model.ingest([
            "[1] [late] hits=10 hz=90 tid=0x1",
            "[2] [con] pos: get_position = (1.000, 2.000, 3.000)  [raw 0x1 0x2]",
            "[3] [con]   player #0: api 0xaa id=7 pos=(4.00, 5.00, 6.00) go=\"Nyx\"",
            "[4] [gct] Fatal error in GC: SuspendThread loop failed",
        ]);
        model
    }

    #[test]
    fn filters_and_clear_do_not_destroy_the_log() {
        let mut m = model();
        assert_eq!(m.camera.as_deref(), Some("(1.000, 2.000, 3.000)"));
        assert!(m.player.as_deref().unwrap().contains("Nyx"));
        m.handle(Key::F(3));
        assert_eq!(m.visible_lines().len(), 1);
        assert!(m.visible_lines()[0].raw.contains("Fatal error"));
        m.handle(Key::F(3));
        m.handle(Key::F(8));
        assert!(m.visible_lines().is_empty());
        m.ingest(["[5] [late] hits=11 hz=90 tid=0x1"]);
        assert_eq!(m.visible_lines().len(), 1);
        assert!(m.lines().next().is_some());
    }

    #[test]
    fn search_scroll_and_follow_are_independent() {
        let mut m = model();
        m.set_viewport(2);
        m.handle(Key::CtrlR);
        assert_eq!(m.mode(), Mode::Search);
        m.set_active_text("fatal");
        m.handle(Key::Enter);
        assert_eq!(m.visible_lines().len(), 1);
        m.set_search("");
        m.handle(Key::PgUp);
        assert!(!m.follow());
        assert!(m.scroll_back() > 0);
        m.handle(Key::F(2));
        assert!(m.follow());
        assert_eq!(m.scroll_back(), 0);
    }

    #[test]
    fn the_mouse_wheel_scrolls_the_log_by_a_notch_and_clamps_at_both_ends() {
        let mut m = model();
        m.ingest((0..20).map(|i| format!("[{i}] [con] line {i}")));
        m.set_viewport(5);
        let max_back = 24 - 5;
        m.handle(Key::ScrollUp);
        assert!(!m.follow());
        assert_eq!(m.scroll_back(), 3);
        for _ in 0..50 {
            m.handle(Key::ScrollUp);
        }
        assert_eq!(
            m.scroll_back(),
            max_back,
            "wheel must not bank hidden offsets"
        );
        assert!(!m.follow());
        for _ in 0..50 {
            m.handle(Key::ScrollDown);
        }
        assert_eq!(m.scroll_back(), 0);
        assert!(m.follow(), "scrolling back to the tail resumes follow");
    }

    #[test]
    fn the_mouse_wheel_scrolls_the_field_manual_and_the_palette_selection() {
        let mut m = ConsoleModel::new(false);
        m.set_viewport(4);
        m.handle(Key::F(1));
        assert!(m.help_visible());
        m.handle(Key::ScrollUp);
        assert!(m.help_scroll() > 0, "wheel must move the manual");
        let manual_rows = crate::model::field_manual("", "--", "").len();
        for _ in 0..40 {
            m.handle(Key::ScrollUp);
        }
        assert_eq!(m.help_scroll(), manual_rows - 4);
        m.handle(Key::ScrollDown);
        assert_eq!(m.help_scroll(), manual_rows - 7);
        // Esc still closes the manual even after scrolling it.
        assert_eq!(m.handle(Key::Esc), Action::None);
        assert!(!m.help_visible());
        assert_eq!(m.help_scroll(), 0);

        m.handle(Key::F(4));
        assert_eq!(m.palette_selection(), 0);
        m.handle(Key::ScrollDown);
        assert_eq!(m.palette_selection(), 1);
        m.handle(Key::ScrollUp);
        assert_eq!(m.palette_selection(), 0);
        // Wrapping matches Up/Down so a wheel never dead-ends the palette.
        m.handle(Key::ScrollUp);
        assert_eq!(m.palette_selection(), m.palette_entries().len() - 1);
    }

    #[test]
    fn enter_history_and_palette_produce_submissions() {
        let mut m = model();
        m.handle(Key::Char('p'));
        m.handle(Key::Char('o'));
        m.handle(Key::Char('s'));
        assert_eq!(m.handle(Key::Enter), Action::Submit("pos".into()));
        m.handle(Key::Up);
        assert_eq!(m.active_text(), "pos");
        m.handle(Key::F(4));
        m.handle(Key::Down);
        assert_eq!(m.palette_selection(), 1);
        // The palette order is the generated table's order, so follow it instead
        // of hard-coding a second copy of the list in the test.
        let second = crate::commands::SPECS[1].name.to_string();
        assert_eq!(m.handle(Key::Enter), Action::Submit(second));
        assert_eq!(m.handle(Key::CtrlC), Action::Exit);
        assert_eq!(m.handle(Key::Esc), Action::Exit);
        assert_eq!(m.handle(Key::F(5)), Action::Refresh);
        assert_eq!(m.handle(Key::F(6)), Action::ToggleAuto);
        assert!(m.auto_refresh());
        assert_eq!(m.handle(Key::F(6)), Action::ToggleAuto);
        assert!(!m.auto_refresh());
    }

    #[test]
    fn successful_transmissions_have_a_distinct_local_echo() {
        let mut m = ConsoleModel::new(false);
        m.record_sent("pos");
        let lines = m.visible_lines();
        let last = lines.last().unwrap();
        assert_eq!(last.tag, "tx");
        assert_eq!(last.message, "> pos");
        assert!(m.visible_lines().iter().any(|line| line.tag == "tx"));
    }

    #[test]
    fn who_reads_the_actual_payload_list_format_and_the_legacy_form() {
        let mut m = ConsoleModel::new(false);
        m.ingest(["[1] [con] who: list 0x12db827d0, size 3, items 0xaa, cap 4"]);
        assert_eq!(m.player_count(), Some(3));
        m.ingest(["[2] [con] who: list size 2, items 0xbb, cap 2"]);
        assert_eq!(m.player_count(), Some(2));
    }

    #[test]
    fn a_received_esp_snapshot_with_no_targets_is_zero_not_unknown() {
        let mut m = ConsoleModel::new(false);
        assert_eq!(m.player_count(), None);
        m.ingest(["[1] [con] esp: head w=1920 h=1080 err=0 cam=(0, 0, 0)"]);
        assert_eq!(m.player_count(), Some(0));
    }

    #[test]
    fn each_esp_snapshot_replaces_the_previous_target_set() {
        let mut m = ConsoleModel::new(false);
        m.ingest([
            "[1] [con] esp: head w=1920 h=1080 err=0 cam=(0, 0, 0)",
            "[2] [con] esp: id=1 d=2 target=one",
            "[3] [con] esp: id=2 d=3 target=two",
        ]);
        assert_eq!(m.player_count(), Some(2));
        m.ingest([
            "[4] [con] esp: head w=1920 h=1080 err=0 cam=(0, 0, 0)",
            "[5] [con] esp: id=3 d=4 target=new",
        ]);
        assert_eq!(m.player_count(), Some(1));
    }

    #[test]
    fn completion_is_applied_to_command_input() {
        let mut m = ConsoleModel::new(false);
        m.set_active_text("wh");
        m.handle(Key::Tab);
        assert_eq!(m.active_text(), "who ");
    }

    #[test]
    fn help_is_a_dismissible_view_not_an_exit() {
        let mut m = ConsoleModel::new(false);
        m.handle(Key::F(1));
        assert!(m.help_visible());
        assert_eq!(m.handle(Key::Esc), Action::None);
        assert!(!m.help_visible());
        assert_eq!(m.handle(Key::Esc), Action::Exit);
    }

    #[test]
    fn help_commands_open_the_local_manual_without_a_payload_roundtrip() {
        for input in ["help", "?"] {
            let mut m = ConsoleModel::new(false);
            m.set_active_text(input);
            assert_eq!(m.handle(Key::Enter), Action::None, "input: {input}");
            assert!(m.help_visible(), "input: {input}");
            assert_eq!(m.active_text(), "", "input: {input}");
        }
    }

    #[test]
    fn palette_searches_descriptions_and_loads_argument_templates() {
        let mut m = ConsoleModel::new(false);
        m.handle(Key::F(4));
        for ch in "players".chars() {
            m.handle(Key::Char(ch));
        }
        assert!(m.palette_entries().iter().any(|item| item.name == "who"));
        m.set_active_text("q");
        let selected = m.selected_palette_command().unwrap();
        assert_eq!(selected.name, "q");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert_eq!(m.mode(), Mode::Command);
        assert_eq!(m.active_text(), "q ");
    }

    #[test]
    fn switching_views_keeps_each_logs_history_scroll_and_unread_counter() {
        let mut m = ConsoleModel::new(false);
        m.configure_sources(&["payload", "game", "eac"]);
        m.set_viewport(2);
        m.ingest_from(0, ["[1] [con] p1", "[2] [con] p2", "[3] [con] p3"]);
        m.ingest_from(1, ["2026 Log - g1", "2026 Log - g2"]);
        assert_eq!(m.active_source(), 0);
        assert_eq!(
            m.source_unread(1),
            2,
            "a hidden log counts what it receives"
        );

        m.handle(Key::ScrollUp);
        assert_eq!(m.scroll_back(), 1);
        assert!(!m.follow());

        m.handle(Key::F(9));
        assert_eq!(m.active_source(), 1);
        assert_eq!(m.source_unread(1), 0, "showing a log clears its counter");
        assert!(m.follow(), "a view that was never scrolled keeps following");
        assert_eq!(m.visible_lines().len(), 2);
        assert!(m.visible_lines()[0].raw.contains("g1"));
        assert!(
            m.visible_lines()
                .iter()
                .all(|line| !line.raw.contains("p1")),
            "views must not mix their lines"
        );

        m.handle(Key::F(10));
        assert_eq!(m.active_source(), 0);
        assert_eq!(m.scroll_back(), 1, "the paused position survived the trip");
        assert!(!m.follow());

        m.handle(Key::F(9));
        m.handle(Key::F(9));
        assert_eq!(m.active_source(), 2, "F9 wraps around the list");
        m.handle(Key::F(9));
        assert_eq!(m.active_source(), 0);
    }

    #[test]
    fn log_commands_switch_views_locally_without_a_payload_roundtrip() {
        let mut m = ConsoleModel::new(false);
        m.configure_sources(&["payload", "game", "eac"]);

        m.set_active_text("logs");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert!(m.notice().contains("1)[payload]"), "{}", m.notice());
        assert!(m.notice().contains("3)eac"), "{}", m.notice());

        m.set_active_text("log eac");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert_eq!(m.active_source(), 2);
        assert_eq!(m.active_text(), "");

        m.set_active_text("log 1");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert_eq!(m.active_source(), 0);

        m.set_active_text("log next");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert_eq!(m.active_source(), 1);

        m.set_active_text("log nonsense");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert!(m.notice().contains("no such log"), "{}", m.notice());
        assert_eq!(m.active_source(), 1, "a bad name must not move the view");
    }

    #[test]
    fn a_startup_replay_is_backfill_and_never_counts_as_unread() {
        let mut m = ConsoleModel::new(false);
        m.configure_sources(&["payload", "game"]);
        m.ingest_replay(1, ["2026 Log - old line 1", "2026 Log - old line 2"]);
        assert_eq!(m.source_unread(1), 0);
        m.select_source("game");
        assert_eq!(m.visible_lines().len(), 2, "replayed lines are still shown");
        m.select_source("payload");
        m.ingest_from(1, ["2026 Log - genuinely new"]);
        assert_eq!(m.source_unread(1), 1);
    }

    #[test]
    fn the_command_echo_lands_in_the_payload_view_that_will_answer_it() {
        let mut m = ConsoleModel::new(false);
        m.configure_sources(&["payload", "game"]);
        m.select_source("game");
        m.record_sent("pos");
        assert_eq!(m.source_unread(0), 1);
        assert!(
            m.visible_lines()
                .iter()
                .all(|line| !line.raw.contains("> pos")),
            "the echo belongs to the payload view, not to the game log"
        );
        m.select_source("payload");
        assert!(m.visible_lines().last().unwrap().raw.contains("> pos"));
    }

    #[test]
    fn dangerous_commands_need_a_second_enter() {
        let mut m = ConsoleModel::new(false);
        m.set_active_text("wq 0x1000 0x2a");
        assert_eq!(m.handle(Key::Enter), Action::None);
        assert_eq!(m.pending_confirmation(), Some("wq 0x1000 0x2a"));
        assert_eq!(
            m.handle(Key::Enter),
            Action::Submit("wq 0x1000 0x2a".into())
        );
        assert_eq!(m.pending_confirmation(), None);

        m.set_active_text("quit");
        assert_eq!(m.handle(Key::Enter), Action::None);
        m.handle(Key::Esc);
        assert_eq!(m.pending_confirmation(), None);
        assert_eq!(m.mode(), Mode::Command);
    }
}
