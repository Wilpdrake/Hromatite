//! hromatite-nuklear - immediate-mode in-game UI built on vendored nuklear.
//!
//! The crate is deliberately independent of the graphics hook and of
//! `hromatite-core`: it owns a nuklear context, lays out the panels and hands
//! the caller plain draw commands. Turning those into GPU work (and feeding
//! [`Input`] from Unity/Windows) is the resident's job.
//!
//! ```no_run
//! use hromatite_nuklear::{Input, Nk};
//!
//! let mut nk = Nk::new().expect("nuklear context");
//! nk.ui_mut().esp = true;
//! let frame = nk.frame(&Input::default());
//! for cmd in &frame.commands {
//!     let _ = (cmd.clip, cmd.texture, &cmd.vertices, &cmd.indices);
//! }
//! ```
//!
//! # Safety posture
//!
//! All `unsafe` lives in the [`sys`] module, which is the only place that
//! touches the C shim. Everything above it is safe Rust; the crate root denies
//! `unsafe_code` and the `sys` module is the single, explicit exception (the
//! same layout `hromatite-core` uses).
//!
//! # Text
//!
//! Every string that reaches nuklear is ASCII. The bundled font is ProggyClean,
//! which has no Cyrillic or CJK glyphs, and the project has already been bitten
//! once by non-ASCII text reaching a native renderer. [`Ui::push_player`] and
//! the panel builder both sanitize; anything else is replaced with `?`.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
#[allow(dead_code)] // the shim is a boundary: it covers the whole widget set the
                   // panels may need, not only what today's layout happens to call
mod sys;

use core::ffi::{c_char, c_void};
use core::marker::PhantomData;
use std::sync::Mutex;

pub use sys::FontTexture;

/// nuklear's TTF decompressor keeps its output cursor in process-wide `static`
/// variables (`nk__dout`, `nk__barrier*` in `nuklear.h`), so baking a font atlas
/// is not re-entrant. The resident bakes once on a single thread, but the crate
/// must not rely on that, so [`Nk::new`] serialises the bake.
static BAKE_LOCK: Mutex<()> = Mutex::new(());

/// Distance readout floor, in metres.
pub const MIN_DISTANCE: f32 = 0.0;
/// Distance readout ceiling, in metres.
pub const MAX_DISTANCE: f32 = 500.0;

const FONT_HEIGHT: f32 = 16.0;

const WINDOW_X: f32 = 16.0;
const WINDOW_Y: f32 = 16.0;
const WINDOW_W: f32 = 340.0;
const WINDOW_H: f32 = 430.0;

/// Size of the collapsed "reopen" window.
const DOT_W: f32 = 140.0;
const DOT_H: f32 = 44.0;

/* ------------------------------------------------------------------ */
/* input                                                               */
/* ------------------------------------------------------------------ */

/// Keyboard state the UI cares about.
///
/// The resident fills this from Unity's `Input` (or from `GetAsyncKeyState`);
/// every field is a level, not an edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub enter: bool,
    pub tab: bool,
    pub backspace: bool,
    pub delete: bool,
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

/// One frame of input, already translated to screen space.
///
/// Coordinates are in the render target's pixels, origin at the top-left, the
/// same convention nuklear uses.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub mouse_x: f32,
    pub mouse_y: f32,
    /// Left button is held right now.
    pub left_down: bool,
    /// Left button went down during this frame. A click is `left_pressed`
    /// followed by `!left_down`; [`Nk::frame`] feeds both transitions.
    pub left_pressed: bool,
    /// Vertical scroll delta for this frame, in nuklear's units.
    pub scroll: f32,
    pub keys: Keys,
}

/* ------------------------------------------------------------------ */
/* panel state                                                         */
/* ------------------------------------------------------------------ */

/// One entry of the player list.
#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub name: String,
    pub distance: f32,
}

/// Camera readout shown at the bottom of the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraReadout {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub yaw: f32,
    pub pitch: f32,
}

