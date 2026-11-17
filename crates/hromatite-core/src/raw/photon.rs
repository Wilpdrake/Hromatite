//! Managed Photon event surface: `PhotonPeer.SendOperation` / `PhotonPeer.OnEvent`.
//!
//! Photon is the transport VRChat uses for everything above the EAC session:
//! *outgoing* operations are `SendOperation(byte operationCode, Dictionary<byte,
//! object> parameters, SendOptions sendOptions)`, so a RaiseEvent (opCode 253)
//! carries the event code in dictionary slot 244 and the payload in slot 245
//! (66 = the EAC heartbeat event). *Incoming* events arrive through
//! `OnEvent(EventData)`.
//!
//! The module does three things, in the order they became possible:
//!
//! 1. **observe** — resolve the managed method through [`crate::m3`] (no il2cpp
//!    API calls, so it is safe from the game's own thread), install one inline
//!    hook with the shared [`crate::raw::hook`] engine, forward every call to
//!    the original untouched;
//! 2. **decode** — read the `Dictionary<byte, object>` argument of a 253
//!    RaiseEvent ([`decode_event_params`]): slot 244 is the event code, slot 245
//!    the `byte[]` payload, so the EAC heartbeat event (66) becomes readable and
//!    comparable with the frame the EOSSDK hook sees;
//! 3. **emulate** — once one live event 66 has been seen, its dictionary and
//!    `byte[]` are remembered as a [`Template`]; [`emulate_send`] overwrites the
//!    bytes of that array and calls the original `SendOperation` itself, and
//!    [`tick`] drives the cadence from the managed tick. This is the transport
//!    that does **not** go through the EOSSDK module at all.
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

// Host builds have no GameAssembly, so resolution, the hooks and the emulation
// are all `cfg(windows)` and everything they share looks unused there.
#![cfg_attr(not(windows), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::sync::Mutex;

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

// ---------------------------------------------------------------------------
// Dictionary<byte, object> decoding
// ---------------------------------------------------------------------------

/// Managed collection the RaiseEvent parameters live in.
pub const DICT_NS: &str = "System.Collections.Generic";
/// Illegal-but-real il2cpp name of the generic type definition instance.
pub const DICT_NAME: &str = "Dictionary`2";
/// `count` field names, Mono style first, then the .NET Core style. The *offset*
/// is always read out of `m3::fields` — only the name is guessed here.
pub const DICT_COUNT_FIELDS: &[&str] = &["count", "_count"];
/// `entries` array field names (see [`DICT_COUNT_FIELDS`]).
pub const DICT_ENTRIES_FIELDS: &[&str] = &["entries", "_entries"];

/// `Il2CppArray` layout, stable across il2cpp versions:
/// `{ Il2CppObject obj; Il2CppArrayBounds* bounds; int32_t max_length; data }`.
pub const ARRAY_LEN_OFF: usize = 0x18;
/// First element of an `Il2CppArray`.
pub const ARRAY_DATA_OFF: usize = 0x20;

/// Boxed primitive layout: `{ Il2CppClass* klass; void* monitor; value }`.
pub const BOX_VALUE_OFF: usize = 0x10;

/// `Entry` record candidates as `(stride, key_off, value_off)`.
///
/// The real one is chosen **by data**, not by documentation: a candidate is
/// accepted only when the same entries array yields both [`PARAM_EVENT_CODE`]
/// and [`PARAM_EVENT_DATA`] as keys. The first entry is the measured
/// `{ int hashCode; int next; byte key; pad; object value }` shape
/// (stride 0x18, value 8-byte aligned at 0x10).
pub const ENTRY_LAYOUTS: &[(usize, usize, usize)] =
    &[(0x18, 0x08, 0x10), (0x20, 0x08, 0x10), (0x10, 0x08, 0x0C)];

/// Upper bound of a plausible dictionary / payload on the raise path.
const MAX_SLOTS: usize = 64;
const MAX_PAYLOAD: usize = 64 * 1024;

/// One resolved `Dictionary<byte, object>` layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    /// Offset of the `count` field inside the dictionary object.
    pub count: usize,
    /// Offset of the `entries` (**`Il2CppArray` pointer**) field.
    pub entries: usize,
    /// Size of one `Entry` record.
    pub stride: usize,
    /// Offset of the key inside an `Entry`.
    pub key: usize,
    /// Offset of the value pointer inside an `Entry`.
    pub value: usize,
}

/// The two slots this crate cares about, already dereferenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slots {
    /// Event code from slot 244 (`None` when the slot is absent/unboxable).
    pub code: Option<u8>,
    /// `(payload_ptr, payload_len)` from slot 245.
    pub payload: Option<(usize, usize)>,
}

/// Minimal memory view, so the decoder is host-testable without a game.
pub trait Mem {
    /// Copy `len` bytes at `addr`; `None` when any part is unreadable.
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>>;
}

/// Production reader: every access goes through the guarded `safe` facade, so a
/// racing GC that freed the dictionary yields `None` instead of an AV.
#[cfg(windows)]
pub struct Game;

#[cfg(windows)]
impl Mem for Game {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        crate::safe::read_bytes(addr, len)
    }
}

/// Pick `(count_off, entries_off)` out of a flattened field list.
///
/// Both names are tried in order; the first present wins. `None` when the class
/// is not a dictionary shape at all (e.g. another `Dictionary`2` generic
/// instance whose fields were optimised differently).
pub fn dictionary_offsets(fields: &[(String, usize)]) -> Option<(usize, usize)> {
    let pick = |names: &[&str]| {
        fields
            .iter()
            .find(|(name, _)| names.iter().any(|want| name == want))
            .map(|(_, off)| *off)
    };
    Some((pick(DICT_COUNT_FIELDS)?, pick(DICT_ENTRIES_FIELDS)?))
}

