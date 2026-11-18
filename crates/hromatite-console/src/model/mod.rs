//! Testable UI state: filtering, scrolling, telemetry and key handling.

mod buffers;
mod input;
mod observe;

use crate::hot::HotStatus;
use crate::input::History;
use buffers::LogBuffer;
use std::collections::BTreeSet;

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

    pub fn mode(&self) -> Mode {
        self.mode
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

    pub fn set_auto_suspended(&mut self, suspended: bool) {
        self.auto_suspended = suspended;
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = notice.into();
    }
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
