//! Kiero-style presentation hook (Rebzzel/kiero's shape, our mechanics).
//!
//! Kiero is an abstraction, not a library: `init(renderer)` hooks the
//! presentation entry point of the graphics API, `bind(method, &original)`
//! publishes the detour, and the user's own `Present` body runs between the
//! start and the end of the frame. This module keeps that shape and replaces
//! the mechanics with what this project has already proven live:
//!
//! * the swapchain's [`Vmt`] slot 8 (`Present`) is swapped, not inline-patched,
//!   so *every* object sharing the table is covered and no byte of `dxgi.dll`
//!   code changes — only one pointer in a data page;
//! * the original entry is forwarded to, so the game still presents;
//! * the per-frame callback ([`render`]) runs before that forward.
//!
//! # Why the swapchain address is supplied by the caller
//!
//! The textbook way to reach the vtable without creating our own device is
//! `CreateDXGIFactory1` → enumerate adapters → create a device/swapchain. That
//! constructs a *second* device, which is both slow and a detection surface.
//! Real Kiero clones instead read the game's own swapchain pointer. We have no
//! offline-confirmed address for Unity 6000.0.67f1's `GfxDevice`, so this
//! module does the honest thing: [`init`] performs reconnaissance (is `dxgi.dll`
//! loaded, does it export the factory entry points, is a candidate's vtable
//! shaped like a swapchain), and [`hook_present_at`] installs the hook on the
//! address the caller supplies. [`hook_module_export`] is the reserve path that
//! needs no swapchain at all: an inline hook on a named export.
//!
//! Nothing here is wired into the payload's startup yet; the module is a
//! building block plus its tests.

// The live installation path is windows-only (`iat` makes the same choice), so
// on the host these items are reached only by this module's tests.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::log;
use crate::raw::vmt::{Vmt, VmtHook};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// `IDXGISwapChain::Present(UINT SyncInterval, UINT Flags)`.
///
/// Slot 8 of the swapchain vtable: three `IUnknown` entries, four `IDXGIObject`
/// entries and `GetDevice` come first. This is the same index Kiero's D3D11
/// example binds.
pub const PRESENT_SLOT: u32 = 8;
/// `IDXGISwapChain::ResizeBuffers`, slot 13.
pub const RESIZE_BUFFERS_SLOT: u32 = 13;
/// `IDXGISwapChain::ResizeTarget`, slot 14.
pub const RESIZE_TARGET_SLOT: u32 = 14;
/// How many slots a swapchain vtable has; used by the shape probe.
const MIN_SWAPCHAIN_SLOTS: u32 = RESIZE_TARGET_SLOT + 1;

/// Which graphics API [`init`] should bind. Kiero calls this `RenderType`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RenderType {
    /// Nothing bound; [`status`] reports `none`.
    #[default]
    None,
    /// Direct3D 11 through `dxgi.dll`'s swapchain.
    D3D11,
}

/// A named swapchain vtable entry, so callers never hard-code the numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Present,
    ResizeBuffers,
    ResizeTarget,
}

impl Method {
    /// Vtable index of this method.
    pub fn slot(self) -> u32 {
        match self {
            Method::Present => PRESENT_SLOT,
            Method::ResizeBuffers => RESIZE_BUFFERS_SLOT,
            Method::ResizeTarget => RESIZE_TARGET_SLOT,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Method::Present => "Present",
            Method::ResizeBuffers => "ResizeBuffers",
            Method::ResizeTarget => "ResizeTarget",
        }
    }
}

// --------------------------------------------------------------- state

/// Original `Present` entry to forward to: either the vtable slot we replaced or
/// `hook.rs`' trampoline for the inline reserve path.
static PRESENT_ORIG: AtomicUsize = AtomicUsize::new(0);
/// Set while the detour is on the stack, so a nested `Present` forwards instead
/// of running the frame callback a second time.
static INSIDE_FRAME: AtomicBool = AtomicBool::new(false);

/// The per-frame callback registered by [`render`]. `FnOnce` by contract, so it
/// is held in an `Option` and disarms itself after the first frame.
type RenderFn = Box<dyn FnMut() + Send>;
static RENDER: Mutex<Option<RenderFn>> = Mutex::new(None);