fn read_word<M: Mem + ?Sized>(mem: &M, addr: usize) -> Option<usize> {
    let bytes = mem.read(addr, 8)?;
    Some(usize::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32<M: Mem + ?Sized>(mem: &M, addr: usize) -> Option<u32> {
    let bytes = mem.read(addr, 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Value pointer stored under `key` in the entries array, or `None`.
fn slot<M: Mem + ?Sized>(
    mem: &M,
    entries: usize,
    count: usize,
    layout: Layout,
    key: u8,
) -> Option<usize> {
    let data = entries.checked_add(ARRAY_DATA_OFF)?;
    for index in 0..count {
        let record = data.checked_add(index.checked_mul(layout.stride)?)?;
        let Some(bytes) = mem.read(record, layout.stride) else {
            continue;
        };
        if bytes.get(layout.key).copied() != Some(key) {
            continue;
        }
        let end = layout.value.checked_add(8)?;
        let word = bytes.get(layout.value..end)?;
        let value = usize::from_le_bytes(word.try_into().ok()?);
        if value >= 0x1_0000 {
            return Some(value);
        }
    }
    None
}

/// The event code, which Photon stores as a **boxed** `byte`.
fn boxed_code<M: Mem + ?Sized>(mem: &M, value: usize) -> Option<u8> {
    let bytes = mem.read(value.checked_add(BOX_VALUE_OFF)?, 4)?;
    let raw = u32::from_le_bytes(bytes.try_into().ok()?);
    let code = (raw & 0xff) as u8;
    // A zero box means we are reading the wrong word (padding, not the payload);
    // a code above 127 is not a Photon event either.
    (code != 0).then_some(code)
}

/// Decode slots 244/245 out of a dictionary object using a **known** layout.
///
/// Returns `None` only when the dictionary itself is unreadable — a readable
/// dictionary with neither slot still yields `Slots { code: None, payload: None }`,
/// which is what the layout probe needs to see.
pub fn decode_slots<M: Mem + ?Sized>(mem: &M, dict: usize, layout: Layout) -> Option<Slots> {
    if dict < 0x1_0000 {
        return None;
    }
    let count = read_u32(mem, dict.checked_add(layout.count)?)? as usize;
    let entries = read_word(mem, dict.checked_add(layout.entries)?)?;
    if entries < 0x1_0000 {
        return None;
    }
    let array_len = read_u32(mem, entries.checked_add(ARRAY_LEN_OFF)?).unwrap_or(0) as usize;
    // Never trust `count`: it is read from the dictionary header and a torn read
    // would walk the past-the-end of the entries array.
    let count = count.min(array_len).min(MAX_SLOTS);
    if count == 0 {
        return Some(Slots {
            code: None,
            payload: None,
        });
    }
    let code = slot(mem, entries, count, layout, PARAM_EVENT_CODE).and_then(|v| boxed_code(mem, v));
    let payload = slot(mem, entries, count, layout, PARAM_EVENT_DATA).and_then(|v| {
        let len = read_u32(mem, v.checked_add(ARRAY_LEN_OFF)?)? as usize;
        (len <= MAX_PAYLOAD && len > 0).then(|| (v + ARRAY_DATA_OFF, len))
    });
    Some(Slots { code, payload })
}

/// Does this layout find **both** slots? The probe's acceptance criterion.
pub fn layout_matches(slots: &Slots) -> bool {
    slots.code.is_some() && slots.payload.is_some()
}

/// Flatten `m3::fields` into `(name, offset)` pairs (the decoder's input).
#[cfg(windows)]
pub fn field_pairs(class: usize) -> Vec<(String, usize)> {
    crate::m3::fields(class)
        .into_iter()
        .map(|field| (field.name, field.offset_or_index as usize))
        .collect()
}

// Published layout: six atomics instead of a lock, because the probe runs on the
// game's own thread inside the detour and must never block.
static L_READY: AtomicBool = AtomicBool::new(false);
static L_COUNT: AtomicUsize = AtomicUsize::new(0);
static L_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static L_STRIDE: AtomicUsize = AtomicUsize::new(0);
static L_KEY: AtomicUsize = AtomicUsize::new(0);
static L_VALUE: AtomicUsize = AtomicUsize::new(0);

fn published() -> Option<Layout> {
    if !L_READY.load(Ordering::Acquire) {
        return None;
    }
    Some(Layout {
        count: L_COUNT.load(Ordering::Relaxed),
        entries: L_ENTRIES.load(Ordering::Relaxed),
        stride: L_STRIDE.load(Ordering::Relaxed),
        key: L_KEY.load(Ordering::Relaxed),
        value: L_VALUE.load(Ordering::Relaxed),
    })
}

fn publish(layout: Layout) {
    L_COUNT.store(layout.count, Ordering::Relaxed);
    L_ENTRIES.store(layout.entries, Ordering::Relaxed);
    L_STRIDE.store(layout.stride, Ordering::Relaxed);
    L_KEY.store(layout.key, Ordering::Relaxed);
    L_VALUE.store(layout.value, Ordering::Relaxed);
    L_READY.store(true, Ordering::Release);
}

fn unpublish() {
    L_READY.store(false, Ordering::Release);
}

/// Try every dictionary class found in the class table and every [`ENTRY_LAYOUTS`]
/// candidate until one of them actually yields both slots for **this** object.
#[cfg(windows)]
fn probe_layout(dict: usize) -> Option<Layout> {
    let mem = Game;
    let mut tried = 0usize;
    for (_, class) in crate::m3::find(DICT_NS, DICT_NAME, SCAN_LIMIT) {
        let Some((count, entries)) = dictionary_offsets(&field_pairs(class)) else {
            continue;
        };
        for &(stride, key, value) in ENTRY_LAYOUTS {
            let layout = Layout {
                count,
                entries,
                stride,
                key,
                value,
            };
            tried += 1;
            if let Some(slots) = decode_slots(&mem, dict, layout)
                && layout_matches(&slots)
            {
                publish(layout);
                log::log_tag(
                    TAG,
                    &format!(
                        "dictionary layout: count+{:#x} entries+{:#x} entry={:#x} key+{:#x} value+{:#x} (probe {tried})",
                        layout.count, layout.entries, layout.stride, layout.key, layout.value
                    ),
                );
                return Some(layout);
            }
        }
    }
    log::log_tag(
        TAG,
        &format!(
            "dictionary probe failed after {tried} layout(s) on {} hit(s) for {DICT_NAME}",
            crate::m3::find(DICT_NS, DICT_NAME, SCAN_LIMIT).len()
        ),
    );
    None
}

/// Decode the `Dictionary<byte, object>` argument of a 253 RaiseEvent into
/// `(event_code, payload_len, payload_ptr)`: slot [`PARAM_EVENT_CODE`] gives the
/// code, slot [`PARAM_EVENT_DATA`] the `byte[]` payload.
///
/// The layout is probed once and cached; a failed decode with a cached layout
/// invalidates it, so a GC that moved a *different* dictionary shape into place
/// costs one re-probe instead of a permanent silent `None`.
#[cfg(windows)]
pub fn decode_event_params(params: usize) -> Option<(u8, usize, *const u8)> {
    let mem = Game;
    if params < 0x1_0000 {
        return None;
    }
    let layout = match published() {
        Some(layout) => {
            if decode_slots(&mem, params, layout).is_some() {
                layout
            } else {
                unpublish();
                probe_layout(params)?
            }
        }
        None => probe_layout(params)?,
    };
    let slots = decode_slots(&mem, params, layout)?;
    Some((slots.code?, slots.payload?.1, slots.payload?.0 as *const u8))
}

/// Host build: no game memory, so the detour can never decode anything.
#[cfg(not(windows))]
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
// Event capture (in-memory ring, drained by the tick)
// ---------------------------------------------------------------------------

/// A 512-byte EAC frame, the unit the whole chain speaks in.
pub const EAC_FRAME_LEN: usize = 512;
/// Offset of the frame checksum inside the 24-byte EAC header.
pub const EAC_CHECKSUM_OFF: usize = 20;
/// Zero-based `SessionId` field of the EAC header.
const EAC_SESSION_OFF: usize = 4;
/// Zero-based `Sequence` field of the EAC header.
const EAC_SEQUENCE_OFF: usize = 12;

/// Ring capacity: ~17 minutes of a 2 s heartbeat, and a full 144-frame report.
const EVENT_RING_MAX: usize = 256;

/// One observed RaiseEvent, payload already copied out of game memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Captured {
    /// Event code from slot 244.
    pub code: u8,
    /// Milliseconds since the previous captured event.
    pub dt_ms: u32,
    /// The `byte[]` from slot 245.
    pub data: Vec<u8>,
}

static RING: Mutex<VecDeque<Captured>> = Mutex::new(VecDeque::new());
static RING_DROPPED: AtomicU64 = AtomicU64::new(0);
static LAST_EVENT_MS: AtomicU64 = AtomicU64::new(0);

/// Take everything captured since the last drain.
pub fn drain_events() -> Vec<Captured> {
    let mut ring = RING.lock().unwrap_or_else(|poison| poison.into_inner());
    ring.drain(..).collect()
}

/// Events that did not fit in the ring.
pub fn events_dropped() -> u64 {
    RING_DROPPED.load(Ordering::Relaxed)
}

/// Called from the detour; copies the payload only when somebody asked for it.
fn record_event(code: u8, data: Vec<u8>) {
    let now = now_ms();
    let last = LAST_EVENT_MS.swap(now, Ordering::Relaxed);
    let dt_ms = now.saturating_sub(last).min(u64::from(u32::MAX)) as u32;
    let mut ring = RING.lock().unwrap_or_else(|poison| poison.into_inner());
    if ring.len() >= EVENT_RING_MAX {
        ring.pop_front();
        RING_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    ring.push_back(Captured { code, dt_ms, data });
}

// ---------------------------------------------------------------------------
// Emulation: our own RaiseEvent(253, {244:66, 245:<our bytes>})
// ---------------------------------------------------------------------------

/// The live objects a RaiseEvent needs, remembered from the first event 66 the
/// game itself raised: we reuse its `PhotonPeer`, its parameter dictionary and —
/// crucially — its `byte[]` (a managed allocation we cannot make ourselves
/// without calling il2cpp, so we write into the existing array instead).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Template {
    /// `this` (`PhotonPeer` instance) of the captured call.
    pub this: usize,
    /// The `Dictionary<byte, object>` argument.
    pub dict: usize,
    /// The `SendOptions` argument, bit for bit as the game passed it.
    pub opts: u64,
    /// First byte of the payload `Il2CppArray`.
    pub payload: usize,
    /// `max_length` of that array — the most bytes we can put into it.
    pub capacity: usize,
}

static TEMPLATE: Mutex<Option<Template>> = Mutex::new(None);

/// The captured template, if a live event 66 has been seen.
pub fn template() -> Option<Template> {
    *TEMPLATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn remember_template(template: Template) {
    let mut slot = TEMPLATE.lock().unwrap_or_else(|poison| poison.into_inner());
    if slot.is_none() {
        *slot = Some(template);
    }
}

/// Drop the template (a new session has new managed objects).
pub fn forget_template() {
    *TEMPLATE.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
}

/// Send `payload` as `RaiseEvent(253, {244: 66, 245: payload})` ourselves.
///
/// The bytes are written into the captured managed `byte[]` and the original
/// `SendOperation` is then called through its trampoline, so the call looks like
/// any other Photon operation of the game — no EOSSDK, no EAC module.
///
/// Returns the original's return value (`bool` success, as an integer).
#[cfg(windows)]
pub fn emulate_send(payload: &[u8]) -> Result<usize, String> {
    let template = template().ok_or_else(|| "no live event 66 template yet".to_string())?;
    if payload.is_empty() {
        return Err("empty payload".into());
    }
    if payload.len() > template.capacity {
        return Err(format!(
            "payload {} B does not fit the live byte[] ({} B)",
            payload.len(),
            template.capacity
        ));
    }
    let original = SEND_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return Err("SendOperation is not hooked (EAC_EVENTHOOK=1)".into());
    }
    if !crate::safe::write_bytes(template.payload, payload) {
        return Err(format!(
            "cannot write {} B into the live byte[] at {:#x}",
            payload.len(),
            template.payload
        ));
    }
    // SAFETY: `install_outgoing` published this trampoline for the exact method
    // signature below before patching the entry, and all four arguments come from
    // the captured live call (this/dict/opts) or from the constants above.
    let f: unsafe extern "system" fn(u64, u64, u64, u64) -> u64 =
        unsafe { core::mem::transmute(original) };
    // SAFETY: forwarding the captured `this`, the RaiseEvent opcode, the captured
    // dictionary and the captured `SendOptions` word; no pointer is fabricated.
    let result = unsafe {
        f(
            template.this as u64,
            u64::from(OP_RAISE_EVENT),
            template.dict as u64,
            template.opts,
        )
    };
    Ok(result as usize)
}

/// Host build: nothing managed to call.
#[cfg(not(windows))]
pub fn emulate_send(payload: &[u8]) -> Result<usize, String> {
    let _ = payload;
    Err("host build: no managed SendOperation".into())
}

/// Env knob set: `EAC_EVENT_SEND=<path>` — the payload stream to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendConfig {
    /// File holding the payloads (`u32 len | u32 dt | bytes` records, or one blob).
    pub path: std::path::PathBuf,
    /// `EAC_EVENT_SEND_MS`, default [`DEFAULT_SEND_MS`].
    pub interval_ms: u64,
    /// `EAC_EVENT_SEND_START`: first payload index.
    pub start: usize,
    /// `EAC_EVENT_SEND_LOOP` (default on): restart when the stream ends.
    pub looping: bool,
    /// `EAC_EVENT_SEND_RETARGET` (default on): rewrite `SessionId`/`Sequence`/CRC.
    pub retarget: bool,
    /// `EAC_EVENT_SEND_SID`: session id to retarget to; `None` keeps the recorded one.
    pub session: Option<u64>,
    /// `EAC_EVENT_SEND_SEQ`: first `Sequence` of our stream.
    pub seq: u64,
}

/// Default cadence: the measured heartbeat interval is 2007 ms (median over 121
/// live frames), the nominal one 2000 ms.
pub const DEFAULT_SEND_MS: u64 = 2_000;

impl SendConfig {
    /// Parse the environment. `None` when `EAC_EVENT_SEND` is absent or blank.
    /// Takes a lookup so it is testable without the process environment.
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let path = get("EAC_EVENT_SEND")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let flag = |name: &str, default: bool| match get(name) {
            Some(value) => {
                let value = value.trim();
                !(value.is_empty() || value == "0")
            }
            None => default,
        };
        let number = |name: &str, default: u64| {
            get(name)
                .and_then(|value| {
                    let value = value.trim();
                    value
                        .strip_prefix("0x")
                        .map(|hex| u64::from_str_radix(hex, 16).ok())
                        .unwrap_or_else(|| value.parse::<u64>().ok())
                })
                .unwrap_or(default)
        };
        Some(Self {
            path: std::path::PathBuf::from(path),
            interval_ms: number("EAC_EVENT_SEND_MS", DEFAULT_SEND_MS).max(50),
            start: number("EAC_EVENT_SEND_START", 0) as usize,
            looping: flag("EAC_EVENT_SEND_LOOP", true),
            retarget: flag("EAC_EVENT_SEND_RETARGET", true),
            session: get("EAC_EVENT_SEND_SID").and_then(|value| {
                let value = value.trim();
                value
                    .strip_prefix("0x")
                    .map(|hex| u64::from_str_radix(hex, 16).ok())
                    .unwrap_or_else(|| value.parse::<u64>().ok())
            }),
            seq: number("EAC_EVENT_SEND_SEQ", 0),
        })
    }
}

/// Split a capture-format stream (`u32 len | u32 dt_ms | bytes`, the format of
/// [`crate::capture`] in `hromatite-eac`) into payloads.
///
/// `None` when the buffer is not a well-formed record stream — the caller then
/// treats the whole file as one payload, which is what a raw frame on disk is.
pub fn parse_payload_records(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
        if len == 0 || len > 1024 * 1024 {
            return None;
        }
        let start = at + 8;
        let end = start.checked_add(len)?;
        if end > bytes.len() {
            // A capture cut mid-write loses only its last record.
            break;
        }
        out.push(bytes[start..end].to_vec());
        at = end;
    }
    (!out.is_empty()).then_some(out)
}

