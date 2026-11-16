//! Native Unity PlayerLoop discovery and tick hook.
//!
//! `GetCurrentPlayerLoop` returns a managed 0x28-byte structure containing a
//! native `void(*)()` callback. We hook that existing callback rather than
//! replacing the loop with `SetPlayerLoop`.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub index: usize,
    pub type_name: String,
    pub path: String,
    pub children: usize,
    pub update_delegate: usize,
    pub update_function: usize,
    pub loop_condition_function: usize,
}

use crate::{console, log};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const GET_CURRENT_PLAYER_LOOP_RVA: usize = 0x086c_da10;
const CLASS_FROM_TYPE_RVA: usize = 0x00c2_8040;
const BUILD_SHA256: &str = lateupdate::AUTO_BUILD_SHA256;
use crate::lateupdate;

static PLAYER_ORIG: AtomicUsize = AtomicUsize::new(0);
static PLAYER_ENTRY: AtomicUsize = AtomicUsize::new(0);
static PLAYER_NAME: AtomicUsize = AtomicUsize::new(0);
static PLAYER_HITS: AtomicU64 = AtomicU64::new(0);
static PLAYER_TID: AtomicU64 = AtomicU64::new(0);
static PLAYER_START_MS: AtomicU64 = AtomicU64::new(0);
const PLAYER_INSTALLING: usize = 1;
const PLAYER_ATTACHED: usize = 2;
const PLAYER_DETACHED: usize = 3;
static PLAYER_STATE: AtomicUsize = AtomicUsize::new(PLAYER_DETACHED);

fn now_ms() -> u64 {
    // SAFETY: GetTickCount64 is a parameterless query with no pointer inputs.
    unsafe { crate::ffi::GetTickCount64() }
}

fn type_name(reflection: usize) -> String {
    let Some((ga, _)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        return String::new();
    };
    let desc = crate::safe::read_usize(reflection.wrapping_add(0x10)).unwrap_or(0);
    if desc < 0x10000 {
        return String::new();
    }
    // SAFETY: class_from_type consumes the Il2CppType stored in a managed
    // Il2CppReflectionType at +0x10 and returns an Il2CppClass pointer.
    let klass =
        unsafe { crate::veh::call2(ga.wrapping_add(CLASS_FROM_TYPE_RVA), desc, 0) }.unwrap_or(0);
    if klass < 0x10000 {
        String::new()
    } else {
        crate::m3::class_full_name(klass)
    }
}

/// Read the public managed representation of the current native loop. No
/// SetPlayerLoop/SetPlayerLoopInternal call is made anywhere in this module.
pub fn snapshot() -> Result<Vec<Entry>, String> {
    if crate::symbols::GAMEASSEMBLY_SHA256 != BUILD_SHA256 {
        return Err("PlayerLoop RVAs belong to another GameAssembly build".into());
    }
    let (ga, gasize) =
        crate::procs::own_module(crate::GAME_ASSEMBLY).ok_or("GameAssembly.dll is not loaded")?;
    let getter = ga.wrapping_add(GET_CURRENT_PLAYER_LOOP_RVA);
    if getter < ga || getter >= ga.saturating_add(gasize) {
        return Err("GetCurrentPlayerLoop is outside GameAssembly.dll".into());
    }
    let mut root = [0usize; 5];
    // SAFETY: GetCurrentPlayerLoop returns PlayerLoopSystem (0x28 bytes) via a
    // hidden output pointer in rcx. root is exactly that writable stack buffer.
    let answer = unsafe { crate::veh::call2(getter, root.as_mut_ptr() as usize, 0) }
        .map_err(|error| format!("GetCurrentPlayerLoop faulted: {error}"))?;
    if answer == 0 {
        return Err("GetCurrentPlayerLoop returned NULL".into());
    }
    let children = root[1];
    let mut entries = Vec::new();
    if children != 0 {
        read_children(children, "", 0, &mut entries)?;
    }
    Ok(entries)
}

