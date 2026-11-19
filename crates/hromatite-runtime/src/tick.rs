//! The managed tick: what runs inside Unity's own frame callback.
//!
//! `hromatite-core`'s LateUpdate / PlayerLoop detours call
//! [`hromatite_il2cpp::hook::tick`], which dispatches through the
//! [`TickTarget`](hromatite_il2cpp::hook::TickTarget) published here. Splitting it
//! this way keeps the detours free of the command channel and the feature
//! router, and lets the two crates stay acyclic.
//!
//! Order inside one throttled pass:
//!   1. GC reports from the inline guard and the worker trace (formatting, file
//!      I/O - never inside the collector's stop-the-world detour);
//!   2. the hot-reload router (`hot::router::tick`): feature canaries, rollback,
//!      retirement;
//!   3. command-file polling and dispatch.
//!
//! The per-frame entry (`frame`) is separate and unthrottled: `Graphics.DrawMesh`
//! applies to exactly one frame.

use crate::console;
use crate::hot::router;
use hromatite_il2cpp::{gcthreads, hook, log};
use std::sync::Mutex;

static READER: Mutex<Option<console::Reader>> = Mutex::new(None);

/// One-time setup: arm the command reader and tell the log where commands go.
/// Idempotent; called from `hook::prepare_tick` on every (re)installation.
fn prepare() {
    // The interval itself is resolved lazily by `router::tick_interval_ms`
    // (`HROMATITE_TICK_MS`, then the hot configuration).
    if let Ok(mut reader) = READER.lock()
        && reader.is_none()
    {
        *reader = Some(console::Reader::new());
    }
    log::log_tag(
        "con",
        &format!("console file {}", console::cmd_path().display()),
    );
}

/// Every frame, before the throttle: the feature image's per-frame entry.
fn frame() {
    router::frame();
}

/// Throttle for the full pass, read from the hot configuration.
fn interval_ms() -> u64 {
    router::tick_interval_ms()
}

/// Throttled work on the game's managed thread.
fn work() {
    // Inline repairs publish lock-free counters in GetThreadContext; formatting
    // and file IO happen only after the collector's stop-the-world detour.
    if let Some((count, last_tid)) = hook::take_inline_repairs() {
        log::log_tag(
            "gct",
            &format!("inline guard: {count} stale record(s) reaped; last id={last_tid:#x}"),
        );
    }
    for line in hromatite_il2cpp::gcguard::report_lines() {
        log::log_tag("gct", &line);
    }
    for line in gcthreads::drain_worker_events() {
        log::log_tag("gct", &line);
    }

    // Versioned feature work uses this same already-established game tick. It
    // never creates a worker and never replaces the resident GC guard.
    router::tick();

    let _ = log::guard("hook::work", || {
        let mut reader = {
            let mut guard = match READER.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.take().unwrap_or_default()
        };
        // Never hold the reader's mutex across a longjmp boundary.
        let cmds = log::guard("reader.poll", || reader.poll()).unwrap_or_default();
        {
            let mut guard = match READER.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = Some(reader);
        }
        for line in cmds {
            log::log_tag("con", &format!("> {line}"));
            if let Some(out) = router::command(&line) {
                for line in out {
                    log::log_tag("con", &line);
                }
                continue;
            }
            if !router::resident_command(&line) && router::dispatch(&line) {
                continue;
            }
            let answered = log::guard("console::dispatch", || console::dispatch(&line));
            let (out, action) = match answered {
                Some(value) => value,
                None => (
                    vec!["command panicked (see PANIC line above)".to_string()],
                    console::Action::None,
                ),
            };
            for line in out {
                log::log_tag("con", &line);
            }
            match action {
                console::Action::Seal | console::Action::Quit => {
                    log::log_tag(
                        "con",
                        "seal/quit is unavailable with the managed in-image runtime",
                    );
                }
                console::Action::None => {}
            }
        }
    });
}

/// True only for reload transitions, where the game's own method must run
/// before any payload work.
fn original_first() -> bool {
    router::original_first_required()
}

/// The implementation published into `hromatite_il2cpp::hook`.
pub static TARGET: hook::TickTarget = hook::TickTarget {
    prepare,
    frame,
    interval_ms,
    work,
    original_first,
};

/// Publish the tick. Called once by [`crate::startup`], before any detour can
/// fire and before the LateUpdate/PlayerLoop installers run.
pub fn install() {
    hook::set_tick_target(&TARGET);
}
