//! `hromatite-overlay` — host-side, read-only telemetry + control panel.
//!
//! The game is *not* touched: the payload answers `pos` / `who` into its own log
//! file (that channel already exists and adds no sockets, pipes or threads to
//! the game), and this program draws everything in a separate X11 window:
//! telemetry, toggles, sliders, dragging, "attach to the game window".
//!
//! Raw Xlib FFI on purpose: the workspace has no external crates.
//!
//! ```text
//! tools/hromatite-overlay.sh
//! ```
//!
//! Controls:
//!   * F8               — toggle click-through / interactive;
//!   * drag the header  — move the panel (remembered in the config file);
//!   * `attach` toggle  — the window follows the VRChat window;
//!   * toggles/sliders  — what to show, box distance, opacity.

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
struct XRectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

const CW_BACK_PIXEL: c_ulong = 1 << 1;
const CW_EVENT_MASK: c_ulong = 1 << 11;
const CW_OVERRIDE_REDIRECT: c_ulong = 1 << 9;

const EXPOSURE_MASK: c_long = 1 << 12;
const KEY_PRESS_MASK: c_long = 1 << 0;
const BUTTON_PRESS_MASK: c_long = 1 << 2;
const BUTTON_RELEASE_MASK: c_long = 1 << 3;
const POINTER_MOTION_MASK: c_long = 1 << 6;
const STRUCTURE_MASK: c_long = 1 << 17;

const INPUT_OUTPUT: c_uint = 1;
const PROP_MODE_REPLACE: c_int = 0;
const SHAPE_INPUT: c_int = 2;
const SHAPE_SET: c_int = 0;
const UNSORTED: c_int = 0;
const XA_WINDOW: c_ulong = 33;
const XA_CARDINAL: c_ulong = 6;

