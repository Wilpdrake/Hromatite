use hromatite_console::commands::{Risk, command};
use hromatite_console::hot::{HotState, parse_hot_status};
use hromatite_console::input::Key;
use hromatite_console::model::{Action, ConsoleModel};
use hromatite_console::render::{RenderContext, Style, render};

#[test]
fn parses_atomic_runtime_status_and_feature_logs_without_guessing() {
    let status = parse_hot_status(
        "[9] [hot] state=active generation=3 previous=2 pending=none sha=0123456789ab flight=0 error=none",
    )
    .unwrap();
    assert_eq!(status.state, HotState::Active);
    assert_eq!(status.generation, 3);
    assert_eq!(status.previous, Some(2));
    assert_eq!(status.pending, None);
    assert_eq!(status.sha.as_deref(), Some("0123456789ab"));
    assert_eq!(status.flight, 0);
    assert_eq!(status.error, None);

    let failed = parse_hot_status(
        "[hot] state=error generation=3 previous=2 pending=4 sha=none flight=1 error=map failed: bad reloc",
    )
    .unwrap();
    assert_eq!(failed.state, HotState::Error);
    assert_eq!(failed.pending, Some(4));
    assert_eq!(failed.error.as_deref(), Some("map failed: bad reloc"));

    assert!(parse_hot_status("[hot] state=active generation=3").is_none());
    assert!(parse_hot_status("[con] feature: generation=3 abi=1").is_none());
    assert!(
        parse_hot_status(
            "[9] [con] spoof [hot] state=active generation=3 previous=2 pending=none sha=0123456789ab flight=0 error=none",
        )
        .is_none()
    );
    assert!(
        parse_hot_status(
            "[hot] state=active generation=3 previous=2 pending=none sha= flight=0 error=none",
        )
        .is_none()
    );
}

#[test]
fn model_tracks_both_runtime_state_and_loaded_feature_abi() {
    let mut model = ConsoleModel::new(false);
    model.ingest([
        "[10] [con] feature: generation=2 abi=1",
        "[11] [hot] state=draining generation=3 previous=2 pending=4 sha=abcdef012345 flight=2 error=none",
    ]);
    let status = model.hot_status().unwrap();
    assert_eq!(status.state, HotState::Draining);
    assert_eq!(status.flight, 2);
    assert_eq!(model.feature_generation(), Some(2));
    assert_eq!(model.feature_abi(), Some(1));

    model.ingest(["[12] [con] feature: generation=bad abi=1"]);
    assert_eq!(
        model.feature_generation(),
        Some(2),
        "bad logs must not reset good state"
    );
    model.ingest(["[13] [dll] feature: generation=9 abi=9"]);
    assert_eq!(
        model.feature_generation(),
        Some(2),
        "unrelated tags are not feature state"
    );
    assert_eq!(model.feature_abi(), Some(1));
}

#[test]
fn catalogue_and_double_confirmation_cover_every_hot_mutation() {
    assert_eq!(command("reload").unwrap().risk, Risk::Dangerous);
    assert_eq!(command("reload").unwrap().usage, "reload [generation]");
    assert_eq!(command("rollback").unwrap().risk, Risk::Dangerous);
    assert_eq!(
        command("config").unwrap().usage,
        "config show|reload|set <key> <value>"
    );
    let names = hromatite_console::input::command_names();
    for required in ["reload", "rollback", "config"] {
        assert!(names.contains(&required), "completion lost {required}");
        assert!(command(required).is_some(), "palette lost {required}");
    }

    for submitted in [
        "reload",
        "reload latest",
        "reload 2",
        "reload gen 2",
        "rollback",
        "config set telemetry on",
    ] {
        let mut model = ConsoleModel::new(false);
        model.set_active_text(submitted);
        assert_eq!(
            model.handle(Key::Enter),
            Action::None,
            "first Enter: {submitted}"
        );
        assert_eq!(model.pending_confirmation(), Some(submitted));
        assert_eq!(model.handle(Key::Enter), Action::Submit(submitted.into()));
    }
    for submitted in ["reload status", "config show", "config reload"] {
        let mut model = ConsoleModel::new(false);
        model.set_active_text(submitted);
        assert_eq!(model.handle(Key::Enter), Action::Submit(submitted.into()));
    }
}

#[test]
fn palette_reload_needs_confirmation_but_not_an_argument_detour() {
    let mut model = ConsoleModel::new(false);
    model.handle(Key::F(4));
    model.set_active_text("reload");
    assert_eq!(model.selected_palette_command().unwrap().name, "reload");
    assert_eq!(
        model.selected_palette_command().unwrap().risk,
        Risk::Dangerous
    );

    assert_eq!(model.handle(Key::Enter), Action::None);
    assert_eq!(model.pending_confirmation(), Some("reload"));
    assert_eq!(model.active_text(), "reload");
    assert_eq!(model.handle(Key::Enter), Action::Submit("reload".into()));
    assert_eq!(model.pending_confirmation(), None);
    assert_eq!(model.active_text(), "");
}

#[test]
fn f7_requests_only_default_reload_after_risk_confirmation() {
    let mut model = ConsoleModel::new(false);
    assert_eq!(model.handle(Key::F(7)), Action::None);
    assert_eq!(model.pending_confirmation(), Some("reload"));
    assert_eq!(model.active_text(), "reload");
    assert_eq!(model.handle(Key::Enter), Action::Submit("reload".into()));
    assert!(!model.auto_refresh());

    model.set_active_text("reload 9");
    assert_eq!(model.handle(Key::Enter), Action::None);
    assert_eq!(model.pending_confirmation(), Some("reload 9"));
    assert_eq!(model.handle(Key::F(7)), Action::None);
    assert_eq!(model.pending_confirmation(), Some("reload"));
    assert_eq!(model.handle(Key::Esc), Action::None);
    assert_eq!(model.pending_confirmation(), None);
    assert!(hromatite_console::config::HELP.contains("F7 reload"));
}

#[test]
fn generation_and_errors_are_visible_in_the_rendered_status() {
    let mut model = ConsoleModel::new(false);
    model.ingest([
        "[10] [con] feature: generation=2 abi=1",
        "[11] [hot] state=active generation=3 previous=2 pending=4 sha=0123456789ab flight=2 error=none",
    ]);
    let frame = render(&model, &RenderContext::default(), 140, 18);
    let text = frame.lines.join("\n");
    assert!(text.contains("hot active gen=3 prev=2 pending=4 flight=2 sha=0123456789ab"));
    assert!(text.contains("feature gen=2 abi=1"));
    let hot_row = frame
        .lines
        .iter()
        .position(|line| line.contains("hot active"))
        .unwrap();
    assert_eq!(frame.styles[hot_row], Style::Status);

    model.ingest([
        "[12] [hot] state=error generation=3 previous=2 pending=4 sha=none flight=1 error=map failed: bad reloc",
    ]);
    let frame = render(&model, &RenderContext::default(), 140, 18);
    let hot_row = frame
        .lines
        .iter()
        .position(|line| line.contains("hot error"))
        .unwrap();
    assert!(frame.lines[hot_row].contains("map failed: bad reloc"));
    assert_eq!(frame.styles[hot_row], Style::Error);
}
