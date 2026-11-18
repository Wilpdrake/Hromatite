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

use crate::log;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Why a PlayerLoop installation attempt did not produce a hook.
///
/// The distinction matters: an empty subsystem list right after `GameAssembly`
/// loads is *normal* (Unity builds the loop while it initialises), so the
/// installer has to keep polling instead of refusing for the rest of the
/// session. Only a populated loop that contradicts the selector is final.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeError {
    /// Unity has not published this part of the loop yet; a retry can succeed.
    NotReady(String),
    /// The loop exists and cannot be used as asked; retrying changes nothing.
    Fatal(String),
}

impl ProbeError {
    fn message(&self) -> &str {
        match self {
            ProbeError::NotReady(reason) | ProbeError::Fatal(reason) => reason,
        }
    }
}


/// The addresses this module calls. Both come from the build descriptor (or
/// `raw::discover` for an unknown build), never from a literal: the previous
/// version gated the whole module on `lateupdate::AUTO_BUILD_SHA256`, so a game
/// update silently turned PlayerLoop off even though the getter had merely
/// moved.
#[derive(Clone, Copy, Debug)]
struct Api {
    /// `PlayerLoop.GetCurrentPlayerLoop(PlayerLoopSystem* out, MethodInfo*)`.
    getter: usize,
    /// `il2cpp_class_from_il2cpp_type`; `0` when the build does not carry it,
    /// in which case the `Il2CppType.data` pointer is used directly.
    class_from_type: usize,
}

fn api() -> Result<Api, String> {
    let (base, size) =
        crate::procs::own_module(crate::GAME_ASSEMBLY).ok_or("GameAssembly.dll is not loaded")?;
    let runtime = crate::buildinfo::runtime_anchors(base, size)
        .ok_or("no PlayerLoop anchor for this GameAssembly build")?;
    if runtime.get_current_player_loop == 0 {
        return Err("the build descriptor has no GetCurrentPlayerLoop anchor".into());
    }
    let getter = base
        .checked_add(runtime.get_current_player_loop as usize)
        .filter(|address| *address >= base && *address < base.saturating_add(size))
        .ok_or("GetCurrentPlayerLoop is outside GameAssembly.dll")?;
    let class_from_type = match crate::buildinfo::il2cpp_anchors(base, size) {
        Some(anchors) if anchors.class_from_type != 0 => {
            base.wrapping_add(anchors.class_from_type as usize)
        }
        _ => 0,
    };
    Ok(Api { getter, class_from_type })
}

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

fn type_name(api: &Api, reflection: usize) -> String {
    // A managed `System.Type` keeps its `Il2CppType*` at +0x10.
    let Some(desc) = crate::safe::read_usize(reflection.wrapping_add(0x10)) else {
        return String::new();
    };
    if desc < 0x10000 {
        return String::new();
    }
    let resolved = if api.class_from_type >= 0x10000 {
        // SAFETY: class_from_type consumes the Il2CppType stored in a managed
        // Il2CppReflectionType at +0x10 and returns an Il2CppClass pointer.
        unsafe { crate::veh::call2(api.class_from_type, desc, 0) }.unwrap_or(0)
    } else {
        0
    };
    // `Il2CppType.data` is the `Il2CppClass*` for class and value types, so the
    // name can be read without calling anything when no anchor is known.
    let klass = if resolved >= 0x10000 {
        resolved
    } else {
        crate::safe::read_usize(desc).unwrap_or(0)
    };
    if klass < 0x10000 {
        return String::new();
    }
    let name = crate::m3::class_full_name(klass);
    // A wrong layout offset yields garbage here; the callers only need a label,
    // so anything that is not identifier-shaped is dropped instead of logged.
    if plausible_type_name(&name) {
        name
    } else {
        String::new()
    }
}

fn plausible_type_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'`' | b'<'
                | b'>' | b'[' | b']' | b',' | b'/' | b'*'))
}

/// One read of the live loop: the entries plus how many of them lost their type
/// name (an unnamed entry means the managed `Type` was not readable yet, which is
/// exactly how an early read looks).
struct Scan {
    entries: Vec<Entry>,
    unnamed: usize,
}

/// Read the public managed representation of the current native loop. No
/// SetPlayerLoop/SetPlayerLoopInternal call is made anywhere in this module.
fn scan() -> Result<Scan, ProbeError> {
    let api = api().map_err(ProbeError::Fatal)?;
    let mut root = [0usize; 5];
    // SAFETY: GetCurrentPlayerLoop returns PlayerLoopSystem (0x28 bytes) via a
    // hidden output pointer in rcx. root is exactly that writable stack buffer.
    let answer = unsafe { crate::veh::call2(api.getter, root.as_mut_ptr() as usize, 0) }
        .map_err(|error| ProbeError::Fatal(format!("GetCurrentPlayerLoop faulted: {error}")))?;
    if answer == 0 {
        return Err(ProbeError::NotReady(
            "GetCurrentPlayerLoop returned NULL".into(),
        ));
    }
    let children = root[1];
    if children == 0 {
        return Err(ProbeError::NotReady(
            "the player loop has no subsystem list yet".into(),
        ));
    }
    let mut scan = Scan {
        entries: Vec::new(),
        unnamed: 0,
    };
    read_children(&api, children, "", 0, &mut scan)?;
    if scan.entries.is_empty() {
        return Err(ProbeError::NotReady(
            "the player loop subsystem list is still empty".into(),
        ));
    }
    Ok(scan)
}

