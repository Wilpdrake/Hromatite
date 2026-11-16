//! Fixed-size ANSI/plain frame renderer.

use crate::commands::Risk;
use crate::hot::HotState;
use crate::model::{ConsoleModel, Mode};
use crate::parser::LineKind;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default, Clone)]
pub struct RenderContext {
    pub log_path: String,
    pub command_path: String,
    pub log_bytes: Option<u64>,
    pub log_age_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Normal,
    Header,
    Status,
    Error,
    Gc,
    Late,
    Accent,
    Dim,
    Input,
}

pub struct Frame {
    pub lines: Vec<String>,
    pub styles: Vec<Style>,
    /// Zero-based terminal row/column where the cursor should be placed.
    pub cursor: (usize, usize),
}

pub fn render(model: &ConsoleModel, context: &RenderContext, width: u16, height: u16) -> Frame {
    if width < 24 || height < 8 {
        let w = width as usize;
        let h = height as usize;
        let mut lines = vec![" ".repeat(w); h];
        let styles = vec![Style::Error; h];
        if h > 0 {
            put(&mut lines[0], " 24x8 required", w);
        }
        if h > 1 {
            put(&mut lines[1], &format!(" current {width}x{height}"), w);
        }
        return Frame {
            lines,
            styles,
            cursor: (0, 0),
        };
    }
    let width = width as usize;
    let height = height as usize;
    let mut lines = vec![" ".repeat(width); height];
    let mut styles = vec![Style::Normal; height];

    let link = if model.demo() {
        "DEMO // SYNTHETIC"
    } else {
        match context.log_age_secs {
            Some(age) if age <= 10 => "LINKED",
            Some(_) => "STALE",
            None => "NO LOG",
        }
    };
    let age = context
        .log_age_secs
        .map_or_else(|| "--".into(), |age| format!("{age}s"));
    let automatic = if !model.auto_refresh() {
        "OFF"
    } else if model.auto_suspended() {
        "HELD"
    } else {
        "ON"
    };
    put(
        &mut lines[0],
        &format!(" HROMATITE // OPS  {link}  rx {age}  AUTO {automatic}"),
        width,
    );
    styles[0] = Style::Header;

    let tick = model.tick_hz().map_or("--".into(), |hz| format!("{hz}Hz"));
    let gc = match model.gc_healthy() {
        Some(true) => "GC OK",
        Some(false) => "GC FAULT",
        None => "GC --",
    };
    let players = model.player_count().map_or("--".into(), |n| n.to_string());
    let camera = model.camera.as_deref().unwrap_or("--");
    put(
        &mut lines[1],
        &format!(" tick {tick} | {gc} | players {players} | cam {camera}"),
        width,
    );
    styles[1] = if model.gc_healthy() == Some(false) {
        Style::Error
    } else {
        Style::Status
    };
    let feature = match (model.feature_generation(), model.feature_abi()) {
        (Some(generation), Some(abi)) => format!("feature gen={generation} abi={abi}"),
        _ => "feature --".into(),
    };
    let hot_failed = model
        .hot_status()
        .is_some_and(|status| status.state == HotState::Error || status.error.is_some());
    let hot = if let Some(status) = model.hot_status() {
        let previous = status
            .previous
            .map_or("--".into(), |value| value.to_string());
        let pending = status
            .pending
            .map_or("--".into(), |value| value.to_string());
        let mut text = format!(
            " hot {} gen={} prev={} pending={} flight={} sha={} | {feature}",
            status.state_name,
            status.generation,
            previous,
            pending,
            status.flight,
            status.sha.as_deref().unwrap_or("--"),
        );
        if let Some(error) = &status.error {
            text.push_str(" | ");
            text.push_str(error);
        }
        text
    } else {
        format!(" hot -- | {feature}")
    };
    put(&mut lines[2], &hot, width);
    styles[2] = if hot_failed {
        Style::Error
    } else {
        Style::Status
    };
    lines[3] = "─".repeat(width);
    styles[3] = Style::Dim;

    let body_start = 4;
    let body_len = height.saturating_sub(body_start + 3);
    let body = model.window(body_len);
    for (offset, line) in body.iter().enumerate() {
        let formatted = if line.tag == "raw" {
            format!(" raw  | {}", line.message)
        } else {
            format!(" {:<4} | {}", line.tag, line.message)
        };
        put(&mut lines[body_start + offset], &formatted, width);
        styles[body_start + offset] = match line.kind {
            LineKind::Error => Style::Error,
            LineKind::Gc => Style::Gc,
            LineKind::Late => Style::Late,
            LineKind::Info if line.tag == "con" => Style::Late,
            LineKind::Info if line.tag == "tx" => Style::Accent,
            LineKind::Info => Style::Normal,
        };
    }
    if body.is_empty() && body_len > 0 {
        put(
            &mut lines[body_start],
            " (no matching lines — waiting for log)",
            width,
        );
        styles[body_start] = Style::Dim;
    }
    if model.mode() == Mode::Palette && body_len > 0 {
        put(
            &mut lines[body_start],
            " command graph | type to filter | up/down select | enter accept",
            width,
        );
        styles[body_start] = Style::Accent;
        let palette = model.palette_entries();
        let capacity = body_len.saturating_sub(1);
        let start = model
            .palette_selection()
            .saturating_sub(capacity.saturating_sub(1));
        for (index, command) in palette.iter().enumerate().skip(start).take(capacity) {
            let selected = index == model.palette_selection();
            let marker = if selected { ">" } else { " " };
            let risk = match command.risk {
                Risk::ReadOnly => "SAFE",
                Risk::Advanced => "ADV",
                Risk::Dangerous => "RISK",
            };
            put(
                &mut lines[body_start + 1 + index - start],
                &format!(
                    " {marker} {risk:<4} {:<24} {}",
                    command.usage, command.summary
                ),
                width,
            );
            styles[body_start + 1 + index - start] = if command.risk == Risk::Dangerous {
                Style::Error
            } else if selected {
                Style::Accent
            } else {
                Style::Normal
            };
        }
    }

    if model.help_visible() {
        for row in body_start..body_start + body_len {
            lines[row] = " ".repeat(width);
            styles[row] = Style::Normal;
        }
        let bytes = context
            .log_bytes
            .map(human_bytes)
            .unwrap_or_else(|| "--".into());
        let help = [
            " FIELD MANUAL // help / ? / F1; Esc closes".to_string(),
            " local view: works without a live payload answer".into(),
            " enter send | tab complete | up/down command history".into(),
            " F4 or : payload commands | / or Ctrl-R log filter".into(),
            " F2 follow | F3 errors+GC | PgUp/PgDn scrollback".into(),
            " F5 refresh once | F6 auto: pos 1s, summary 5s".into(),
            " F7 reload (second Enter confirms) | F8 clear view only".into(),
            " Ctrl-C quit | Esc back".into(),
            " RISK commands require a second Enter; edits cancel it".into(),
            " channel append-only; no new game threads or sockets".into(),
            format!(" log {} ({bytes})", context.log_path),
            format!(" cmd {}", context.command_path),
        ];
        for (offset, text) in help.iter().enumerate().take(body_len) {
            put(&mut lines[body_start + offset], text, width);
            styles[body_start + offset] = if offset == 0 {
                Style::Accent
            } else {
                Style::Status
            };
        }
    }

    let filter_row = height - 3;
    let follow = if model.follow() { "FOLLOW" } else { "PAUSED" };
    let errors = if model.errors_only() { "ERR+GC" } else { "ALL" };
    let search = if model.search().is_empty() {
        "—".into()
    } else {
        format!("\"{}\"", model.search())
    };
    let state = format!(
        " {follow} | {errors} | search {search} | back {}",
        model.scroll_back()
    );
    let completions = if model.completions().is_empty() {
        state
    } else {
        format!(" Tab: {}", model.completions().join("  "))
    };
    put(&mut lines[filter_row], &completions, width);
    styles[filter_row] = Style::Dim;

    let input_row = height - 2;
    let prompt = if model.help_visible() {
        " help> "
    } else if model.pending_confirmation().is_some() {
        " danger> "
    } else {
        match model.mode() {
            Mode::Command => " ops> ",
            Mode::Search => " grep> ",
            Mode::Palette => " run> ",
        }
    };
    let prompt_width = UnicodeWidthStr::width(prompt);
    let available = width.saturating_sub(prompt_width + 1);
    let before_cursor = &model.active_text()[..model.active_cursor()];
    let start = scroll_input_to_fit(before_cursor, available);
    let visible_input = &model.active_text()[start..];
    let shown = take_columns(visible_input, available);
    put(&mut lines[input_row], &format!("{prompt}{shown}"), width);
    styles[input_row] = if model.pending_confirmation().is_some() {
        Style::Error
    } else {
        Style::Input
    };
    let cursor_columns = UnicodeWidthStr::width(&before_cursor[start..]).min(available);
    let cursor = (input_row, (prompt_width + cursor_columns).min(width - 1));

    let help = if model.notice().is_empty() {
        "F1 help  F4 commands  F5 refresh  F6 auto  / search  PgUp scroll  Esc back".into()
    } else {
        model.notice().to_string()
    };
    put(&mut lines[height - 1], &format!(" {help}"), width);
    styles[height - 1] = Style::Dim;

    Frame {
        lines,
        styles,
        cursor,
    }
}

