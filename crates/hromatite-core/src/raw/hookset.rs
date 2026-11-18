//! A small process-wide manager for inline hooks.
//!
//! `hook::hook_inline` and `midhook::install` patch one address each; they keep
//! no shared state. [`HookSet`] is the registry that owns them: it refuses a
//! second hook on a target, a hook whose target lies inside another hook's
//! overwritten window, and a target that is not readable, then tracks every
//! active entry so it can be uninstalled or cleared later.
//!
//! The registry mutex is held only for the few instructions that read or mutate
//! the vector. It is dropped before the engine is called and before any
//! `unhook` write: a blocking mutex on Unity's managed thread would hang the
//! game, so no guard is ever held across a call into the game.

use std::sync::{Mutex, MutexGuard};

use crate::log;
use super::hook::{self, Hook};
use super::midhook::{self, MidHook};

/// Which engine owns a registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Function entry (`hook::hook_inline`).
    Inline,
    /// Mid-function (`midhook::install`).
    Mid,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Inline => "inline",
            Kind::Mid => "mid",
        }
    }
}

/// Why an install was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookError {
    /// The target is below `0x10000` or its first bytes are not readable.
    Unreadable,
    /// An active hook already owns this exact target.
    DuplicateTarget,
    /// The target lies inside another active hook's overwritten window.
    AddressOccupied,
    /// The engine refused the window or the OS write failed.
    InstallFailed,
}

enum Slot {
    Inline(Hook),
    Mid(MidHook),
}

struct Entry {
    target: usize,
    detour: usize,
    kind: Kind,
    active: bool,
    /// First byte after the patched window (`target + prologue`).
    end: usize,
    slot: Option<Slot>,
}

/// Registry of installed inline hooks. Cheap to clone-free share: every method
/// takes `&self`.
pub struct HookSet {
    hooks: Mutex<Vec<Entry>>,
}

impl Default for HookSet {
    fn default() -> Self {
        Self::new()
    }
}

impl HookSet {
    pub fn new() -> Self {
        Self {
            hooks: Mutex::new(Vec::new()),
        }
    }

    /// Lock the registry, recovering from poisoning instead of panicking: a
    /// poisoned set is still better served than taking the game down.
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        match self.hooks.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Install an entry hook at `target`.
    pub fn install_inline(&self, target: usize, detour: usize) -> Result<(), HookError> {
        self.install(target, detour, Kind::Inline)
    }

    /// Install a mid-function hook at `target`.
    pub fn install_mid(&self, target: usize, detour: usize) -> Result<(), HookError> {
        self.install(target, detour, Kind::Mid)
    }

    fn install(&self, target: usize, detour: usize, kind: Kind) -> Result<(), HookError> {
        // Registry checks first: they are cheap and touch no game memory, so a
        // duplicate is refused even before the target is validated.
        {
            let g = self.lock();
            if g.iter().any(|e| e.active && e.target == target) {
                return Err(HookError::DuplicateTarget);
            }
            if g
                .iter()
                .any(|e| e.active && target >= e.target && target < e.end)
            {
                return Err(HookError::AddressOccupied);
            }
        }
        if target < 0x10000 || !crate::safe::readable_range(target, 5) {
            return Err(HookError::Unreadable);
        }
        // The engine runs without the registry lock: it allocates a page,
        // writes the trampoline and patches the target.
        let slot = match kind {
            Kind::Inline => hook::hook_inline(target, detour).map(Slot::Inline),
            Kind::Mid => midhook::install(target, detour).map(Slot::Mid),
        };
        let slot = slot.ok_or(HookError::InstallFailed)?;
        let end = target
            + match &slot {
                Slot::Inline(h) => h.prologue,
                Slot::Mid(h) => h.prologue,
            };
        let mut g = self.lock();
        // A concurrent install may have taken the same target while we patched.
        // Drop the registry guard, then put the fresh patch's bytes back: its
        // snapshot was taken after the winner's patch, so this restores the
        // winner instead of leaving an untracked jump behind.
        if g.iter().any(|e| e.active && e.target == target) {
            drop(g);
            match slot {
                Slot::Inline(mut h) => {
                    h.unhook();
                }
                Slot::Mid(mut h) => {
                    h.unhook();
                }
            }
            return Err(HookError::DuplicateTarget);
        }
        g.push(Entry {
            target,
            detour,
            kind,
            active: true,
            end,
            slot: Some(slot),
        });
        log::log_tag(
            "hookset",
            &format!("install: {kind:?} {target:#x} -> {detour:#x} (window {}b)", end - target),
        );
        Ok(())
    }