fn read_children(
    children: usize,
    prefix: &str,
    depth: usize,
    entries: &mut Vec<Entry>,
) -> Result<(), String> {
    if depth > 16 {
        return Err("PlayerLoop nesting exceeds 16 levels".into());
    }
    let count = crate::safe::read_u32(children.wrapping_add(0x18))
        .ok_or("PlayerLoop child array is unreadable")? as usize;
    if count > 128 || entries.len().saturating_add(count) > 512 {
        return Err(format!(
            "PlayerLoop child count {count} exceeds the safety bound"
        ));
    }
    for index in 0..count {
        let at = children.wrapping_add(0x20 + index * 0x28);
        let mut words = [0usize; 5];
        for (slot, word) in words.iter_mut().enumerate() {
            *word = crate::safe::read_usize(at.wrapping_add(slot * 8))
                .ok_or_else(|| format!("PlayerLoop child {index} qword {slot} is unreadable"))?;
        }
        let name = type_name(words[0]);
        let segment = if name.is_empty() {
            format!("#{index}")
        } else {
            name
        };
        let path = if prefix.is_empty() {
            segment
        } else {
            format!("{prefix}/{segment}")
        };
        let nested = words[1];
        let mut entry =
            entry_from_words(index, path.clone(), words).ok_or("invalid PlayerLoop entry")?;
        entry.path = path.clone();
        entries.push(entry);
        if nested != 0 {
            read_children(nested, &path, depth + 1, entries)?;
        }
    }
    Ok(())
}

fn describe_entries(entries: &[Entry]) -> Vec<String> {
    let mut lines = vec![format!("PlayerLoop: {} root subsystem(s)", entries.len())];
    for entry in entries {
        lines.push(format!(
            "  #{} {} children={:#x} delegate={:#x} update={:#x} condition={:#x}",
            entry.index,
            if entry.path.is_empty() {
                "<unnamed>"
            } else {
                &entry.path
            },
            entry.children,
            entry.update_delegate,
            entry.update_function,
            entry.loop_condition_function
        ));
    }
    lines
}

pub fn snapshot_lines() -> Vec<String> {
    match snapshot() {
        Ok(entries) => describe_entries(&entries),
        Err(error) => vec![format!("PlayerLoop: {error}")],
    }
}

/// Native PlayerLoop callbacks are void(*)(void). The original call is
/// mandatory even if our guarded bookkeeping fails.
///
/// # Safety
/// This detour is only patched over the concrete updateFunction returned by
/// GetCurrentPlayerLoop, whose callback contract has no managed parameters.
pub unsafe extern "system" fn player_loop_detour() {
    let original = PLAYER_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is the trampoline for the same no-argument native
    // PlayerLoop updateFunction that was returned by Unity.
    let f: unsafe extern "system" fn() = unsafe { core::mem::transmute(original) };
    let original_first = crate::raw::hot::original_first_required();
    if original_first {
        // SAFETY: the trampoline for the selected native updateFunction has already
        // been validated; the reload transition must not delay this real game call.
        unsafe { f() };
    }
    run_with_original(
        || {
            log::guard("player_loop_detour::pre", || {
                let hit = PLAYER_HITS.fetch_add(1, Ordering::Relaxed) + 1;
                let tid = crate::gcthreads::own_tid() as u64;
                PLAYER_TID.store(tid, Ordering::Relaxed);
                if hit == 1 {
                    log::log_tag(
                        "playerloop",
                        &format!("first native PlayerLoop hit: tid={tid:#x}"),
                    );
                }
                crate::hook::tick();
            })
            .map(|_| ())
            .ok_or(())
        },
        // SAFETY: during reload the original runs first, so loader/init/shutdown
        // cannot hold the original behind our transaction. Otherwise ordinary
        // pre-original telemetry is preserved, and the callback runs exactly once.
        || {
            if !original_first {
                // SAFETY: the no-argument trampoline preserves the original callback.
                unsafe { f() }
            }
        },
    );
}