/// Colour a complete frame and home/position the real cursor. Raw `Frame::lines`
/// remain available for deterministic, escape-free snapshots.
pub fn ansi_frame(frame: &Frame) -> String {
    let mut out = String::from("\x1b[H\x1b[0m");
    for (index, line) in frame.lines.iter().enumerate() {
        let code = match frame.styles[index] {
            Style::Normal => "0;37",
            Style::Header => "1;32",
            Style::Status => "0;36",
            Style::Error => "1;31",
            Style::Gc => "0;33",
            Style::Late => "0;32",
            Style::Accent => "1;36",
            Style::Dim => "0;90",
            Style::Input => "1;37",
        };
        let rendered = if matches!(frame.styles[index], Style::Normal | Style::Gc | Style::Late) {
            accent_values(line, code)
        } else {
            line.clone()
        };
        out.push_str(&format!("\x1b[{code}m{rendered}\x1b[0m"));
        if index + 1 < frame.lines.len() {
            out.push_str("\r\n");
        }
    }
    out.push_str(&format!(
        "\x1b[{};{}H",
        frame.cursor.0 + 1,
        frame.cursor.1 + 1
    ));
    out
}

fn accent_values(line: &str, base_code: &str) -> String {
    let mut output = String::with_capacity(line.len() + 32);
    let mut unstyled = 0;
    let mut at = 0;
    while at < line.len() {
        if let Some(length) = accented_token_len(&line[at..]) {
            output.push_str(&line[unstyled..at]);
            output.push_str("\x1b[1;36m");
            output.push_str(&line[at..at + length]);
            output.push_str(&format!("\x1b[{base_code}m"));
            at += length;
            unstyled = at;
        } else {
            at += line[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    output.push_str(&line[unstyled..]);
    output
}

fn accented_token_len(text: &str) -> Option<usize> {
    for prefix in ["0x", "id=", "tid=", "hits=", "hz="] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let length = rest
                .bytes()
                .take_while(|byte| byte.is_ascii_hexdigit() || matches!(byte, b'x' | b'X' | b'.'))
                .count();
            if length > 0 {
                return Some(prefix.len() + length);
            }
        }
    }
    None
}

fn put(destination: &mut String, text: &str, width: usize) {
    let safe: String = text.chars().map(sanitize_char).collect();
    *destination = fit(&safe, width);
}

fn fit(text: &str, width: usize) -> String {
    let text_width = UnicodeWidthStr::width(text);
    let mut result = if text_width <= width {
        text.to_string()
    } else if width == 0 {
        String::new()
    } else {
        let mut value = take_columns(text, width - 1);
        value.push('…');
        value
    };
    result.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(UnicodeWidthStr::width(result.as_str())),
    ));
    result
}