fn take_render() -> Option<RenderFn> {
    match RENDER.lock() {
        Ok(mut slot) => slot.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

fn put_render(callback: RenderFn) {
    match RENDER.lock() {
        Ok(mut slot) => {
            // A frame that ran while we were presenting may have registered a
            // replacement; never overwrite it.
            if slot.is_none() {
                *slot = Some(callback);
            }
        }
        Err(poisoned) => {
            let mut slot = poisoned.into_inner();
            if slot.is_none() {
                *slot = Some(callback);
            }
        }
    }
}

fn clear_render() {
    match RENDER.lock() {
        Ok(mut slot) => *slot = None,
        Err(poisoned) => *poisoned.into_inner() = None,
    }
}

/// Clears [`INSIDE_FRAME`] even when the frame callback unwound strangely.
struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        INSIDE_FRAME.store(false, Ordering::Release);
    }
}

/// Call the frame callback with the payload's panic containment on Windows.
///
/// The host build has no vectored handler, so the callback runs plain there —
/// which also keeps the Linux test link free of the Win32 guard entry points
/// (the same reason `iat`'s live layer is windows-only).
#[cfg(windows)]
fn guarded_frame(callback: &mut RenderFn) {
    let _ = log::guard("kiero::render", callback);
}

#[cfg(not(windows))]
fn guarded_frame(callback: &mut RenderFn) {
    callback();
}

/// Undo the reserve-path inline hook. Off Windows the slot is always empty (the
/// installer refuses), so dropping it is the whole job.
#[cfg(windows)]
fn unhook_module(hook: &mut Option<crate::raw::hook::Hook>) {
    if let Some(mut installed) = hook.take() {
        let _ = installed.unhook();
    }
}

#[cfg(not(windows))]
fn unhook_module(hook: &mut Option<crate::raw::hook::Hook>) {
    *hook = None;
}

/// Call the registered frame callback, re-arming it for the next frame.
///
/// The callback is taken out of the lock before it runs: `log::guard` longjmps
/// on a panic, and a `MutexGuard` abandoned across that boundary would poison
/// the slot (the documented hazard in `context.rs`). The closure only borrows
/// the callback, so a longjmp still leaves it usable and it is re-armed.
fn run_render() {
    let Some(mut callback) = take_render() else {
        return;
    };
    guarded_frame(&mut callback);
    put_render(callback);
}

/// Forward to the original `Present`. Split out so the detour body stays a
/// three-line read and the transmute has one documented home.
fn present_forward(original: usize, this: usize, sync: u32, flags: u32) -> i32 {
    // SAFETY: `original` is published only by `hook_present_at` (the vtable
    // entry that was replaced) or `hook_module_export` (`hook.rs`' trampoline
    // for the same three-argument `extern "system"` signature). The detour
    // receives exactly `Present`'s arguments and forwards them unchanged.
    let f: unsafe extern "system" fn(usize, u32, u32) -> i32 =
        unsafe { core::mem::transmute(original) };
    // SAFETY: same contract as the transmute; no argument is invented.
    unsafe { f(this, sync, flags) }
}

/// The `Present` detour. Runs the frame callback once, then forwards to the
/// game's own entry and returns its result unchanged.
///
/// # Safety
/// Installed only as the replacement of an `IDXGISwapChain::Present` vtable
/// slot (or an inline hook on an export with the same ABI). `this`, `sync` and
/// `flags` belong to the caller and are forwarded untouched.
unsafe extern "system" fn present_detour(this: usize, sync: u32, flags: u32) -> i32 {
    let original = PRESENT_ORIG.load(Ordering::Acquire);
    if original == 0 {
        // No hook installed (yet): there is nothing to forward to.
        return 0;
    }
    if INSIDE_FRAME.swap(true, Ordering::AcqRel) {
        // A nested Present on the same thread (the game presenting from two
        // paths): forward without running the frame callback twice.
        return present_forward(original, this, sync, flags);
    }
    let _frame = FrameGuard;
    run_render();
    present_forward(original, this, sync, flags)
}

// --------------------------------------------------------------- Kiero