fn native_callback_target(
    update_function: usize,
    read_slot: impl FnOnce(usize) -> Option<usize>,
) -> Result<usize, String> {
    read_slot(update_function)
        .filter(|target| *target >= 0x10000)
        .ok_or_else(|| format!("PlayerLoop callback slot {update_function:#x} is unreadable"))
}

fn target_change_allowed(previous: usize, candidate: usize) -> bool {
    previous == 0 || previous == candidate
}

pub fn install(selector: &str) -> Result<Entry, String> {
    if PLAYER_STATE.load(Ordering::Acquire) == PLAYER_ATTACHED {
        let entry = PLAYER_ENTRY.load(Ordering::Acquire);
        return Err(format!(
            "a native PlayerLoop callback is already hooked at {entry:#x}"
        ));
    }
    let entries = snapshot()?;
    for line in describe_entries(&entries) {
        log::log_tag("playerloop", &line);
    }
    let selected = select_root(&entries, selector).ok_or_else(|| {
        format!("one unambiguous root subsystem {selector:?} with a native callback is required")
    })?;
    if selected.update_function < 0x10000 {
        return Err("selected PlayerLoop updateFunction is NULL".into());
    }
    // This build stores a slot with the actual code address as its first qword,
    // not executable bytes at updateFunction itself. Patch the loaded stub.
    let target = native_callback_target(selected.update_function, crate::safe::read_usize)?;
    let previous = PLAYER_ENTRY.load(Ordering::Acquire);
    if !target_change_allowed(previous, target) {
        return Err(format!(
            "another native PlayerLoop target is configured at {previous:#x}; restart before changing subsystems"
        ));
    }
    let known_code = [crate::GAME_ASSEMBLY, crate::UNITY_PLAYER]
        .into_iter()
        .any(|name| {
            crate::procs::own_module(name)
                .is_some_and(|(base, size)| target >= base && target < base.saturating_add(size))
        });
    if !known_code {
        return Err(format!(
            "PlayerLoop updateFunction {target:#x} is outside GameAssembly/UnityPlayer"
        ));
    }
    let prep = crate::hook::prepare(target)
        .ok_or_else(|| format!("cannot prepare native PlayerLoop trampoline at {target:#x}"))?;
    crate::hook::prepare_tick();
    log::log_tag(
        "playerloop",
        &format!("console file {}", console::cmd_path().display()),
    );
    PLAYER_HITS.store(0, Ordering::Relaxed);
    PLAYER_TID.store(0, Ordering::Relaxed);
    PLAYER_START_MS.store(now_ms(), Ordering::Relaxed);
    let leaked_name = Box::leak(Box::new(selected.path.clone())) as *const String as usize;
    PLAYER_NAME.store(leaked_name, Ordering::Relaxed);
    PLAYER_ORIG.store(prep.trampoline, Ordering::Release);
    PLAYER_ENTRY.store(target, Ordering::Release);
    PLAYER_STATE.store(PLAYER_INSTALLING, Ordering::Release);
    let Some(hook) = crate::hook::patch(&prep, player_loop_detour as *const () as usize) else {
        PLAYER_ORIG.store(0, Ordering::Release);
        PLAYER_NAME.store(0, Ordering::Relaxed);
        PLAYER_STATE.store(PLAYER_DETACHED, Ordering::Release);
        return Err("native PlayerLoop patch failed".into());
    };
    crate::hook::register(hook);
    if PLAYER_STATE
        .compare_exchange(
            PLAYER_INSTALLING,
            PLAYER_ATTACHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Err("native PlayerLoop callback was detached during installation".into());
    }
    log::log_tag(
        "playerloop",
        &format!(
            "hooked {} at {target:#x}, native void(*)(void) updateFunction",
            selected.path
        ),
    );
    Ok(selected)
}