/// Everything the resident reads or writes.
///
/// The resident pushes player positions in, flips toggles from its own config,
/// and reads the toggles back when it draws the ESP.
#[derive(Clone, Debug)]
pub struct Ui {
    pub esp: bool,
    pub boxes: bool,
    pub crosshair: bool,
    /// ESP distance, clamped to [`MIN_DISTANCE`]::[`MAX_DISTANCE`] every frame.
    pub distance: f32,
    pub players: Vec<Player>,
    pub camera: CameraReadout,
    /// Whether the panel is expanded. When false a small reopen button is drawn.
    pub open: bool,
    /// Free-form status line; sanitized to ASCII on the way to nuklear.
    pub status: String,
}

impl Default for Ui {
    fn default() -> Ui {
        Ui {
            esp: true,
            boxes: true,
            crosshair: false,
            distance: 50.0,
            players: Vec::new(),
            camera: CameraReadout::default(),
            open: true,
            status: String::from("idle"),
        }
    }
}

impl Ui {
    /// Append a player, sanitizing the name to ASCII.
    pub fn push_player(&mut self, name: &str, distance: f32) {
        self.players.push(Player { name: ascii_lossy(name), distance });
    }
}

/* ------------------------------------------------------------------ */
/* draw output                                                         */
/* ------------------------------------------------------------------ */

/// A vertex of one draw command.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    /// Screen-space position in pixels.
    pub pos: [f32; 2],
    /// Texture coordinate; `0.5, 0.5` is the white pixel nuklear uses for
    /// filled shapes.
    pub uv: [f32; 2],
    /// Linear RGBA, `0..=255` per channel.
    pub color: [u8; 4],
}

/// One GPU draw call: a clip rectangle, a vertex slice, an index slice and a
/// texture id.
///
/// `indices` index into `vertices` (already rebased, so they start at 0).
/// `texture` is `0` for the white-pixel texture nuklear uses for shapes and `1`
/// for the baked font atlas; see [`Nk::font_texture`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawCommand {
    /// `x, y, w, h` in pixels. Zero size means "no clipping".
    pub clip: [f32; 4],
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u16>,
    pub texture: u32,
}

/// Counters that make the frame observable without a GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub draw_commands: usize,
    pub vertices: usize,
    pub indices: usize,
    pub player_rows: usize,
}

/// The result of one [`Nk::frame`] call: everything needed to draw the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub commands: Vec<DrawCommand>,
    pub stats: FrameStats,
}

/* ------------------------------------------------------------------ */
/* the context                                                         */
/* ------------------------------------------------------------------ */

/// An owning, opaque nuklear context plus the panel state.
///
/// Not `Send`/`Sync`: the nuklear context, its font atlas and its buffers are
/// plain malloc'd C objects with no locking.
pub struct Nk {
    ctx: *mut c_void,
    font: *mut c_void,
    cmds: *mut c_void,
    verts: *mut c_void,
    elems: *mut c_void,
    /// The fixed command buffer backing the context. Its address is handed to
    /// the shim once and the shim keeps using it, so `Nk` must hold on to it.
    #[allow(dead_code)]
    cmd_mem: sys::AlignedBox,
    ui: Ui,
    _not_thread_safe: PhantomData<*mut ()>,
}

/// Temporary ownership used while [`Nk::new`] assembles the C side, so a failed
/// allocation cannot leak the handles already taken.
struct Parts {
    font: *mut c_void,
    cmds: *mut c_void,
    verts: *mut c_void,
    elems: *mut c_void,
    ctx: *mut c_void,
    mem: Option<sys::AlignedBox>,
}

impl Default for Parts {
    fn default() -> Parts {
        Parts {
            font: core::ptr::null_mut(),
            cmds: core::ptr::null_mut(),
            verts: core::ptr::null_mut(),
            elems: core::ptr::null_mut(),
            ctx: core::ptr::null_mut(),
            mem: None,
        }
    }
}

impl Drop for Parts {
    fn drop(&mut self) {
        sys::context_free(self.ctx);
        sys::buffer_free(self.cmds);
        sys::buffer_free(self.verts);
        sys::buffer_free(self.elems);
        sys::font_free(self.font);
    }
}