/// One bound presentation hook.
///
/// The struct owns the installed swaps so [`Kiero::shutdown`] can undo them;
/// the free functions ([`init`], [`status`], [`render`], …) operate on a
/// process-wide instance because the detour is a single global entry point.
pub struct Kiero {
    backend: RenderType,
    /// `(base, size)` of `dxgi.dll`, once [`Kiero::init`] found it.
    dxgi: Option<(usize, usize)>,
    swapchain: usize,
    present: Option<VmtHook<'static>>,
    /// Reserve path: an inline hook on a module export.
    module_hook: Option<crate::raw::hook::Hook>,
}

impl Kiero {
    pub fn new() -> Self {
        Self {
            backend: RenderType::None,
            dxgi: None,
            swapchain: 0,
            present: None,
            module_hook: None,
        }
    }

    /// Reconnaissance: confirm the graphics stack is present and record it.
    ///
    /// No hook is installed here — the swapchain object is created by the game
    /// and we have no confirmed address for it, so [`Kiero::hook_present_at`]
    /// is the call that actually binds. `true` means "this backend is usable".
    pub fn init(&mut self, render_type: RenderType) -> bool {
        #[cfg(not(windows))]
        {
            let _ = render_type;
            log::log_tag(
                "kiero",
                "init: no graphics stack on this host (the live layer is windows-only)",
            );
            self.backend = RenderType::None;
            return false;
        }
        #[cfg(windows)]
        {
            match render_type {
                RenderType::None => {
                    self.backend = RenderType::None;
                    true
                }
                RenderType::D3D11 => {
                    let Some((base, size)) = crate::procs::own_module("dxgi.dll") else {
                        log::log_tag("kiero", "init: dxgi.dll is not loaded");
                        self.backend = RenderType::None;
                        return false;
                    };
                    // The factory entry point proves the module is a real DXGI
                    // image and gives the reserve path a callable target.
                    let factory = crate::raw::sys::export_address("dxgi.dll", "CreateDXGIFactory1")
                        .or_else(|| {
                            crate::raw::sys::export_address("dxgi.dll", "CreateDXGIFactory")
                        });
                    if factory.is_none() {
                        log::log_tag("kiero", "init: dxgi.dll exports no DXGI factory");
                        self.backend = RenderType::None;
                        return false;
                    }
                    self.dxgi = Some((base, size));
                    self.backend = RenderType::D3D11;
                    log::log_tag(
                        "kiero",
                        &format!(
                            "init: dxgi.dll {base:#x}+{size:#x}, backend d3d11; no swapchain yet — hook_present_at <addr>"
                        ),
                    );
                    true
                }
            }
        }
    }

    /// Does `swapchain` look like an `IDXGISwapChain`?
    ///
    /// This is a shape check, not proof: every slot of the first 15 entries
    /// must be a readable, non-null code pointer. It is what keeps a caller's
    /// guess from silently patching an unrelated object's data page.
    pub fn probe(swapchain: usize) -> bool {
        let Some(vmt) = Vmt::new(swapchain) else {
            log::log_tag("kiero", &format!("probe: {swapchain:#x} has no readable vtable"));
            return false;
        };
        for index in 0..MIN_SWAPCHAIN_SLOTS {
            let Some(slot) = vmt.slot(index) else {
                log::log_tag(
                    "kiero",
                    &format!("probe: {swapchain:#x} slot {index} is not readable"),
                );
                return false;
            };
            if slot == 0 || !crate::safe::readable_at(slot) {
                log::log_tag(
                    "kiero",
                    &format!("probe: {swapchain:#x} slot {index} -> {slot:#x} is not code"),
                );
                return false;
            }
        }
        true
    }