fn refreshed_state(state: usize, current: Option<bool>) -> usize {
    match (state, current) {
        (PLAYER_INSTALLING, Some(true)) => PLAYER_DETACHED,
        (PLAYER_INSTALLING, _) => PLAYER_INSTALLING,
        (_, Some(false)) => PLAYER_ATTACHED,
        _ => PLAYER_DETACHED,
    }
}

pub fn refresh_attachment(hooks: &[(usize, bool)]) {
    let entry = PLAYER_ENTRY.load(Ordering::Acquire);
    let current = hooks
        .iter()
        .find(|(target, _)| *target == entry)
        .map(|(_, detached)| *detached);
    let state = PLAYER_STATE.load(Ordering::Acquire);
    let next = refreshed_state(state, current);
    if next != state {
        // Before registration there is nothing for `unhook` to restore. The CAS
        // after register resolves an actual detach without a publication race.
        PLAYER_STATE.store(next, Ordering::Release);
    }
}

pub fn status() -> Vec<String> {
    let entry = PLAYER_ENTRY.load(Ordering::Acquire);
    if entry == 0 || PLAYER_STATE.load(Ordering::Acquire) != PLAYER_ATTACHED {
        return vec!["PlayerLoop: native callback not hooked".into()];
    }
    let name_ptr = PLAYER_NAME.load(Ordering::Relaxed);
    let name = if name_ptr == 0 {
        String::new()
    } else {
        // SAFETY: install leaks one immutable type name for the process lifetime.
        unsafe { &*(name_ptr as *const String) }.clone()
    };
    let hits = PLAYER_HITS.load(Ordering::Relaxed);
    let elapsed = now_ms().saturating_sub(PLAYER_START_MS.load(Ordering::Relaxed));
    let rate = if elapsed == 0 {
        0.0
    } else {
        hits as f64 * 1000.0 / elapsed as f64
    };
    vec![format!(
        "PlayerLoop: native pre-original, class {name}, entry={entry:#x}, hits={hits} ({rate:.1}/s), last tid={:#x}",
        PLAYER_TID.load(Ordering::Relaxed)
    )]
}

pub fn entry_from_words(index: usize, type_name: String, words: [usize; 5]) -> Option<Entry> {
    Some(Entry {
        index,
        path: type_name.clone(),
        type_name,
        children: words[1],
        update_delegate: words[2],
        update_function: words[3],
        loop_condition_function: words[4],
    })
}

fn run_with_original<T, E>(
    work: impl FnOnce() -> Result<(), E>,
    original: impl FnOnce() -> T,
) -> T {
    let _ = work();
    original()
}