impl Nk {
    /// Create a context with the default font baked in.
    ///
    /// Returns `None` only if the shim could not allocate (out of memory).
    pub fn new() -> Option<Nk> {
        // Recover from poisoning rather than panicking: a poisoned lock still
        // guarantees exclusive access, which is all we need here.
        let _guard = BAKE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut parts = Parts::default();
        parts.font = sys::font_new(FONT_HEIGHT)?;
        let align = cmd_align();
        parts.mem = Some(sys::AlignedBox::new(sys::cmd_size_recommended(), align)?);
        parts.cmds = sys::buffer_new()?;
        parts.verts = sys::buffer_new()?;
        parts.elems = sys::buffer_new()?;
        parts.ctx = sys::context_new(parts.font, parts.mem.as_ref()?.as_ptr(), parts.mem.as_ref()?.len())?;

        let mem = parts.mem.take()?;
        let nk = Nk {
            ctx: parts.ctx,
            font: parts.font,
            cmds: parts.cmds,
            verts: parts.verts,
            elems: parts.elems,
            cmd_mem: mem,
            ui: Ui::default(),
            _not_thread_safe: PhantomData,
        };
        // Ownership of the handles moved into `nk`; the guard must not free them
        // on the way out. Everything `parts` still holds is a copy of a pointer
        // `nk` now owns, and `mem` was taken above.
        core::mem::forget(parts);
        Some(nk)
    }

    /// Read the panel state.
    pub fn ui(&self) -> &Ui {
        &self.ui
    }

