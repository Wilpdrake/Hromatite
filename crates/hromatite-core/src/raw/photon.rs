//! Managed Photon event surface: `PhotonPeer.SendOperation` / `PhotonPeer.OnEvent`.
//!
//! Photon is the transport VRChat uses for everything above the EAC session:
//! *outgoing* operations are `SendOperation(byte operationCode, Dictionary<byte,
//! object> parameters, SendOptions sendOptions)`, so a RaiseEvent (opCode 253)
//! carries the event code in dictionary slot 244 and the payload in slot 245
//! (66 = the EAC heartbeat event). *Incoming* events arrive through
//! `OnEvent(EventData)`.
//!
//! Step 1 (this module) is **observation only**: resolve the managed method
//! through [`crate::m3`] (no il2cpp API calls, so it is safe from the game's own
//! thread), install one inline hook with the shared [`crate::raw::hook`] engine,
//! log a throttled sample of the raw arguments and forward every call to the
//! original untouched. Nothing is injected yet, and
//! [`decode_event_params`] is a measured-layout stub (see its TODO).
//!
//! # Argument ABI
//!
//! IL2CPP instance methods take `this` in `rcx`, the declared arguments next
//! (`rdx`, `r8`, `r9`) and the hidden `MethodInfo*` as the last slot (on the
//! stack here: `this` + 3 declared arguments already fill the four registers).
//! The detour therefore only reads `rcx`/`rdx`/`r8`/`r9` as `u64` and forwards
//! them unchanged — exactly the subset the trampoline can restore. A method
//! whose body actually *reads* the hidden `MethodInfo*` sees it displaced by the
//! detour's own frame; that is why the live run must be checked for `SendOperation`
//! returning normally (see §17 of `AGENTS.md`).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::log;

/// Log tag for every line of this module (`[photon] …`).
pub const TAG: &str = "photon";

/// Photon operation code of `OpRaiseEvent` (`SendOperation(253, params, …)`).
pub const OP_RAISE_EVENT: u8 = 253;
/// RaiseEvent parameter key holding the event code.
pub const PARAM_EVENT_CODE: u8 = 244;
/// RaiseEvent parameter key holding the event payload.
pub const PARAM_EVENT_DATA: u8 = 245;
/// The EAC heartbeat event code (the one this project ultimately wants to raise).
pub const EVENT_EAC_HEARTBEAT: u8 = 66;

/// Upper bound of the class-table scan (`m3::find` stops at 64 hits anyway).
pub const SCAN_LIMIT: usize = 200_000;

/// Log the first this many calls, then every `LOG_EVERY`th.
const LOG_FIRST: u64 = 50;
const LOG_EVERY: u64 = 200;

/// `(namespace, class)` pairs tried for the peer class, in order. Photon is
/// shipped as `Assembly-CSharp` code in some builds and as a separate assembly
/// in others, so the empty namespace is a deliberate second chance.
pub const PEER_CANDIDATES: &[(&str, &str)] = &[
    ("ExitGames.Client.Photon", "PhotonPeer"),
    ("", "PhotonPeer"),
    ("ExitGames.Client.Photon", "LoadBalancingPeer"),
    ("", "LoadBalancingPeer"),
];

/// Outgoing method names, in preference order (`SendOperationInternal` and the
/// older `OpCustom` shape are kept as fallbacks — the live build decides).
pub const SEND_METHODS: &[&str] = &["SendOperation", "SendOperationInternal", "OpCustom"];
/// Incoming method names, in preference order.
pub const EVENT_METHODS: &[&str] = &["OnEvent", "OnEventInternal", "OnOperationResponse"];

/// `SendOperation` declares three parameters (`byte`, `Dictionary<byte,object>`,
/// `SendOptions`); the resolver prefers that arity but never requires it.
const SEND_PARAMETERS: u8 = 3;

/// One resolved managed method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolved {
    /// `Il2CppClass*`
    pub class: usize,
    /// `MethodInfo*`
    pub info: usize,
    /// `methodPointer` (the code the inline hook patches)
    pub entry: usize,
    /// Declared parameter count as read from `MethodInfo +0x52`.
    pub parameters: u8,
}

