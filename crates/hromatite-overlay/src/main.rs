//! `hromatite-overlay` — host-side, read-only ESP + control panel.
//!
//! The game is *not* touched: the payload answers `esp` / `pos` / `who` into its
//! own log file (that channel already exists and adds no sockets, pipes or
//! threads to the game), and this program draws everything in two X11 windows of
//! its own:
//!
//!   * the **panel** — rounded, translucent, draggable: link state, radar,
//!     target list with distances, camera readout, switches and sliders;
//!   * the **ESP layer** — sits exactly over the game's client area, always
//!     click-through, and is shaped to only the pixels it actually draws, so
//!     everything else is transparent and the game is untouched.
//!
//! Raw Xlib FFI on purpose: the workspace has no external crates.
//!
//! ```text
//! tools/hromatite-overlay.sh [--prefix DIR] [--demo]
//! ```
//!
//! Controls:
//!   * F8              — panel click-through / interactive;
//!   * F9              — ESP boxes on/off;
//!   * drag the header — move the panel (remembered in the config file);
//!   * `attach` toggle — the ESP layer follows the VRChat window;
//!   * `--demo`        — synthetic targets, no game needed (design preview).

#![allow(non_snake_case)]

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

// ---- Xlib -----------------------------------------------------------------

type Display = c_void;
type Window = c_ulong;
type Atom = c_ulong;
type GContext = *mut c_void;
type XFontStruct = c_void;
type XID = c_ulong;

#[repr(C)]
struct XSetWindowAttributes {
    background_pixmap: c_ulong,
    background_pixel: c_ulong,
    border_pixmap: c_ulong,
    border_pixel: c_ulong,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    _pad1: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    _pad2: c_int,
    event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    _pad3: c_int,
    colormap: c_ulong,
    cursor: c_ulong,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XRectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

const CW_BACK_PIXMAP: c_ulong = 1 << 0;
const CW_BACK_PIXEL: c_ulong = 1 << 1;
const CW_OVERRIDE_REDIRECT: c_ulong = 1 << 9;
const CW_EVENT_MASK: c_ulong = 1 << 11;

const EXPOSURE_MASK: c_long = 1 << 12;
const KEY_PRESS_MASK: c_long = 1 << 0;
const BUTTON_PRESS_MASK: c_long = 1 << 2;
const BUTTON_RELEASE_MASK: c_long = 1 << 3;
const POINTER_MOTION_MASK: c_long = 1 << 6;
const STRUCTURE_MASK: c_long = 1 << 17;

const INPUT_OUTPUT: c_uint = 1;
const PROP_MODE_REPLACE: c_int = 0;
const SHAPE_BOUNDING: c_int = 0;
const SHAPE_INPUT: c_int = 2;
const SHAPE_SET: c_int = 0;
const UNSORTED: c_int = 0;
const XA_WINDOW: c_ulong = 33;
const XA_CARDINAL: c_ulong = 6;

const KEY_F8: c_ulong = 0xffc5;
const KEY_F9: c_ulong = 0xffc6;

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XDefaultScreen(d: *mut Display) -> c_int;
    fn XRootWindow(d: *mut Display, screen: c_int) -> Window;
    fn XDefaultDepth(d: *mut Display, screen: c_int) -> c_int;
    fn XCreateWindow(
        d: *mut Display,
        parent: Window,
        x: c_int,
        y: c_int,
        w: c_uint,
        h: c_uint,
        border: c_uint,
        depth: c_int,
        class: c_uint,
        visual: *mut c_void,
        valuemask: c_ulong,
        attrs: *mut XSetWindowAttributes,
    ) -> Window;
    fn XMapWindow(d: *mut Display, w: Window) -> c_int;
    fn XResizeWindow(d: *mut Display, w: Window, width: c_uint, height: c_uint) -> c_int;
    fn XMoveWindow(d: *mut Display, w: Window, x: c_int, y: c_int) -> c_int;
    fn XRaiseWindow(d: *mut Display, w: Window) -> c_int;
    fn XCreatePixmap(d: *mut Display, drawable: XID, w: c_uint, h: c_uint, depth: c_uint) -> XID;
    fn XFreePixmap(d: *mut Display, pixmap: XID) -> c_int;
    fn XCopyArea(
        d: *mut Display,
        src: XID,
        dest: XID,
        gc: GContext,
        sx: c_int,
        sy: c_int,
        w: c_uint,
        h: c_uint,
        dx: c_int,
        dy: c_int,
    ) -> c_int;
    fn XSetGraphicsExposures(d: *mut Display, gc: GContext, onoff: c_int) -> c_int;
    fn XStoreName(d: *mut Display, w: Window, name: *const c_char) -> c_int;
    fn XCreateGC(d: *mut Display, w: Window, mask: c_ulong, values: *mut c_void) -> GContext;
    fn XFreeGC(d: *mut Display, gc: GContext) -> c_int;
    fn XSetForeground(d: *mut Display, gc: GContext, pixel: c_ulong) -> c_int;
    fn XFillRectangle(
        d: *mut Display,
        w: XID,
        gc: GContext,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XDrawLine(
        d: *mut Display,
        w: XID,
        gc: GContext,
        x1: c_int,
        y1: c_int,
        x2: c_int,
        y2: c_int,
    ) -> c_int;
    fn XDrawArc(
        d: *mut Display,
        w: XID,
        gc: GContext,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        angle1: c_int,
        angle2: c_int,
    ) -> c_int;
    fn XFillArc(
        d: *mut Display,
        w: XID,
        gc: GContext,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        angle1: c_int,
        angle2: c_int,
    ) -> c_int;
    fn XDrawString(
        d: *mut Display,
        w: XID,
        gc: GContext,
        x: c_int,
        y: c_int,
        s: *const c_char,
        len: c_int,
    ) -> c_int;
    fn XLoadQueryFont(d: *mut Display, name: *const c_char) -> *mut XFontStruct;
    fn XSetFont(d: *mut Display, gc: GContext, font: c_ulong) -> c_int;
    fn XInternAtom(d: *mut Display, name: *const c_char, only_if_exists: c_int) -> Atom;
    fn XChangeProperty(
        d: *mut Display,
        w: Window,
        property: Atom,
        kind: Atom,
        format: c_int,
        mode: c_int,
        data: *const u8,
        n: c_int,
    ) -> c_int;
    fn XGetWindowProperty(
        d: *mut Display,
        w: Window,
        property: Atom,
        offset: c_long,
        length: c_long,
        delete: c_int,
        req_type: Atom,
        actual_type: *mut Atom,
        actual_format: *mut c_int,
        nitems: *mut c_ulong,
        bytes_after: *mut c_ulong,
        prop: *mut *mut u8,
    ) -> c_int;
    fn XFetchName(d: *mut Display, w: Window, name: *mut *mut c_char) -> c_int;
    fn XFree(p: *mut c_void) -> c_int;
    fn XGetGeometry(
        d: *mut Display,
        drawable: XID,
        root: *mut Window,
        x: *mut c_int,
        y: *mut c_int,
        width: *mut c_uint,
        height: *mut c_uint,
        border: *mut c_uint,
        depth: *mut c_uint,
    ) -> c_int;
    fn XTranslateCoordinates(
        d: *mut Display,
        src: Window,
        dest: Window,
        src_x: c_int,
        src_y: c_int,
        dest_x: *mut c_int,
        dest_y: *mut c_int,
        child: *mut Window,
    ) -> c_int;
    fn XGrabKey(
        d: *mut Display,
        keycode: c_int,
        modifiers: c_uint,
        grab_window: Window,
        owner_events: c_int,
        pointer_mode: c_int,
        keyboard_mode: c_int,
    ) -> c_int;
    fn XKeysymToKeycode(d: *mut Display, keysym: c_ulong) -> u8;
    fn XFlush(d: *mut Display) -> c_int;
    fn XPending(d: *mut Display) -> c_int;
    fn XNextEvent(d: *mut Display, ev: *mut [u8; 192]) -> c_int;
}

#[link(name = "Xext")]
unsafe extern "C" {
    fn XShapeCombineRectangles(
        d: *mut Display,
        w: Window,
        kind: c_int,
        x: c_int,
        y: c_int,
        rects: *mut XRectangle,
        n: c_int,
        op: c_int,
        ordering: c_int,
    ) -> c_int;
    fn XShapeQueryExtension(d: *mut Display, ev: *mut c_int, err: *mut c_int) -> c_int;
}

// ---- palette --------------------------------------------------------------

const C_BG: u32 = 0x000a_0c11;
const C_PANEL: u32 = 0x0010_131b;
const C_CARD: u32 = 0x0016_1c27;
const C_CARD_HI: u32 = 0x001c_2433;
const C_BORDER: u32 = 0x0026_2f42;
const C_TRACK: u32 = 0x0022_2a3a;
const C_TEXT: u32 = 0x00dc_e3ee;
const C_MUTED: u32 = 0x007a_849c;
const C_DIM: u32 = 0x0048_5166;
const C_ACCENT: u32 = 0x004d_e1ff;
const C_ACCENT2: u32 = 0x00a9_7bff;
const C_OK: u32 = 0x004d_f0a8;
const C_WARN: u32 = 0x00ff_d166;
const C_BAD: u32 = 0x00ff_5d73;
const C_BLACK: u32 = 0x0000_0000;

fn lerp_col(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| -> u32 {
        let ca = (a >> s) & 0xff;
        let cb = (b >> s) & 0xff;
        (ca as f32 + (cb as f32 - ca as f32) * t) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

fn dist_color(d: f32) -> u32 {
    if d < 0.0 {
        C_MUTED
    } else if d < 10.0 {
        C_ACCENT
    } else if d < 25.0 {
        C_ACCENT2
    } else if d < 60.0 {
        0x007f_a8ff
    } else {
        C_MUTED
    }
}

// ---- config ---------------------------------------------------------------

struct Config {
    esp: bool,
    names: bool,
    snaplines: bool,
    radar: bool,
    show_cam: bool,
    attach: bool,
    box_dist: f32,
    opacity: f32,
    x: i32,
    y: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            esp: true,
            names: true,
            snaplines: true,
            radar: true,
            show_cam: true,
            attach: false,
            box_dist: 60.0,
            opacity: 0.88,
            x: 32,
            y: 48,
        }
    }
}

fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        });
    base.join("hromatite-overlay.conf")
}

