//! In-game UI: nuklear panels drawn into the game's own frame.
//!
//! The resident owns this, not the hot-reloadable feature image, because the
//! D3D11 immediate context belongs to the render thread: [`crate::raw::kiero`]
//! hooks `IDXGISwapChain::Present` on that thread and the frame is built and
//! drawn there. Everything non-`Send` (the nuklear context, the D3D11 objects)
//! therefore lives in a thread-local the per-frame closure reaches into.
//!
//! Off unless `HROMATITE_UI=1`: installing a Present hook is the most visible
//! thing this project can do, so it is opt-in like the other live hooks.

#![cfg_attr(not(windows), allow(dead_code))]

use crate::log;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

static SWAPCHAIN: AtomicUsize = AtomicUsize::new(0);
static HOOKED: AtomicBool = AtomicBool::new(false);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static WIDTH_BITS: AtomicU32 = AtomicU32::new(0);
static HEIGHT_BITS: AtomicU32 = AtomicU32::new(0);
static BACKEND_READY: AtomicBool = AtomicBool::new(false);
static PANEL_OPEN: AtomicBool = AtomicBool::new(true);
static STARTED: AtomicBool = AtomicBool::new(false);
static FAILED: Mutex<Option<String>> = Mutex::new(None);

fn publish_status(frames: u64, width: f32, height: f32, backend_ready: bool, panel_open: bool) {
    FRAMES.store(frames, Ordering::Release);
    WIDTH_BITS.store(width.to_bits(), Ordering::Release);
    HEIGHT_BITS.store(height.to_bits(), Ordering::Release);
    BACKEND_READY.store(backend_ready, Ordering::Release);
    PANEL_OPEN.store(panel_open, Ordering::Release);
    STARTED.store(true, Ordering::Release);
}

fn publish_failure(note: String) {
    STARTED.store(true, Ordering::Release);
    match FAILED.lock() {
        Ok(mut guard) => *guard = Some(note),
        Err(poisoned) => *poisoned.into_inner() = Some(note),
    }
}

/// `HROMATITE_UI=1` — install the Present hook and draw the UI.
pub fn enabled() -> bool {
    std::env::var("HROMATITE_UI").is_ok_and(|value| value.trim() == "1")
}

pub fn hooked() -> bool {
    HOOKED.load(Ordering::Acquire)
}

/// One line per fact; safe to call from the console at any time (including from
/// the `PlayerLoop`/`LateUpdate` game thread while `Present` runs on Unity's
/// render thread).
pub fn status() -> Vec<String> {
    let mut out = vec![format!(
        "ui: {} (HROMATITE_UI=1 to enable), hook {}",
        if enabled() { "enabled" } else { "disabled" },
        if hooked() { "installed" } else { "not installed" },
    )];
    out.push(format!("ui: swapchain {:#x}", SWAPCHAIN.load(Ordering::Acquire)));
    if !STARTED.load(Ordering::Acquire) {
        out.push("ui: no frame rendered yet".into());
        return out;
    }
    let frames = FRAMES.load(Ordering::Acquire);
    let width = f32::from_bits(WIDTH_BITS.load(Ordering::Acquire));
    let height = f32::from_bits(HEIGHT_BITS.load(Ordering::Acquire));
    let ready = BACKEND_READY.load(Ordering::Acquire);
    let open = PANEL_OPEN.load(Ordering::Acquire);
    out.push(format!(
        "ui: frames {frames} viewport {width:.0}x{height:.0} backend {} panel {}",
        if ready { "ready" } else { "unavailable" },
        if open { "open" } else { "closed" },
    ));
    let failed = match FAILED.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if let Some(note) = failed {
        out.push(format!("ui: stopped — {note}"));
    }
    out
}

/// Install the Present hook and the per-frame callback. Returns `true` when the
/// hook is in place (the backend itself is built lazily on the first frame).
#[cfg(windows)]
pub fn install() -> bool {
    imp::install()
}

