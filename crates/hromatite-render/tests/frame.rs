//! Host-side tests for the nuklear UI crate.
//!
//! They run the real C shim: a frame is built, converted and copied out exactly
//! the way the resident will do it. No GPU, no hook, no game.

use hromatite_render::nk::{Input, Keys, Nk, Ui};

fn frame_with(nk: &mut Nk, x: f32, y: f32, left_down: bool) -> hromatite_render::nk::Frame {
    let input = Input {
        mouse_x: x,
        mouse_y: y,
        left_pressed: left_down,
        left_down,
        scroll: 0.0,
        keys: Keys::default(),
    };
    nk.frame(&input)
}

/// A full click: release first, then press.
///
/// nuklear only records `clicked_pos` when the button state *changes*, so a
/// scan that presses at a new point every frame without releasing in between
/// would keep hitting the first point.
fn click(nk: &mut Nk, x: f32, y: f32) {
    frame_with(nk, x, y, false);
    let _ = frame_with(nk, x, y, true);
}

/// Restore the state a fresh `Ui` would have, so a scan starts from a known point.
fn reset(ui: &mut Ui) {
    ui.open = true;
    ui.esp = true;
    ui.boxes = true;
    ui.crosshair = false;
    ui.distance = 50.0;
    ui.players.clear();
    ui.camera = Default::default();
    ui.status = String::new();
}

/// Scan a rectangle for a click point that makes `want` true.
///
/// `reset` runs before every attempt, so a test can either restore a fresh `Ui`
/// or deliberately keep the state left by the previous click.
///
/// The alternative would be hard-coding pixel coordinates, which breaks on the
/// first layout tweak. Scanning keeps the test about behaviour ("a click on the
/// close button closes the window") instead of about geometry.
fn find_click_in(
    nk: &mut Nk,
    x0: i32,
    x1: i32,
    y0: i32,
    y1: i32,
    reset: impl Fn(&mut Ui),
    want: impl Fn(&Ui) -> bool,
) -> Option<(f32, f32)> {
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            reset(nk.ui_mut());
            click(nk, x as f32, y as f32);
            if want(nk.ui()) {
                return Some((x as f32, y as f32));
            }
            x += 1;
        }
        y += 1;
    }
    None
}

#[test]
fn frame_without_input_is_not_empty() {
    let mut nk = Nk::new().expect("context");
    let frame = nk.frame(&Input::default());
    assert!(
        !frame.commands.is_empty(),
        "a frame with no input at all must still emit draw commands"
    );
    assert!(
        frame.commands.iter().any(|c| !c.vertices.is_empty()),
        "at least one command must carry geometry"
    );
}

#[test]
fn close_button_click_closes_the_window() {
    let mut nk = Nk::new().expect("context");
    let hit = find_click_in(&mut nk, 200, 356, 20, 110, reset, |ui| !ui.open)
        .expect("some point in the header must be the close button");
    assert!(!nk.ui().open, "click at {hit:?} must close the window");
}

#[test]
fn reopen_button_click_opens_the_window() {
    let mut nk = Nk::new().expect("context");
    let _hit = find_click_in(&mut nk, 200, 356, 20, 110, reset, |ui| !ui.open)
        .expect("close button");
    assert!(!nk.ui().open);

    let reopen = find_click_in(&mut nk, 16, 160, 16, 70, |_| {}, |ui| ui.open)
        .expect("the collapsed window must offer a reopen button");
    assert!(nk.ui().open, "click at {reopen:?} must reopen the window");
}

#[test]
fn esp_checkbox_click_flips_state() {
    let mut nk = Nk::new().expect("context");
    assert!(nk.ui().esp);
    let hit = find_click_in(&mut nk, 16, 356, 20, 140, reset, |ui| !ui.esp)
        .expect("some point in the toggle row must be the ESP checkbox");
    assert!(!nk.ui().esp, "click at {hit:?} must toggle ESP off");
}

#[test]
fn distance_is_clamped_to_range() {
    let mut nk = Nk::new().expect("context");

    nk.ui_mut().distance = 1.0e6;
    let _ = nk.frame(&Input::default());
    assert!(
        nk.ui().distance <= hromatite_render::nk::MAX_DISTANCE,
        "distance must never exceed MAX_DISTANCE, got {}",
        nk.ui().distance
    );

    nk.ui_mut().distance = -1.0e6;
    let _ = nk.frame(&Input::default());
    assert!(
        nk.ui().distance >= hromatite_render::nk::MIN_DISTANCE,
        "distance must never go below MIN_DISTANCE, got {}",
        nk.ui().distance
    );
}

#[test]
fn player_list_emits_one_row_per_player() {
    let mut nk = Nk::new().expect("context");
    for i in 0..5 {
        nk.ui_mut().push_player(&format!("player{i}"), i as f32 * 3.5);
    }
    let frame = nk.frame(&Input::default());
    assert_eq!(frame.stats.player_rows, 5, "one label row per player");
}

#[test]
fn non_ascii_player_names_are_sanitized() {
    let mut nk = Nk::new().expect("context");
    nk.ui_mut().push_player("Vlad", 1.0);
    nk.ui_mut().push_player("Вася", 2.0);

    let ui = nk.ui();
    assert!(ui.players[0].name.is_ascii());
    assert!(
        ui.players[1].name.is_ascii(),
        "nuklear's default font cannot draw Cyrillic: {:?}",
        ui.players[1].name
    );

    // And the frame itself must survive a directly-assigned non-ASCII name.
    let ui = nk.ui_mut();
    ui.players.push(hromatite_render::nk::Player {
        name: String::from("Прямое имя"),
        distance: 3.0,
    });
    let frame = nk.frame(&Input::default());
    assert_eq!(frame.stats.player_rows, 3);
}

#[test]
fn draw_indices_stay_inside_the_vertex_range() {
    let mut nk = Nk::new().expect("context");
    nk.ui_mut().push_player("a", 1.0);
    nk.ui_mut().push_player("b", 2.0);
    let frame = nk.frame(&Input::default());

    assert!(!frame.commands.is_empty());
    for (i, cmd) in frame.commands.iter().enumerate() {
        let out_of_range = cmd
            .indices
            .iter()
            .find(|&&idx| (idx as usize) >= cmd.vertices.len());
        assert!(
            out_of_range.is_none(),
            "command {i} indexes vertex {out_of_range:?} outside {} vertices",
            cmd.vertices.len()
        );
        assert!(cmd.texture <= 1, "texture ids are 0 (shape) or 1 (font)");
    }
}

#[test]
fn font_atlas_is_baked() {
    let nk = Nk::new().expect("context");
    let atlas = nk.font_texture().expect("baked font atlas");
    let (w, h) = (atlas.width(), atlas.height());
    assert!(w > 0, "atlas width");
    assert!(h > 0, "atlas height");
    assert_eq!(atlas.data().len(), w as usize * h as usize * 4);
    assert!(
        atlas.data().iter().any(|&b| b != 0),
        "the baked atlas must not be blank"
    );
}

/// Regression test: nuklear's TTF decompressor keeps its cursor in process-wide
/// `static`s, so two concurrent `Nk::new()` calls used to corrupt the font bake
/// and abort on `nk_decompress`'s assertion.
#[test]
fn contexts_can_be_created_concurrently() {
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let mut nk = Nk::new().expect("context");
                let frame = nk.frame(&Input::default());
                assert!(!frame.commands.is_empty());
            });
        }
    });
}