impl Config {
    fn load() -> Self {
        let mut c = Config::default();
        if let Ok(text) = std::fs::read_to_string(config_path()) {
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let v = v.trim();
                match k.trim() {
                    "esp" => c.esp = v == "1",
                    "names" => c.names = v == "1",
                    "snaplines" => c.snaplines = v == "1",
                    "radar" => c.radar = v == "1",
                    "show_cam" => c.show_cam = v == "1",
                    "attach" => c.attach = v == "1",
                    "box_dist" => c.box_dist = v.parse().unwrap_or(c.box_dist),
                    "opacity" => c.opacity = v.parse().unwrap_or(c.opacity),
                    "x" => c.x = v.parse().unwrap_or(c.x),
                    "y" => c.y = v.parse().unwrap_or(c.y),
                    _ => {}
                }
            }
        }
        c
    }

    fn save(&self) {
        let text = format!(
            "esp={}\nnames={}\nsnaplines={}\nradar={}\nshow_cam={}\nattach={}\nbox_dist={}\nopacity={}\nx={}\ny={}\n",
            self.esp as u8,
            self.names as u8,
            self.snaplines as u8,
            self.radar as u8,
            self.show_cam as u8,
            self.attach as u8,
            self.box_dist,
            self.opacity,
            self.x,
            self.y
        );
        if let Ok(mut f) = File::create(config_path()) {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

// ---- telemetry ------------------------------------------------------------

/// One target on screen. `x/y/h` are smoothed towards `tx/ty/th` so boxes glide
/// instead of jumping at the payload's ~5 Hz update rate.
struct Target {
    id: u32,
    name: String,
    dist: f32,
    /// World position (`w=` in `esp`, or the `who` line's `pos=`).
    world: Option<(f32, f32, f32)>,
    tx: f32,
    ty: f32,
    th: f32,
    x: f32,
    y: f32,
    h: f32,
    alpha: f32,
    seen: Instant,
}

#[derive(Default)]
struct Telemetry {
    targets: Vec<Target>,
    cam: Option<(f32, f32, f32)>,
    fwd: Option<(f32, f32, f32)>,
    px: (u32, u32),
    err: f32,
    esp_at: Option<Instant>,
    who_at: Option<Instant>,
}

fn parse_vec3(s: &str) -> Option<(f32, f32, f32)> {
    let inner = s.trim().trim_start_matches('(').trim_end_matches(')');
    let mut it = inner.split(',').map(|p| p.trim().parse::<f32>().ok());
    match (it.next()?, it.next()?, it.next()?) {
        (Some(a), Some(b), Some(c)) => Some((a, b, c)),
        _ => None,
    }
}

/// Value of `key=` inside a log line. The key has to start a token, otherwise
/// `d=` would also match the tail of `head=`.
fn kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=");
    let bytes = line.as_bytes();
    let mut at = 0usize;
    while let Some(i) = line[at..].find(&pat) {
        let start = at + i;
        if start == 0 || bytes[start - 1] == b' ' {
            let rest = &line[start + pat.len()..];
            let end = rest.find(' ').unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
        at = start + pat.len();
    }
    None
}

/// `key=(x, y, z)` — parentheses, because the value itself contains spaces and
/// the generic `kv` stops at the first one.
fn kv_vec3(line: &str, key: &str) -> Option<(f32, f32, f32)> {
    let pat = format!("{key}=(");
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let end = rest.find(')')?;
    parse_vec3(&format!("({})", &rest[..end]))
}

/// Everything after `key=` (names may contain spaces).
fn kv_rest<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("{key}=");
    let i = line.find(&pat)?;
    Some(line[i + pat.len()..].trim())
}

impl Telemetry {
    fn feed(&mut self, line: &str) -> bool {
        let line = line.trim_end_matches('\r');
        let mut changed = false;
        if let Some(head) = line.find("esp: head") {
            let head = &line[head..];
            if let (Some(w), Some(h)) = (kv(head, "w"), kv(head, "h"))
                && let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>())
                && w > 0
                && h > 0
            {
                self.px = (w, h);
            }
            self.cam = kv_vec3(head, "cam");
            self.fwd = kv_vec3(head, "fwd");
            self.err = kv(head, "err").and_then(|v| v.parse().ok()).unwrap_or(-1.0);
            self.esp_at = Some(Instant::now());
            // No `clear()`: ids arrive right after the header, and wiping the
            // list every frame would restart every fade-in and kill the glide.
            changed = true;
        } else if let Some(rest) = line.find("esp: id=") {
            let body = &line[rest..];
            let num = |k: &str| kv(body, k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
            let Some(id) = kv(body, "id").and_then(|v| v.parse::<u32>().ok()) else {
                return false;
            };
            let name = kv_rest(body, "name").unwrap_or("").to_string();
            let world = kv(body, "w").and_then(|s| {
                let mut it = s.split(',').map(|p| p.trim().parse::<f32>().ok());
                match (it.next()?, it.next()?, it.next()?) {
                    (Some(a), Some(b), Some(c)) => Some((a, b, c)),
                    _ => None,
                }
            });
            let t = Target {
                id,
                name,
                dist: num("d"),
                world,
                tx: num("x"),
                ty: num("y"),
                th: num("h"),
                x: num("x"),
                y: num("y"),
                h: num("h"),
                alpha: 0.0,
                seen: Instant::now(),
            };
            // Same id twice within one update: replace, do not duplicate.
            if let Some(old) = self.targets.iter_mut().find(|o| o.id == id) {
                let (ax, ay, ah, aa) = (old.x, old.y, old.h, old.alpha);
                *old = t;
                old.x = ax;
                old.y = ay;
                old.h = ah;
                old.alpha = aa;
            } else {
                self.targets.push(t);
            }
            changed = true;
        } else if line.contains("pos: get_position = ") {
            if let Some(rest) = line.split("pos: get_position = ").nth(1)
                && let Some(v) = rest.split("  [raw").next().and_then(parse_vec3)
            {
                self.cam = Some(v);
                changed = true;
            }
        } else if line.contains("player #") && line.contains("pos=(") {
            // Fallback path for a payload without `esp`: `who` lines only.
            let id = kv(line, "id")
                .and_then(|r| r.parse::<u32>().ok())
                .unwrap_or(u32::MAX);
            let pos = line
                .split("pos=(")
                .nth(1)
                .and_then(|r| parse_vec3(&format!("({}", r.split(')').next().unwrap_or(""))));
            let name = line
                .split("go=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap_or("")
                .to_string();
            if let Some((px, py, pz)) = pos {
                let d = match self.cam {
                    Some((cx, cy, cz)) => {
                        ((px - cx).powi(2) + (py - cy).powi(2) + (pz - cz).powi(2)).sqrt()
                    }
                    None => -1.0,
                };
                let t = Target {
                    id,
                    name,
                    dist: d,
                    world: Some((px, py, pz)),
                    tx: 0.0,
                    ty: 0.0,
                    th: 0.0,
                    x: 0.0,
                    y: 0.0,
                    h: 0.0,
                    alpha: 1.0,
                    seen: Instant::now(),
                };
                if let Some(old) = self.targets.iter_mut().find(|o| o.id == id) {
                    old.dist = d;
                    old.world = Some((px, py, pz));
                    old.seen = Instant::now();
                } else {
                    self.targets.push(t);
                }
                changed = true;
            }
            self.who_at = Some(Instant::now());
        }
        changed
    }

    /// Fade-in / fade-out and glide. Returns true while anything is animating.
    fn animate(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for t in &mut self.targets {
            let k = (dt * 14.0).clamp(0.0, 1.0);
            t.x += (if t.tx > 0.0 { t.tx } else { t.x } - t.x) * k;
            t.y += (if t.ty > 0.0 { t.ty } else { t.y } - t.y) * k;
            t.h += (if t.th > 0.0 { t.th } else { t.h } - t.h) * k;
            let want = if t.seen.elapsed() < Duration::from_millis(900) {
                1.0
            } else {
                0.0
            };
            let step = (dt * 6.0).clamp(0.0, 1.0);
            if (t.alpha - want).abs() > 0.01 {
                t.alpha += (want - t.alpha) * step;
                moving = true;
            }
            if (t.x - t.tx).abs() > 0.5 && t.tx > 0.0 {
                moving = true;
            }
        }
        self.targets.retain(|t| t.alpha > 0.01 || t.seen.elapsed() < Duration::from_secs(3));
        moving
    }

    fn best_dist(&self) -> f32 {
        self.targets
            .iter()
            .filter(|t| t.dist >= 0.0)
            .map(|t| t.dist)
            .fold(f32::MAX, f32::min)
    }
}

// ---- ui -------------------------------------------------------------------

struct Gfx {
    d: *mut Display,
    root: Window,
    panel: Window,
    panel_pix: XID,
    panel_gc: GContext,
    esp: Window,
    esp_pix: XID,
    esp_gc: GContext,
    /// Geometry of the ESP layer (also the game client area).
    geo: (i32, i32, u32, u32),
    shape_ok: bool,
}

struct Ui {
    cfg: Config,
    interactive: bool,
    dragging: bool,
    drag_dx: i32,
    drag_dy: i32,
    slider: Option<u8>,
    hover: Option<u8>,
    toast: Option<(String, Instant)>,
    cam_win: Window,
    cam_win_at: Option<Instant>,
}

impl Ui {
    fn say(&mut self, s: &str) {
        self.toast = Some((s.to_string(), Instant::now()));
    }
}

const W_ESP: u8 = 1;
const W_NAMES: u8 = 2;
const W_SNAP: u8 = 3;
const W_RADAR: u8 = 4;
const W_CAM: u8 = 5;
const W_DIST: u8 = 6;
const W_ATTACH: u8 = 7;
const W_PASS: u8 = 8;
const W_OPA: u8 = 9;

const PANEL_W: u32 = 340;
const PAD: i32 = 12;
const HEADER_H: i32 = 30;
const BASELINE: i32 = 10;
const CHAR_W: i32 = 6;
const RADAR_SZ: i32 = 132;

enum Row {
    Status,
    Radar,
    Section(&'static str),
    Player(u32, String, f32),
    Note(String),
    Switch(u8, &'static str, bool),
    Slider(u8, &'static str, String, f32),
}

impl Row {
    fn height(&self) -> i32 {
        match self {
            Row::Status => 18,
            Row::Radar => RADAR_SZ + 10,
            Row::Section(_) => 22,
            Row::Player(..) => 17,
            Row::Note(_) => 15,
            Row::Switch(..) => 23,
            Row::Slider(..) => 30,
        }
    }
}

fn build_rows(cfg: &Config, tel: &Telemetry, interactive: bool) -> Vec<Row> {
    let mut rows = vec![Row::Status];
    if cfg.radar {
        rows.push(Row::Radar);
    }
    rows.push(Row::Section("targets"));
    let mut list: Vec<&Target> = tel
        .targets
        .iter()
        .filter(|t| t.dist >= 0.0 || !t.name.is_empty())
        .collect();
    list.sort_by(|a, b| a.dist.total_cmp(&b.dist));
    if list.is_empty() {
        rows.push(Row::Note("no targets in this instance".into()));
    }
    for t in list.iter().take(5) {
        rows.push(Row::Player(t.id, t.name.clone(), t.dist));
    }
    if cfg.show_cam {
        rows.push(Row::Section("camera"));
        let cam = match tel.cam {
            Some((x, y, z)) => format!("{x:.2}   {y:.2}   {z:.2}"),
            None => "-   -   -".to_string(),
        };
        rows.push(Row::Note(cam));
    }
    rows.push(Row::Section("visuals"));
    rows.push(Row::Switch(W_ESP, "ESP boxes", cfg.esp));
    rows.push(Row::Switch(W_NAMES, "names + distance", cfg.names));
    rows.push(Row::Switch(W_SNAP, "snaplines", cfg.snaplines));
    rows.push(Row::Switch(W_RADAR, "radar", cfg.radar));
    rows.push(Row::Slider(
        W_DIST,
        "max distance",
        format!("{:.0} m", cfg.box_dist),
        ((cfg.box_dist - 5.0) / 195.0).clamp(0.0, 1.0),
    ));
    rows.push(Row::Section("window"));
    rows.push(Row::Switch(W_ATTACH, "follow the game window", cfg.attach));
    rows.push(Row::Switch(W_PASS, "interactive (clicks)", interactive));
    rows.push(Row::Slider(
        W_OPA,
        "opacity",
        format!("{:.2}", cfg.opacity),
        ((cfg.opacity - 0.2) / 0.8).clamp(0.0, 1.0),
    ));
    rows
}

/// Assign y positions. Both drawing and hit-testing go through this, so a widget
/// can never be drawn where it is not clickable.
fn place(rows: &[Row]) -> Vec<(i32, i32)> {
    let mut y = HEADER_H + PAD;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let h = r.height();
        out.push((y, h));
        y += h;
    }
    out
}

fn panel_height(placed: &[(i32, i32)]) -> u32 {
    let last = placed
        .last()
        .map(|(y, h)| y + h)
        .unwrap_or(HEADER_H + PAD);
    (last + 26).max(120) as u32
}

fn hit(rows: &[Row], placed: &[(i32, i32)], my: i32) -> Option<u8> {
    for (row, (y, h)) in rows.iter().zip(placed) {
        if my < *y || my >= *y + *h {
            continue;
        }
        match row {
            Row::Switch(id, ..) => return Some(*id),
            Row::Slider(id, ..) => return Some(*id),
            _ => {}
        }
    }
    None
}

fn set_slider(cfg: &mut Config, id: u8, mx: i32) {
    let (x, w) = (PAD, PANEL_W as i32 - PAD * 2);
    let t = ((mx - x) as f32 / w as f32).clamp(0.0, 1.0);
    match id {
        W_DIST => cfg.box_dist = 5.0 + t * 195.0,
        W_OPA => cfg.opacity = 0.2 + t * 0.8,
        _ => {}
    }
}

// ---- drawing primitives ---------------------------------------------------

unsafe fn fill(g: &Gfx, dr: XID, gc: GContext, x: i32, y: i32, w: i32, h: i32, col: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // SAFETY: caller passes live display/drawable/gc; the rectangle is clipped
    // by the server, so coordinates outside the window are harmless.
    unsafe {
        XSetForeground(g.d, gc, col as c_ulong);
        XFillRectangle(g.d, dr, gc, x, y, w as c_uint, h as c_uint);
    }
}

/// Rounded rectangle without anti-aliasing: one 1px-tall run per row, with the
/// corner inset from a circle. Cheap and it reads as "rounded" at panel size.
unsafe fn round_rect(
    g: &Gfx,
    dr: XID,
    gc: GContext,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    col: u32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = r.min(w / 2).min(h / 2).max(0);
    let col = col as c_ulong;
    // SAFETY: see `fill`; same live handles.
    unsafe {
        XSetForeground(g.d, gc, col);
        if r == 0 {
            XFillRectangle(g.d, dr, gc, x, y, w as c_uint, h as c_uint);
            return;
        }
        for dy in 0..h {
            let inset = if dy < r {
                let d = (r - dy) as f32;
                r - (r as f32 * r as f32 - d * d).max(0.0).sqrt() as i32
            } else if dy >= h - r {
                let d = (dy - (h - r - 1)) as f32;
                r - (r as f32 * r as f32 - d * d).max(0.0).sqrt() as i32
            } else {
                0
            };
            XFillRectangle(
                g.d,
                dr,
                gc,
                x + inset,
                y + dy,
                (w - inset * 2).max(0) as c_uint,
                1,
            );
        }
    }
}

unsafe fn line(g: &Gfx, dr: XID, gc: GContext, x1: i32, y1: i32, x2: i32, y2: i32, col: u32) {
    // SAFETY: live handles; Xlib clips.
    unsafe {
        XSetForeground(g.d, gc, col as c_ulong);
        XDrawLine(g.d, dr, gc, x1, y1, x2, y2);
    }
}

unsafe fn circle(g: &Gfx, dr: XID, gc: GContext, cx: i32, cy: i32, r: i32, col: u32, filled: bool) {
    // SAFETY: live handles; arcs use 64ths of a degree.
    unsafe {
        XSetForeground(g.d, gc, col as c_ulong);
        if filled {
            XFillArc(
                g.d,
                dr,
                gc,
                cx - r,
                cy - r,
                (r * 2) as c_uint,
                (r * 2) as c_uint,
                0,
                360 * 64,
            );
        } else {
            XDrawArc(
                g.d,
                dr,
                gc,
                cx - r,
                cy - r,
                (r * 2) as c_uint,
                (r * 2) as c_uint,
                0,
                360 * 64,
            );
        }
    }
}

unsafe fn arc(
    g: &Gfx,
    dr: XID,
    gc: GContext,
    cx: i32,
    cy: i32,
    r: i32,
    a1: i32,
    a2: i32,
    col: u32,
) {
    // SAFETY: live handles; Xlib clips.
    unsafe {
        XSetForeground(g.d, gc, col as c_ulong);
        XDrawArc(
            g.d,
            dr,
            gc,
            cx - r,
            cy - r,
            (r * 2) as c_uint,
            (r * 2) as c_uint,
            a1 * 64,
            a2 * 64,
        );
    }
}

unsafe fn text(g: &Gfx, dr: XID, gc: GContext, x: i32, y_top: i32, s: &str, col: u32) {
    // SAFETY: caller passes live handles; Xlib copies the string (latin-1).
    unsafe {
        XSetForeground(g.d, gc, col as c_ulong);
        XDrawString(
            g.d,
            dr,
            gc,
            x,
            y_top + BASELINE,
            s.as_ptr() as *const c_char,
            s.len() as c_int,
        );
    }
}

unsafe fn text_right(g: &Gfx, dr: XID, gc: GContext, x_right: i32, y_top: i32, s: &str, col: u32) {
    let w = s.chars().count() as i32 * CHAR_W;
    // SAFETY: see `text`.
    unsafe { text(g, dr, gc, x_right - w, y_top, s, col) };
}

fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('.');
    out
}

// ---- masks ----------------------------------------------------------------

fn rounded_mask(w: i32, h: i32, r: i32) -> Vec<XRectangle> {
    let mut out = Vec::new();
    for dy in 0..h {
        let inset = if dy < r {
            let d = (r - dy) as f32;
            r - (r as f32 * r as f32 - d * d).max(0.0).sqrt() as i32
        } else if dy >= h - r {
            let d = (dy - (h - r - 1)) as f32;
            r - (r as f32 * r as f32 - d * d).max(0.0).sqrt() as i32
        } else {
            0
        };
        out.push(XRectangle {
            x: inset as i16,
            y: dy as i16,
            width: (w - inset * 2).max(0) as u16,
            height: 1,
        });
    }
    out
}

fn shape_window(g: &Gfx, win: Window, kind: c_int, rects: &mut [XRectangle]) {
    if !g.shape_ok {
        return;
    }
    // SAFETY: live display/window; the rectangle array is alive for the call.
    unsafe {
        XShapeCombineRectangles(
            g.d,
            win,
            kind,
            0,
            0,
            rects.as_mut_ptr(),
            rects.len() as c_int,
            SHAPE_SET,
            UNSORTED,
        );
    }
}

fn apply_opacity(g: &Gfx, win: Window, opacity: f32) {
    // SAFETY: live display/window; EWMH property is advisory.
    unsafe {
        let atom = XInternAtom(g.d, c"_NET_WM_WINDOW_OPACITY".as_ptr(), 0);
        let v: c_ulong = (opacity.clamp(0.05, 1.0) as f64 * u32::MAX as f64) as c_ulong;
        let d = v.to_ne_bytes();
        XChangeProperty(
            g.d,
            win,
            atom,
            XA_CARDINAL,
            32,
            PROP_MODE_REPLACE,
            d.as_ptr(),
            1,
        );
    }
}

// ---- panel drawing --------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw_panel(
    g: &Gfx,
    ui: &Ui,
    tel: &Telemetry,
    height: u32,
    rows: &[Row],
    placed: &[(i32, i32)],
    started: Instant,
    phase: f32,
) {
    let cfg = &ui.cfg;
    let pw = PANEL_W as i32;
    let ph = height as i32;
    let dr = g.panel_pix;
    let gc = g.panel_gc;
    // SAFETY: every handle passed here was created at startup and lives for the
    // whole run; drawing is the only side effect.
    unsafe {
        fill(g, dr, gc, 0, 0, pw, ph, C_BG);
        round_rect(g, dr, gc, 0, 0, pw, ph, 10, C_BORDER);
        round_rect(g, dr, gc, 1, 1, pw - 2, ph - 2, 9, C_PANEL);

        // Header: subtle vertical gradient, then an accent rule.
        for i in 0..HEADER_H {
            let t = i as f32 / HEADER_H as f32;
            fill(
                g,
                dr,
                gc,
                1,
                1 + i,
                pw - 2,
                1,
                lerp_col(C_CARD_HI, C_PANEL, t),
            );
        }
        fill(g, dr, gc, 14, 9, 3, 12, C_ACCENT);
        text(g, dr, gc, 24, 8, "HROMATITE", C_TEXT);
        text(g, dr, gc, 24 + 9 * CHAR_W + 6, 8, "OVERLAY", C_DIM);
        text_right(g, dr, gc, pw - 14, 8, "F8 / F9", C_DIM);
        fill(g, dr, gc, 10, HEADER_H, pw - 20, 1, C_BORDER);

        // Link state: green while `esp` answers, amber otherwise.
        let fresh = tel
            .esp_at
            .map(|t| t.elapsed() < Duration::from_secs(2))
            .unwrap_or(false);
        let link_col = if fresh { C_OK } else { C_WARN };
        let pulse = 0.55 + 0.45 * (phase * 3.0).sin().abs();
        circle(g, dr, gc, PAD + 4, HEADER_H + PAD + 6, 3, lerp_col(C_PANEL, link_col, pulse), true);

        for (row, (y, h)) in rows.iter().zip(placed) {
            match row {
                Row::Status => {
                    let state = if fresh { "LINKED" } else { "NO LINK" };
                    text(g, dr, gc, PAD + 14, *y, state, link_col);
                    let n = tel.targets.len();
                    let nearest = tel.best_dist();
                    let tail = if nearest.is_finite() {
                        format!("| {n} tgt | nearest {nearest:.0}m")
                    } else {
                        format!("| {n} tgt")
                    };
                    text(g, dr, gc, PAD + 14 + 8 * CHAR_W, *y, &tail, C_MUTED);
                    if cfg.show_cam && tel.err >= 0.0 {
                        text_right(
                            g,
                            dr,
                            gc,
                            pw - PAD,
                            *y,
                            &format!("proj {:.0}px", tel.err),
                            if tel.err < 8.0 { C_DIM } else { C_WARN },
                        );
                    }
                }
                Row::Section(t) => {
                    fill(g, dr, gc, PAD, *y + 8, 3, 9, C_ACCENT2);
                    let mut up = t.to_uppercase();
                    up = up.replace('_', " ");
                    text(g, dr, gc, PAD + 9, *y + 6, &up, C_MUTED);
                    let used = PAD + 9 + up.chars().count() as i32 * CHAR_W + 8;
                    fill(g, dr, gc, used, *y + 12, pw - PAD - used, 1, C_TRACK);
                }
                Row::Radar => draw_radar(g, tel, cfg, PAD, *y, RADAR_SZ, phase),
                Row::Player(id, name, dist) => {
                    let mut col = dist_color(*dist);
                    if *dist > cfg.box_dist {
                        col = C_BAD;
                    }
                    fill(g, dr, gc, PAD, *y + 3, 2, 11, col);
                    text(g, dr, gc, PAD + 9, *y + 2, &format!("#{id}"), col);
                    let label = if name.is_empty() {
                        "player".to_string()
                    } else {
                        fit(name, 20)
                    };
                    text(g, dr, gc, PAD + 9 + 5 * CHAR_W, *y + 2, &label, C_TEXT);
                    let right = if *dist >= 0.0 {
                        format!("{dist:.1} m")
                    } else {
                        "-".to_string()
                    };
                    text_right(g, dr, gc, pw - PAD, *y + 2, &right, col);
                    let bar = ((pw - PAD * 2) as f32 * (1.0 - (dist / 120.0).clamp(0.0, 1.0)))
                        .max(0.0) as i32;
                    fill(g, dr, gc, PAD, *y + 15, pw - PAD * 2, 1, C_TRACK);
                    fill(g, dr, gc, PAD, *y + 15, bar, 1, col);
                }
                Row::Note(s) => text(g, dr, gc, PAD, *y, s, C_DIM),
                Row::Switch(id, label, on) => {
                    let hov = ui.hover == Some(*id);
                    if hov {
                        round_rect(g, dr, gc, PAD - 5, *y, pw - PAD * 2 + 10, *h, 6, C_CARD);
                    }
                    text(g, dr, gc, PAD, *y + 4, label, if hov { C_TEXT } else { C_MUTED });
                    let (tx, ty, tw, th) = (pw - PAD - 30, *y + 4, 30, 14);
                    round_rect(g, dr, gc, tx, ty, tw, th, 7, if *on { C_ACCENT } else { C_TRACK });
                    let kx = if *on { tx + tw - 12 } else { tx + 2 };
                    circle(g, dr, gc, kx + 5, ty + 7, 5, if *on { C_BG } else { C_DIM }, true);
                }
                Row::Slider(id, label, value, t) => {
                    let hov = ui.hover == Some(*id);
                    text(g, dr, gc, PAD, *y, label, if hov { C_TEXT } else { C_MUTED });
                    text_right(g, dr, gc, pw - PAD, *y, value, C_ACCENT);
                    let (tx, ty, tw) = (PAD, *y + 18, pw - PAD * 2);
                    round_rect(g, dr, gc, tx, ty, tw, 4, 2, C_TRACK);
                    let fw = (tw as f32 * *t).max(2.0) as i32;
                    round_rect(g, dr, gc, tx, ty, fw, 4, 2, C_ACCENT);
                    let kx = (tx + fw - 3).clamp(PAD, pw - PAD - 6);
                    round_rect(g, dr, gc, kx, ty - 4, 6, 12, 3, C_TEXT);
                }
            }
        }

        // Footer: uptime + the one hint that saves people.
        let foot = ph - 18;
        let msg = match &ui.toast {
            Some((s, at)) if at.elapsed() < Duration::from_millis(1600) => s.clone(),
            _ => format!(
                "up {:.0}s | drag the header | {} key(s)",
                started.elapsed().as_secs_f32(),
                if ui.interactive { "F8 exit" } else { "F8 grab" }
            ),
        };
        text(g, dr, gc, PAD, foot, &fit(&msg, 36), C_DIM);

        // One blit: the window never shows a half-drawn frame.
        XCopyArea(g.d, g.panel_pix, g.panel, g.panel_gc, 0, 0, PANEL_W, height as u32, 0, 0);
    }
}

fn draw_radar(g: &Gfx, tel: &Telemetry, cfg: &Config, x: i32, y: i32, size: i32, phase: f32) {
    let dr = g.panel_pix;
    let gc = g.panel_gc;
    let cx = x + size / 2;
    let cy = y + size / 2 - 2;
    let r = size / 2 - 6;
    let range = cfg.box_dist.max(10.0);
    // SAFETY: live handles, drawing only.
    unsafe {
        round_rect(g, dr, gc, x, y, size, size, 8, C_CARD);
        circle(g, dr, gc, cx, cy, r, C_TRACK, false);
        circle(g, dr, gc, cx, cy, r / 2, C_TRACK, false);
        line(g, dr, gc, cx - r, cy, cx + r, cy, C_TRACK);
        line(g, dr, gc, cx, cy - r, cx, cy + r, C_TRACK);

        // Sweep: the radar is rotated so camera forward is up.
        let sweep = (phase * 40.0) % 360.0;
        fade_cone(g, cx, cy, r, sweep);

        circle(g, dr, gc, cx, cy, 2, C_TEXT, true);

        let yaw = match tel.fwd {
            Some((fx, _, fz)) if fx * fx + fz * fz > 1e-6 => fx.atan2(fz),
            _ => 0.0,
        };
        let (sin, cos) = (yaw.sin(), yaw.cos());
        let Some(cam) = tel.cam else {
            text(g, dr, gc, x + 8, y + size - 16, "no camera", C_DIM);
            return;
        };
        let mut plotted = 0;
        for t in tel.targets.iter() {
            let Some((rx, rz)) = radar_plot(t, cam, sin, cos) else {
                continue;
            };
            let (px, py) = (
                cx + (rx / range * r as f32) as i32,
                cy - (rz / range * r as f32) as i32,
            );
            if (px - cx).pow(2) + (py - cy).pow(2) > r * r {
                continue;
            }
            circle(g, dr, gc, px, py, 2, dist_color(t.dist), true);
            plotted += 1;
        }
        if plotted == 0 {
            text(g, dr, gc, x + 8, y + size - 16, "no contacts", C_DIM);
        }
        text(g, dr, gc, x + 8, y + 6, &format!("{:.0}m", range), C_DIM);
    }
}

/// Radar position of a target, when the payload gave us world coordinates.
/// (`esp` only carries screen pixels; `who` carries positions, so this returns
/// `None` on the pure-`esp` path and the dots simply do not appear.)
fn radar_plot(t: &Target, cam: (f32, f32, f32), sin: f32, cos: f32) -> Option<(f32, f32)> {
    let w = t.world?;
    let (dx, dz) = (w.0 - cam.0, w.2 - cam.2);
    Some((dx * cos - dz * sin, dx * sin + dz * cos))
}

fn fade_cone(g: &Gfx, cx: i32, cy: i32, r: i32, sweep: f32) {
    // SAFETY: live handles, drawing only.
    unsafe {
        line(
            g,
            g.panel_pix,
            g.panel_gc,
            cx,
            cy,
            cx + (r as f32 * (sweep - 90.0).to_radians().cos()) as i32,
            cy + (r as f32 * (sweep - 90.0).to_radians().sin()) as i32,
            C_DIM,
        );
        arc(g, g.panel_pix, g.panel_gc, cx, cy, r - 1, -(sweep as i32), 22, C_ACCENT2);
    }
}

// ---- ESP layer ------------------------------------------------------------

fn draw_esp(g: &Gfx, cfg: &Config, tel: &Telemetry) -> Vec<XRectangle> {
    let (_, _, gw, gh) = g.geo;
    let dr = g.esp_pix;
    let gc = g.esp_gc;
    let mut rects: Vec<XRectangle> = Vec::new();
    if gw == 0 || gh == 0 {
        return rects;
    }
    let (pw, ph) = tel.px;
    let kx = if pw > 0 { gw as f32 / pw as f32 } else { 1.0 };
    let ky = if ph > 0 { gh as f32 / ph as f32 } else { 1.0 };
    // SAFETY: live handles; drawing into our own pixmap.
    unsafe {
        fill(g, dr, gc, 0, 0, gw as i32, gh as i32, C_BLACK);
        let mut push = |x: i32, y: i32, w: i32, h: i32| {
            if w > 0 && h > 0 {
                rects.push(XRectangle {
                    x: x as i16,
                    y: y as i16,
                    width: w as u16,
                    height: h as u16,
                });
            }
        };

        // Centre mark: tiny, so it reads as a reticle and not as GUI.
        let (cxp, cyp) = (gw as i32 / 2, gh as i32 / 2);
        fill(g, dr, gc, cxp, cyp - 1, 1, 3, C_ACCENT);
        fill(g, dr, gc, cxp - 1, cyp, 3, 1, C_ACCENT);
        push(cxp, cyp - 1, 1, 3);
        push(cxp - 1, cyp, 3, 1);

        for t in &tel.targets {
            if t.alpha < 0.05 || t.tx <= 0.0 {
                continue;
            }
            let sx = t.x * kx;
            let sy = t.y * ky;
            let hy = t.h * ky;
            let box_h = (sy - hy).abs().clamp(8.0, gh as f32);
            if t.dist > cfg.box_dist && t.dist >= 0.0 {
                continue;
            }
            let box_w = (box_h * 0.52).clamp(6.0, gw as f32);
            let (bx, by) = ((sx - box_w / 2.0) as i32, hy.min(sy) as i32);
            let (bw, bh) = (box_w as i32, box_h as i32);
            let col = dist_color(t.dist);

            if cfg.esp {
                let arm = (bw.min(bh) / 3).clamp(5, 24);
                let th = 2;
                // Four corner brackets: the classic "cheat" box without the
                // full rectangle that UI-scanning anti-cheats count as an overlay.
                for (cx0, cy0, dx, dy) in [
                    (bx, by, 1, 1),
                    (bx + bw, by, -1, 1),
                    (bx, by + bh, 1, -1),
                    (bx + bw, by + bh, -1, -1),
                ] {
                    let hx = if dx > 0 { cx0 } else { cx0 - arm };
                    fill(g, dr, gc, hx, if dy > 0 { cy0 } else { cy0 - th }, arm, th, col);
                    push(hx, if dy > 0 { cy0 } else { cy0 - th }, arm, th);
                    let vy = if dy > 0 { cy0 } else { cy0 - arm };
                    fill(g, dr, gc, if dx > 0 { cx0 } else { cx0 - th }, vy, th, arm, col);
                    push(if dx > 0 { cx0 } else { cx0 - th }, vy, th, arm);
                }
            }

            if cfg.names {
                let label = if t.name.is_empty() {
                    format!("#{}  {:.1}m", t.id, t.dist)
                } else {
                    format!("{}  {:.1}m", fit(&t.name, 18), t.dist)
                };
                let lw = label.chars().count() as i32 * CHAR_W + 8;
                let lx = (bx + bw / 2 - lw / 2).clamp(0, (gw as i32 - lw).max(0));
                let ly = (by - 15).max(0);
                fill(g, dr, gc, lx, ly, lw, 13, C_BLACK);
                fill(g, dr, gc, lx, ly, 2, 13, col);
                push(lx, ly, lw, 13);
                text(g, dr, gc, lx + 5, ly, &label, col);
            }

            if cfg.snaplines {
                // Stepped 2px chunks: a bounding shape can only contain
                // rectangles, so a diagonal line has to be built from them.
                let (x0, y0) = (gw as i32 / 2, gh as i32);
                let (x1, y1) = (bx + bw / 2, by + bh);
                let steps = 44;
                let col_dim = lerp_col(C_BLACK, col, t.alpha * 0.55);
                for i in 0..steps {
                    let f = i as f32 / steps as f32;
                    let px = (x0 as f32 + (x1 - x0) as f32 * f) as i32;
                    let py = (y0 as f32 + (y1 - y0) as f32 * f) as i32;
                    fill(g, dr, gc, px, py, 2, 2, col_dim);
                    push(px, py, 2, 2);
                }
            }
        }
        XCopyArea(g.d, g.esp_pix, g.esp, g.esp_gc, 0, 0, gw, gh, 0, 0);
    }
    rects
}

// ---- main -----------------------------------------------------------------

fn main() {
    let mut demo = false;
    let prefix = prefix_from_args(&mut demo);
    let user_dir = prefix.join("drive_c/users/steamuser");
    let log_path = user_dir.join("hromatite.log");
    let cmd_path = user_dir.join("hromatite-cmd.txt");
    println!("overlay: log {} (demo {demo})", log_path.display());

    let cfg = Config::load();

    // SAFETY: XOpenDisplay(NULL) follows $DISPLAY.
    let display = unsafe { XOpenDisplay(std::ptr::null()) };
    if display.is_null() {
        eprintln!("overlay: cannot open X display (set DISPLAY/XAUTHORITY)");
        std::process::exit(2);
    }
    // SAFETY: live Display handle.
    let root = unsafe { XRootWindow(display, XDefaultScreen(display)) };
    // SAFETY: live Display handle.
    let depth = unsafe { XDefaultDepth(display, XDefaultScreen(display)) } as c_uint;

    let panel = make_window(display, root, cfg.x, cfg.y, PANEL_W, 260, true);
    let esp = make_window(display, root, 0, 0, 16, 16, false);
    if panel == 0 || esp == 0 {
        eprintln!("overlay: XCreateWindow failed");
        std::process::exit(2);
    }

    // SAFETY: window and handles are live.
    unsafe {
        let title = CString::new("hromatite overlay").unwrap();
        XStoreName(display, panel, title.as_ptr());
        XMapWindow(display, panel);
        XMapWindow(display, esp);

        let state = XInternAtom(display, c"_NET_WM_STATE".as_ptr(), 0);
        let above = XInternAtom(display, c"_NET_WM_STATE_ABOVE".as_ptr(), 0);
        let atom_t = XInternAtom(display, c"ATOM".as_ptr(), 0);
        let data = above.to_ne_bytes();
        for w in [panel, esp] {
            XChangeProperty(display, w, state, atom_t, 32, PROP_MODE_REPLACE, data.as_ptr(), 1);
        }

        // F8/F9 work even while the panel is click-through (grab on the root).
        for keysym in [KEY_F8, KEY_F9] {
            let keycode = XKeysymToKeycode(display, keysym) as c_int;
            for extra in [0u32, 2, 1 << 1] {
                XGrabKey(display, keycode, extra, root, 1, 1, 1);
            }
        }
        XRaiseWindow(display, panel);
        XFlush(display);
    }

    let panel_gc = unsafe { XCreateGC(display, panel, 0, std::ptr::null_mut()) };
    let font = load_font(display, panel_gc);
    let panel_pix = unsafe { XCreatePixmap(display, panel, PANEL_W, 260, depth) };
    let panel_pgc = unsafe { XCreateGC(display, panel_pix, 0, std::ptr::null_mut()) };
    if let Some(fid) = font {
        // SAFETY: live GCs.
        unsafe {
            XSetFont(display, panel_pgc, fid);
            XSetFont(display, panel_gc, fid);
        }
    }
    let esp_gc = unsafe { XCreateGC(display, esp, 0, std::ptr::null_mut()) };
    let esp_pix = unsafe { XCreatePixmap(display, esp, 16, 16, depth) };
    let esp_pgc = unsafe { XCreateGC(display, esp_pix, 0, std::ptr::null_mut()) };
    if let Some(fid) = font {
        unsafe {
            XSetFont(display, esp_pgc, fid);
        }
    }
    // SAFETY: no GraphicsExpose storm on every blit.
    unsafe {
        XSetGraphicsExposures(display, panel_gc, 0);
        XSetGraphicsExposures(display, esp_gc, 0);
    }

    let shape_ok = {
        // SAFETY: extension query on a live display.
        let (mut ev, mut err) = (0, 0);
        let ok = unsafe { XShapeQueryExtension(display, &mut ev, &mut err) };
        ok != 0
    };

    let mut g = Gfx {
        d: display,
        root,
        panel,
        panel_pix,
        panel_gc: panel_pgc,
        esp,
        esp_pix,
        esp_gc: esp_pgc,
        geo: (0, 0, 16, 16),
        shape_ok,
    };

    let mut ui = Ui {
        cfg,
        interactive: false,
        dragging: false,
        drag_dx: 0,
        drag_dy: 0,
        slider: None,
        hover: None,
        toast: None,
        cam_win: 0,
        cam_win_at: None,
    };
    // Panel: rounded mask for drawing, empty input mask = clicks pass through.
    let mut mask = rounded_mask(PANEL_W as i32, 260, 10);
    shape_window(&g, panel, SHAPE_BOUNDING, &mut mask);
    shape_window(&g, panel, SHAPE_INPUT, &mut []);
    apply_opacity(&g, panel, ui.cfg.opacity);
    apply_opacity(&g, esp, 0.95);
    // Nothing drawn yet and no input area at all: until a game window is found
    // the ESP layer must be invisible and must not eat a single click.
    shape_window(&g, esp, SHAPE_BOUNDING, &mut []);
    shape_window(&g, esp, SHAPE_INPUT, &mut []);

    let mut tel = Telemetry::default();
    let mut log = File::open(&log_path).ok();
    let mut carry: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 1 << 16];
    let mut last_esp = Instant::now() - Duration::from_secs(5);
    let mut last_fallback = Instant::now() - Duration::from_secs(5);
    let mut dirty = true;
    let mut last_frame = Instant::now();
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut last_game_lookup = Instant::now() - Duration::from_secs(5);
    let started = Instant::now();
    let mut panel_h = 260u32;
    let mut last_shape = Instant::now() - Duration::from_secs(1);
    let mut demo_t = 0.0f32;

    loop {
        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f32().min(0.1);
        last_frame = now;

        // --- telemetry requests -------------------------------------------
        let mut wants = String::new();
        if demo {
            demo_t += dt;
            demo_feed(&mut tel, demo_t);
            dirty = true;
        } else {
            let esp_fresh = tel
                .esp_at
                .map(|t| t.elapsed() < Duration::from_secs(2))
                .unwrap_or(false);
            if now.duration_since(last_esp) >= Duration::from_millis(250) {
                wants.push_str("esp\n");
                last_esp = now;
            }
            // Fallback only: a payload without `esp` still answers `pos`/`who`.
            if !esp_fresh && now.duration_since(last_fallback) >= Duration::from_millis(1200) {
                wants.push_str("pos\nwho\n");
                last_fallback = now;
            }
            if !wants.is_empty()
                && let Ok(mut f) = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&cmd_path)
            {
                let _ = f.write_all(wants.as_bytes());
                let _ = f.flush();
            }

            if log.is_none() {
                log = File::open(&log_path).ok();
            }
            if let Some(f) = log.as_mut() {
                loop {
                    match f.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(n) => carry.extend_from_slice(&scratch[..n]),
                        Err(_) => break,
                    }
                }
                while let Some(pos) = carry.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = carry.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                    if text.contains("esp:")
                        || text.contains("pos: get_position")
                        || text.contains("player #")
                    {
                        if tel.feed(&text) {
                            dirty = true;
                        }
                    }
                }
                if carry.len() > 1 << 20 {
                    carry.clear();
                }
            }
        }

        if tel.animate(dt) {
            dirty = true;
        }

        // --- layout --------------------------------------------------------
        let rows = build_rows(&ui.cfg, &tel, ui.interactive);
        let placed = place(&rows);
        let want_h = panel_height(&placed);
        if want_h != panel_h {
            panel_h = want_h;
            // SAFETY: live window; the back buffer is recreated to match.
            unsafe {
                XResizeWindow(display, panel, PANEL_W, panel_h);
                XFreePixmap(display, g.panel_pix);
                XFreeGC(display, g.panel_gc);
            }
            let new_pix = unsafe { XCreatePixmap(display, panel, PANEL_W, panel_h, depth) };
            let new_gc = unsafe { XCreateGC(display, new_pix, 0, std::ptr::null_mut()) };
            if let Some(fid) = font {
                unsafe {
                    XSetFont(display, new_gc, fid);
                }
            }
            g.panel_pix = new_pix;
            g.panel_gc = new_gc;
            let mut mask = rounded_mask(PANEL_W as i32, panel_h as i32, 10);
            shape_window(&g, panel, SHAPE_BOUNDING, &mut mask);
            let mut input = if ui.interactive {
                rounded_mask(PANEL_W as i32, panel_h as i32, 10)
            } else {
                Vec::new()
            };
            shape_window(&g, panel, SHAPE_INPUT, &mut input);
            dirty = true;
        }

        // --- panel frame ---------------------------------------------------
        if dirty || now.duration_since(last_draw) >= Duration::from_millis(120) {
            last_draw = now;
            dirty = false;
            draw_panel(
                &g,
                &ui,
                &tel,
                panel_h,
                &rows,
                &placed,
                started,
                now.duration_since(started).as_secs_f32(),
            );
        }

        // --- game window / ESP ---------------------------------------------
        if now.duration_since(last_game_lookup) >= Duration::from_millis(700) {
            last_game_lookup = now;
            let found = find_window(&g, c"VRChat");
            ui.cam_win = found;
            ui.cam_win_at = Some(now);
        }
        let game = ui.cam_win;
        if game != 0 {
            if let Some((gx, gy, gw, gh)) = window_rect(&g, game) {
                if (gx, gy, gw, gh) != g.geo {
                    // SAFETY: live windows; the ESP layer mirrors the client area.
                    unsafe {
                        XMoveWindow(display, esp, gx, gy);
                        XResizeWindow(display, esp, gw.max(1), gh.max(1));
                    }
                    g.geo = (gx, gy, gw, gh);
                    unsafe {
                        XFreePixmap(display, g.esp_pix);
                    }
                    let new_pix = unsafe { XCreatePixmap(display, esp, gw, gh, depth) };
                    let new_gc = unsafe { XCreateGC(display, new_pix, 0, std::ptr::null_mut()) };
                    if let Some(fid) = font {
                        unsafe {
                            XSetFont(display, new_gc, fid);
                        }
                    }
                    g.esp_pix = new_pix;
                    g.esp_gc = new_gc;
                    shape_window(&g, esp, SHAPE_INPUT, &mut []);
                }
                if let Some((tx, ty)) = game_origin(&g, game)
                    && ui.cfg.attach
                    && !ui.dragging
                    && (ui.cfg.x != tx + 24 || ui.cfg.y != ty + 24)
                {
                    // SAFETY: live windows.
                    unsafe {
                        XMoveWindow(display, panel, tx + 24, ty + 24);
                    }
                    ui.cfg.x = tx + 24;
                    ui.cfg.y = ty + 24;
                }
                let draw_esp_now = ui.cfg.esp || ui.cfg.snaplines;
                if draw_esp_now && now.duration_since(last_shape) >= Duration::from_millis(33) {
                    last_shape = now;
                    let mut rects = draw_esp(&g, &ui.cfg, &tel);
                    shape_window(&g, esp, SHAPE_BOUNDING, &mut rects);
                } else if !draw_esp_now && now.duration_since(last_shape) >= Duration::from_millis(200)
                {
                    last_shape = now;
                    shape_window(&g, esp, SHAPE_BOUNDING, &mut []);
                }
                // Keep our windows above the game.
                if now.duration_since(last_game_lookup) < Duration::from_millis(20) {
                    // SAFETY: live windows.
                    unsafe {
                        XRaiseWindow(display, esp);
                        XRaiseWindow(display, panel);
                    }
                }
            }
        }

        // --- events --------------------------------------------------------
        let mut shape_dirty = false;
        // SAFETY: event pumping on a live display.
        unsafe {
            while XPending(display) > 0 {
                let mut ev = [0u8; 192];
                XNextEvent(display, &mut ev);
                let kind = i32_at(&ev, 0);
                match kind {
                    2 => {
                        // KeyPress (from the root grab).
                        let key = i32_at(&ev, 84) as u32;
                        let f8 = XKeysymToKeycode(display, KEY_F8) as u32;
                        let f9 = XKeysymToKeycode(display, KEY_F9) as u32;
                        if key == f8 {
                            ui.interactive = !ui.interactive;
                            shape_dirty = true;
                            dirty = true;
                            let msg = if ui.interactive {
                                "interactive: clicks reach the panel"
                            } else {
                                "click-through: the game gets every click"
                            };
                            ui.say(msg);
                        } else if key == f9 {
                            ui.cfg.esp = !ui.cfg.esp;
                            ui.cfg.save();
                            dirty = true;
                            ui.say(if ui.cfg.esp {
                                "ESP boxes on"
                            } else {
                                "ESP boxes off"
                            });
                        }
                    }
                    4 => {
                        let (mx, my) = (i32_at(&ev, 64), i32_at(&ev, 68));
                        if my < HEADER_H {
                            ui.dragging = true;
                            ui.drag_dx = mx;
                            ui.drag_dy = my;
                        } else if let Some(id) = hit(&rows, &placed, my) {
                            match id {
                                W_ESP => ui.cfg.esp = !ui.cfg.esp,
                                W_NAMES => ui.cfg.names = !ui.cfg.names,
                                W_SNAP => ui.cfg.snaplines = !ui.cfg.snaplines,
                                W_RADAR => ui.cfg.radar = !ui.cfg.radar,
                                W_CAM => ui.cfg.show_cam = !ui.cfg.show_cam,
                                W_ATTACH => ui.cfg.attach = !ui.cfg.attach,
                                W_PASS => {
                                    ui.interactive = !ui.interactive;
                                    shape_dirty = true;
                                }
                                W_DIST => {
                                    ui.slider = Some(W_DIST);
                                    set_slider(&mut ui.cfg, W_DIST, mx);
                                }
                                W_OPA => {
                                    ui.slider = Some(W_OPA);
                                    set_slider(&mut ui.cfg, W_OPA, mx);
                                    apply_opacity(&g, panel, ui.cfg.opacity);
                                }
                                _ => {}
                            }
                            ui.cfg.save();
                            dirty = true;
                        }
                    }
                    5 => {
                        ui.dragging = false;
                        ui.slider = None;
                        ui.cfg.save();
                    }
                    6 => {
                        let (mx, my) = (i32_at(&ev, 64), i32_at(&ev, 68));
                        let (rx, ry) = (i32_at(&ev, 72), i32_at(&ev, 76));
                        if ui.dragging {
                            let nx = rx - ui.drag_dx;
                            let ny = ry - ui.drag_dy;
                            XMoveWindow(display, panel, nx, ny);
                            ui.cfg.x = nx;
                            ui.cfg.y = ny;
                            dirty = true;
                        } else if let Some(id) = ui.slider {
                            set_slider(&mut ui.cfg, id, mx);
                            apply_opacity(&g, panel, ui.cfg.opacity);
                            dirty = true;
                        } else {
                            let h = hit(&rows, &placed, my);
                            if h != ui.hover {
                                ui.hover = h;
                                dirty = true;
                            }
                        }
                    }
                    12 | 22 => dirty = true,
                    _ => {}
                }
            }
        }
        if shape_dirty {
            let mut mask = if ui.interactive {
                rounded_mask(PANEL_W as i32, panel_h as i32, 10)
            } else {
                Vec::new()
            };
            shape_window(&g, panel, SHAPE_INPUT, &mut mask);
            // SAFETY: keep the panel above the game after the shape change.
            unsafe {
                XRaiseWindow(display, panel);
            }
            dirty = true;
        }

        sleep(Duration::from_millis(16));
    }
}

