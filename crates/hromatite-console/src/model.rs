//! Testable UI state: filtering, scrolling, telemetry and key handling.

use crate::commands::{CATALOG, CommandSpec, Risk, command};
use crate::hot::{HotStatus, parse_hot_status};
use crate::input::{History, Key, complete};
use crate::parser::{LineKind, LogLine, parse_line};
use std::collections::{BTreeSet, VecDeque};

const MAX_LINES: usize = 20_000;

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
    ToggleAuto,
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
    pending_confirmation: Option<String>,
    history: History,
    completions: Vec<&'static str>,
    notice: String,
    demo: bool,
    auto_refresh: bool,
    auto_suspended: bool,
    show_help: bool,
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
            pending_confirmation: None,
            history: History::default(),
            completions: Vec::new(),
            notice: "type help + Enter; Ctrl-R searches logs".into(),
            demo,
            auto_refresh: false,
            auto_suspended: false,
            show_help: false,
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

    pub fn ingest(&mut self, inputs: impl IntoIterator<Item = impl AsRef<str>>) {
        let mut added_visible = 0;
        for input in inputs {
            let line = parse_line(input.as_ref());
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

    pub fn record_sent(&mut self, command: &str) {
        self.ingest([format!("> {}", command.trim())]);
        if let Some(line) = self.lines.back_mut() {
            line.tag = "tx".into();
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
        if self.show_help && key != Key::F(1) {
            return Action::None;
        }
        match key {
            Key::F(1) => self.show_help = !self.show_help,
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
                self.clear_before = self.lines.len();
                self.scroll_back = 0;
                self.follow = true;
                self.notice = "view cleared — log file unchanged".into();
            }
            Key::CtrlR => self.begin_search(),
            Key::Char('/') if self.mode == Mode::Command && self.active.is_empty() => {
                self.begin_search()
            }
            Key::Char(':') if self.mode == Mode::Command && self.active.is_empty() => {
                self.begin_palette()
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

    fn prepare_submission(&mut self, submitted: String) -> Action {
        if matches!(submitted.as_str(), "help" | "?") {
            self.pending_confirmation = None;
            self.mode = Mode::Command;
            self.history.push(&submitted);
            self.set_active_text("");
            self.completions.clear();
            self.notice = "local field manual — Esc closes".into();
            self.show_help = true;
            return Action::None;
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

    pub fn palette_entries(&self) -> Vec<&'static CommandSpec> {
        if self.mode != Mode::Palette {
            return Vec::new();
        }
        let query = self.active.to_lowercase();
        CATALOG
            .iter()
            .filter(|item| {
                query.is_empty()
                    || item.name.contains(&query)
                    || item.usage.to_lowercase().contains(&query)
                    || item.summary.to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn selected_palette_command(&self) -> Option<&'static CommandSpec> {
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