/// Move a recorded frame into the current session: `SessionId` and `Sequence`
/// are rewritten and the CRC-32C is recomputed over the frame with the checksum
/// field zeroed. Anything that is not a 512-byte frame is returned unchanged.
pub fn retarget_frame(frame: &[u8], session: Option<u64>, seq: u64) -> Vec<u8> {
    let mut out = frame.to_vec();
    if out.len() != EAC_FRAME_LEN {
        return out;
    }
    if let Some(session) = session {
        out[EAC_SESSION_OFF..EAC_SESSION_OFF + 8].copy_from_slice(&session.to_le_bytes());
    }
    out[EAC_SEQUENCE_OFF..EAC_SEQUENCE_OFF + 8].copy_from_slice(&seq.to_le_bytes());
    crate::crc::seal_checksum(&mut out, EAC_CHECKSUM_OFF);
    out
}

/// Delivery state of [`tick`]. Only the game thread touches it.
struct SendState {
    parsed: bool,
    cfg: Option<SendConfig>,
    payloads: Vec<Vec<u8>>,
    cursor: usize,
    seq: u64,
    next_at_ms: u64,
    sent: u64,
    failures: u64,
}

impl SendState {
    const fn new() -> Self {
        Self {
            parsed: false,
            cfg: None,
            payloads: Vec::new(),
            cursor: 0,
            seq: 0,
            next_at_ms: 0,
            sent: 0,
            failures: 0,
        }
    }
}