fn i32_at(buf: &[u8; 192], off: usize) -> i32 {
    i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn make_window(d: *mut Display, root: Window, x: i32, y: i32, w: u32, h: u32, input: bool) -> Window {
    let mut attrs = XSetWindowAttributes {
        background_pixmap: 0,
        background_pixel: C_BG as c_ulong,
        border_pixmap: 0,
        border_pixel: 0,
        bit_gravity: 0,
        win_gravity: 0,
        backing_store: 0,
        _pad1: 0,
        backing_planes: 0,
        backing_pixel: 0,
        save_under: 0,
        _pad2: 0,
        event_mask: if input {
            EXPOSURE_MASK
                | KEY_PRESS_MASK
                | BUTTON_PRESS_MASK
                | BUTTON_RELEASE_MASK
                | POINTER_MOTION_MASK
                | STRUCTURE_MASK
        } else {
            EXPOSURE_MASK
        },
        do_not_propagate_mask: 0,
        override_redirect: 1,
        _pad3: 0,
        colormap: 0,
        cursor: 0,
    };
    let mask = CW_BACK_PIXEL | CW_EVENT_MASK | CW_OVERRIDE_REDIRECT | CW_BACK_PIXMAP;
    // SAFETY: attributes are fully initialised above.
    unsafe {
        XCreateWindow(
            d,
            root,
            x,
            y,
            w,
            h,
            0,
            0,
            INPUT_OUTPUT,
            std::ptr::null_mut(),
            mask,
            &mut attrs,
        )
    }
}

fn load_font(d: *mut Display, gc: GContext) -> Option<c_ulong> {
    for name in [
        "-misc-fixed-medium-r-normal--13-120-75-75-c-60-iso10646-1",
        "fixed",
        "6x13",
    ] {
        let c = CString::new(name).ok()?;
        // SAFETY: font lookup may return NULL, handled here.
        let f = unsafe { XLoadQueryFont(d, c.as_ptr()) };
        if !f.is_null() {
            // SAFETY: XFontStruct{fid} is the second machine word.
            let fid = unsafe { *(f as *const c_ulong).add(1) };
            // SAFETY: live GC.
            unsafe {
                XSetFont(d, gc, fid);
            }
            return Some(fid);
        }
    }
    None
}

/// Find a toplevel window by name through `_NET_CLIENT_LIST`.
fn find_window(g: &Gfx, want: &CStr) -> Window {
    // SAFETY: property read on the root window with a live display.
    unsafe {
        let atom = XInternAtom(g.d, c"_NET_CLIENT_LIST".as_ptr(), 0);
        let mut kind: Atom = 0;
        let mut fmt: c_int = 0;
        let mut n: c_ulong = 0;
        let mut after: c_ulong = 0;
        let mut prop: *mut u8 = std::ptr::null_mut();
        if XGetWindowProperty(
            g.d,
            g.root,
            atom,
            0,
            256,
            0,
            XA_WINDOW,
            &mut kind,
            &mut fmt,
            &mut n,
            &mut after,
            &mut prop,
        ) != 0
            || prop.is_null()
        {
            return 0;
        }
        let ids = std::slice::from_raw_parts(prop as *const Window, n as usize);
        let mut found = 0;
        for &w in ids {
            let mut name: *mut c_char = std::ptr::null_mut();
            if XFetchName(g.d, w, &mut name) != 0 && !name.is_null() {
                let s = CStr::from_ptr(name).to_string_lossy().to_string();
                XFree(name as *mut c_void);
                if s == want.to_string_lossy() {
                    found = w;
                    break;
                }
            }
        }
        XFree(prop as *mut c_void);
        found
    }
}

fn game_origin(g: &Gfx, w: Window) -> Option<(i32, i32)> {
    window_rect(g, w).map(|(x, y, _, _)| (x, y))
}

/// Client-area rectangle of a window, in root coordinates.
fn window_rect(g: &Gfx, w: Window) -> Option<(i32, i32, u32, u32)> {
    let mut root: Window = 0;
    let mut x = 0;
    let mut y = 0;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut border = 0u32;
    let mut depth = 0u32;
    // SAFETY: queries on a live window id.
    unsafe {
        if XGetGeometry(
            g.d,
            w,
            &mut root,
            &mut x,
            &mut y,
            &mut width,
            &mut height,
            &mut border,
            &mut depth,
        ) == 0
        {
            return None;
        }
        let (mut ax, mut ay) = (0, 0);
        let mut child: Window = 0;
        XTranslateCoordinates(g.d, w, g.root, 0, 0, &mut ax, &mut ay, &mut child);
        Some((ax, ay, width.max(1), height.max(1)))
    }
}

// ---- demo -----------------------------------------------------------------

/// Synthetic telemetry: no game, no payload, just enough to design against.
fn demo_feed(tel: &mut Telemetry, t: f32) {
    let (w, h) = (1920u32, 1080u32);
    let cam = (0.0f32, 1.6, 0.0);
    let yaw = t * 0.35;
    let fwd = (yaw.sin(), 0.0, yaw.cos());
    let mut lines = vec![format!(
        "[0] [con] esp: head w={w} h={h} err=0.3 cam=({:.2}, {:.2}, {:.2}) fwd=({:.2}, {:.2}, {:.2})",
        cam.0, cam.1, cam.2, fwd.0, fwd.1, fwd.2
    )];
    let names = ["Nyx", "Kitten", "v0id", "Sable", "m1nt"];
    for (i, name) in names.iter().enumerate() {
        let a = yaw + (i as f32 - 2.0) * 0.28 + (t * 0.7 + i as f32).sin() * 0.06;
        let d = 6.0 + i as f32 * 7.5;
        let world = (cam.0 + a.sin() * d, cam.1 + 0.1 * i as f32, cam.2 + a.cos() * d);
        let rel = (world.0 - cam.0, world.2 - cam.2);
        let right = rel.0 * fwd.2 - rel.1 * fwd.0;
        let forward = rel.0 * fwd.0 + rel.1 * fwd.2;
        if forward <= 0.2 {
            continue;
        }
        let half = (w as f32 / 2.0) / 0.577; // ~60° vertical FOV
        let sx = w as f32 / 2.0 + right / forward * half;
        let sy = h as f32 / 2.0 + 90.0 - i as f32 * 8.0;
        // A person 1.7 m tall at `d` metres is roughly `h / (2 * d * tan(fov/2))`.
        let hy = sy - (h as f32 * 1.7 / (2.0 * d * 0.577)).clamp(28.0, 340.0);
        lines.push(format!(
            "[0] [con] esp: id={} d={d:.2} x={sx:.1} y={sy:.1} h={hy:.1} w={:.2},{:.2},{:.2} name={name}",
            i + 1,
            world.0,
            world.1,
            world.2
        ));
    }
    for l in lines {
        tel.feed(&l);
    }
}

fn prefix_from_args(demo: &mut bool) -> PathBuf {
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--prefix" => {
                if let Some(v) = it.next() {
                    return PathBuf::from(v);
                }
            }
            "--demo" => *demo = true,
            "-h" | "--help" => {
                println!(
                    "hromatite-overlay [--prefix DIR] [--demo]\n\
                     F8 toggles click-through, F9 toggles the ESP layer,\n\
                     drag the header to move the panel."
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
    Path::new(&home).join(".local/share/Steam/steamapps/compatdata/438100/pfx")
}