const KEY_F8: c_ulong = 0xffc5;

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XDefaultScreen(d: *mut Display) -> c_int;
    fn XRootWindow(d: *mut Display, screen: c_int) -> Window;
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
    fn XDefaultDepth(d: *mut Display, screen: c_int) -> c_int;
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
    fn XSetForeground(d: *mut Display, gc: GContext, pixel: c_ulong) -> c_int;
    fn XFillRectangle(
        d: *mut Display,
        w: Window,
        gc: GContext,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XDrawRectangle(
        d: *mut Display,
        w: Window,
        gc: GContext,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XDrawString(
        d: *mut Display,
        w: Window,
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
    fn XMoveWindow(d: *mut Display, w: Window, x: c_int, y: c_int) -> c_int;
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
    fn XRaiseWindow(d: *mut Display, w: Window) -> c_int;
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

// ---- config ---------------------------------------------------------------

struct Config {
    show_list: bool,
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
            show_list: true,
            show_cam: true,
            attach: false,
            box_dist: 30.0,
            opacity: 0.85,
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
                match k.trim() {
                    "show_list" => c.show_list = v.trim() == "1",
                    "show_cam" => c.show_cam = v.trim() == "1",
                    "attach" => c.attach = v.trim() == "1",
                    "box_dist" => c.box_dist = v.trim().parse().unwrap_or(c.box_dist),
                    "opacity" => c.opacity = v.trim().parse().unwrap_or(c.opacity),
                    "x" => c.x = v.trim().parse().unwrap_or(c.x),
                    "y" => c.y = v.trim().parse().unwrap_or(c.y),
                    _ => {}
                }
            }
        }
        c
    }

    fn save(&self) {
        let text = format!(
            "show_list={}\nshow_cam={}\nattach={}\nbox_dist={}\nopacity={}\nx={}\ny={}\n",
            self.show_list as u8,
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

#[derive(Default)]
struct Telemetry {
    cam: Option<(f32, f32, f32)>,
    players: Vec<(u32, Option<(f32, f32, f32)>)>,
    cam_at: Option<Instant>,
}

fn parse_vec3(s: &str) -> Option<(f32, f32, f32)> {
    let inner = s.trim().trim_start_matches('(').trim_end_matches(')');
    let mut it = inner.split(',').map(|p| p.trim().parse::<f32>().ok());
    match (it.next()?, it.next()?, it.next()?) {
        (Some(a), Some(b), Some(c)) => Some((a, b, c)),
        _ => None,
    }
}

impl Telemetry {
    fn feed(&mut self, line: &str) {
        if line.contains("pos: get_position = ") {
            if let Some(rest) = line.split("pos: get_position = ").nth(1)
                && let Some(v) = rest.split("  [raw").next().and_then(parse_vec3)
            {
                self.cam = Some(v);
                self.cam_at = Some(Instant::now());
            }
        } else if line.contains("player #") && line.contains("pos=(") {
            let id = line
                .split("id=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|r| r.parse::<u32>().ok())
                .unwrap_or(0);
            let pos = line
                .split("pos=(")
                .nth(1)
                .and_then(|r| parse_vec3(&format!("({}", r.split(')').next().unwrap_or(""))));
            self.players.push((id, pos));
        }
    }

    fn begin_who(&mut self) {
        self.players.clear();
    }
}

// ---- ui -------------------------------------------------------------------

struct Gfx {
    display: *mut Display,
    win: Window,
    gc: GContext,
    /// Back buffer: everything is drawn here and blitted in one `XCopyArea`,
    /// which is what keeps the panel free of flicker at 60 fps.
    pixmap: XID,
    pgc: GContext,
    root: Window,
}

struct Ui {
    cfg: Config,
    interactive: bool,
    dragging: bool,
    drag_dx: i32,
    drag_dy: i32,
    slider: Option<u8>,
    hover: Option<u8>,
}

// Widget ids.
const W_TOGGLE_LIST: u8 = 1;
const W_TOGGLE_CAM: u8 = 2;
const W_SLIDER_DIST: u8 = 3;
const W_SLIDER_OPA: u8 = 4;
const W_TOGGLE_ATTACH: u8 = 5;
const W_TOGGLE_PASS: u8 = 6;

const PANEL_W: u32 = 470;
const PANEL_H: u32 = 250;
const HEADER_H: i32 = 26;
const ROW_H: i32 = 26;

fn widget_rects() -> [(u8, i32, i32, i32, i32); 6] {
    let x = 12;
    let w = PANEL_W as i32 - 24;
    [
        (W_TOGGLE_LIST, x, 34, w, 18),
        (W_TOGGLE_CAM, x, 34 + ROW_H, w, 18),
        (W_SLIDER_DIST, x, 34 + ROW_H * 2, w, 18),
        (W_SLIDER_OPA, x, 34 + ROW_H * 3, w, 18),
        (W_TOGGLE_ATTACH, x, 34 + ROW_H * 4, w, 18),
        (W_TOGGLE_PASS, x, 34 + ROW_H * 5, w, 18),
    ]
}

fn hit(mx: i32, my: i32) -> Option<u8> {
    for (id, x, y, w, h) in widget_rects() {
        if mx >= x && mx < x + w && my >= y && my < y + h {
            return Some(id);
        }
    }
    None
}

fn main() {
    let prefix = prefix_from_args();
    let user_dir = prefix.join("drive_c/users/steamuser");
    let log_path = user_dir.join("hromatite.log");
    let cmd_path = user_dir.join("hromatite-cmd.txt");
    println!("overlay: log {}", log_path.display());

    let mut cfg = Config::load();

    // SAFETY: XOpenDisplay(NULL) follows $DISPLAY.
    let display = unsafe { XOpenDisplay(std::ptr::null()) };
    if display.is_null() {
        eprintln!("overlay: cannot open X display (set DISPLAY/XAUTHORITY)");
        std::process::exit(2);
    }
    // SAFETY: live Display handle.
    let root = unsafe { XRootWindow(display, XDefaultScreen(display)) };

    let mut attrs = XSetWindowAttributes {
        background_pixmap: 0,
        background_pixel: 0x0010_1014,
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
        event_mask: EXPOSURE_MASK
            | KEY_PRESS_MASK
            | BUTTON_PRESS_MASK
            | BUTTON_RELEASE_MASK
            | POINTER_MOTION_MASK
            | STRUCTURE_MASK,
        do_not_propagate_mask: 0,
        override_redirect: 1,
        _pad3: 0,
        colormap: 0,
        cursor: 0,
    };

    // SAFETY: attributes initialised above.
    let win = unsafe {
        XCreateWindow(
            display,
            root,
            cfg.x,
            cfg.y,
            PANEL_W,
            PANEL_H,
            0,
            0,
            INPUT_OUTPUT,
            std::ptr::null_mut(),
            CW_BACK_PIXEL | CW_EVENT_MASK | CW_OVERRIDE_REDIRECT,
            &mut attrs,
        )
    };
    if win == 0 {
        eprintln!("overlay: XCreateWindow failed");
        std::process::exit(2);
    }

    // SAFETY: window and handles are live.
    unsafe {
        let title = CString::new("hromatite overlay").unwrap();
        XStoreName(display, win, title.as_ptr());
        XMapWindow(display, win);

        let state = XInternAtom(display, c"_NET_WM_STATE".as_ptr(), 0);
        let above = XInternAtom(display, c"_NET_WM_STATE_ABOVE".as_ptr(), 0);
        let atom_t = XInternAtom(display, c"ATOM".as_ptr(), 0);
        let data = above.to_ne_bytes();
        XChangeProperty(display, win, state, atom_t, 32, PROP_MODE_REPLACE, data.as_ptr(), 1);

        // F8 works even while the panel is click-through (grab on the root).
        let keycode = XKeysymToKeycode(display, KEY_F8) as c_int;
        for extra in [0u32, 2, 1 << 1] {
            XGrabKey(display, keycode, extra, root, 1, 1, 1);
        }
        XRaiseWindow(display, win);
        XFlush(display);
    }

    // SAFETY: a GC is created once for the window's lifetime.
    let gc = unsafe { XCreateGC(display, win, 0, std::ptr::null_mut()) };
    let font = CString::new("fixed").unwrap();
    // SAFETY: font lookup may fail (NULL) — handled below.
    let font_ptr = unsafe { XLoadQueryFont(display, font.as_ptr()) };
    let (glyph_w, glyph_h) = (6, 13);
    if !font_ptr.is_null() {
        // SAFETY: XFontStruct{ext_data, fid, ...} — fid is the second word.
        let fid = unsafe { *(font_ptr as *const c_ulong).add(1) };
        // SAFETY: live GC and fid from the loaded font.
        unsafe { XSetFont(display, gc, fid) };
    }

    // Backing pixmap: draw off-screen, blit once per frame.
    // SAFETY: default depth/screen of the live display; window id is valid.
    let depth = unsafe { XDefaultDepth(display, XDefaultScreen(display)) } as c_uint;
    let pixmap = unsafe { XCreatePixmap(display, win, PANEL_W, PANEL_H, depth) };
    let pgc = unsafe { XCreateGC(display, pixmap, 0, std::ptr::null_mut()) };
    if !font_ptr.is_null() {
        let fid = unsafe { *(font_ptr as *const c_ulong).add(1) };
        unsafe { XSetFont(display, pgc, fid) };
    }
    // SAFETY: no GraphicsExpose storm on every blit.
    unsafe { XSetGraphicsExposures(display, gc, 0) };

    let gfx = Gfx {
        display,
        win,
        gc,
        pixmap,
        pgc,
        root,
    };
    let mut ui = Ui {
        cfg,
        interactive: false,
        dragging: false,
        drag_dx: 0,
        drag_dy: 0,
        slider: None,
        hover: None,
    };
    apply_shape(&gfx, false);
    apply_opacity(&gfx, ui.cfg.opacity);

    let mut tel = Telemetry::default();
    let mut log = File::open(&log_path).ok();
    let mut carry: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 1 << 16];
    let mut last_pos = Instant::now() - Duration::from_secs(5);
    let mut last_who = Instant::now() - Duration::from_secs(5);
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut game_win: Window = 0;
    let started = Instant::now();

    loop {
        let now = Instant::now();
        let mut wants = String::new();
        // The payload polls the command file every ~250 ms; asking a bit more
        // often keeps the numbers as fresh as the channel allows.
        if ui.cfg.show_cam && now.duration_since(last_pos) >= Duration::from_millis(200) {
            wants.push_str("pos\n");
            last_pos = now;
        }
        if ui.cfg.show_list && now.duration_since(last_who) >= Duration::from_millis(2500) {
            wants.push_str("who\n");
            last_who = now;
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
                    Ok(n) => {
                        carry.extend_from_slice(&scratch[..n]);
                        dirty = true;
                    }
                    Err(_) => break,
                }
            }
            while let Some(pos) = carry.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = carry.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                if text.contains("who: VRCPlayerApi") {
                    tel.begin_who();
                }
                if text.contains("[con] pos:")
                    || text.contains("[con] who:")
                    || text.contains("player #")
                {
                    tel.feed(&text);
                }
            }
            if carry.len() > 1 << 20 {
                carry.clear();
            }
        }

        // Attach: follow the game's toplevel window.
        if ui.cfg.attach {
            if game_win == 0 {
                game_win = find_window(&gfx, c"VRChat");
            }
            if game_win != 0
                && let Some((gx, gy)) = window_origin(&gfx, game_win)
                && (gx != ui.cfg.x || gy != ui.cfg.y)
            {
                // SAFETY: both windows live; move ours onto the game's corner.
                unsafe {
                    XMoveWindow(display, win, gx, gy);
                }
                ui.cfg.x = gx;
                ui.cfg.y = gy;
                dirty = true;
            }
        } else {
            game_win = 0;
        }

        // Redraw when something changed (telemetry, hover, drag) and at ~4 Hz
        // otherwise, so the `age` counter stays alive without burning CPU on
        // identical frames — each frame is a full 470x250 blit through kwin.
        if dirty || now.duration_since(last_draw) >= Duration::from_millis(250) {
            last_draw = now;
            dirty = false;
            draw(&gfx, &ui, &tel, glyph_w, glyph_h, started);
        }

        let mut shape_dirty = false;
        // SAFETY: event pumping on a live display.
        unsafe {
            while XPending(display) > 0 {
                let mut ev = [0u8; 192];
                XNextEvent(display, &mut ev);
                let kind = i32_at(&ev, 0);
                match kind {
                    2 => {
                        ui.interactive = !ui.interactive;
                        shape_dirty = true;
                        dirty = true;
                    }
                    4 => {
                        let (mx, my) = (i32_at(&ev, 64), i32_at(&ev, 68));
                        if my < HEADER_H {
                            ui.dragging = true;
                            ui.drag_dx = mx;
                            ui.drag_dy = my;
                        } else if let Some(id) = hit(mx, my) {
                            match id {
                                W_TOGGLE_LIST => ui.cfg.show_list = !ui.cfg.show_list,
                                W_TOGGLE_CAM => ui.cfg.show_cam = !ui.cfg.show_cam,
                                W_TOGGLE_ATTACH => {
                                    ui.cfg.attach = !ui.cfg.attach;
                                    if !ui.cfg.attach {
                                        game_win = 0;
                                    }
                                }
                                W_TOGGLE_PASS => {
                                    ui.interactive = !ui.interactive;
                                    shape_dirty = true;
                                }
                                W_SLIDER_DIST => {
                                    ui.slider = Some(W_SLIDER_DIST);
                                    set_slider(&mut ui.cfg, id, mx);
                                }
                                W_SLIDER_OPA => {
                                    ui.slider = Some(W_SLIDER_OPA);
                                    set_slider(&mut ui.cfg, id, mx);
                                    apply_opacity(&gfx, ui.cfg.opacity);
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
                            XMoveWindow(display, win, nx, ny);
                            ui.cfg.x = nx;
                            ui.cfg.y = ny;
                            dirty = true;
                        } else if let Some(id) = ui.slider {
                            set_slider(&mut ui.cfg, id, mx);
                            apply_opacity(&gfx, ui.cfg.opacity);
                            dirty = true;
                        } else {
                            ui.hover = hit(mx, my);
                            dirty = true;
                        }
                    }
                    12 | 22 => dirty = true,
                    _ => {}
                }
            }
        }
        if shape_dirty {
            apply_shape(&gfx, ui.interactive);
            // SAFETY: keep the panel above the game after the shape change.
            unsafe {
                XRaiseWindow(display, win);
            }
        }

        sleep(Duration::from_millis(33));
    }
}

fn i32_at(buf: &[u8; 192], off: usize) -> i32 {
    i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn set_slider(cfg: &mut Config, id: u8, mx: i32) {
    let (x, w) = (12, PANEL_W as i32 - 24);
    let t = ((mx - x) as f32 / w as f32).clamp(0.0, 1.0);
    match id {
        W_SLIDER_DIST => cfg.box_dist = 5.0 + t * 195.0,
        W_SLIDER_OPA => cfg.opacity = 0.2 + t * 0.8,
        _ => {}
    }
}

fn apply_shape(g: &Gfx, interactive: bool) {
    // SAFETY: live display/window; the extension is optional.
    unsafe {
        let mut ev = 0;
        let mut err = 0;
        if XShapeQueryExtension(g.display, &mut ev, &mut err) == 0 {
            return;
        }
        if interactive {
            let mut r = XRectangle {
                x: 0,
                y: 0,
                width: PANEL_W as u16,
                height: PANEL_H as u16,
            };
            XShapeCombineRectangles(
                g.display,
                g.win,
                SHAPE_INPUT,
                0,
                0,
                &mut r,
                1,
                SHAPE_SET,
                UNSORTED,
            );
        } else {
            XShapeCombineRectangles(
                g.display,
                g.win,
                SHAPE_INPUT,
                0,
                0,
                std::ptr::null_mut(),
                0,
                SHAPE_SET,
                UNSORTED,
            );
        }
    }
}

fn apply_opacity(g: &Gfx, opacity: f32) {
    // SAFETY: live display/window; EWMH property is advisory.
    unsafe {
        let atom = XInternAtom(g.display, c"_NET_WM_WINDOW_OPACITY".as_ptr(), 0);
        let v: c_ulong = (opacity.clamp(0.05, 1.0) as f64 * u32::MAX as f64) as c_ulong;
        let d = v.to_ne_bytes();
        XChangeProperty(
            g.display,
            g.win,
            atom,
            XA_CARDINAL,
            32,
            PROP_MODE_REPLACE,
            d.as_ptr(),
            1,
        );
    }
}

/// Find a toplevel window by name through `_NET_CLIENT_LIST`.
fn find_window(g: &Gfx, want: &CStr) -> Window {
    // SAFETY: property read on the root window with a live display.
    unsafe {
        let atom = XInternAtom(g.display, c"_NET_CLIENT_LIST".as_ptr(), 0);
        let mut kind: Atom = 0;
        let mut fmt: c_int = 0;
        let mut n: c_ulong = 0;
        let mut after: c_ulong = 0;
        let mut prop: *mut u8 = std::ptr::null_mut();
        if XGetWindowProperty(
            g.display,
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
            if XFetchName(g.display, w, &mut name) != 0 && !name.is_null() {
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

fn window_origin(g: &Gfx, w: Window) -> Option<(i32, i32)> {
    let mut root: Window = 0;
    let mut x = 0;
    let mut y = 0;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut border = 0u32;
    let mut depth = 0u32;
    // SAFETY: queries on live window ids.
    unsafe {
        if XGetGeometry(
            g.display,
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
        XTranslateCoordinates(g.display, w, g.root, 0, 0, &mut ax, &mut ay, &mut child);
        Some((ax, ay))
    }
}

fn draw(g: &Gfx, ui: &Ui, tel: &Telemetry, gh: i32, _gw: i32, started: Instant) {
    let cfg = &ui.cfg;
    let _ = gh;
    // SAFETY: all handles live; drawing only.
    unsafe {
        let d = g.display;
        let w = g.pixmap;
        let gc = g.pgc;
        XSetForeground(d, gc, 0x0010_1014);
        XFillRectangle(d, w, gc, 0, 0, PANEL_W, PANEL_H);

        XSetForeground(d, gc, 0x0018_2030);
        XFillRectangle(d, w, gc, 0, 0, PANEL_W, HEADER_H as u32);
        XSetForeground(d, gc, 0x0060_d0ff);
        text(g, 10, 17, "HROMATITE · x11 overlay");
        XSetForeground(d, gc, 0x0080_8080);
        text(g, 250, 17, "F8: click-through/interactive");

        for (id, x, y, ww, _hh) in widget_rects() {
            let hovered = ui.hover == Some(id);
            match id {
                W_SLIDER_DIST | W_SLIDER_OPA => {
                    let (label, t) = if id == W_SLIDER_DIST {
                        (
                            format!("box distance  {:5.0} m", cfg.box_dist),
                            ((cfg.box_dist - 5.0) / 195.0).clamp(0.0, 1.0),
                        )
                    } else {
                        (
                            format!("opacity       {:5.2}", cfg.opacity),
                            ((cfg.opacity - 0.2) / 0.8).clamp(0.0, 1.0),
                        )
                    };
                    XSetForeground(d, gc, if hovered { 0x00e0_e0e0 } else { 0x00c0_c0c0 });
                    text(g, x, y + 12, &label);
                    XSetForeground(d, gc, 0x0030_3848);
                    XFillRectangle(d, w, gc, x, y + 14, ww as u32, 3);
                    let kx = x + (t * ww as f32) as i32 - 3;
                    XSetForeground(d, gc, 0x0060_d0ff);
                    XFillRectangle(d, w, gc, kx.max(x), y + 10, 6, 11);
                }
                _ => {
                    let on = match id {
                        W_TOGGLE_LIST => cfg.show_list,
                        W_TOGGLE_CAM => cfg.show_cam,
                        W_TOGGLE_ATTACH => cfg.attach,
                        W_TOGGLE_PASS => ui.interactive,
                        _ => false,
                    };
                    let label = match id {
                        W_TOGGLE_LIST => "player list",
                        W_TOGGLE_CAM => "camera readout",
                        W_TOGGLE_ATTACH => "attach to the VRChat window",
                        W_TOGGLE_PASS => "interactive (clicks reach the panel)",
                        _ => "",
                    };
                    XSetForeground(d, gc, 0x0030_3848);
                    XFillRectangle(d, w, gc, x, y + 2, 14, 14);
                    XSetForeground(d, gc, if on { 0x0060_d0ff } else { 0x0050_5868 });
                    XDrawRectangle(d, w, gc, x, y + 2, 13, 13);
                    if on {
                        XFillRectangle(d, w, gc, x + 3, y + 5, 8, 8);
                    }
                    XSetForeground(d, gc, if hovered { 0x00f0_f0f0 } else { 0x00c0_c0c0 });
                    text(g, x + 22, y + 14, label);
                }
            }
        }

        let mut yy = 34 + ROW_H * 6 + 16;
        XSetForeground(d, gc, 0x00d0_d0d0);
        if cfg.show_cam {
            let line = match tel.cam {
                Some((x, y, z)) => format!("CAM  {x:8.2} {y:8.2} {z:8.2}"),
                None => "CAM  (waiting for pos)".to_string(),
            };
            text(g, 12, yy, &line);
            yy += 14;
        }
        if cfg.show_list {
            if tel.players.is_empty() {
                text(g, 12, yy, "players: (waiting for who)");
            } else {
                for (i, (id, pos)) in tel.players.iter().enumerate().take(3) {
                    let line = match pos {
                        Some((x, y, z)) => format!("P{:<3} {x:8.2} {y:8.2} {z:8.2}", id),
                        None => format!("P{:<3} (no position)", id),
                    };
                    text(g, 12, yy + i as i32 * 14, &line);
                }
            }
        }

        let age = tel.cam_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
        XSetForeground(d, gc, 0x0080_8080);
        text(
            g,
            12,
            PANEL_H as i32 - 8,
            &format!(
                "age {age:4.1}s · up {:5.0}s · {} player(s) · drag the header to move",
                started.elapsed().as_secs_f32(),
                tel.players.len()
            ),
        );

        // One blit: the window never shows a half-drawn frame.
        XCopyArea(d, g.pixmap, g.win, g.gc, 0, 0, PANEL_W, PANEL_H, 0, 0);
    }
}

fn text(g: &Gfx, x: i32, y: i32, s: &str) {
    // SAFETY: caller guarantees live handles; Xlib copies the string.
    unsafe {
        XDrawString(
            g.display,
            g.pixmap,
            g.pgc,
            x,
            y,
            s.as_ptr() as *const c_char,
            s.len() as c_int,
        );
    }
}

fn prefix_from_args() -> PathBuf {
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--prefix" => {
                if let Some(v) = it.next() {
                    return PathBuf::from(v);
                }
            }
            "-h" | "--help" => {
                println!(
                    "hromatite-overlay [--prefix DIR]\n\
                     F8 toggles click-through; drag the header to move the panel."
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
    Path::new(&home).join(".local/share/Steam/steamapps/compatdata/438100/pfx")
}