static SEND: Mutex<SendState> = Mutex::new(SendState::new());
static WANT_PAYLOADS: AtomicU8 = AtomicU8::new(0);
static CAP_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Does anybody want the payload bytes? Both the capture file and the emitter
/// need them; without either, the detour only counts hits.
///
/// Cached after the first call: this runs on every RaiseEvent, and the
/// environment does not change under a running game.
fn wants_payloads() -> bool {
    match WANT_PAYLOADS.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let path = std::env::var("EAC_EVENT_CAP")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            let capture = path.is_some();
            *CAP_PATH.lock().unwrap_or_else(|poison| poison.into_inner()) = path;
            let want =
                capture || std::env::var("EAC_EVENT_SEND").is_ok_and(|value| !value.trim().is_empty());
            WANT_PAYLOADS.store(if want { 1 } else { 2 }, Ordering::Relaxed);
            want
        }
    }
}

/// Append captured events to `EAC_EVENT_CAP` in the capture format and log the
/// first few lines; nothing happens when the switch is off.
fn drain_capture(now: u64) {
    let path = CAP_PATH
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let Some(path) = path else {
        return;
    };
    let events = drain_events();
    if events.is_empty() {
        return;
    }
    let mut blob = Vec::new();
    for event in &events {
        blob.extend_from_slice(&(event.data.len() as u32).to_le_bytes());
        blob.extend_from_slice(&event.dt_ms.to_le_bytes());
        blob.extend_from_slice(&event.data);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            if let Err(err) = file.write_all(&blob) {
                log::log_tag(TAG, &format!("capture {path}: write failed: {err}"));
            }
        }
        Err(err) => log::log_tag(TAG, &format!("capture {path}: open failed: {err}")),
    }
    for event in events.iter().take(3) {
        let head: String = event
            .data
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        log::log_tag(
            TAG,
            &format!(
                "event {} len={} dt={} ms head={head} (t={} ms, dropped={})",
                event.code,
                event.data.len(),
                event.dt_ms,
                now,
                events_dropped()
            ),
        );
    }
    if events.len() > 3 {
        log::log_tag(TAG, &format!("captured {} events", events.len()));
    }
}