// ---------------------------------------------------------------------------
// Pure helpers (host-testable, no game memory)
// ---------------------------------------------------------------------------

/// Photon passes the operation code as a `byte`; only the low byte of the
/// register matters (the rest is unspecified after the IL2CPP prologue).
pub fn op_code(raw: u64) -> u8 {
    raw as u8
}

/// Is this call `OpRaiseEvent` (253), i.e. an application-level event?
pub fn is_raise_event(raw: u64) -> bool {
    op_code(raw) == OP_RAISE_EVENT
}

/// Bounded logging: the first [`LOG_FIRST`] calls, then every [`LOG_EVERY`]th.
/// `SendOperation` runs per frame, so an unbounded log would kill the game.
pub fn should_log(hit: u64) -> bool {
    hit != 0 && (hit <= LOG_FIRST || hit.is_multiple_of(LOG_EVERY))
}

/// Split `Namespace.Class` at the last dot. `None` for an empty/blank value;
/// a bare name resolves to the empty namespace (the `find` wildcard).
pub fn parse_qualified(text: &str) -> Option<(&str, &str)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match text.rsplit_once('.') {
        Some((ns, name)) if !name.is_empty() => Some((ns.trim(), name)),
        Some(_) => None,
        None => Some(("", text)),
    }
}

/// One observation line for a non-253 operation.
pub fn hit_line(hit: u64, tid: u64, op: u64, params: u64, send_options: u64) -> String {
    format!(
        "hit={hit} tid={tid:#x} op={} params=0x{params:x} send_opts=0x{send_options:x}",
        op_code(op)
    )
}

/// One observation line for a 253 RaiseEvent, with the decoded parts when the
/// dictionary layout is known (`None` until [`decode_event_params`] lands).
pub fn raise_line(hit: u64, tid: u64, params: u64, decoded: Option<(u8, usize)>) -> String {
    match decoded {
        Some((code, len)) => format!(
            "op=253 hit={hit} tid={tid:#x} params_ptr=0x{params:x} event={code} payload_len={len}"
        ),
        None => format!("op=253 hit={hit} tid={tid:#x} params_ptr=0x{params:x}"),
    }
}

/// Decode the `Dictionary<byte, object>` argument of a 253 RaiseEvent into
/// `(event_code, payload_len, payload_ptr)`: slot [`PARAM_EVENT_CODE`] gives the
/// code, slot [`PARAM_EVENT_DATA`] the `byte[]` payload.
///
/// TODO(photon step 2): the il2cpp `Dictionary<byte, object>` layout has not been
/// measured on this build yet, so this is a stub and returns `None` (the hook
/// then logs the raw pointer only). The measured path will be: entry array from
/// the dictionary header → `(hashCode, next, key, value)` records → the
/// `Il2CppArray` of the payload at `+0x20`, length at `+0x18`. Every read must go
/// through [`crate::safe::read_usize`]/[`crate::safe::readable_at`].
pub fn decode_event_params(params: usize) -> Option<(u8, usize, *const u8)> {
    let _ = params;
    None
}

/// Pure half of the pending decoder: turn the two dictionary lookups into
/// `(event_code, payload_len)`. `None` when either slot is missing or the code
/// does not fit the Photon `byte` it is sent as.
pub fn event_code_from_slots(code: Option<i64>, payload_len: Option<usize>) -> Option<(u8, usize)> {
    let code = u8::try_from(code?).ok()?;
    Some((code, payload_len?))
}

// ---------------------------------------------------------------------------
// Hook state
// ---------------------------------------------------------------------------

static SEND_ORIG: AtomicUsize = AtomicUsize::new(0);
static SEND_ENTRY: AtomicUsize = AtomicUsize::new(0);
static SEND_INFO: AtomicUsize = AtomicUsize::new(0);
static SEND_ATTACHED: AtomicBool = AtomicBool::new(false);
static SEND_HITS: AtomicU64 = AtomicU64::new(0);
static SEND_EVENTS: AtomicU64 = AtomicU64::new(0);

static EVENT_ORIG: AtomicUsize = AtomicUsize::new(0);
static EVENT_ENTRY: AtomicUsize = AtomicUsize::new(0);
static EVENT_ATTACHED: AtomicBool = AtomicBool::new(false);
static EVENT_HITS: AtomicU64 = AtomicU64::new(0);

