//! Command input editing, completion, palette, risk confirmation, and key dispatch.

use super::{Action, ConsoleModel, Mode};
use crate::commands::{Risk, SPECS, Spec, command};
use crate::input::{Key, complete};

impl ConsoleModel {
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