/// Load the payload stream once, then send one payload per `interval_ms`.
fn tick_send(now: u64) {
    let mut state = SEND.lock().unwrap_or_else(|poison| poison.into_inner());
    if !state.parsed {
        state.parsed = true;
        state.cfg = SendConfig::parse(|name| std::env::var(name).ok());
        if let Some(cfg) = state.cfg.clone() {
            match std::fs::read(&cfg.path) {
                Ok(bytes) => {
                    let raw = bytes.len();
                    state.payloads =
                        parse_payload_records(&bytes).unwrap_or_else(|| vec![bytes.clone()]);
                    state.cursor = cfg.start.min(state.payloads.len().saturating_sub(1));
                    state.seq = cfg.seq;
                    state.next_at_ms = now + cfg.interval_ms;
                    log::log_tag(
                        TAG,
                        &format!(
                            "send: {} payload(s) from {} ({} B), every {} ms, retarget={} loop={}",
                            state.payloads.len(),
                            cfg.path.display(),
                            raw,
                            cfg.interval_ms,
                            cfg.retarget,
                            cfg.looping
                        ),
                    );
                }
                Err(err) => log::log_tag(
                    TAG,
                    &format!("send: cannot read {}: {err}", cfg.path.display()),
                ),
            }
        }
    }
    let Some(cfg) = state.cfg.clone() else {
        return;
    };
    if state.payloads.is_empty() || now < state.next_at_ms {
        return;
    }
    if state.cursor >= state.payloads.len() {
        if !cfg.looping {
            return;
        }
        state.cursor = cfg.start.min(state.payloads.len() - 1);
    }
    let mut payload = state.payloads[state.cursor].clone();
    state.cursor += 1;
    let seq = state.seq;
    state.seq = state.seq.wrapping_add(1);
    state.next_at_ms = now + cfg.interval_ms;
    let sent_before = state.sent;
    let failures_before = state.failures;
    drop(state);

    if cfg.retarget {
        payload = retarget_frame(&payload, cfg.session, seq);
    }
    let result = emulate_send(&payload);

    let mut state = SEND.lock().unwrap_or_else(|poison| poison.into_inner());
    match result {
        Ok(value) => {
            state.sent = sent_before + 1;
            if should_log(state.sent) {
                log::log_tag(
                    TAG,
                    &format!(
                        "sent event 66: {} B seq={seq} ret={value:#x} ({} ok, {} failed)",
                        payload.len(),
                        state.sent,
                        state.failures
                    ),
                );
            }
        }
        Err(reason) => {
            state.failures = failures_before + 1;
            if should_log(state.failures) {
                let failures = state.failures;
                log::log_tag(
                    TAG,
                    &format!("send failed ({failures}): {reason}"),
                );
            }
        }
    }
}