fn take_columns(text: &str, width: usize) -> String {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if UnicodeWidthStr::width(&text[..next]) > width {
            break;
        }
        end = next;
    }
    text[..end].to_string()
}

fn sanitize_char(ch: char) -> char {
    match ch {
        '\t' => ' ',
        ch if ch.is_control() => '�',
        ch => ch,
    }
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn scroll_input_to_fit(before_cursor: &str, available: usize) -> usize {
    if available == 0 || UnicodeWidthStr::width(before_cursor) <= available {
        return 0;
    }
    before_cursor
        .char_indices()
        .skip(1)
        .find_map(|(index, _)| {
            (UnicodeWidthStr::width(&before_cursor[index..]) <= available).then_some(index)
        })
        .unwrap_or(before_cursor.len())
}

#[cfg(test)]
mod tests {
    use super::{RenderContext, render};
    use crate::model::ConsoleModel;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn compact_frame_prioritizes_link_tick_gc_and_targets() {
        let mut model = ConsoleModel::new(true);
        model.ingest([
            "[7] [late] hits=42 hz=90.0 tid=0x7",
            "[8] [con] who: list size 1, items 0xaa, cap 4",
            "[9] [gct] 17 record(s), 17 live, 0 DEAD",
            "[10] [con] pos: get_position = (1.000, 2.000, 3.000)  [raw 0x1 0x2]",
        ]);
        let context = RenderContext {
            log_path: "/demo/hromatite.log".into(),
            command_path: "/demo/hromatite-cmd.txt".into(),
            log_bytes: Some(512),
            log_age_secs: Some(2),
        };
        let frame = render(&model, &context, 100, 18);
        assert_eq!(frame.lines.len(), 18);
        for (index, line) in frame.lines.iter().enumerate() {
            assert_eq!(
                UnicodeWidthStr::width(line.as_str()),
                100,
                "row {index}: {line:?}"
            );
        }
        assert!(frame.lines[0].contains("HROMATITE // OPS"));
        assert!(frame.lines[0].contains("DEMO // SYNTHETIC"));
        assert!(frame.lines[0].contains("rx 2s"));
        assert!(frame.lines[0].contains("AUTO OFF"));
        assert!(frame.lines[1].contains("tick 90.0Hz"));
        assert!(frame.lines[1].contains("GC OK"));
        assert!(frame.lines[1].contains("players 1"));
        let text = frame.lines.join("\n");
        assert!(text.contains("con"));
        assert!(text.contains("get_position"));
        assert!(!text.contains("[10]"));
        assert!(!text.contains("/demo/hromatite-cmd.txt"));
    }

    #[test]
    fn palette_shows_descriptions_and_marks_dangerous_commands() {
        let mut model = ConsoleModel::new(false);
        model.handle(crate::input::Key::F(4));
        model.set_active_text("write");
        let frame = render(&model, &RenderContext::default(), 100, 20);
        let text = frame.lines.join("\n");
        assert!(text.contains("RISK"));
        assert!(text.contains("wq <addr> <hex64>"));
        assert!(text.contains("write one qword"));
        assert!(text.contains("type to filter"));
    }

    #[test]
    fn help_keeps_paths_and_protocol_details_available_on_demand() {
        let mut model = ConsoleModel::new(false);
        model.handle(crate::input::Key::F(1));
        let context = RenderContext {
            log_path: "/pfx/hromatite.log".into(),
            command_path: "/pfx/hromatite-cmd.txt".into(),
            log_bytes: Some(512),
            log_age_secs: Some(0),
        };
        let text = render(&model, &context, 100, 20).lines.join("\n");
        assert!(text.contains("FIELD MANUAL"));
        assert!(text.contains("F6 auto"));
        assert!(text.contains("/pfx/hromatite.log"));
        assert!(text.contains("512 B"));
        assert!(text.contains("/pfx/hromatite-cmd.txt"));
        assert!(text.contains("append-only"));
    }

    #[test]
    fn wide_player_names_have_exact_terminal_row_geometry() {
        let mut model = ConsoleModel::new(false);
        model.camera = Some("玩家界🙂".into());
        let frame = render(&model, &RenderContext::default(), 60, 12);
        assert!(frame.lines[1].contains("玩家界🙂"));
        for (row, line) in frame.lines.iter().enumerate() {
            assert_eq!(
                UnicodeWidthStr::width(line.as_str()),
                60,
                "row {row}: {line:?}"
            );
        }
    }

    #[test]
    fn undersized_frames_stay_inside_the_real_terminal_and_explain_the_limit() {
        let model = ConsoleModel::new(false);
        let frame = render(&model, &RenderContext::default(), 10, 6);
        assert_eq!(frame.lines.len(), 6);
        assert!(
            frame
                .lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) == 10)
        );
        assert!(frame.cursor.0 < 6 && frame.cursor.1 < 10);
        assert!(frame.lines.join("\n").contains("24x8"));
    }

    #[test]
    fn wide_input_cursor_and_scrolling_use_terminal_columns() {
        let mut model = ConsoleModel::new(false);
        model.set_active_text("界🙂x");
        let frame = render(&model, &RenderContext::default(), 24, 8);
        assert_eq!(frame.cursor, (6, 11));

        model.set_active_text("1234567890123界🙂xy");
        let frame = render(&model, &RenderContext::default(), 24, 8);
        assert_eq!(frame.lines[6].trim_end(), " ops> 34567890123界🙂xy");
        assert_eq!(frame.cursor, (6, 23));
        for line in &frame.lines {
            assert_eq!(UnicodeWidthStr::width(line.as_str()), 24);
        }
    }

    #[test]
    fn ansi_renderer_accents_addresses_and_ids_but_plain_text_stays_clean() {
        let mut model = ConsoleModel::new(false);
        model.ingest(["[1] [con] esp: id=7 target=0x3027c1c0"]);
        let frame = super::render(&model, &RenderContext::default(), 80, 12);
        assert!(!frame.lines.join("\n").contains('\u{1b}'));
        let ansi = super::ansi_frame(&frame);
        assert!(ansi.contains("\u{1b}[1;36m0x3027c1c0\u{1b}[0;32m"));
        assert!(ansi.contains("\u{1b}[1;36mid=7\u{1b}[0;32m"));
    }

    #[test]
    fn plain_renderer_strips_terminal_controls_from_log_data() {
        let mut model = ConsoleModel::new(false);
        model.ingest(["[1] [dll] malicious \u{1b}[2J text"]);
        let context = RenderContext::default();
        let text = render(&model, &context, 70, 12).lines.join("\n");
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("malicious"));
    }
}