static ENV_OUT: AtomicU8 = AtomicU8::new(0);
static ENV_IN: AtomicU8 = AtomicU8::new(0);
static POLLS: AtomicU64 = AtomicU64::new(0);

/// Attempt an install at most every this many polls (~10 s at the default tick).
const POLL_EVERY: u64 = 40;
/// Report a failing attempt only every this many attempts, so a slow Photon
/// bootstrap does not fill the log with four candidate lines every 10 s.
const POLL_LOG_EVERY: u64 = 6;

fn env_flag(name: &str, cache: &AtomicU8) -> bool {
    match cache.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = std::env::var(name)
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false);
            cache.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// `EAC_EVENTHOOK=1`: hook the outgoing `SendOperation`.
pub fn enabled_outgoing() -> bool {
    env_flag("EAC_EVENTHOOK", &ENV_OUT)
}

/// `EAC_EVENTHOOK_IN=1`: additionally hook the incoming `OnEvent`.
pub fn enabled_incoming() -> bool {
    env_flag("EAC_EVENTHOOK_IN", &ENV_IN)
}

/// Counters for a live status report: `(installed, hits, raise_events)`.
pub fn send_counters() -> (bool, u64, u64) {
    (
        SEND_ATTACHED.load(Ordering::Acquire),
        SEND_HITS.load(Ordering::Relaxed),
        SEND_EVENTS.load(Ordering::Relaxed),
    )
}