pub fn select_root(entries: &[Entry], selector: &str) -> Option<Entry> {
    let short = selector.rsplit(['+', '.']).next().unwrap_or(selector);
    let mut matches = entries.iter().filter(|entry| {
        entry.update_function >= 0x10000
            && (entry.path == selector
                || entry.type_name == selector
                || entry.type_name.rsplit(['+', '.']).next() == Some(short))
    });
    if let Some(entry) = matches.next() {
        if matches.next().is_some() {
            return None;
        }
        return Some(entry.clone());
    }
    let root = entries.iter().find(|entry| {
        entry.path == entry.type_name && entry.type_name.rsplit(['+', '.']).next() == Some(short)
    })?;
    let prefix = format!("{}/", root.path);
    entries
        .iter()
        .find(|entry| entry.path.starts_with(&prefix) && entry.update_function >= 0x10000)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, PLAYER_ATTACHED, PLAYER_INSTALLING, entry_from_words, native_callback_target,
        refreshed_state, run_with_original, select_root, target_change_allowed,
    };

    fn entries() -> Vec<Entry> {
        vec![
            entry_from_words(
                0,
                "UnityEngine.LowLevel.PlayerLoop+Initialization".into(),
                [0x10, 0x20, 0x30, 0x40, 0x50],
            )
            .expect("first entry"),
            entry_from_words(
                1,
                "UnityEngine.LowLevel.PlayerLoop+Update".into(),
                [0x10, 0x20, 0x30, 0x12340, 0x50],
            )
            .expect("second entry"),
        ]
    }

    #[test]
    fn managed_struct_fields_map_to_the_measured_0x28_layout() {
        let entry = &entries()[1];
        assert_eq!(entry.children, 0x20);
        assert_eq!(entry.update_delegate, 0x30);
        assert_eq!(entry.update_function, 0x12340);
        assert_eq!(entry.loop_condition_function, 0x50);
    }

    #[test]
    fn selector_chooses_one_named_native_callback() {
        assert_eq!(
            select_root(&entries(), "Update").unwrap().update_function,
            0x12340
        );
        assert_eq!(
            select_root(&entries(), "UnityEngine.LowLevel.PlayerLoop+Update")
                .unwrap()
                .update_function,
            0x12340
        );
        assert!(select_root(&entries(), "PostLateUpdate").is_none());
    }

    #[test]
    fn selector_rejects_zero_callbacks_and_ambiguous_names() {
        let mut list = entries();
        assert!(select_root(&list, "Initialization").is_none());
        list[1].type_name = "Other.Update".into();
        list.push(list[1].clone());
        assert!(select_root(&list, "Update").is_none());
    }

    #[test]
    fn detached_hook_cannot_move_an_in_flight_frame_to_another_subsystem() {
        assert!(target_change_allowed(0, 0x1000));
        assert!(target_change_allowed(0x1000, 0x1000));
        assert!(!target_change_allowed(0x1000, 0x2000));
    }

    #[test]
    fn unhook_during_registration_wins_the_publication_race() {
        assert_eq!(refreshed_state(PLAYER_INSTALLING, None), PLAYER_INSTALLING);
        assert_eq!(
            refreshed_state(PLAYER_INSTALLING, Some(false)),
            PLAYER_INSTALLING
        );
        assert_ne!(
            refreshed_state(PLAYER_INSTALLING, Some(true)),
            PLAYER_ATTACHED
        );
        assert_eq!(
            refreshed_state(PLAYER_ATTACHED, Some(false)),
            PLAYER_ATTACHED
        );
        assert_ne!(
            refreshed_state(PLAYER_ATTACHED, Some(true)),
            PLAYER_ATTACHED
        );
    }

    #[test]
    fn zero_callback_root_selects_the_first_native_callback_in_its_own_branch() {
        let mut list = entries();
        list[1].update_function = 0;
        let mut child = list[1].clone();
        child.path = "UnityEngine.LowLevel.PlayerLoop+Update/ScriptRunBehaviourUpdate".into();
        child.type_name = "UnityEngine.LowLevel.PlayerLoop.Update.ScriptRunBehaviourUpdate".into();
        child.update_function = 0x56780;
        list.push(child);
        let selected = select_root(&list, "Update").expect("native child in Update branch");
        assert_eq!(
            selected.type_name,
            "UnityEngine.LowLevel.PlayerLoop.Update.ScriptRunBehaviourUpdate"
        );
        assert_eq!(selected.update_function, 0x56780);
    }

    #[test]
    fn update_function_slot_is_dereferenced_before_patching() {
        let field = 0x6ffffc4d8378usize;
        let code = 0x6ffffaaefb80usize;
        let target = native_callback_target(field, |slot| {
            assert_eq!(slot, field);
            (slot == field).then_some(code)
        });
        assert_eq!(target.unwrap(), code);
        assert!(native_callback_target(field, |_| None).is_err());
    }

    #[test]
    fn original_callback_still_runs_when_payload_work_fails() {
        let mut original_called = false;
        run_with_original(
            || Err("payload fault"),
            || {
                original_called = true;
            },
        );
        assert!(original_called);
    }
}