/// Per-tick work of this module, called next to [`poll`] from the established
/// managed tick: retry the hook install, dump captured events, push our own
/// heartbeat. Inside a [`log::guard`] so a panic never unwinds into the game.
pub fn tick() {
    if !enabled_outgoing() && !enabled_incoming() {
        return;
    }
    let _ = log::guard("photon::tick", || {
        let now = now_ms();
        if wants_payloads() {
            drain_capture(now);
        }
        tick_send(now);
    });
}

fn now_ms() -> u64 {
    #[cfg(windows)]
    {
        // SAFETY: GetTickCount64 is a parameterless query with no pointer inputs.
        unsafe { crate::ffi::GetTickCount64() }
    }
    #[cfg(not(windows))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u64
    }
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
    // Nothing matched: say what the class table actually holds for this name, so
    // a renamed type (or a wrong table) is visible in one log line.
    let like = crate::m3::find_contains("Photon", SCAN_LIMIT, 8);
    log::log_tag(
        TAG,
        &format!("no peer class matched; Photon-like classes: {like:?}"),
    );
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

/// Side effects of a decoded RaiseEvent: remember the first event 66 as the
/// emulation [`Template`] and, when a sink asked for it, copy the payload.
#[cfg(windows)]
fn observe(this: u64, dict: u64, opts: u64, code: u8, len: usize, ptr: *const u8) {
    if code == EVENT_EAC_HEARTBEAT && len > 0 && len <= MAX_PAYLOAD {
        remember_template(Template {
            this: this as usize,
            dict: dict as usize,
            opts,
            payload: ptr as usize,
            capacity: len,
        });
    }
    if wants_payloads()
        && let Some(data) = crate::safe::read_bytes(ptr as usize, len)
    {
        record_event(code, data);
    }
}