/// Counters of the incoming hook: `(installed, hits)`.
pub fn event_counters() -> (bool, u64) {
    (
        EVENT_ATTACHED.load(Ordering::Acquire),
        EVENT_HITS.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Pick a method out of `(class, methods)` chain snapshots. Pure: the chain is
/// already flattened by the caller (see [`resolve_chain`]).
///
/// Rules: the name must match one of `names` (earlier = preferred), the method
/// must be an instance method (`flags & 0x10 == 0`), and a match with the
/// preferred arity (`preferred`) wins over an earlier one without it.
fn pick_method<'a>(
    chain: &'a [(usize, Vec<crate::m3::Method>)],
    names: &[&str],
    preferred: Option<u8>,
) -> Option<(usize, &'a crate::m3::Method)> {
    for name in names {
        let mut fallback: Option<(usize, &crate::m3::Method)> = None;
        for (class, methods) in chain {
            for method in methods {
                if method.name != *name || method.flags & 0x10 != 0 || method.entry < 0x10000 {
                    continue;
                }
                if preferred.is_none_or(|want| method.parameters_count == want) {
                    return Some((*class, method));
                }
                // Right name, wrong arity: keep it unless a better name wins.
                fallback.get_or_insert((*class, method));
            }
        }
        if fallback.is_some() {
            return fallback;
        }
    }
    None
}

#[cfg(windows)]
fn resolve_chain(class: usize) -> Vec<(usize, Vec<crate::m3::Method>)> {
    crate::m3::chain(class)
        .into_iter()
        .map(|c| (c, crate::m3::methods(c)))
        .collect()
}

#[cfg(windows)]
fn resolve_named(names: &[&str], preferred: Option<u8>) -> Option<Resolved> {
    let module = crate::procs::own_module(crate::GAME_ASSEMBLY);
    let Some((base, size)) = module else {
        log::log_tag(TAG, "GameAssembly.dll is not loaded; nothing to resolve");
        return None;
    };
    let mut seen: Vec<usize> = Vec::new();
    for (ns, class_name) in PEER_CANDIDATES {
        let hits = crate::m3::find(ns, class_name, SCAN_LIMIT);
        if hits.is_empty() {
            log::log_tag(
                TAG,
                &format!("candidate ns={ns:?} class={class_name} absent from the class table"),
            );
            continue;
        }
        for (index, class) in hits {
            if seen.contains(&class) {
                continue;
            }
            seen.push(class);
            let chain = resolve_chain(class);
            let Some((_, method)) = pick_method(&chain, names, preferred) else {
                log::log_tag(
                    TAG,
                    &format!(
                        "candidate {} (index {index}, {class:#x}) has none of {names:?}",
                        crate::m3::class_full_name(class)
                    ),
                );
                continue;
            };
            if method.entry < base || method.entry >= base.saturating_add(size) {
                log::log_tag(
                    TAG,
                    &format!(
                        "candidate {} method={} entry {:#x} is outside GameAssembly {base:#x}..{:#x}",
                        crate::m3::class_full_name(class),
                        method.name,
                        method.entry,
                        base.saturating_add(size)
                    ),
                );
                continue;
            }
            return Some(Resolved {
                class,
                info: method.info,
                entry: method.entry,
                parameters: method.parameters_count,
            });
        }
    }
    None
}

/// Resolve `PhotonPeer.SendOperation` (or `LoadBalancingPeer`, or the
/// `SendOperationInternal`/`OpCustom` fallback) without installing anything.
///
/// `None` whenever the class, the method or GameAssembly itself is missing —
/// this is a probe, not a guarantee.
#[cfg(windows)]
pub fn resolve_send_operation() -> Option<Resolved> {
    resolve_named(SEND_METHODS, Some(SEND_PARAMETERS))
}

/// Resolve the incoming `OnEvent` (see [`EVENT_METHODS`]).
#[cfg(windows)]
pub fn resolve_on_event() -> Option<Resolved> {
    resolve_named(EVENT_METHODS, Some(1))
}

/// Host build: there is no GameAssembly, so resolution must report `None`
/// instead of touching game memory.
#[cfg(not(windows))]
pub fn resolve_send_operation() -> Option<Resolved> {
    None
}

/// Host build counterpart of [`resolve_on_event`].
#[cfg(not(windows))]
pub fn resolve_on_event() -> Option<Resolved> {
    None
}

// ---------------------------------------------------------------------------
// Detours (observation only; every argument is forwarded unchanged)
// ---------------------------------------------------------------------------

/// Outgoing detour for `SendOperation(byte, Dictionary<byte,object>, SendOptions)`.
///
/// # Safety
/// Called from the patched managed method with the IL2CPP x64 ABI: `this` in
/// `rcx`, the operation code in `rdx`, the parameter dictionary in `r8`, the
/// `SendOptions` struct in `r9`. All four are forwarded to the trampoline
/// unchanged; nothing is dereferenced unless a guarded reader accepted it.
pub unsafe extern "system" fn send_operation_detour(
    this: u64,
    op: u64,
    params: u64,
    send_options: u64,
) -> u64 {
    let hit = SEND_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(hit) {
        let tid = u64::from(crate::gcthreads::own_tid());
        if is_raise_event(op) {
            SEND_EVENTS.fetch_add(1, Ordering::Relaxed);
            let decoded = decode_event_params(params as usize).map(|(code, len, _)| (code, len));
            log::log_tag(TAG, &raise_line(hit, tid, params, decoded));
        } else {
            log::log_tag(TAG, &hit_line(hit, tid, op, params, send_options));
        }
    }
    let original = SEND_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: install_outgoing publishes the trampoline of this exact method,
    // whose ABI is the four-integer shape declared here, before patching it.
    let f: unsafe extern "system" fn(u64, u64, u64, u64) -> u64 =
        unsafe { core::mem::transmute(original) };
    // SAFETY: all four arguments come from the managed caller and are forwarded
    // bit-for-bit; the extra hidden `MethodInfo*` stack slot is untouched.
    unsafe { f(this, op, params, send_options) }
}

/// Incoming detour for `OnEvent(EventData)`.
///
/// # Safety
/// Same contract as [`send_operation_detour`], with `this` in `rcx` and the
/// `EventData` object in `rdx`.
pub unsafe extern "system" fn on_event_detour(this: u64, event_data: u64) {
    let hit = EVENT_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log(hit) {
        let tid = u64::from(crate::gcthreads::own_tid());
        log::log_tag(
            TAG,
            &format!("in hit={hit} tid={tid:#x} event_data=0x{event_data:x}"),
        );
    }
    let original = EVENT_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: the trampoline belongs to the two-integer `OnEvent` selected by
    // install_incoming and was published before the patch.
    let f: unsafe extern "system" fn(u64, u64) = unsafe { core::mem::transmute(original) };
    // SAFETY: both arguments are forwarded unchanged.
    unsafe { f(this, event_data) }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// Name of the method we actually hooked (the first entry of `names` that the
/// resolved class really declares), for the log line.
#[cfg(windows)]
fn method_name(class: usize, entry: usize, names: &'static [&'static str]) -> &'static str {
    for c in crate::m3::chain(class) {
        for m in crate::m3::methods(c) {
            if m.entry == entry
                && let Some(hit) = names.iter().find(|name| **name == m.name)
            {
                return hit;
            }
        }
    }
    names[0]
}