    /// Bind [`Method::Present`] on the swapchain object at `swapchain`.
    ///
    /// `swapchain` is the game's own `IDXGISwapChain*`, not a pointer to it: the
    /// first qword of that object is the vtable. Returns `true` when the slot
    /// holds our detour afterwards.
    pub fn hook_present_at(&mut self, swapchain: usize) -> bool {
        #[cfg(not(windows))]
        {
            let _ = swapchain;
            log::log_tag(
                "kiero",
                "hook_present_at: the live layer is windows-only on this host",
            );
            return false;
        }
        #[cfg(windows)]
        {
            if self.present.is_some() {
                log::log_tag(
                    "kiero",
                    &format!("hook_present_at: Present is already hooked on {:#x}", self.swapchain),
                );
                return true;
            }
            if !Self::probe(swapchain) {
                return false;
            }
            let Some(mut vmt) = Vmt::new(swapchain) else {
                return false;
            };
            let detour = present_detour as *const () as usize;
            let Some(hook) = vmt.hook(PRESENT_SLOT, detour) else {
                log::log_tag(
                    "kiero",
                    &format!("hook_present_at: slot {PRESENT_SLOT} of {swapchain:#x} was refused"),
                );
                return false;
            };
            let original = hook.original();
            let table = vmt.table();
            PRESENT_ORIG.store(original, Ordering::Release);
            self.swapchain = swapchain;
            self.present = Some(hook);
            if self.backend == RenderType::None {
                self.backend = RenderType::D3D11;
            }
            log::log_tag(
                "kiero",
                &format!(
                    "present: swapchain {swapchain:#x} vtable {table:#x} slot {PRESENT_SLOT} -> {detour:#x} (original {original:#x})"
                ),
            );
            true
        }
    }

    /// Reserve path: inline-hook a named export instead of swapping a vtable.
    ///
    /// Needs no swapchain address, at the cost of patching code bytes in the
    /// target module (which is why [`Kiero::hook_present_at`] is preferred).
    /// `module` is a loaded module name (`dxgi.dll`), `name` an export.
    pub fn hook_module_export(&mut self, module: &str, name: &str) -> bool {
        #[cfg(not(windows))]
        {
            let _ = (module, name);
            log::log_tag(
                "kiero",
                "hook_module_export: the live layer is windows-only on this host",
            );
            return false;
        }
        #[cfg(windows)]
        {
            if self.module_hook.is_some() {
                return true;
            }
            let Some(target) = crate::raw::sys::export_address(module, name) else {
                log::log_tag("kiero", &format!("hook_module_export: {module}!{name} not found"));
                return false;
            };
            let detour = present_detour as *const () as usize;
            let installed = log::guard("kiero::hook_module_export", || {
                crate::raw::hook::hook_inline(target, detour)
            })
            .flatten();
            let Some(hook) = installed else {
                log::log_tag(
                    "kiero",
                    &format!("hook_module_export: prologue rejected at {target:#x}"),
                );
                return false;
            };
            // An inline hook's trampoline *is* the original entry: it replays
            // the overwritten prologue and jumps back.
            PRESENT_ORIG.store(hook.trampoline, Ordering::Release);
            self.module_hook = Some(hook);
            if self.backend == RenderType::None {
                self.backend = RenderType::D3D11;
            }
            log::log_tag(
                "kiero",
                &format!("present: inline hook on {module}!{name} at {target:#x}"),
            );
            true
        }
    }

    /// Undo every installed hook and clear the frame state.
    pub fn shutdown(&mut self) {
        if let Some(mut hook) = self.present.take() {
            let _ = hook.unhook();
        }
        unhook_module(&mut self.module_hook);
        self.backend = RenderType::None;
        self.dxgi = None;
        self.swapchain = 0;
        PRESENT_ORIG.store(0, Ordering::Release);
        INSIDE_FRAME.store(false, Ordering::Release);
        clear_render();
    }

    /// One of `"none"`, `"d3d11 (present hooked)"`, `"d3d11 (present not
    /// hooked)"`. A `&'static str` keeps the value loggable without formatting.
    pub fn status(&self) -> &'static str {
        match self.backend {
            RenderType::None => "none",
            RenderType::D3D11 => {
                if self.present.is_some() || self.module_hook.is_some() {
                    "d3d11 (present hooked)"
                } else {
                    "d3d11 (present not hooked)"
                }
            }
        }
    }

    pub fn backend(&self) -> RenderType {
        self.backend
    }

    pub fn dxgi(&self) -> Option<(usize, usize)> {
        self.dxgi
    }

    pub fn swapchain(&self) -> usize {
        self.swapchain
    }
}

