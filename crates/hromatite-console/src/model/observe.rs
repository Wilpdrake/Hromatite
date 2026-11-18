//! Log ingestion, status/telemetry observation, and hot-reload status parsing.

use super::ConsoleModel;
use super::buffers::MAX_LINES;
use crate::hot::{HotStatus, parse_hot_status};
use crate::parser::{LogLine, parse_line};

impl ConsoleModel {
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