/// Host build: the detour never decodes anything, so there is nothing to observe.
#[cfg(not(windows))]
fn observe(_this: u64, _dict: u64, _opts: u64, _code: u8, _len: usize, _ptr: *const u8) {}

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
    let raise = is_raise_event(op);
    if raise {
        SEND_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
    // Decode outside the log gate: the template and the capture ring need every
    // event 66, not only the sampled ones.
    let decoded = if raise {
        decode_event_params(params as usize)
    } else {
        None
    };
    if should_log(hit) {
        let tid = u64::from(crate::gcthreads::own_tid());
        match decoded {
            Some((code, len, _)) => log::log_tag(
                TAG,
                &raise_line(hit, tid, params, Some((code, len))),
            ),
            None if raise => log::log_tag(TAG, &raise_line(hit, tid, params, None)),
            None => log::log_tag(TAG, &hit_line(hit, tid, op, params, send_options)),
        }
    }
    if let Some((code, len, ptr)) = decoded {
        observe(this, params, send_options, code, len, ptr);
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
    if EVENT_ATTACHED.load(Ordering::Acquire)
        && EVENT_ENTRY.load(Ordering::Acquire) == resolved.entry
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
    if !POLLS
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(POLL_EVERY)
    {
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
            Err(reason) if loud => log::log_tag(
                TAG,
                &format!("send hook pending (attempt {attempts}): {reason}"),
            ),
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
        EVENT_EAC_HEARTBEAT, OP_RAISE_EVENT, PARAM_EVENT_CODE, decode_event_params,
        event_code_from_slots, hit_line, is_raise_event, op_code, parse_qualified, pick_method,
        raise_line, should_log,
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
    fn the_host_detour_cannot_decode_without_a_game() {
        assert_eq!(decode_event_params(0x1000), None);
        assert!(super::emulate_send(&[0u8; 512]).is_err());
        assert_eq!(super::template(), None);
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
            (
                0x1000usize,
                vec![
                    method("SendOperation", GA + 0x2000, 4, 0x10), // static: rejected
                    method("SendOperation", GA + 0x3000, 2, 0),
                    method("SendOperation", GA + 0x4000, 3, 0), // preferred arity
                ],
            ),
            (0x1800, vec![method("SendOperation", GA + 0x5000, 1, 0)]),
        ];
        let (class, picked) = pick_method(&chain, &["SendOperation"], Some(3)).unwrap();
        assert_eq!(class, 0x1000);
        assert_eq!(picked.entry, GA + 0x4000);
    }

    #[test]
    fn pick_method_falls_back_and_respects_the_name_order() {
        let chain = vec![
            (
                0x1000usize,
                vec![method("SendOperation", GA + 0x2000, 2, 0)],
            ),
            (
                0x1800,
                vec![method("SendOperationInternal", GA + 0x3000, 2, 0)],
            ),
        ];
        let (_, picked) =
            pick_method(&chain, &["SendOperation", "SendOperationInternal"], Some(3)).unwrap();
        assert_eq!(picked.name, "SendOperation");
        assert_eq!(picked.entry, GA + 0x2000);
        assert!(pick_method(&chain, &["OnEvent"], Some(1)).is_none());
        // A method with an unrelocated entry (0) is not a hook target.
        let zero = vec![(0x1000usize, vec![method("OnEvent", 0, 1, 0)])];
        assert!(pick_method(&zero, &["OnEvent"], Some(1)).is_none());
    }

    // -----------------------------------------------------------------
    // Dictionary decoding on a synthetic il2cpp heap
    // -----------------------------------------------------------------

    /// Address floor of the fake heap; everything below is "not a pointer".
    const BASE: usize = 0x1_0000_0000;
    const DICT: usize = BASE + 0x1000;
    const ENTRIES: usize = BASE + 0x2000;
    const CODE_BOX: usize = BASE + 0x3000;
    const PAYLOAD: usize = BASE + 0x4000;

    /// `{ int hashCode; int next; byte key; pad; object value }` — the measured
    /// `Dictionary`2.Entry` shape, which is also the first candidate layout.
    const LAYOUT: super::Layout = super::Layout {
        count: 0x20,
        entries: 0x18,
        stride: 0x18,
        key: 0x08,
        value: 0x10,
    };

    /// Read-only byte array standing in for the game's memory.
    struct Fake(Vec<u8>);

    impl Fake {
        fn new() -> Self {
            Self(vec![0u8; 0x8000])
        }

        fn put(&mut self, addr: usize, bytes: &[u8]) {
            let off = addr - BASE;
            self.0[off..off + bytes.len()].copy_from_slice(bytes);
        }

        /// One `Entry` record: only key and value matter to the decoder.
        fn entry(&mut self, index: usize, key: u8, value: usize, stride: usize, value_off: usize) {
            let record = ENTRIES + super::ARRAY_DATA_OFF + index * stride;
            self.put(record + LAYOUT.key, &[key]);
            self.put(record + value_off, &(value as u64).to_le_bytes());
        }

        /// A dictionary with slots 244/245 exactly like a live RaiseEvent, built
        /// with the given entry stride/value offset so layouts can be compared.
        fn dictionary(code: u8, payload_len: usize, stride: usize, value_off: usize) -> Self {
            let mut fake = Self::new();
            // Dictionary header: `entries` array pointer + `count`.
            fake.put(DICT + 0x18, &(ENTRIES as u64).to_le_bytes());
            fake.put(DICT + 0x20, &2u32.to_le_bytes());
            // The entries array itself: il2cpp header, then the records.
            fake.put(ENTRIES + super::ARRAY_LEN_OFF, &2u32.to_le_bytes());
            // Slot 244: the boxed event code.
            fake.put(CODE_BOX + super::BOX_VALUE_OFF, &[code, 0, 0, 0]);
            fake.entry(0, super::PARAM_EVENT_CODE, CODE_BOX, stride, value_off);
            // Slot 245: the payload `byte[]`.
            fake.put(
                PAYLOAD + super::ARRAY_LEN_OFF,
                &(payload_len as u32).to_le_bytes(),
            );
            fake.put(
                PAYLOAD + super::ARRAY_DATA_OFF,
                &vec![0xABu8; payload_len],
            );
            fake.entry(1, super::PARAM_EVENT_DATA, PAYLOAD, stride, value_off);
            fake
        }
    }

    impl super::Mem for Fake {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            let off = addr.checked_sub(BASE)?;
            self.0.get(off..off.checked_add(len)?).map(<[u8]>::to_vec)
        }
    }

    #[test]
    fn the_measured_entry_shape_yields_the_event_code_and_payload() {
        let mem = Fake::dictionary(66, 512, LAYOUT.stride, LAYOUT.value);
        let slots = super::decode_slots(&mem, DICT, LAYOUT).expect("readable dictionary");
        assert_eq!(slots.code, Some(super::EVENT_EAC_HEARTBEAT));
        assert_eq!(
            slots.payload,
            Some((PAYLOAD + super::ARRAY_DATA_OFF, 512)),
            "payload pointer is the array data, length its max_length"
        );
        assert!(super::layout_matches(&slots));
    }

    #[test]
    fn a_wrong_entry_stride_is_rejected_by_the_same_data() {
        // 0x20 stride over records written at 0x18 lands between keys.
        let mem = Fake::dictionary(66, 512, 0x18, 0x10);
        let wrong = super::Layout {
            stride: 0x20,
            ..LAYOUT
        };
        let slots = super::decode_slots(&mem, DICT, wrong).expect("dictionary reads fine");
        assert!(!super::layout_matches(&slots), "{slots:?}");
        assert_eq!(super::decode_slots(&mem, DICT, LAYOUT).unwrap().code, Some(66));
    }

    #[test]
    fn a_layout_with_the_wrong_value_offset_finds_no_payload() {
        let mem = Fake::dictionary(66, 512, LAYOUT.stride, LAYOUT.value);
        let shifted = super::Layout {
            value: 0x08,
            ..LAYOUT
        };
        let slots = super::decode_slots(&mem, DICT, shifted).expect("dictionary reads fine");
        assert_eq!(slots.code, None, "value offset 0x08 sits on the key");
    }

    #[test]
    fn count_is_clamped_by_the_array_and_by_the_slot_cap() {
        let mut mem = Fake::dictionary(66, 512, LAYOUT.stride, LAYOUT.value);
        // A torn count that promises more records than the array holds.
        mem.put(DICT + 0x20, &4096u32.to_le_bytes());
        let slots = super::decode_slots(&mem, DICT, LAYOUT).expect("readable");
        assert_eq!(slots.code, Some(66), "clamped by max_length, records still found");

        let mut empty = Fake::dictionary(66, 512, LAYOUT.stride, LAYOUT.value);
        empty.put(ENTRIES + super::ARRAY_LEN_OFF, &0u32.to_le_bytes());
        let slots = super::decode_slots(&empty, DICT, LAYOUT).expect("readable");
        assert_eq!(slots, super::Slots { code: None, payload: None });
    }

    #[test]
    fn an_unreadable_dictionary_is_none_rather_than_empty() {
        let mem = Fake::new();
        assert_eq!(super::decode_slots(&mem, DICT, LAYOUT), None);
        // Null / below-the-floor pointers never pass for a dictionary.
        assert_eq!(super::decode_slots(&mem, 0x40, LAYOUT), None);
    }

    #[test]
    fn dictionary_offsets_accept_both_mono_and_core_field_names() {
        let mono = vec![
            ("buckets".to_string(), 0x10usize),
            ("entries".to_string(), 0x18),
            ("count".to_string(), 0x20),
        ];
        assert_eq!(super::dictionary_offsets(&mono), Some((0x20, 0x18)));
        let core = vec![
            ("_buckets".to_string(), 0x10usize),
            ("_entries".to_string(), 0x18),
            ("_count".to_string(), 0x20),
        ];
        assert_eq!(super::dictionary_offsets(&core), Some((0x20, 0x18)));
        let not_a_dict = vec![("m_Capacity".to_string(), 0x10usize)];
        assert_eq!(super::dictionary_offsets(&not_a_dict), None);
    }

    // -----------------------------------------------------------------
    // Capture stream, retargeting and env knobs
    // -----------------------------------------------------------------

    fn record(out: &mut Vec<u8>, dt_ms: u32, data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&dt_ms.to_le_bytes());
        out.extend_from_slice(data);
    }

    #[test]
    fn a_capture_stream_splits_into_payloads_and_a_raw_frame_does_not() {
        let mut bytes = Vec::new();
        record(&mut bytes, 0, &[1u8; 512]);
        record(&mut bytes, 2_007, &[2u8; 512]);
        // A cut-off trailing record heals the stream instead of breaking it.
        bytes.extend_from_slice(&512u32.to_le_bytes());
        let payloads = super::parse_payload_records(&bytes).expect("record stream");
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[1], vec![2u8; 512]);

        // A bare frame on disk is not a record stream: the caller sends it whole.
        assert_eq!(super::parse_payload_records(&[0x5Au8; 512]), None);
        assert_eq!(super::parse_payload_records(&[]), None);
    }

    #[test]
    fn retargeting_moves_the_frame_into_our_session_and_reseals_it() {
        let recorded = vec![0x11u8; 512];
        let out = super::retarget_frame(&recorded, Some(0xABCD), 7);
        assert_eq!(out.len(), 512);
        assert_eq!(
            u64::from_le_bytes(out[4..12].try_into().expect("8 bytes")),
            0xABCD
        );
        assert_eq!(
            u64::from_le_bytes(out[12..20].try_into().expect("8 bytes")),
            7
        );
        let mut zeroed = out.clone();
        zeroed[super::EAC_CHECKSUM_OFF..super::EAC_CHECKSUM_OFF + 4].fill(0);
        assert_eq!(
            crate::crc::crc32c_eac(&zeroed),
            u32::from_le_bytes(
                out[super::EAC_CHECKSUM_OFF..super::EAC_CHECKSUM_OFF + 4]
                    .try_into()
                    .expect("4 bytes")
            ),
            "the stored checksum is CRC-32C over the retargeted frame"
        );
        // Without a session id the recorded one survives.
        let kept = super::retarget_frame(&recorded, None, 9);
        assert_eq!(
            u64::from_le_bytes(kept[4..12].try_into().expect("8 bytes")),
            u64::from_le_bytes(recorded[4..12].try_into().expect("8 bytes"))
        );
        // A short frame is not touched at all.
        assert_eq!(super::retarget_frame(&[0u8; 24], Some(1), 1), vec![0u8; 24]);
    }

    #[test]
    fn the_send_config_defaults_to_the_measured_cadence() {
        assert_eq!(super::SendConfig::parse(|_| None), None);
        assert_eq!(
            super::SendConfig::parse(|name| {
                if name == "EAC_EVENT_SEND" {
                    Some("  ".to_string())
                } else {
                    None
                }
            }),
            None,
            "a blank path disables the sender"
        );

        let default = super::SendConfig::parse(|name| match name {
            "EAC_EVENT_SEND" => Some(" /tmp/frames.bin ".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(default.path, std::path::PathBuf::from("/tmp/frames.bin"));
        assert_eq!(default.interval_ms, super::DEFAULT_SEND_MS);
        assert!(default.looping);
        assert!(default.retarget);
        assert_eq!(default.session, None);
        assert_eq!(default.start, 0);
        assert_eq!(default.seq, 0);

        let explicit = super::SendConfig::parse(|name| match name {
            "EAC_EVENT_SEND" => Some("/tmp/a.bin".to_string()),
            "EAC_EVENT_SEND_MS" => Some("1500".to_string()),
            "EAC_EVENT_SEND_START" => Some("3".to_string()),
            "EAC_EVENT_SEND_LOOP" => Some("0".to_string()),
            "EAC_EVENT_SEND_RETARGET" => Some("0".to_string()),
            "EAC_EVENT_SEND_SID" => Some("0x158b".to_string()),
            "EAC_EVENT_SEND_SEQ" => Some("42".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(explicit.interval_ms, 1_500);
        assert_eq!(explicit.start, 3);
        assert!(!explicit.looping);
        assert!(!explicit.retarget);
        assert_eq!(explicit.session, Some(0x158b));
        assert_eq!(explicit.seq, 42);
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