impl Default for Kiero {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide instance behind the free functions.
static KIERO: Mutex<Option<Kiero>> = Mutex::new(None);

fn with_kiero<R>(f: impl FnOnce(&mut Kiero) -> R) -> R {
    match KIERO.lock() {
        Ok(mut guard) => f(guard.get_or_insert_with(Kiero::new)),
        Err(poisoned) => f(poisoned.into_inner().get_or_insert_with(Kiero::new)),
    }
}

/// [`Kiero::init`] on the process-wide instance.
pub fn init(render_type: RenderType) -> bool {
    with_kiero(|kiero| kiero.init(render_type))
}

/// [`Kiero::status`] of the process-wide instance.
pub fn status() -> &'static str {
    match KIERO.lock() {
        Ok(guard) => guard.as_ref().map_or("none", Kiero::status),
        Err(poisoned) => poisoned.into_inner().as_ref().map_or("none", Kiero::status),
    }
}

/// [`Kiero::probe`] for a candidate swapchain address.
pub fn probe(swapchain: usize) -> bool {
    Kiero::probe(swapchain)
}

/// [`Kiero::hook_present_at`] on the process-wide instance.
pub fn hook_present_at(swapchain: usize) -> bool {
    with_kiero(|kiero| kiero.hook_present_at(swapchain))
}

/// [`Kiero::hook_module_export`] on the process-wide instance.
pub fn hook_module_export(module: &str, name: &str) -> bool {
    with_kiero(|kiero| kiero.hook_module_export(module, name))
}

/// [`Kiero::shutdown`] on the process-wide instance.
pub fn shutdown() {
    with_kiero(|kiero| kiero.shutdown());
}

/// Register the per-frame callback.
///
/// The Present detour runs it once per frame, before forwarding to the game's
/// own `Present` — the window between the start and the end of the frame. The
/// bound is `FnOnce` by contract, so the callback fires on the first frame
/// after registration and then disarms; register again from a frame to keep it
/// alive. With no hook installed nothing ever calls it.
pub fn render<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let mut slot = Some(f);
    let callback: RenderFn = Box::new(move || {
        if let Some(inner) = slot.take() {
            inner();
        }
    });
    match RENDER.lock() {
        Ok(mut guard) => *guard = Some(callback),
        Err(poisoned) => *poisoned.into_inner() = Some(callback),
    }
}

/// Address the detour forwards to, or `0` when nothing is bound.
pub fn original_present() -> usize {
    PRESENT_ORIG.load(Ordering::Acquire)
}