    /// Modify the panel state (toggles, player list, camera readout, ...).
    pub fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    /// The baked font atlas: texture id `1`, RGBA8888.
    ///
    /// Upload these pixels once and keep the texture alive for as long as the
    /// `Nk` exists.
    pub fn font_texture(&self) -> Option<FontTexture<'_>> {
        sys::font_texture(self.font)
    }

    /// Build one frame and return its draw commands.
    ///
    /// Safe to call with a default [`Input`]; an idle frame still produces the
    /// window chrome, and nuklear never panics on an empty input.
    pub fn frame(&mut self, input: &Input) -> Frame {
        let ctx = self.ctx;
        let c = sys::constants();

        // Rust owns the distance invariant; the property widget is only a view.
        if self.ui.distance.is_finite() {
            self.ui.distance = self.ui.distance.clamp(MIN_DISTANCE, MAX_DISTANCE);
        } else {
            self.ui.distance = MIN_DISTANCE;
        }

        sys::input_begin(ctx);
        sys::input_motion(ctx, input.mouse_x, input.mouse_y);
        // `left_pressed` is an edge and nuklear's default button behaviour fires
        // on the press, so a press must be visible as "down" during this frame.
        let left_down = input.left_down || input.left_pressed;
        sys::input_button(ctx, c.button_left, input.mouse_x, input.mouse_y, left_down);
        sys::input_scroll(ctx, 0.0, input.scroll);
        sys::input_key(ctx, c.key_shift, input.keys.shift);
        sys::input_key(ctx, c.key_ctrl, input.keys.ctrl);
        sys::input_key(ctx, c.key_alt, input.keys.alt);
        sys::input_key(ctx, c.key_del, input.keys.delete);
        sys::input_key(ctx, c.key_enter, input.keys.enter);
        sys::input_key(ctx, c.key_tab, input.keys.tab);
        sys::input_key(ctx, c.key_backspace, input.keys.backspace);
        sys::input_key(ctx, c.key_up, input.keys.up);
        sys::input_key(ctx, c.key_down, input.keys.down);
        sys::input_key(ctx, c.key_left, input.keys.left);
        sys::input_key(ctx, c.key_right, input.keys.right);
        sys::input_end(ctx);

        let mut stats = FrameStats::default();
        self.build_ui(&mut stats);

        sys::buffer_clear(self.cmds);
        sys::buffer_clear(self.verts);
        sys::buffer_clear(self.elems);
        let _ = sys::convert(ctx, self.cmds, self.verts, self.elems);
        let commands = self.collect_commands(&mut stats);

        sys::clear(ctx);

        Frame { commands, stats }
    }

    fn build_ui(&mut self, stats: &mut FrameStats) {
        let ctx = self.ctx;
        let c = sys::constants();

        if self.ui.open {
            // No NK_WINDOW_MINIMIZABLE: a minimized window stops laying out, and
            // a stray click in the title bar would silently kill the panel.
            let flags =
                c.flag_border | c.flag_movable | c.flag_scalable | c.flag_title;
            if sys::begin_titled(
                ctx,
                cptr(b"hromatite\0"),
                cptr(b"HROMATITE\0"),
                WINDOW_X,
                WINDOW_Y,
                WINDOW_W,
                WINDOW_H,
                flags,
            ) {
                self.build_panels(stats);
            }
            sys::end(ctx);
        } else {
            let flags = c.flag_border | c.flag_title | c.flag_no_scrollbar;
            if sys::begin_titled(
                ctx,
                cptr(b"hromatite_dot\0"),
                cptr(b"HROMATITE\0"),
                WINDOW_X,
                WINDOW_Y,
                DOT_W,
                DOT_H,
                flags,
            ) {
                sys::layout_row_dynamic(ctx, 26.0, 1);
                if sys::button_label(ctx, cptr(b"OPEN\0")) {
                    self.ui.open = true;
                }
            }
            sys::end(ctx);
        }
    }

    fn build_panels(&mut self, stats: &mut FrameStats) {
        let ctx = self.ctx;
        let c = sys::constants();

        /* header: title on the left, close button on the right */
        sys::layout_row_begin(ctx, c.layout_dynamic, 26.0, 2);
        sys::layout_row_push(ctx, 0.85);
        sys::label(ctx, cptr(b"HROMATITE\0"), c.text_left);
        sys::layout_row_push(ctx, 0.15);
        let closed = sys::button_label(ctx, cptr(b"X\0"));
        sys::layout_row_end(ctx);
        if closed {
            self.ui.open = false;
            return;
        }

        /* toggles */
        sys::layout_row_dynamic(ctx, 22.0, 3);
        self.ui.esp = sys::check_label(ctx, cptr(b"ESP\0"), self.ui.esp);
        self.ui.boxes = sys::check_label(ctx, cptr(b"BOX\0"), self.ui.boxes);
        self.ui.crosshair = sys::check_label(ctx, cptr(b"CROSS\0"), self.ui.crosshair);

        /* distance */
        sys::layout_row_dynamic(ctx, 26.0, 1);
        let mut distance = self.ui.distance;
        sys::property_float(ctx, cptr(b"DIST\0"), MIN_DISTANCE, &mut distance, MAX_DISTANCE, 1.0, 10.0);
        self.ui.distance = distance;

        /* player list */
        sys::layout_row_dynamic(ctx, 170.0, 1);
        let group = c.flag_border | c.flag_title;
        if sys::group_begin_titled(ctx, cptr(b"players\0"), cptr(b"PLAYERS\0"), group) {
            sys::layout_row_dynamic(ctx, 16.0, 1);
            for player in &self.ui.players {
                let name = ascii_lossy(&player.name);
                let line = format!("{name}  {:.1}m", player.distance);
                sys::text(
                    ctx,
                    line.as_bytes().as_ptr() as *const c_char,
                    line.len() as i32,
                    c.text_left,
                );
                stats.player_rows += 1;
            }
        }
        sys::group_end(ctx);

        /* camera readout */
        let cam = self.ui.camera;
        let line = format!(
            "CAM {:.1} {:.1} {:.1}  Y{:.0} P{:.0}",
            cam.x, cam.y, cam.z, cam.yaw, cam.pitch
        );
        sys::layout_row_dynamic(ctx, 16.0, 1);
        sys::text(
            ctx,
            line.as_bytes().as_ptr() as *const c_char,
            line.len() as i32,
            c.text_left,
        );

        /* status line */
        let status = ascii_lossy(&self.ui.status);
        sys::layout_row_dynamic(ctx, 14.0, 1);
        if status.is_empty() {
            sys::text(ctx, cptr(b"-\0"), 1, c.text_left);
        } else {
            sys::text(
                ctx,
                status.as_bytes().as_ptr() as *const c_char,
                status.len() as i32,
                c.text_left,
            );
        }
    }

    fn collect_commands(&self, stats: &mut FrameStats) -> Vec<DrawCommand> {
        let cmd_count = sys::draw_command_count(self.ctx);
        let mut raw = vec![sys::RawDrawCommand::default(); cmd_count];
        let written = sys::copy_draw_commands(
            self.font,
            self.ctx,
            self.cmds,
            raw.as_mut_ptr(),
            raw.len(),
        );
        raw.truncate(written);

        let vert_count = sys::vertex_count(self.verts);
        let mut verts = vec![sys::RawVertex::default(); vert_count];
        let vwritten = sys::copy_vertices(self.verts, verts.as_mut_ptr(), verts.len());
        verts.truncate(vwritten);

        let elem_count = sys::element_count(self.elems);
        let mut elems = vec![0u16; elem_count];
        let ewritten = sys::copy_elements(self.elems, elems.as_mut_ptr(), elems.len());
        elems.truncate(ewritten);

        stats.draw_commands = raw.len();
        stats.vertices = verts.len();
        stats.indices = elems.len();

        let mut out = Vec::with_capacity(raw.len());
        let mut offset = 0usize;
        for cmd in &raw {
            let end = (offset + cmd.elem_count as usize).min(elems.len());
            let slice = &elems[offset..end];
            offset = end;
            if slice.is_empty() {
                continue;
            }

            // nuklear indexes vertices globally; rebase the slice so a command
            // carries only the vertices it actually uses.
            let mut lo = u16::MAX;
            let mut hi = 0u16;
            for &idx in slice {
                lo = lo.min(idx);
                hi = hi.max(idx);
            }
            if hi < lo || hi as usize >= verts.len() {
                continue;
            }
            let (lo, hi) = (lo as usize, hi as usize);
            let vertices: Vec<Vertex> = verts[lo..=hi]
                .iter()
                .map(|v| Vertex {
                    pos: [v.x, v.y],
                    uv: [v.u, v.v],
                    color: [v.r, v.g, v.b, v.a],
                })
                .collect();
            let indices: Vec<u16> = slice.iter().map(|&idx| idx - lo as u16).collect();

            out.push(DrawCommand {
                clip: [cmd.clip_x, cmd.clip_y, cmd.clip_w, cmd.clip_h],
                vertices,
                indices,
                texture: cmd.texture_id,
            });
        }
        out
    }
}

impl Drop for Nk {
    fn drop(&mut self) {
        // Order matters: the context references the font handle and the fixed
        // command buffer, so it goes first. `cmd_mem` drops after this method.
        sys::context_free(self.ctx);
        sys::buffer_free(self.cmds);
        sys::buffer_free(self.verts);
        sys::buffer_free(self.elems);
        sys::font_free(self.font);
    }
}

/* ------------------------------------------------------------------ */
/* helpers                                                             */
/* ------------------------------------------------------------------ */

fn cmd_align() -> usize {
    sys::context_align()
        .max(sys::draw_command_align())
        .max(sys::vertex_align())
        .max(16)
}

/// Pointer to a static, NUL-terminated ASCII literal.
///
/// The literal must include the trailing `\0`; nuklear's label/button/property
/// entry points take C strings.
fn cptr(bytes: &'static [u8]) -> *const c_char {
    bytes.as_ptr() as *const c_char
}

/// Replace everything non-ASCII with `?`.
///
/// The bundled font cannot draw it, and the project has already been bitten by
/// non-ASCII text reaching a native renderer once.
fn ascii_lossy(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii() { c } else { '?' }).collect()
}