#[cfg(windows)]
fn hook_line(tag: &str, resolved: &Resolved, name: &str, rva: usize) -> String {
    format!(
        "hooked {} method={name} MethodInfo={:#x} entry={:#x} (GA+{rva:#x}) params={} kind={tag}",
        crate::m3::class_full_name(resolved.class),
        resolved.info,
        resolved.entry,
        resolved.parameters
    )
}

/// Resolve and hook `PhotonPeer.SendOperation`. Idempotent: a second call for
/// the same entry returns the cached resolution without touching the code.
#[cfg(windows)]
pub fn install_outgoing() -> Result<Resolved, String> {
    let resolved = resolve_send_operation().ok_or_else(|| {
        format!(
            "no {:?} found on {:?}",
            SEND_METHODS,
            PEER_CANDIDATES.iter().map(|(_, c)| c).collect::<Vec<_>>()
        )
    })?;
    if SEND_ATTACHED.load(Ordering::Acquire) && SEND_ENTRY.load(Ordering::Acquire) == resolved.entry
    {
        return Ok(resolved);
    }
    let (base, _) = crate::procs::own_module(crate::GAME_ASSEMBLY)
        .ok_or_else(|| format!("{} is not loaded", crate::GAME_ASSEMBLY))?;
    let prep = crate::raw::hook::prepare(resolved.entry)
        .ok_or_else(|| format!("cannot prepare a trampoline at {:#x}", resolved.entry))?;
    // Publish the trampoline *before* the patch: a thread already inside the
    // prologue may reach the detour immediately.
    SEND_ORIG.store(prep.trampoline, Ordering::Release);
    SEND_ENTRY.store(resolved.entry, Ordering::Release);
    SEND_INFO.store(resolved.info, Ordering::Release);
    let Some(hook) = crate::raw::hook::patch(&prep, send_operation_detour as *const () as usize)
    else {
        SEND_ORIG.store(0, Ordering::Release);
        SEND_ENTRY.store(0, Ordering::Release);
        return Err(format!(
            "patch failed at {:#x} (prologue, atomic window or page protection)",
            resolved.entry
        ));
    };
    SEND_ATTACHED.store(true, Ordering::Release);
    crate::raw::hook::register(hook);
    let name = method_name(resolved.class, resolved.entry, SEND_METHODS);
    log::log_tag(
        TAG,
        &hook_line("send", &resolved, name, resolved.entry.saturating_sub(base)),
    );
    Ok(resolved)
}

/// Resolve and hook the incoming `OnEvent` (`EAC_EVENTHOOK_IN=1`).
#[cfg(windows)]
pub fn install_incoming() -> Result<Resolved, String> {
    let resolved = resolve_on_event()
        .ok_or_else(|| format!("no {:?} found on a Photon peer class", EVENT_METHODS))?;
    if EVENT_ATTACHED.load(Ordering::Acquire) && EVENT_ENTRY.load(Ordering::Acquire) == resolved.entry
    {
        return Ok(resolved);
    }
    let (base, _) = crate::procs::own_module(crate::GAME_ASSEMBLY)
        .ok_or_else(|| format!("{} is not loaded", crate::GAME_ASSEMBLY))?;
    let prep = crate::raw::hook::prepare(resolved.entry)
        .ok_or_else(|| format!("cannot prepare a trampoline at {:#x}", resolved.entry))?;
    EVENT_ORIG.store(prep.trampoline, Ordering::Release);
    EVENT_ENTRY.store(resolved.entry, Ordering::Release);
    let Some(hook) = crate::raw::hook::patch(&prep, on_event_detour as *const () as usize) else {
        EVENT_ORIG.store(0, Ordering::Release);
        EVENT_ENTRY.store(0, Ordering::Release);
        return Err(format!(
            "patch failed at {:#x} (prologue, atomic window or page protection)",
            resolved.entry
        ));
    };
    EVENT_ATTACHED.store(true, Ordering::Release);
    crate::raw::hook::register(hook);
    let name = method_name(resolved.class, resolved.entry, EVENT_METHODS);
    log::log_tag(
        TAG,
        &hook_line("in", &resolved, name, resolved.entry.saturating_sub(base)),
    );
    Ok(resolved)
}