/// True while the detour is on the stack on this thread.
pub fn inside_frame() -> bool {
    INSIDE_FRAME.load(Ordering::Acquire)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi;
    use crate::raw::vmt::{Live, TableMem};
    use std::time::Duration;

    /// Serialises the tests that touch the process-wide render slot; the
    /// default test harness runs them in parallel.
    static ORDER: Mutex<()> = Mutex::new(());

    /// Stands in for the game's `Present`: records that it ran and returns a
    /// value the test can recognise.
    unsafe extern "system" fn fake_present(this: usize, sync: u32, flags: u32) -> i32 {
        let _ = (this, sync, flags);
        FAKE_PRESENT_HITS.fetch_add(1, Ordering::SeqCst);
        0x1234
    }

    fn fake_present_address() -> usize {
        let f: unsafe extern "system" fn(usize, u32, u32) -> i32 = fake_present;
        f as usize
    }

    static FAKE_PRESENT_HITS: AtomicUsize = AtomicUsize::new(0);
    static RENDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn init_without_the_game_reports_none() {
        let _guard = ORDER.lock().unwrap_or_else(|p| p.into_inner());
        shutdown();
        let mut kiero = Kiero::new();
        assert_eq!(kiero.status(), "none");
        assert_eq!(kiero.backend(), RenderType::None);
        // `dxgi.dll` is not loaded in the host test process.
        assert!(
            !kiero.init(RenderType::D3D11),
            "the host has no DXGI swapchain to bind"
        );
        assert_eq!(kiero.status(), "none");
        assert_eq!(super::status(), "none", "the free function agrees");
    }

    #[test]
    fn render_without_a_hook_does_nothing() {
        let _guard = ORDER.lock().unwrap_or_else(|p| p.into_inner());
        shutdown();
        RENDER_HITS.store(0, Ordering::SeqCst);
        render(|| {
            RENDER_HITS.fetch_add(1, Ordering::SeqCst);
        });
        // Nothing is bound, so the callback must never be reached.
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(RENDER_HITS.load(Ordering::SeqCst), 0);
        assert_eq!(original_present(), 0);
        assert!(!inside_frame());
        shutdown();
        assert_eq!(RENDER_HITS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_detour_runs_the_frame_then_forwards_and_guards_reentry() {
        let _guard = ORDER.lock().unwrap_or_else(|p| p.into_inner());
        shutdown();
        FAKE_PRESENT_HITS.store(0, Ordering::SeqCst);
        RENDER_HITS.store(0, Ordering::SeqCst);
        SEQUENCE.store(0, Ordering::SeqCst);

        // Publish a real Rust fn as the "original" so the whole detour path is
        // exercised on the host, not just the bookkeeping.
        PRESENT_ORIG.store(fake_present_address(), Ordering::Release);

        // The frame callback records its order, then re-enters Present: the
        // nested call must forward to the original without re-running the
        // callback.
        render(|| {
            SEQUENCE.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the detour is published with a real `Present` signature
            // and `PRESENT_ORIG` holds a fn pointer of exactly that ABI.
            let nested = unsafe { present_detour(0x10, 1, 0) };
            assert_eq!(nested, 0x1234, "the nested call reached the original");
            SEQUENCE.fetch_add(1, Ordering::SeqCst);
        });

        // SAFETY: same contract as the nested call above.
        let result = unsafe { present_detour(0x20, 1, 0) };
        assert_eq!(result, 0x1234, "the original's return value is preserved");
        assert_eq!(FAKE_PRESENT_HITS.load(Ordering::SeqCst), 2);
        assert_eq!(RENDER_HITS.load(Ordering::SeqCst), 0);
        // 0 -> frame callback entered; 1 -> nested forward returned; the
        // outer callback never re-ran.
        assert_eq!(SEQUENCE.load(Ordering::SeqCst), 2);
        assert!(!inside_frame(), "the frame flag is released");

        // A second frame runs the callback again only if it was re-registered:
        // `FnOnce` disarms after one frame.
        // SAFETY: same detour contract; the original is still published.
        let again = unsafe { present_detour(0x30, 1, 0) };
        assert_eq!(again, 0x1234);
        assert_eq!(FAKE_PRESENT_HITS.load(Ordering::SeqCst), 3);
        assert_eq!(SEQUENCE.load(Ordering::SeqCst), 2, "the callback disarmed");

        shutdown();
        assert_eq!(original_present(), 0);
        assert!(!inside_frame());
    }

    #[test]
    fn method_slots_match_the_swapchain_vtable() {
        assert_eq!(Method::Present.slot(), 8);
        assert_eq!(Method::ResizeBuffers.slot(), 13);
        assert_eq!(Method::ResizeTarget.slot(), 14);
        assert_eq!(Method::Present.name(), "Present");
        assert_eq!(PRESENT_SLOT, Method::Present.slot());
        assert_eq!(RESIZE_BUFFERS_SLOT, Method::ResizeBuffers.slot());
        assert_eq!(RESIZE_TARGET_SLOT, Method::ResizeTarget.slot());
    }

    #[test]
    fn shutdown_clears_the_backend_and_the_installed_hooks() {
        let _guard = ORDER.lock().unwrap_or_else(|p| p.into_inner());
        shutdown();
        let mut kiero = Kiero::new();
        kiero.shutdown();
        assert_eq!(kiero.status(), "none");
        assert_eq!(kiero.backend(), RenderType::None);
        assert_eq!(kiero.dxgi(), None);
        assert_eq!(kiero.swapchain(), 0);
        assert!(!kiero.present.is_some());
        assert!(kiero.module_hook.is_none());
    }

    #[test]
    fn live_is_the_default_backend_and_carries_no_hook_of_its_own() {
        // `Live` exists so `Vmt::new` has a real backend; the tests exercise
        // the synthetic one. This pins the wiring.
        let live = Live;
        assert_eq!(live.read_u64(0), None, "null is never readable");
        assert!(live.protect(0, 8, ffi::PAGE_READWRITE).is_none());
        assert!(!live.write_u64(0, 1));
        live.flush(0, 8);
    }
}