    /// Remove the hook at `target`. Returns `false` when no active entry owns
    /// it. The registry lock is released before the bytes are restored.
    pub fn uninstall(&self, target: usize) -> bool {
        let slot = {
            let mut g = self.lock();
            let Some(entry) = g.iter_mut().find(|e| e.active && e.target == target) else {
                return false;
            };
            entry.active = false;
            entry.slot.take()
        };
        match slot {
            Some(Slot::Inline(mut h)) => h.unhook(),
            Some(Slot::Mid(mut h)) => h.unhook(),
            None => true,
        }
    }

    /// Remove every active hook, restoring its bytes. Returns how many active
    /// entries were removed.
    pub fn clear(&self) -> usize {
        let (slots, released) = {
            let mut g = self.lock();
            let mut out = Vec::new();
            let mut released = 0usize;
            for entry in g.iter_mut() {
                if entry.active {
                    entry.active = false;
                    released += 1;
                    if let Some(slot) = entry.slot.take() {
                        out.push(slot);
                    }
                }
            }
            (out, released)
        };
        for mut slot in slots {
            let _ = match slot {
                Slot::Inline(ref mut h) => h.unhook(),
                Slot::Mid(ref mut h) => h.unhook(),
            };
        }
        released
    }

    /// One line per registry entry, oldest first.
    pub fn status(&self) -> Vec<String> {
        let g = self.lock();
        g.iter()
            .map(|e| {
                let state = if e.active { "active" } else { "removed" };
                format!(
                    "{} target={:#x} detour={:#x} window={}b {state}",
                    e.kind.as_str(),
                    e.target,
                    e.detour,
                    e.end - e.target
                )
            })
            .collect()
    }

    /// Number of active hooks.
    pub fn len(&self) -> usize {
        self.lock().iter().filter(|e| e.active).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test-only: register an entry without touching game memory, so the
    /// duplicate/occupancy policy can be exercised on the host.
    #[cfg(test)]
    fn seed(&self, target: usize, detour: usize, kind: Kind, prologue: usize) {
        let mut g = self.lock();
        g.push(Entry {
            target,
            detour,
            kind,
            active: true,
            end: target + prologue,
            slot: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 0x20_0000;
    const B: usize = 0x30_0000;

    #[test]
    fn install_rejects_unreadable_memory() {
        let set = HookSet::new();
        assert_eq!(set.install_mid(0, B), Err(HookError::Unreadable));
        assert_eq!(set.install_inline(0x1000, B), Err(HookError::Unreadable));
        assert!(set.is_empty());
    }

    #[test]
    fn a_second_hook_on_the_same_target_is_refused() {
        let set = HookSet::new();
        set.seed(A, B, Kind::Mid, 5);
        assert_eq!(set.install_mid(A, B), Err(HookError::DuplicateTarget));
        assert_eq!(set.install_inline(A, B), Err(HookError::DuplicateTarget));
        assert_eq!(set.len(), 1, "the refusal must not add an entry");
    }

    #[test]
    fn a_hook_inside_another_window_is_refused() {
        let set = HookSet::new();
        set.seed(A, B, Kind::Inline, 5);
        assert_eq!(
            set.install_mid(A + 2, B),
            Err(HookError::AddressOccupied),
            "the patched window is owned by the first hook"
        );
        assert_eq!(
            set.install_mid(A + 4, B),
            Err(HookError::AddressOccupied),
            "the last patched byte is still inside"
        );
        // `end` is exclusive: the byte after the window is free. On the host it
        // is not mapped, so the refusal is the readability gate, not occupancy.
        assert_eq!(set.install_mid(A + 5, B), Err(HookError::Unreadable));
    }

    #[test]
    fn uninstall_marks_the_entry_inactive_and_is_idempotent() {
        let set = HookSet::new();
        set.seed(A, B, Kind::Mid, 5);
        assert_eq!(set.len(), 1);
        assert!(set.uninstall(A));
        assert_eq!(set.len(), 0);
        assert!(!set.uninstall(A), "the second uninstall finds nothing");
        let status = set.status();
        assert_eq!(status.len(), 1, "history is kept");
        assert!(status[0].contains("removed"), "{}", status[0]);
        assert!(status[0].contains("mid"), "{}", status[0]);
        assert!(status[0].contains("target=0x20000"), "{}", status[0]);
        assert!(status[0].contains("window=5b"), "{}", status[0]);
    }

    #[test]
    fn clear_removes_every_active_entry_once() {
        let set = HookSet::new();
        set.seed(A, B, Kind::Inline, 5);
        set.seed(B, A, Kind::Mid, 7);
        assert_eq!(set.len(), 2);
        assert_eq!(set.clear(), 2);
        assert!(set.is_empty());
        assert_eq!(set.clear(), 0);
        assert_eq!(set.status().len(), 2, "entries stay for diagnostics");
    }

    #[test]
    fn default_matches_new() {
        let set = HookSet::default();
        assert!(set.is_empty());
        assert!(set.status().is_empty());
    }
}