/// Host build: nothing can be hooked without GameAssembly.
#[cfg(not(windows))]
pub fn install_outgoing() -> Result<Resolved, String> {
    Err("host build: GameAssembly.dll is unavailable".into())
}

/// Host build counterpart of [`install_incoming`].
#[cfg(not(windows))]
pub fn install_incoming() -> Result<Resolved, String> {
    Err("host build: GameAssembly.dll is unavailable".into())
}

// ---------------------------------------------------------------------------
// Lazy install from the managed tick + env wiring
// ---------------------------------------------------------------------------

/// Called from the established managed tick: retries the install until it lands.
/// Cheap when everything is already attached; `EAC_EVENTHOOK=1` gates it.
pub fn poll() {
    let want_out = enabled_outgoing();
    let want_in = enabled_incoming();
    if !want_out && !want_in {
        return;
    }
    if !POLLS.fetch_add(1, Ordering::Relaxed).is_multiple_of(POLL_EVERY) {
        return;
    }
    let attempts = POLLS.load(Ordering::Relaxed) / POLL_EVERY;
    let loud = attempts.is_multiple_of(POLL_LOG_EVERY);
    if want_out && !SEND_ATTACHED.load(Ordering::Acquire) {
        match install_outgoing() {
            Ok(target) => log::log_tag(
                TAG,
                &format!(
                    "send hook ready: MethodInfo={:#x} entry={:#x} params={}",
                    target.info, target.entry, target.parameters
                ),
            ),
            Err(reason) if loud => {
                log::log_tag(TAG, &format!("send hook pending (attempt {attempts}): {reason}"))
            }
            Err(_) => {}
        }
    }
    if want_in && !EVENT_ATTACHED.load(Ordering::Acquire) {
        match install_incoming() {
            Ok(target) => log::log_tag(
                TAG,
                &format!("incoming hook ready: entry={:#x}", target.entry),
            ),
            Err(reason) if loud => log::log_tag(
                TAG,
                &format!("incoming hook pending (attempt {attempts}): {reason}"),
            ),
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        decode_event_params, event_code_from_slots, hit_line, is_raise_event, op_code,
        parse_qualified, pick_method, raise_line, should_log, EVENT_EAC_HEARTBEAT,
        OP_RAISE_EVENT, PARAM_EVENT_CODE,
    };

    /// Plausible managed code address floor used by the picker (`entry >= 0x10000`).
    const GA: usize = 0x7f00_0000_0000;

    fn method(name: &str, entry: usize, parameters: u8, flags: u16) -> crate::m3::Method {
        crate::m3::Method {
            name: name.to_string(),
            entry,
            info: 0x1000,
            return_kind: 1,
            parameters_count: parameters,
            flags,
        }
    }

    #[test]
    fn op_code_uses_the_low_byte_only() {
        assert_eq!(op_code(253), OP_RAISE_EVENT);
        assert!(is_raise_event(253));
        // IL2CPP leaves the upper bits of a `byte` argument unspecified.
        assert!(is_raise_event(0xdead_beef_0000_00fd));
        assert!(!is_raise_event(252));
        assert!(!is_raise_event(0x1_00));
    }

    #[test]
    fn logging_is_bounded() {
        assert!(should_log(1));
        assert!(should_log(50));
        assert!(!should_log(51));
        assert!(should_log(200));
        assert!(!should_log(201));
        assert!(should_log(400));
        assert!(!should_log(0));
    }

    #[test]
    fn qualified_names_split_at_the_last_dot() {
        assert_eq!(
            parse_qualified("ExitGames.Client.Photon.PhotonPeer"),
            Some(("ExitGames.Client.Photon", "PhotonPeer"))
        );
        assert_eq!(parse_qualified("PhotonPeer"), Some(("", "PhotonPeer")));
        assert_eq!(parse_qualified("  PhotonPeer  "), Some(("", "PhotonPeer")));
        assert_eq!(parse_qualified("  "), None);
        assert_eq!(parse_qualified("Photon."), None);
    }

    #[test]
    fn hit_line_names_the_operation_and_keeps_the_raw_pointers() {
        let line = hit_line(7, 0x1234, 2, 0xdead_beef, 0x40);
        assert!(line.starts_with("hit=7 tid=0x1234 op=2 "), "{line}");
        assert!(line.contains("params=0xdeadbeef"), "{line}");
        assert!(line.contains("send_opts=0x40"), "{line}");
    }

    #[test]
    fn raise_line_reports_253_even_before_the_decoder_exists() {
        let line = raise_line(3, 0x99, 0x7fff_0000_1a30, None);
        assert!(line.contains("op=253"), "{line}");
        assert!(line.contains("params_ptr=0x7fff00001a30"), "{line}");
        assert!(!line.contains("event="), "{line}");
        let decoded = raise_line(3, 0x99, 0x1000, Some((EVENT_EAC_HEARTBEAT, 512)));
        assert!(decoded.contains("event=66"), "{decoded}");
        assert!(decoded.contains("payload_len=512"), "{decoded}");
    }

    #[test]
    fn decoder_is_a_stub_for_now() {
        assert_eq!(decode_event_params(0x1000), None);
    }

    #[test]
    fn event_slots_need_the_code_and_the_payload() {
        assert_eq!(event_code_from_slots(Some(66), Some(512)), Some((66, 512)));
        assert_eq!(event_code_from_slots(Some(66), None), None);
        assert_eq!(event_code_from_slots(None, Some(4)), None);
        // 244/245 are the dictionary keys; the code itself is a byte.
        assert_eq!(event_code_from_slots(Some(300), Some(4)), None);
        assert_eq!(
            event_code_from_slots(Some(i64::from(PARAM_EVENT_CODE) - 1), Some(0)),
            Some((243, 0))
        );
    }

    #[test]
    fn preferred_arity_beats_a_static_or_short_overload() {
        let chain = vec![
            (0x1000usize, vec![
                method("SendOperation", GA + 0x2000, 4, 0x10), // static: rejected
                method("SendOperation", GA + 0x3000, 2, 0),
                method("SendOperation", GA + 0x4000, 3, 0), // preferred arity
            ]),
            (0x1800, vec![method("SendOperation", GA + 0x5000, 1, 0)]),
        ];
        let (class, picked) = pick_method(&chain, &["SendOperation"], Some(3)).unwrap();
        assert_eq!(class, 0x1000);
        assert_eq!(picked.entry, GA + 0x4000);
    }

    #[test]
    fn pick_method_falls_back_and_respects_the_name_order() {
        let chain = vec![
            (0x1000usize, vec![method("SendOperation", GA + 0x2000, 2, 0)]),
            (0x1800, vec![method("SendOperationInternal", GA + 0x3000, 2, 0)]),
        ];
        let (_, picked) = pick_method(&chain, &["SendOperation", "SendOperationInternal"], Some(3))
            .unwrap();
        assert_eq!(picked.name, "SendOperation");
        assert_eq!(picked.entry, GA + 0x2000);
        assert!(pick_method(&chain, &["OnEvent"], Some(1)).is_none());
        // A method with an unrelocated entry (0) is not a hook target.
        let zero = vec![(0x1000usize, vec![method("OnEvent", 0, 1, 0)])];
        assert!(pick_method(&zero, &["OnEvent"], Some(1)).is_none());
    }

    #[test]
    fn host_resolution_reports_none_without_game_assembly() {
        // The host build has no GameAssembly.dll at all; the resolver must say
        // `None` rather than dereference anything.
        assert!(super::resolve_send_operation().is_none());
        assert!(super::resolve_on_event().is_none());
        assert_eq!(super::send_counters(), (false, 0, 0));
        assert_eq!(super::event_counters(), (false, 0));
    }
}