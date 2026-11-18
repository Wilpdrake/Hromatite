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

use crate::log;
use hromatite_nuklear::{Input, Keys, Nk};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static SWAPCHAIN: AtomicUsize = AtomicUsize::new(0);
static HOOKED: AtomicBool = AtomicBool::new(false);

/// `HROMATITE_UI=1` — install the Present hook and draw the UI.
pub fn enabled() -> bool {
    std::env::var("HROMATITE_UI").is_ok_and(|value| value.trim() == "1")
}

pub fn hooked() -> bool {
    HOOKED.load(Ordering::Acquire)
}

/// One line per fact; safe to call from the console at any time.
pub fn status() -> Vec<String> {
    let mut out = vec![format!(
        "ui: {} (HROMATITE_UI=1 to enable), hook {}",
        if enabled() { "enabled" } else { "disabled" },
        if hooked() { "installed" } else { "not installed" },
    )];
    out.push(format!("ui: swapchain {:#x}", SWAPCHAIN.load(Ordering::Acquire)));
    #[cfg(windows)]
    imp::status_into(&mut out);
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
    use crate::raw::d3d11::Backend;
    use core::cell::RefCell;

    // User32: the panel is driven by the window's own mouse and keyboard state,
    // so no Unity call is needed and the UI works before il2cpp is up.
    unsafe extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetClientRect(window: isize, rect: *mut i32) -> i32;
        fn GetCursorPos(point: *mut i32) -> i32;
        fn ScreenToClient(window: isize, point: *mut i32) -> i32;
        fn GetAsyncKeyState(key: i32) -> i16;
    }

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
        width: f32,
        height: f32,
        frames: u64,
        last_left: bool,
        failed: Option<String>,
    }

    thread_local! {
        static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
    }

    pub fn status_into(out: &mut Vec<String>) {
        LOCAL.with(|cell| {
            let cell = cell.borrow();
            let Some(local) = cell.as_ref() else {
                out.push("ui: no frame rendered yet".into());
                return;
            };
            out.push(format!(
                "ui: frames {} viewport {:.0}x{:.0} backend {} panel {}",
                local.frames,
                local.width,
                local.height,
                if local.backend.is_some() { "ready" } else { "unavailable" },
                if local.nk.ui().open { "open" } else { "closed" },
            ));
            if let Some(note) = &local.failed {
                out.push(format!("ui: stopped — {note}"));
            }
        });
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
                    log::log_tag("ui", "first frame: nuklear context could not be created");
                    return;
                };
                *cell = Some(Local {
                    nk,
                    backend: None,
                    width: 1.0,
                    height: 1.0,
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

            let (width, height) = window_size();
            local.width = width;
            local.height = height;

            // The backend is built on the render thread: the immediate context
            // is not thread-safe and every call below must come from here.
            if local.backend.is_none() {
                let swapchain = SWAPCHAIN.load(Ordering::Acquire);
                match Backend::new(swapchain) {
                    Ok(mut backend) => {
                        if let Some(atlas) = local.nk.font_texture() {
                            let (atlas_width, atlas_height) = (atlas.width(), atlas.height());
                            match backend.upload_font(atlas.data(), atlas_width, atlas_height) {
                                Ok(()) => {}
                                Err(error) => {
                                    local.failed = Some(error.clone());
                                    log::log_tag("ui", &format!("font atlas: {error}"));
                                }
                            }
                        }
                        backend.set_size(width, height);
                        local.backend = Some(backend);
                    }
                    Err(error) => {
                        local.failed = Some(error.clone());
                        log::log_tag("ui", &format!("backend: {error}"));
                        return;
                    }
                }
            }

            let input = read_input(width, height, local.last_left);
            local.last_left = input.left_down;
            let frame = local.nk.frame(&input);
            let Some(backend) = local.backend.as_mut() else {
                return;
            };
            backend.set_size(width, height);
            let Some(state) = backend.save() else {
                local.failed = Some("could not read the game's D3D11 state".into());
                log::log_tag("ui", "save: the game's D3D11 state could not be captured");
                return;
            };
            backend.draw(&frame, &state);
            backend.restore(&state);
            local.frames = local.frames.saturating_add(1);
        });
    }

    fn window_size() -> (f32, f32) {
        // SAFETY: user32 reads the foreground window and writes a 16-byte RECT.
        unsafe {
            let window = GetForegroundWindow();
            if window == 0 {
                return (1.0, 1.0);
            }
            let mut rect = [0i32; 4];
            if GetClientRect(window, rect.as_mut_ptr()) == 0 {
                return (1.0, 1.0);
            }
            let width = (rect[2] - rect[0]).max(1) as f32;
            let height = (rect[3] - rect[1]).max(1) as f32;
            (width, height)
        }
    }

    fn key_down(key: i32) -> bool {
        // SAFETY: `GetAsyncKeyState` only reads the keyboard state table.
        unsafe { GetAsyncKeyState(key) < 0 }
    }

    fn read_input(width: f32, height: f32, last_left: bool) -> Input {
        let mut input = Input {
            mouse_x: 0.0,
            mouse_y: 0.0,
            left_down: false,
            left_pressed: false,
            scroll: 0.0,
            keys: Keys::default(),
        };
        // SAFETY: user32 writes a `POINT` and translates it; both buffers are
        // 16-byte aligned stack arrays.
        unsafe {
            let window = GetForegroundWindow();
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
        // nuklear clips to the window; a cursor outside it must not draw a
        // hover state at a stale position.
        let _ = (width, height);
        input
    }
}