#[cfg(not(windows))]
pub fn install() -> bool {
    log::log_tag("ui", "install: the in-game UI is windows-only");
    false
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::ffi::{
        GetAsyncKeyState, GetClientRect, GetCursorPos, GetForegroundWindow, ScreenToClient,
    };
    use crate::raw::d3d11::{swapchain_desc, Backend};
    use core::cell::RefCell;
    use hromatite_nuklear::{Input, Keys, Nk};

    const VK_LBUTTON: i32 = 0x01;
    const VK_BACK: i32 = 0x08;
    const VK_TAB: i32 = 0x09;
    const VK_RETURN: i32 = 0x0D;
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_LEFT: i32 = 0x25;
    const VK_UP: i32 = 0x26;
    const VK_RIGHT: i32 = 0x27;
    const VK_DOWN: i32 = 0x28;
    const VK_DELETE: i32 = 0x2E;

    /// Everything the frame path needs, kept on the render thread.
    struct Local {
        nk: Nk,
        backend: Option<Backend>,
        window: isize,
        width: f32,
        height: f32,
        frames: u64,
        last_left: bool,
        failed: Option<String>,
    }

    thread_local! {
        static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
    }

    pub fn install() -> bool {
        let Some(swapchain) = crate::raw::d3d11::find_swapchain() else {
            log::log_tag("ui", "install: no IDXGISwapChain found; UI stays off");
            return false;
        };
        SWAPCHAIN.store(swapchain, Ordering::Release);
        if !crate::raw::kiero::hook_present_at(swapchain) {
            log::log_tag("ui", "install: could not hook Present; UI stays off");
            return false;
        }
        HOOKED.store(true, Ordering::Release);
        // Reachable from the Present detour; the closure itself is `Send`, the
        // non-`Send` state lives in the thread-local above.
        crate::raw::kiero::render_each(present_frame);
        log::log_tag(
            "ui",
            &format!("install: Present hooked at swapchain {swapchain:#x}; nuklear UI active"),
        );
        true
    }

    /// The per-frame entry point. Runs on the render thread, once per `Present`.
    fn present_frame() {
        LOCAL.with(|cell| {
            let mut cell = cell.borrow_mut();
            if cell.is_none() {
                let Some(nk) = Nk::new() else {
                    let note = "first frame: nuklear context could not be created".to_string();
                    publish_failure(note.clone());
                    log::log_tag("ui", &note);
                    return;
                };
                *cell = Some(Local {
                    nk,
                    backend: None,
                    window: 0,
                    width: 1280.0,
                    height: 720.0,
                    frames: 0,
                    last_left: false,
                    failed: None,
                });
            }
            let Some(local) = cell.as_mut() else {
                return;
            };
            if local.failed.is_some() {
                return;
            }

            let swapchain = SWAPCHAIN.load(Ordering::Acquire);
            let (width, height, window) =
                target_window_and_size(swapchain, local.window, local.width, local.height);
            local.window = window;
            local.width = width;
            local.height = height;

            // The backend is built on the render thread: the immediate context
            // is not thread-safe and every call below must come from here.
            if local.backend.is_none() {
                match Backend::new(swapchain) {
                    Ok(mut backend) => {
                        if let Some(atlas) = local.nk.font_texture() {
                            let (atlas_width, atlas_height) = (atlas.width(), atlas.height());
                            match backend.upload_font(atlas.data(), atlas_width, atlas_height) {
                                Ok(()) => {}
                                Err(error) => {
                                    local.failed = Some(error.clone());
                                    publish_status(
                                        local.frames,
                                        local.width,
                                        local.height,
                                        false,
                                        local.nk.ui().open,
                                    );
                                    publish_failure(error.clone());
                                    log::log_tag("ui", &format!("font atlas: {error}"));
                                    return;
                                }
                            }
                        }
                        backend.set_size(width, height);
                        local.backend = Some(backend);
                    }
                    Err(error) => {
                        local.failed = Some(error.clone());
                        publish_status(
                            local.frames,
                            local.width,
                            local.height,
                            false,
                            local.nk.ui().open,
                        );
                        publish_failure(error.clone());
                        log::log_tag("ui", &format!("backend: {error}"));
                        return;
                    }
                }
            }

            let input = read_input(window, width, height, local.last_left);
            local.last_left = input.left_down;
            let frame = local.nk.frame(&input);
            let Some(backend) = local.backend.as_mut() else {
                return;
            };
            backend.set_size(width, height);
            let Some(state) = backend.save() else {
                let note = "could not read the game's D3D11 state".to_string();
                local.failed = Some(note.clone());
                publish_status(
                    local.frames,
                    local.width,
                    local.height,
                    true,
                    local.nk.ui().open,
                );
                publish_failure(note);
                log::log_tag("ui", "save: the game's D3D11 state could not be captured");
                return;
            };
            backend.draw(&frame, &state);
            backend.restore(&state);
            local.frames = local.frames.saturating_add(1);
            publish_status(
                local.frames,
                local.width,
                local.height,
                true,
                local.nk.ui().open,
            );
        });
    }

    /// Resolve the swapchain's own `OutputWindow` and dimensions so Alt-Tabbing
    /// to another window (e.g. the terminal console) never collapses the
    /// viewport to `1x1`.
    fn target_window_and_size(
        swapchain: usize,
        prev_window: isize,
        prev_width: f32,
        prev_height: f32,
    ) -> (f32, f32, isize) {
        let mut window = prev_window;
        let mut desc_w = 0u32;
        let mut desc_h = 0u32;
        if let Some((w, h, hwnd)) = swapchain_desc(swapchain) {
            desc_w = w;
            desc_h = h;
            if hwnd != 0 {
                window = hwnd;
            }
        }
        if window == 0 {
            // SAFETY: user32 reads the foreground window handle.
            window = unsafe { GetForegroundWindow() };
        }
        if desc_w > 0 && desc_h > 0 {
            return (desc_w as f32, desc_h as f32, window);
        }
        if window != 0 {
            let mut rect = [0i32; 4];
            // SAFETY: `rect` is a valid 4-i32 stack buffer (`RECT`).
            if unsafe { GetClientRect(window, rect.as_mut_ptr()) } != 0 {
                let w = rect[2] - rect[0];
                let h = rect[3] - rect[1];
                if w > 0 && h > 0 {
                    return (w as f32, h as f32, window);
                }
            }
        }
        (prev_width.max(1.0), prev_height.max(1.0), window)
    }

    fn key_down(key: i32) -> bool {
        // SAFETY: `GetAsyncKeyState` only reads the keyboard state table.
        unsafe { GetAsyncKeyState(key) < 0 }
    }

    fn read_input(target_window: isize, width: f32, height: f32, last_left: bool) -> Input {
        let mut input = Input {
            mouse_x: 0.0,
            mouse_y: 0.0,
            left_down: false,
            left_pressed: false,
            scroll: 0.0,
            keys: Keys::default(),
        };
        // SAFETY: user32 reads the foreground window, writes a `POINT` and
        // translates it; `point` is a valid 2-i32 stack array.
        unsafe {
            let fg = GetForegroundWindow();
            let window = if target_window != 0 {
                target_window
            } else {
                fg
            };
            let mut point = [0i32; 2];
            if window != 0
                && GetCursorPos(point.as_mut_ptr()) != 0
                && ScreenToClient(window, point.as_mut_ptr()) != 0
            {
                input.mouse_x = point[0] as f32;
                input.mouse_y = point[1] as f32;
            }
        }
        input.left_down = key_down(VK_LBUTTON);
        input.left_pressed = input.left_down && !last_left;
        input.keys = Keys {
            shift: key_down(VK_SHIFT),
            ctrl: key_down(VK_CONTROL),
            alt: key_down(VK_MENU),
            enter: key_down(VK_RETURN),
            tab: key_down(VK_TAB),
            backspace: key_down(VK_BACK),
            delete: key_down(VK_DELETE),
            up: key_down(VK_UP),
            down: key_down(VK_DOWN),
            left: key_down(VK_LEFT),
            right: key_down(VK_RIGHT),
        };
        let _ = (width, height);
        input
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_published_telemetry_across_threads() {
        publish_status(42, 1920.0, 1080.0, true, true);
        let lines = std::thread::spawn(status)
            .join()
            .expect("status thread joins");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("frames 42 viewport 1920x1080 backend ready panel open")),
            "unexpected status lines: {lines:?}"
        );
    }
}