/// Snapshot for the console (`loopc`): every readiness state is reported as a
/// plain message, the retry policy belongs to [`install`].
pub fn snapshot() -> Result<Vec<Entry>, String> {
    scan()
        .map(|scan| scan.entries)
        .map_err(|error| error.message().to_string())
}

fn read_children(
    api: &Api,
    children: usize,
    prefix: &str,
    depth: usize,
    scan: &mut Scan,
) -> Result<(), ProbeError> {
    if depth > 16 {
        return Err(ProbeError::Fatal(
            "PlayerLoop nesting exceeds 16 levels".into(),
        ));
    }
    let count = crate::safe::read_u32(children.wrapping_add(0x18))
        .ok_or_else(|| ProbeError::Fatal("PlayerLoop child array is unreadable".into()))?
        as usize;
    if count > 128 || scan.entries.len().saturating_add(count) > 512 {
        return Err(ProbeError::Fatal(format!(
            "PlayerLoop child count {count} exceeds the safety bound"
        )));
    }
    for index in 0..count {
        let at = children.wrapping_add(0x20 + index * 0x28);
        let mut words = [0usize; 5];
        for (slot, word) in words.iter_mut().enumerate() {
            *word = crate::safe::read_usize(at.wrapping_add(slot * 8)).ok_or_else(|| {
                ProbeError::Fatal(format!("PlayerLoop child {index} qword {slot} is unreadable"))
            })?;
        }
        let name = type_name(api, words[0]);
        let segment = if name.is_empty() {
            scan.unnamed += 1;
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
        let mut entry = entry_from_words(index, path.clone(), words)
            .ok_or_else(|| ProbeError::Fatal("invalid PlayerLoop entry".into()))?;
        entry.path = path.clone();
        scan.entries.push(entry);
        if nested != 0 {
            read_children(api, nested, &path, depth + 1, scan)?;
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
    let original_first = crate::hook::original_first_required();
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

/// Pick the subsystem to hook, and say *why* nothing was picked.
///
/// An empty or unnamed loop is a transient state (Unity publishes it while it
/// initialises), so it must not be reported as a final failure — that is what
/// made `HROMATITE_PLAYER_LOOP=1` give up permanently a fraction of a second
/// after `GameAssembly` loaded.
fn select_or_probe(scan: &Scan, selector: &str) -> Result<Entry, ProbeError> {
    if let Some(entry) = select_root(&scan.entries, selector) {
        return Ok(entry);
    }
    if scan.entries.is_empty() {
        return Err(ProbeError::NotReady(
            "the player loop has no subsystems yet".into(),
        ));
    }
    if scan.unnamed > 0 {
        return Err(ProbeError::NotReady(format!(
            "no subsystem matched {selector:?} and {} of {} entries have no readable type name yet",
            scan.unnamed,
            scan.entries.len()
        )));
    }
    // Unity assigns the native callbacks after it publishes the tree, so a named
    // subsystem without one is still a transient state: keep polling, and let the
    // deadline report it if it never fills in.
    Err(ProbeError::NotReady(format!(
        "no subsystem {selector:?} carries a native callback yet ({} entries, all named)",
        scan.entries.len()
    )))
}

/// How long the installer keeps polling for a populated loop.
fn wait_budget_ms() -> u64 {
    std::env::var("HROMATITE_PLAYER_LOOP_WAIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000)
        .clamp(500, 120_000)
}

/// One installation attempt. Readiness problems come back as
/// [`ProbeError::NotReady`] so [`install`] can retry them.
fn try_install(selector: &str) -> Result<Entry, ProbeError> {
    if PLAYER_STATE.load(Ordering::Acquire) == PLAYER_ATTACHED {
        let entry = PLAYER_ENTRY.load(Ordering::Acquire);
        return Err(ProbeError::Fatal(format!(
            "a native PlayerLoop callback is already hooked at {entry:#x}"
        )));
    }
    let scan = scan()?;
    if scan.unnamed == 0 {
        for line in describe_entries(&scan.entries) {
            log::log_tag("playerloop", &line);
        }
    }
    let selected = select_or_probe(&scan, selector)?;
    if selected.update_function < 0x10000 {
        return Err(ProbeError::NotReady(format!(
            "{selector:?} has no native updateFunction yet"
        )));
    }
    // This build stores a slot with the actual code address as its first qword,
    // not executable bytes at updateFunction itself. Patch the loaded stub.
    let target = match native_callback_target(selected.update_function, crate::safe::read_usize) {
        Ok(target) => target,
        Err(reason) => return Err(ProbeError::NotReady(reason)),
    };
    log::log_tag(
        "playerloop",
        &format!(
            "target: {} updateFunction slot {:#x} -> code {target:#x}",
            selected.path, selected.update_function
        ),
    );
    let previous = PLAYER_ENTRY.load(Ordering::Acquire);
    if !target_change_allowed(previous, target) {
        return Err(ProbeError::Fatal(format!(
            "another native PlayerLoop target is configured at {previous:#x}; restart before changing subsystems"
        )));
    }
    let known_code = [crate::GAME_ASSEMBLY, crate::UNITY_PLAYER]
        .into_iter()
        .any(|name| {
            crate::procs::own_module(name)
                .is_some_and(|(base, size)| target >= base && target < base.saturating_add(size))
        });
    if !known_code {
        return Err(ProbeError::Fatal(format!(
            "PlayerLoop updateFunction {target:#x} is outside GameAssembly/UnityPlayer"
        )));
    }
    let prep = crate::hook::prepare(target).ok_or_else(|| {
        ProbeError::Fatal(format!(
            "cannot prepare native PlayerLoop trampoline at {target:#x}"
        ))
    })?;
    crate::hook::prepare_tick();
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
        return Err(ProbeError::Fatal(
            "native PlayerLoop patch failed".into(),
        ));
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
        return Err(ProbeError::Fatal(
            "native PlayerLoop callback was detached during installation".into(),
        ));
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

/// Hook the selected root subsystem, polling until Unity has published the loop.
///
/// The payload is injected as soon as `GameAssembly` is mapped, which is
/// *before* Unity builds its `PlayerLoopSystem` tree: a read at that moment sees
/// eight roots with empty child arrays and no callbacks at all. The previous
/// implementation treated that as a permanent failure, so PlayerLoop never hooked
/// on a normal launch.
pub fn install(selector: &str) -> Result<Entry, String> {
    let budget_ms = wait_budget_ms();
    let started = Instant::now();
    let mut last = String::from("the player loop was never readable");
    let outcome = crate::lateupdate::retry_for(
        Duration::from_millis(budget_ms),
        // Each attempt walks the whole loop and resolves every subsystem's type
        // name, so 500 ms keeps the probing cheap without making a normal launch
        // wait noticeably.
        Duration::from_millis(500),
        |attempt| match try_install(selector) {
            Ok(entry) => Some(Ok(entry)),
            Err(ProbeError::Fatal(reason)) => Some(Err(reason)),
            Err(ProbeError::NotReady(reason)) => {
                last = reason;
                if attempt % 20 == 0 {
                    log::log_tag(
                        "playerloop",
                        &format!(
                            "waiting for the player loop: attempt {attempt}, elapsed {}ms/{budget_ms}ms, last: {last}",
                            started.elapsed().as_millis()
                        ),
                    );
                }
                None
            }
        },
    );
    match outcome {
        Some(Ok(entry)) => Ok(entry),
        Some(Err(reason)) => Err(reason),
        None => Err(format!(
            "the player loop did not become ready within {budget_ms}ms; last: {last}"
        )),
    }
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
        Entry, PLAYER_ATTACHED, PLAYER_INSTALLING, ProbeError, Scan, entry_from_words,
        native_callback_target, refreshed_state, run_with_original, select_or_probe, select_root,
        target_change_allowed, wait_budget_ms,
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

    fn scan_of(entries: Vec<Entry>, unnamed: usize) -> Scan {
        Scan { entries, unnamed }
    }

    #[test]
    fn an_empty_loop_is_a_readiness_state_not_a_failure() {
        // Unity publishes the subsystem tree after the payload is mapped, so an
        // early read must be retried instead of ending the installation.
        let empty = scan_of(Vec::new(), 0);
        assert!(matches!(
            select_or_probe(&empty, "Update"),
            Err(ProbeError::NotReady(_))
        ));
        let unnamed = scan_of(entries(), 2);
        match select_or_probe(&unnamed, "PostLateUpdate") {
            Err(ProbeError::NotReady(reason)) => assert!(reason.contains("no readable type name")),
            other => panic!("expected NotReady, got {other:?}"),
        }
        let named = scan_of(entries(), 0);
        match select_or_probe(&named, "PostLateUpdate") {
            Err(ProbeError::NotReady(reason)) => assert!(reason.contains("native callback yet")),
            other => panic!("expected NotReady, got {other:?}"),
        }
        assert_eq!(
            select_or_probe(&named, "Update")
                .expect("named Update is hookable")
                .update_function,
            0x12340
        );
    }

    #[test]
    fn the_wait_budget_is_bounded() {
        // SAFETY: this test is the only writer of this variable in the crate's
        // test binary, and it restores nothing: the value is written last and
        // every case is checked immediately.
        unsafe {
            std::env::set_var("HROMATITE_PLAYER_LOOP_WAIT_MS", "0");
        }
        assert_eq!(wait_budget_ms(), 500);
        // SAFETY: as above; a single-threaded assertion follows the write.
        unsafe {
            std::env::set_var("HROMATITE_PLAYER_LOOP_WAIT_MS", "999999");
        }
        assert_eq!(wait_budget_ms(), 120_000);
        // SAFETY: as above.
        unsafe {
            std::env::remove_var("HROMATITE_PLAYER_LOOP_WAIT_MS");
        }
        assert_eq!(wait_budget_ms(), 30_000);
    }
}
