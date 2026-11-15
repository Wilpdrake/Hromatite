//! Fixed-size ANSI/plain frame renderer.

use crate::model::{ConsoleModel, Mode, PALETTE};
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
    let width = width.max(24) as usize;
    let height = height.max(8) as usize;
    let mut lines = vec![" ".repeat(width); height];
    let mut styles = vec![Style::Normal; height];

    let mode = if model.demo() {
        "DEMO — SYNTHETIC (no game)"
    } else {
        "LIVE"
    };
    put(
        &mut lines[0],
        &format!(" HROMATITE CONSOLE  [{mode}]"),
        width,
    );
    styles[0] = Style::Header;

    let bytes = context
        .log_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "—".into());
    let age = context
        .log_age_secs
        .map_or_else(|| "—".into(), |age| format!("{age}s"));
    put(
        &mut lines[1],
        &format!(" log: {} | {} | age {age}", context.log_path, bytes),
        width,
    );
    styles[1] = Style::Status;

    let camera = model.camera.as_deref().map_or("no camera", |v| v);
    let player = model.player.as_deref().map_or("no player telemetry", |v| v);
    put(
        &mut lines[2],
        &format!(" camera: {camera} | player: {player}"),
        width,
    );
    styles[2] = Style::Status;
    let late = model.last_late.as_deref().map_or("late: no status", |v| v);
    let gc = model.last_gc.as_deref().map_or("GC: no status", |v| v);
    put(&mut lines[3], &format!(" {late} | {gc}"), width);
    styles[3] = Style::Status;
    put(
        &mut lines[4],
        &format!(" cmd: {}", context.command_path),
        width,
    );
    styles[4] = Style::Status;
    lines[5] = "─".repeat(width);
    styles[5] = Style::Dim;

    let body_start = 6;
    let body_len = height.saturating_sub(body_start + 3);
    let body = model.window(body_len);
    for (offset, line) in body.iter().enumerate() {
        put(
            &mut lines[body_start + offset],
            &format!(" {}", line.raw),
            width,
        );
        styles[body_start + offset] = match line.kind {
            LineKind::Error => Style::Error,
            LineKind::Gc => Style::Gc,
            LineKind::Late => Style::Late,
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
            " QUICK COMMANDS — ↑/↓ choose, Enter sends",
            width,
        );
        styles[body_start] = Style::Accent;
        for (index, command) in PALETTE.iter().enumerate().take(body_len.saturating_sub(1)) {
            let marker = if index == model.palette_selection() {
                ">"
            } else {
                " "
            };
            put(
                &mut lines[body_start + 1 + index],
                &format!(" {marker} {command}"),
                width,
            );
            styles[body_start + 1 + index] = if index == model.palette_selection() {
                Style::Accent
            } else {
                Style::Normal
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
    let prompt = match model.mode() {
        Mode::Command => " command> ",
        Mode::Search => " filter> ",
        Mode::Palette => " palette> ",
    };
    let prompt_width = UnicodeWidthStr::width(prompt);
    let available = width.saturating_sub(prompt_width + 1);
    let before_cursor = &model.active_text()[..model.active_cursor()];
    let start = scroll_input_to_fit(before_cursor, available);
    let visible_input = &model.active_text()[start..];
    let shown = take_columns(visible_input, available);
    put(&mut lines[input_row], &format!("{prompt}{shown}"), width);
    styles[input_row] = Style::Input;
    let cursor_columns = UnicodeWidthStr::width(&before_cursor[start..]).min(available);
    let cursor = (input_row, (prompt_width + cursor_columns).min(width - 1));

    let help = if model.notice().is_empty() {
        "F2 follow  F3 errors/GC  F4 palette  F5 telemetry  F8 clear  ^R search  Esc quit".into()
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
            Style::Header => "1;36;44",
            Style::Status => "0;36",
            Style::Error => "1;31",
            Style::Gc => "0;33",
            Style::Late => "0;32",
            Style::Accent => "1;35",
            Style::Dim => "0;90",
            Style::Input => "1;37",
        };
        out.push_str(&format!("\x1b[{code}m{line}\x1b[0m"));
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
    fn full_frame_has_fixed_geometry_and_status() {
        let mut model = ConsoleModel::new(true);
        model.ingest(["[7] [late] hits=42 hz=90 tid=0x7"]);
        let context = RenderContext {
            log_path: "/demo/hromatite.log".into(),
            command_path: "/demo/hromatite-cmd.txt".into(),
            log_bytes: Some(512),
            log_age_secs: Some(2),
        };
        let frame = render(&model, &context, 80, 18);
        assert_eq!(frame.lines.len(), 18);
        for (index, line) in frame.lines.iter().enumerate() {
            assert_eq!(
                UnicodeWidthStr::width(line.as_str()),
                80,
                "row {index}: {line:?}"
            );
        }
        let text = frame.lines.join("\n");
        assert!(text.contains("DEMO — SYNTHETIC"));
        assert!(text.contains("512 B"));
        assert!(text.contains("age 2s"));
        assert!(text.contains("FOLLOW"));
        assert!(text.contains("hits=42"));
        assert!(text.contains("/demo/hromatite-cmd.txt"));
    }

    #[test]
    fn wide_player_names_have_exact_terminal_row_geometry() {
        let mut model = ConsoleModel::new(false);
        model.player = Some("玩家界🙂".into());
        let frame = render(&model, &RenderContext::default(), 60, 12);
        assert!(frame.lines[2].contains("玩家界🙂"));
        for (row, line) in frame.lines.iter().enumerate() {
            assert_eq!(
                UnicodeWidthStr::width(line.as_str()),
                60,
                "row {row}: {line:?}"
            );
        }
    }

    #[test]
    fn wide_input_cursor_and_scrolling_use_terminal_columns() {
        let mut model = ConsoleModel::new(false);
        model.set_active_text("界🙂x");
        let frame = render(&model, &RenderContext::default(), 24, 8);
        assert_eq!(frame.cursor, (6, 15));

        model.set_active_text("1234567890界🙂xy");
        let frame = render(&model, &RenderContext::default(), 24, 8);
        assert_eq!(frame.lines[6].trim_end(), " command> 4567890界🙂xy");
        assert_eq!(frame.cursor, (6, 23));
        for line in &frame.lines {
            assert_eq!(UnicodeWidthStr::width(line.as_str()), 24);
        }
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
