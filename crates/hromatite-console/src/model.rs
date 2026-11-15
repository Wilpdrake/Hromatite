//! Testable UI state: filtering, scrolling, telemetry and key handling.

use crate::input::{History, Key, complete};
use crate::parser::{LineKind, LogLine, parse_line};
use std::collections::VecDeque;

const MAX_LINES: usize = 20_000;
pub const PALETTE: &[&str] = &["pos", "who", "esp", "late", "gct"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Command,
    Search,
    Palette,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    Refresh,
    Exit,
}

pub struct ConsoleModel {
    lines: VecDeque<LogLine>,
    clear_before: usize,
    sequence: usize,
    follow: bool,
    scroll_back: usize,
    errors_only: bool,
    search: String,
    mode: Mode,
    active: String,
    cursor: usize,
    saved_command: String,
    palette_selection: usize,
    history: History,
    completions: Vec<&'static str>,
    notice: String,
    demo: bool,
    pub camera: Option<String>,
    pub player: Option<String>,
    pub last_late: Option<String>,
    pub last_gc: Option<String>,
}

impl ConsoleModel {
    pub fn new(demo: bool) -> Self {
        Self {
            lines: VecDeque::new(),
            clear_before: 0,
            sequence: 0,
            follow: true,
            scroll_back: 0,
            errors_only: false,
            search: String::new(),
            mode: Mode::Command,
            active: String::new(),
            cursor: 0,
            saved_command: String::new(),
            palette_selection: 0,
            history: History::default(),
            completions: Vec::new(),
            notice: "type help + Enter; Ctrl-R searches logs".into(),
            demo,
            camera: None,
            player: None,
            last_late: None,
            last_gc: None,
        }
    }

    pub fn ingest(&mut self, inputs: impl IntoIterator<Item = impl AsRef<str>>) {
        let mut added_visible = 0;
        for input in inputs {
            let line = parse_line(input.as_ref());
            if line.tag == "late" {
                self.last_late = Some(line.message.clone());
            }
            if line.tag == "gct" {
                self.last_gc = Some(line.message.clone());
            }
            update_telemetry(self, &line);
            let visible = self.matches_filters(&line);
            self.lines.push_back(line);
            self.sequence += 1;
            if visible {
                added_visible += 1;
            }
        }
        if !self.follow {
            self.scroll_back += added_visible;
        }
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.clear_before = self.clear_before.saturating_sub(1);
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    pub fn visible_lines(&self) -> Vec<&LogLine> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= self.clear_before)
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
        let back = self.scroll_back.min(max_back);
        let end = visible.len().saturating_sub(back);
        let start = end.saturating_sub(height);
        visible[start..end].to_vec()
    }

    pub fn handle(&mut self, key: Key) -> Action {
        if matches!(key, Key::CtrlC | Key::Esc) {
            return Action::Exit;
        }
        match key {
            Key::F(2) => {
                self.follow = !self.follow;
                if self.follow {
                    self.scroll_back = 0;
                }
                self.notice = if self.follow {
                    "follow resumed".into()
                } else {
                    "follow paused".into()
                };
            }
            Key::F(3) => {
                self.errors_only = !self.errors_only;
                self.scroll_back = 0;
                self.notice = if self.errors_only {
                    "filter: errors + GC".into()
                } else {
                    "filter: all".into()
                };
            }
            Key::F(4) => {
                self.mode = Mode::Palette;
                self.palette_selection = 0;
                self.completions.clear();
            }
            Key::F(5) => {
                self.notice = "telemetry requested: pos, who, late, gct".into();
                return Action::Refresh;
            }
            Key::F(8) => {
                self.clear_before = self.lines.len();
                self.scroll_back = 0;
                self.follow = true;
                self.notice = "view cleared — log file unchanged".into();
            }
            Key::CtrlR => self.begin_search(),
            Key::Char('/') if self.mode == Mode::Command && self.active.is_empty() => {
                self.begin_search()
            }
            Key::PgUp => {
                self.follow = false;
                self.scroll_back = self.scroll_back.saturating_add(1);
            }
            Key::PgDn => {
                self.scroll_back = self.scroll_back.saturating_sub(1);
                if self.scroll_back == 0 {
                    self.follow = true;
                }
            }
            Key::Tab if self.mode == Mode::Command => {
                let completed = complete(&self.active);
                self.completions = completed.matches;
                self.set_active_text(&completed.text);
            }
            Key::Up | Key::Down if self.mode == Mode::Palette => {
                if key == Key::Up {
                    self.palette_selection = self.palette_selection.saturating_sub(1);
                } else {
                    self.palette_selection = (self.palette_selection + 1).min(PALETTE.len() - 1);
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
            Key::Enter => match self.mode {
                Mode::Command => {
                    let command = self.active.trim().to_string();
                    if !command.is_empty() {
                        self.history.push(&command);
                        self.set_active_text("");
                        self.completions.clear();
                        self.notice = format!("sent: {command}");
                        return Action::Submit(command);
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
                }
                Mode::Palette => {
                    let command = PALETTE[self.palette_selection].to_string();
                    self.mode = Mode::Command;
                    self.history.push(&command);
                    self.set_active_text("");
                    self.notice = format!("sent: {command}");
                    return Action::Submit(command);
                }
            },
            Key::Char(ch) if self.mode != Mode::Palette => {
                self.active.insert(self.cursor, ch);
                self.cursor += ch.len_utf8();
                self.after_edit();
            }
            Key::Backspace if self.mode != Mode::Palette => {
                if let Some((start, _)) = previous_char(&self.active, self.cursor) {
                    self.active.drain(start..self.cursor);
                    self.cursor = start;
                    self.after_edit();
                }
            }
            Key::Delete if self.mode != Mode::Palette => {
                if self.cursor < self.active.len() {
                    let end = next_char(&self.active, self.cursor);
                    self.active.drain(self.cursor..end);
                    self.after_edit();
                }
            }
            Key::Left if self.mode != Mode::Palette => {
                if let Some((start, _)) = previous_char(&self.active, self.cursor) {
                    self.cursor = start;
                }
            }
            Key::Right if self.mode != Mode::Palette => {
                self.cursor = next_char(&self.active, self.cursor);
            }
            Key::Home if self.mode != Mode::Palette => self.cursor = 0,
            Key::End if self.mode != Mode::Palette => self.cursor = self.active.len(),
            _ => {}
        }
        Action::None
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
            self.scroll_back = 0;
            self.follow = true;
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
        self.scroll_back = 0;
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn scroll_back(&self) -> usize {
        self.scroll_back
    }

    pub fn errors_only(&self) -> bool {
        self.errors_only
    }

    pub fn palette_selection(&self) -> usize {
        self.palette_selection
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

fn update_telemetry(model: &mut ConsoleModel, line: &LogLine) {
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
        assert_eq!(m.handle(Key::Enter), Action::Submit("who".into()));
        assert_eq!(m.handle(Key::CtrlC), Action::Exit);
        assert_eq!(m.handle(Key::Esc), Action::Exit);
        assert_eq!(m.handle(Key::F(5)), Action::Refresh);
    }

    #[test]
    fn completion_is_applied_to_command_input() {
        let mut m = ConsoleModel::new(false);
        m.set_active_text("wh");
        m.handle(Key::Tab);
        assert_eq!(m.active_text(), "who ");
    }
}
