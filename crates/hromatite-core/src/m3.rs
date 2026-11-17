//! Unity 6 IL2CPP metadata access for the *current* GameAssembly build.
//!
//! The layout below was measured live on 2026-11-15 (sha256 `648f61b1…`) and the
//! class record itself still validates on the 2026-11-17 build
//! (`245768f2…`); only the addresses moved:
//!
//! ```text
//! Il2CppClass +0x00              → const Il2CppImage*
//!            +0x10              → Il2CppClass* parent
//!            +0x18              → const char* namespaze
//!            +0x50              → const MethodInfo**  (array of pointers)
//!            +0x80              → FieldInfo[]         (inline records)
//!            +0x98              → const char* name
//!            +0x124             → uint16 fieldsCount
//! FieldInfo  +0x00              → const Il2CppType*
//!            +0x08              → int32 offset
//!            +0x10              → const char* name
//!            +0x18              → uint32 token
//!            +0x20              → Il2CppClass* parent        (stride 0x28)
//! MethodInfo +0x00              → methodPointer
//!            +0x18              → const char* name
//!            +0x20              → const Il2CppType* return_type
//!            +0x28              → Il2CppClass* klass
//!            +0x30              → parameters
//! ```
//!
//! # Why discovery instead of recorded RVAs
//!
//! VRChat re-randomises GameAssembly on every patch, and `s_TypeInfoDefinition
//! Table` is a **global** in `.data` that il2cpp fills with a pointer to a
//! heap-resident `Il2CppClass**`; the array is not part of the image. The
//! 2026-11-17 log proved the old "scan the image for the array" pass wrong: it
//! picked the module header and a run of pointers *before* the module.
//!
//! Resolution order in [`find`]:
//!
//! 1. [`class_by_name_via_api`] — `il2cpp_class_from_name` (with an
//!    `il2cpp_image_get_class` sweep as a second chance). The il2cpp export
//!    block only shifts as a whole, so its RVAs survive layout moves best.
//! 2. [`class_table`] — rediscover the `Il2CppClass**` from runtime data:
//!    legacy global slot, then a qword in the image that points at a valid
//!    array, then (last) an array inside the image.
//! 3. [`discover_class`] — no table at all: find the class by its ASCII name
//!    and confirm the record by namespace, back-pointer image and methods.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::raw::memscan;
use crate::{procs, safe};

/// `Il2CppClass**` (s_TypeInfoDefinitionTable) inside GameAssembly `.data` on
/// the 2026-11-15 build. Kept as a *hint*: the table is rediscovered when the
/// slot does not validate.
pub const CLASS_TABLE_RVA: u32 = 0x0e66_c870;
/// `il2cpp_image_get_class` (new build; export-block alignment, 2026-11-17).
pub const IMAGE_GET_CLASS_RVA: u32 = 0x00c2_c0a0;
/// `il2cpp_class_get_fields` (new build).
pub const CLASS_GET_FIELDS_RVA: u32 = 0x00c2_9de0;
/// `il2cpp_class_from_name` (new build).
pub const CLASS_FROM_NAME_RVA: u32 = 0x00c2_9dc0;

pub const CLASS_IMAGE_OFF: usize = 0x00;
pub const CLASS_NS_OFF: usize = 0x18;
pub const CLASS_METHODS_OFF: usize = 0x50;
pub const CLASS_FIELDS_OFF: usize = 0x80;
pub const CLASS_NAME_OFF: usize = 0x98;
pub const CLASS_FIELDS_COUNT_OFF: usize = 0x124;

/// `Il2CppImage.name` (short assembly name).
const IMAGE_NAME_OFF: usize = 0x08;
/// Longest class/namespace/image name accepted from memory.
const MAX_NAME_LEN: usize = 200;

pub const FIELD_STRIDE: usize = 0x28;
const FIELD_TYPE_OFF: usize = 0x00;
const FIELD_NAME_OFF: usize = 0x10;
// Verified by il2cpp_field_get_offset/value/set_value (tools/data/
// il2cpp-export-names.json): offset is int32 at +0x08; token is at +0x18.
const FIELD_OFFSET_OFF: usize = 0x08;
const FIELD_PARENT_OFF: usize = 0x20;

const METHOD_NAME_OFF: usize = 0x18;
const METHOD_RETURN_TYPE_OFF: usize = 0x20;

/// Classes the payload has to resolve, plus the class record offsets that may
/// have moved. Every candidate offset is *confirmed against data*, never
/// assumed: a patch that shifts `name` to +0x90 costs one extra probe.
const NAME_OFFSETS: [usize; 6] = [0x98, 0x90, 0xA0, 0x88, 0x80, 0x10];
const NS_OFFSETS: [usize; 4] = [0x18, 0x10, 0x20, 0x08];
const METHODS_OFFSETS: [usize; 3] = [0x50, 0x58, 0x48];

/// Entries scored when a candidate table or record is validated.
const SCAN_ENTRIES: usize = 32;
/// Cheap first look at a pointer found in the image.
const QUICK_ENTRIES: usize = 16;
/// Share of entries that must be plausible classes (percent).
const MIN_VALID_PERCENT: usize = 90;
/// How many address-space regions a scan will walk (generous; VRChat uses ~1k).
const MAX_REGIONS: usize = 200_000;
/// Memory-scan hits logged/tried before giving up.
const MAX_STRING_HITS: usize = 64;
const MAX_WORD_HITS: usize = 256;
/// `il2cpp_image_get_class` sweep bound (from the verified feature path).
#[cfg(target_os = "windows")]
const MAX_CLASS_INDEX: usize = 40_000;
/// How many assemblies the index sweep visits before giving up.
#[cfg(target_os = "windows")]
const MAX_SWEEP_IMAGES: usize = 16;

fn method_return_kind_with<R, U>(mi: usize, read_usize: R, read_u32: U) -> u32
where
    R: FnOnce(usize) -> Option<usize>,
    U: FnOnce(usize) -> Option<u32>,
{
    read_usize(mi + METHOD_RETURN_TYPE_OFF)
        .and_then(|ty| read_u32(ty + 0x08))
        .map(|bits| (bits >> 16) & 0xff)
        .unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------
// Validation primitives (pure, host-tested)
// ---------------------------------------------------------------------------

/// Does `text` look like a metadata identifier?
///
/// il2cpp emits letters, digits and the punctuation below for compiler-generated
/// names (`<>c__DisplayClass1_0`, `List\`1`, `A/B`). Requiring this of *every*
/// accepted name is what rejects runs of arbitrary ASCII (`"n"`, `"|#"`) from
/// a mis-identified table.
pub fn identifier_like(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_NAME_LEN
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'`'
                        | b'<'
                        | b'>'
                        | b'.'
                        | b'/'
                        | b'+'
                        | b'-'
                        | b'$'
                        | b'['
                        | b']'
                        | b','
                )
        })
}

/// A namespace is either absent (`""`) or an identifier.
fn namespace_like(text: &str) -> bool {
    text.is_empty() || identifier_like(text)
}

/// First candidate offset accepted by `confirm`, in the order given.
fn pick_offset(candidates: &[usize], confirm: impl Fn(usize) -> bool) -> Option<usize> {
    candidates.iter().copied().find(|offset| confirm(*offset))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TableScore {
    /// Entries that validate as complete `Il2CppClass` records.
    valid: usize,
    /// Entries that grow relative to their predecessor.
    increasing: usize,
    /// Distinct entries among the valid ones.
    unique: usize,
    /// Valid entries whose name is at least two characters (kills all-`"n"`).
    long_names: usize,
}

impl TableScore {
    fn total(&self) -> usize {
        self.valid + self.increasing + self.unique
    }

    fn accepts(&self, span: usize) -> bool {
        self.valid * 100 >= span * MIN_VALID_PERCENT
            && self.increasing * 100 >= span * MIN_VALID_PERCENT
            && self.unique == self.valid
            && self.long_names * 2 >= self.valid
    }
}

/// Score the first `span` entries of `table` through injected readers.
///
/// Injectable on purpose: the whole accept/reject rule is exercised on the host
/// without a live process.
fn score_table_with<RU, RS>(
    table: usize,
    span: usize,
    name_off: usize,
    ns_off: usize,
    read_usize: &RU,
    read_text: &RS,
) -> TableScore
where
    RU: Fn(usize) -> Option<usize>,
    RS: Fn(usize) -> Option<String>,
{
    let mut score = TableScore::default();
    let mut previous = 0usize;
    let mut seen: Vec<usize> = Vec::with_capacity(span);
    for index in 0..span {
        let Some(slot) = index
            .checked_mul(8)
            .and_then(|offset| table.checked_add(offset))
        else {
            break;
        };
        let Some(entry) = read_usize(slot) else {
            break;
        };
        if entry > previous {
            score.increasing += 1;
        }
        previous = entry;
        if entry < 0x1_0000 {
            continue;
        }
        let Some(name_ptr) = entry.checked_add(name_off).and_then(read_usize) else {
            continue;
        };
        let Some(name) = read_text(name_ptr) else {
            continue;
        };
        if !identifier_like(&name) {
            continue;
        }
        let Some(ns_ptr) = entry.checked_add(ns_off).and_then(read_usize) else {
            continue;
        };
        let Some(namespace) = read_text(ns_ptr) else {
            continue;
        };
        if !namespace_like(&namespace) {
            continue;
        }
        // A real record points back at its `Il2CppImage`, whose short name is a
        // non-empty ASCII identifier. Code pointers in a wrong array do not.
        let Some(image) = read_usize(entry) else {
            continue;
        };
        if image < 0x1_0000 {
            continue;
        }
        let Some(image_name) = image
            .checked_add(IMAGE_NAME_OFF)
            .and_then(read_usize)
            .and_then(read_text)
        else {
            continue;
        };
        if !identifier_like(&image_name) {
            continue;
        }
        score.valid += 1;
        if name.len() >= 2 {
            score.long_names += 1;
        }
        // A repeated record means this is not an array of distinct classes; it
        // simply is not counted as unique.
        if !seen.contains(&entry) {
            seen.push(entry);
            score.unique += 1;
        }
    }
    score
}

// ---------------------------------------------------------------------------
// Live readers
// ---------------------------------------------------------------------------

fn name_at(pointer: usize) -> Option<String> {
    if pointer < 0x10000 {
        return Some(String::new());
    }
    safe::ascii_at(pointer, MAX_NAME_LEN + 1)
}

fn score_table(table: usize, span: usize) -> TableScore {
    score_table_with(
        table,
        span,
        CLASS_NAME_OFF,
        CLASS_NS_OFF,
        &safe::read_usize,
        &name_at,
    )
}

fn plausible(table: usize, span: usize) -> bool {
    if table < 0x1_0000 || !safe::readable_at(table) {
        return false;
    }
    score_table(table, span).accepts(span)
}

/// Name + namespace of `entry` at the given offsets, both identifier-checked.
fn class_record_with<RU, RS>(
    entry: usize,
    name_off: usize,
    ns_off: usize,
    read_usize: &RU,
    read_text: &RS,
) -> Option<(String, String)>
where
    RU: Fn(usize) -> Option<usize>,
    RS: Fn(usize) -> Option<String>,
{
    let name_ptr = read_usize(entry.checked_add(name_off)?)?;
    let name = read_text(name_ptr)?;
    if !identifier_like(&name) {
        return None;
    }
    let ns_ptr = read_usize(entry.checked_add(ns_off)?)?;
    let namespace = read_text(ns_ptr)?;
    namespace_like(&namespace).then_some((name, namespace))
}

fn class_record(entry: usize, name_off: usize, ns_off: usize) -> Option<(String, String)> {
    class_record_with(entry, name_off, ns_off, &safe::read_usize, &name_at)
}

/// Does the record at `entry` point at an image whose short name reads back?
fn image_back_pointer(entry: usize) -> bool {
    let Some(image) = safe::read_usize(entry) else {
        return false;
    };
    if image < 0x1_0000 || !safe::readable_at(image) {
        return false;
    }
    safe::read_usize(image + IMAGE_NAME_OFF)
        .and_then(name_at)
        .is_some_and(|text| identifier_like(&text))
}

/// Number of leading `MethodInfo` records in `array` that look real: an
/// identifier-like name and (when present) an entry inside GameAssembly.
fn method_count(array: usize, image: (usize, usize), limit: usize) -> usize {
    let (base, size) = image;
    let end = base.saturating_add(size);
    let mut count = 0usize;
    for index in 0..limit {
        let Some(mi) = index
            .checked_mul(8)
            .and_then(|offset| array.checked_add(offset))
        else {
            break;
        };
        let Some(info) = safe::read_usize(mi) else {
            break;
        };
        if info < 0x1_0000 || !safe::readable_at(info) {
            break;
        }
        let Some(name_ptr) = safe::read_usize(info + METHOD_NAME_OFF) else {
            break;
        };
        let Some(name) = name_at(name_ptr) else {
            break;
        };
        if !identifier_like(&name) {
            break;
        }
        let Some(entry) = safe::read_usize(info) else {
            break;
        };
        if entry != 0 && !(base != 0 && entry >= base && entry < end) {
            break;
        }
        count += 1;
    }
    count
}

// ---------------------------------------------------------------------------
// Class table discovery
// ---------------------------------------------------------------------------

/// Cheap shape test for a candidate *array of class pointers*: one snapshot of
/// the first [`QUICK_ENTRIES`] slots must hold distinct, growing, mapped
/// pointers. Most qwords in `.data` do not survive this, which keeps the
/// expensive per-record validation to a handful of candidates.
fn quick_array_shape(table: usize, regions: &memscan::RegionMap) -> bool {
    let Some(bytes) = safe::read_bytes(table, QUICK_ENTRIES * 8) else {
        return false;
    };
    let mut previous = 0usize;
    let mut growing = 0usize;
    let mut seen: Vec<usize> = Vec::with_capacity(QUICK_ENTRIES);
    for index in 0..bytes.len() / 8 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
        let entry = u64::from_le_bytes(raw) as usize;
        if entry < 0x1_0000 || entry <= previous {
            continue;
        }
        if !regions.contains(entry, 8) || seen.contains(&entry) {
            continue;
        }
        seen.push(entry);
        previous = entry;
        growing += 1;
    }
    growing * 100 >= QUICK_ENTRIES * MIN_VALID_PERCENT
}

/// Third path: a qword inside the image that holds a pointer to a heap-resident
/// `Il2CppClass**` (the measured shape: `s_TypeInfoDefinitionTable` is a global).
fn scan_for_table_pointer(spans: &[memscan::ScanSpan], base: usize) -> Option<(usize, usize)> {
    let regions = memscan::RegionMap::capture(MAX_REGIONS);
    let mut tested: HashSet<usize> = HashSet::new();
    let mut best: Option<(usize, usize, usize)> = None; // (score, word address, table)
    let mut seen_words = 0usize;
    let perfect = SCAN_ENTRIES * 3;
    memscan::scan_qwords(spans, |word_address, word| {
        // Cheap checks first: a table pointer is an aligned, mapped address.
        if word < 0x1_0000 || word & 7 != 0 {
            return false;
        }
        if !regions.contains(word, 8) {
            return false;
        }
        seen_words += 1;
        if !tested.insert(word) {
            return false;
        }
        if !quick_array_shape(word, &regions) {
            return false;
        }
        let score = score_table(word, SCAN_ENTRIES);
        if !score.accepts(SCAN_ENTRIES) {
            if best.is_none() {
                crate::log::log_tag(
                    "m3",
                    &format!(
                        "class table: word {:#x} -> {:#x} rejected ({:?})",
                        word_address.saturating_sub(base),
                        word,
                        score
                    ),
                );
            }
            return false;
        }
        let total = score.total();
        if best.is_none_or(|(top, _, _)| total > top) {
            best = Some((total, word_address, word));
        }
        total >= perfect
    });
    crate::log::log_tag(
        "m3",
        &format!(
            "class table: {} aligned mapped qword(s) probed, best {:?}",
            seen_words,
            best.map(|(score, address, table)| (score, address - base, table))
        ),
    );
    best.map(|(_, address, table)| (address, table))
}

/// Second path: the array itself lies inside the image (older builds). Coarse
/// 64 KiB then 8 B, always clamped to a region inside `[base, base+size)`.
fn scan_for_table_in_image(spans: &[memscan::ScanSpan]) -> Option<usize> {
    let mut best = (0usize, 0usize);
    for span in spans {
        let mut at = span.base.next_multiple_of(0x1_0000);
        while at < span.end() {
            let score = score_table(at, QUICK_ENTRIES);
            if score.accepts(QUICK_ENTRIES) {
                let total = score.total();
                if total > best.0 {
                    best = (total, at);
                }
            }
            at += 0x1_0000;
        }
    }
    if best.0 == 0 {
        return None;
    }
    // Refine the winner inside its 64 KiB window, still inside the image.
    let from = best.1.saturating_sub(0x1_0000);
    let to = best.1.saturating_add(0x1_0000);
    let mut found = best.1;
    let mut found_score = best.0;
    for span in spans {
        let start = (from.max(span.base) + 7) & !7;
        let stop = to.min(span.end());
        let mut fine = start;
        while fine < stop {
            let score = score_table(fine, SCAN_ENTRIES);
            if score.accepts(SCAN_ENTRIES) && score.total() > found_score {
                found = fine;
                found_score = score.total();
            }
            fine += 8;
        }
    }
    crate::log::log_tag(
        "m3",
        &format!("class table: array inside the image at {found:#x} (score {found_score})"),
    );
    Some(found)
}

/// Find `s_TypeInfoDefinitionTable` in the *current* build.
///
/// The recorded RVA is only a hint: it pointed at the global variable on
/// 2026-11-15 and VRChat re-randomises the image on every patch. Each path
/// validates with [`TableScore::accepts`], so a plausible-looking wrong array
/// (`"n"` names, duplicates, non-growing pointers) is rejected rather than
/// silently used.
fn locate_table(base: usize, size: usize) -> Option<usize> {
    let end = base.checked_add(size)?;

    // 1. The measured shape: RVA = global slot holding the array pointer.
    if let Some(slot) = base.checked_add(CLASS_TABLE_RVA as usize)
        && slot.checked_add(8)? <= end
        && let Some(pointer) = safe::read_usize(slot)
        && plausible(pointer, SCAN_ENTRIES)
    {
        crate::log::log_tag(
            "m3",
            &format!(
                "class table: legacy slot {slot:#x} -> {pointer:#x} ({:?})",
                score_table(pointer, SCAN_ENTRIES)
            ),
        );
        return Some(pointer);
    }

    // 2. Same shape, new RVA: some word in the image holds the array pointer.
    let spans = memscan::spans_within(base, size, MAX_REGIONS);
    crate::log::log_tag(
        "m3",
        &format!(
            "class table: {} readable image region(s), {size:#x} B to scan",
            spans.len()
        ),
    );
    if let Some((word_address, table)) = scan_for_table_pointer(&spans, base) {
        crate::log::log_tag(
            "m3",
            &format!(
                "class table: global {word_address:#x} (rva {:#x}) -> {table:#x}",
                word_address.saturating_sub(base)
            ),
        );
        return Some(table);
    }

    // 3. Last: the array itself inside the image.
    scan_for_table_in_image(&spans).inspect(|table| {
        crate::log::log_tag("m3", &format!("class table: in-image array {table:#x}"));
    })
}

/// Resolved class table (cached: the pointer is stable for the run).
pub fn class_table() -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    static TRIES: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    // A full scan is no good on every tick: the table fills lazily, so a miss is
    // retried, but at most once every few calls.
    let tries = TRIES.fetch_add(1, Ordering::Relaxed) + 1;
    if tries > 1 && !tries.is_multiple_of(8) {
        return None;
    }
    let table = locate_table(base, size)?;
    CACHE.store(table, Ordering::Relaxed);
    Some(table)
}

// ---------------------------------------------------------------------------
// il2cpp export paths (primary on Windows)
// ---------------------------------------------------------------------------

/// Cached outcome of an `il2cpp_class_from_name`/index-sweep lookup.
static API_HIT: Mutex<Option<(String, String, usize)>> = Mutex::new(None);
static DISCOVERY_HIT: Mutex<Option<(String, String, ClassHit)>> = Mutex::new(None);

fn cached_api(ns: &str, name: &str) -> Option<usize> {
    let guard = API_HIT.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref() {
        Some((ns_key, name_key, class)) if ns_key == ns && name_key == name => Some(*class),
        _ => None,
    }
}

fn store_api(ns: &str, name: &str, class: usize) {
    let mut guard = API_HIT.lock().unwrap_or_else(PoisonError::into_inner);
    *guard = Some((ns.to_string(), name.to_string(), class));
}

fn cached_discovery(ns: &str, name: &str) -> Option<ClassHit> {
    let guard = DISCOVERY_HIT.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref() {
        Some((ns_key, name_key, hit)) if ns_key == ns && name_key == name => Some(*hit),
        _ => None,
    }
}

fn store_discovery(ns: &str, name: &str, hit: ClassHit) {
    let mut guard = DISCOVERY_HIT.lock().unwrap_or_else(PoisonError::into_inner);
    *guard = Some((ns.to_string(), name.to_string(), hit));
}

/// Every `Il2CppImage` reachable by walking the packed image array from the
/// corlib image (stride 0x48 in both directions), paired with its short name.
#[cfg(target_os = "windows")]
fn images_around(corlib: usize) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for step in 0i64..768 {
        for sign in [1i64, -1] {
            let index = step * sign;
            let Some(image) = corlib.checked_add_signed((index * 0x48) as isize) else {
                continue;
            };
            let Some(name) = safe::read_usize(image + IMAGE_NAME_OFF).and_then(name_at) else {
                continue;
            };
            if name.is_empty() || !identifier_like(&name) {
                continue;
            }
            if out.iter().any(|(known, _)| *known == image) {
                continue;
            }
            out.push((image, name));
        }
    }
    out
}

/// Put likely assemblies first: the requested one, then anything whose name
/// shares a token with the namespace (`ExitGames.Client.Photon` → `Photon`).
#[cfg(target_os = "windows")]
fn order_images(
    mut images: Vec<(usize, String)>,
    assembly: &str,
    ns: &str,
) -> Vec<(usize, String)> {
    let assembly = assembly.to_ascii_lowercase();
    let tokens: Vec<String> = ns
        .split('.')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect();
    let weight = |name: &str| -> u8 {
        let low = name.to_ascii_lowercase();
        if !assembly.is_empty() && (low == assembly || low == format!("{assembly}.dll")) {
            return 0;
        }
        if !assembly.is_empty() && low.starts_with(&assembly) {
            return 1;
        }
        if low == "assembly-csharp" {
            return 2;
        }
        if tokens.iter().any(|token| low.contains(token)) {
            return 3;
        }
        9
    };
    images.sort_by_key(|(_, name)| weight(name));
    images
}

#[cfg(target_os = "windows")]
fn class_matches(class: usize, ns: &str, name: &str) -> bool {
    class_name(class).as_deref() == Some(name)
        && (ns.is_empty() || class_namespace(class).as_deref() == Some(ns))
}

#[cfg(target_os = "windows")]
fn api_lookup(ns: &str, name: &str, assembly: &str) -> Option<usize> {
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    let end = base.checked_add(size)?;
    let from_name = base.checked_add(CLASS_FROM_NAME_RVA as usize)?;
    let get_class = base.checked_add(IMAGE_GET_CLASS_RVA as usize)?;
    if from_name.checked_add(8)? > end || get_class.checked_add(8)? > end {
        return None;
    }
    let Some(corlib) = crate::raw::il2cpp::find_domain() else {
        crate::log::log_tag("m3", "class_from_name: corlib image getter failed");
        return None;
    };
    let images = order_images(images_around(corlib), assembly, ns);
    crate::log::log_tag(
        "m3",
        &format!(
            "class_from_name {ns}.{name}: corlib {corlib:#x}, {} image(s), fn {from_name:#x}",
            images.len()
        ),
    );
    let ns_c = memscan::stash_cstr(ns)?;
    let name_c = memscan::stash_cstr(name)?;

    // Path A: the API call. Cheap and materialises the class if needed.
    for (image, image_name) in &images {
        match memscan::call3(from_name, *image, ns_c, name_c) {
            Some(class) if class >= 0x10000 => {
                if class_matches(class, ns, name) {
                    crate::log::log_tag(
                        "m3",
                        &format!(
                            "class_from_name: {ns}.{name} = {class:#x} via image {image:#x} ({image_name})"
                        ),
                    );
                    return Some(class);
                }
                crate::log::log_tag(
                    "m3",
                    &format!(
                        "class_from_name: image {image:#x} ({image_name}) returned {class:#x} = {}",
                        class_full_name(class)
                    ),
                );
            }
            Some(_) => {}
            None => {
                crate::log::log_tag(
                    "m3",
                    &format!("class_from_name: fault on image {image:#x} ({image_name})"),
                );
            }
        }
    }

    // Path B: sweep `il2cpp_image_get_class(image, index)`. The 2026-11-15 run
    // proved this export works; it also materialises lazily-built types.
    let mut faults = 0usize;
    for (image, image_name) in images.iter().take(MAX_SWEEP_IMAGES) {
        for index in 0..MAX_CLASS_INDEX {
            let class = match memscan::call2(get_class, *image, index) {
                Some(class) => class,
                None => {
                    faults += 1;
                    if faults > 32 {
                        break;
                    }
                    continue;
                }
            };
            if class < 0x10000 {
                continue;
            }
            if class_matches(class, ns, name) {
                crate::log::log_tag(
                    "m3",
                    &format!(
                        "image_get_class: {ns}.{name} = {class:#x} via image {image:#x} ({image_name}), index {index}"
                    ),
                );
                return Some(class);
            }
        }
        if faults > 32 {
            break;
        }
    }
    crate::log::log_tag(
        "m3",
        &format!(
            "class_from_name/image_get_class: {ns}.{name} not found in {} image(s)",
            images.len()
        ),
    );
    None
}

#[cfg(not(target_os = "windows"))]
fn api_lookup(ns: &str, name: &str, assembly: &str) -> Option<usize> {
    let _ = (ns, name, assembly);
    None
}

/// Resolve a class through il2cpp's own exports — the primary path, because the
/// export block survives a layout move better than any recorded data address.
///
/// `assembly` may be empty: every loaded image is tried (the likely ones first).
/// A hit (and a confirmed miss) is cached for the run.
pub fn class_by_name_via_api(ns: &str, name: &str, assembly: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    if let Some(class) = cached_api(ns, name) {
        return (class != 0).then_some(class);
    }
    let found = api_lookup(ns, name, assembly);
    match found {
        Some(class) => {
            store_api(ns, name, class);
            Some(class)
        }
        None => {
            // Remember the miss so a retry loop does not repeat a full sweep.
            if cached_api(ns, name).is_none() {
                store_api(ns, name, 0);
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Table-less discovery (fallback of last resort)
// ---------------------------------------------------------------------------

/// A class confirmed by data rather than by a table index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassHit {
    /// `typeDefinitionIndex` when known, `usize::MAX` when found by name only.
    pub index: usize,
    pub class: usize,
    pub name_off: usize,
    pub ns_off: usize,
    pub methods_off: usize,
}

fn heap_spans(anchor: usize) -> Vec<memscan::ScanSpan> {
    let mut spans = memscan::readable_spans(MAX_REGIONS);
    spans.retain(|span| span.size >= 0x1000);
    // Metadata strings live in the heap around GameAssembly; scan near first.
    spans.sort_by_key(|span| span.base.abs_diff(anchor));
    spans
}

/// Find a class by its ASCII name, without any class table.
///
/// The name string is located in memory, every qword pointing at it yields a
/// candidate record (`word - name_off`), and a candidate is accepted only when
/// namespace, back-pointer image and at least three `MethodInfo` records all
/// confirm. Every rejected candidate is logged with its reason so a live run
/// explains itself.
pub fn discover_class(ns: &str, name: &str) -> Option<ClassHit> {
    if name.is_empty() {
        return None;
    }
    if let Some(hit) = cached_discovery(ns, name) {
        // A cached miss is a `class == 0` record: no point rescanning the heap.
        return (hit.class != 0).then_some(hit);
    }
    let missing = ClassHit {
        index: usize::MAX,
        class: 0,
        name_off: 0,
        ns_off: 0,
        methods_off: 0,
    };
    let (ga_base, ga_size) = procs::own_module(crate::GAME_ASSEMBLY).unwrap_or((0, 0));
    let image = (ga_base, ga_size);
    let spans = heap_spans(ga_base);
    let strings = memscan::scan_ascii(&spans, name.as_bytes(), MAX_STRING_HITS);
    crate::log::log_tag(
        "m3",
        &format!(
            "discover {ns}.{name}: {} region(s), {} string occurrence(s)",
            spans.len(),
            strings.len()
        ),
    );
    if strings.is_empty() {
        store_discovery(ns, name, missing);
        return None;
    }
    let words = memscan::scan_words(&spans, &strings, MAX_WORD_HITS);
    crate::log::log_tag(
        "m3",
        &format!(
            "discover {ns}.{name}: {} pointer(s) to those strings",
            words.len()
        ),
    );
    let mut logged = 0usize;
    let mut reject = |reason: String| {
        if logged < 24 {
            logged += 1;
            crate::log::log_tag("m3", &format!("discover {ns}.{name}: {reason}"));
        }
    };
    for word_address in words {
        for name_off in NAME_OFFSETS {
            let Some(base) = word_address.checked_sub(name_off) else {
                continue;
            };
            if base < 0x1_0000 {
                continue;
            }
            let Some(namespace_off) = pick_offset(&NS_OFFSETS, |offset| {
                base.checked_add(offset)
                    .and_then(safe::read_usize)
                    .and_then(name_at)
                    .is_some_and(|found| found == ns)
            }) else {
                continue;
            };
            let Some((found_name, _)) = class_record(base, name_off, namespace_off) else {
                continue;
            };
            if found_name != name {
                reject(format!(
                    "base {base:#x} name_off {name_off:#x}: name {found_name:?}"
                ));
                continue;
            }
            if !image_back_pointer(base) {
                reject(format!(
                    "base {base:#x} name_off {name_off:#x}: image back-pointer missing"
                ));
                continue;
            }
            let Some(methods_off) = pick_offset(&METHODS_OFFSETS, |offset| {
                let Some(array) = base.checked_add(offset).and_then(safe::read_usize) else {
                    return false;
                };
                array >= 0x1_0000 && method_count(array, image, 8) >= 3
            }) else {
                reject(format!(
                    "base {base:#x} name_off {name_off:#x}: fewer than 3 usable methods"
                ));
                continue;
            };
            let hit = ClassHit {
                index: usize::MAX,
                class: base,
                name_off,
                ns_off: namespace_off,
                methods_off,
            };
            crate::log::log_tag(
                "m3",
                &format!(
                    "discover {ns}.{name}: accepted {base:#x} (name +{name_off:#x}, ns +{namespace_off:#x}, methods +{methods_off:#x})"
                ),
            );
            store_discovery(ns, name, hit);
            return Some(hit);
        }
    }
    crate::log::log_tag(
        "m3",
        &format!("discover {ns}.{name}: no candidate confirmed"),
    );
    store_discovery(ns, name, missing);
    None
}

// ---------------------------------------------------------------------------
// Public layout accessors (signatures unchanged)
// ---------------------------------------------------------------------------

/// `Il2CppClass*` for a `typeDefinitionIndex`.
pub fn class_at(index: usize) -> Option<usize> {
    let table = class_table()?;
    let p = safe::read_usize(table + index * 8)?;
    (p >= 0x10000 && safe::readable_at(p)).then_some(p)
}

/// Parent class (`Il2CppClass.parent`, measured at `+0x10` on this build:
/// Camera → Behaviour → Component → Object → System.Object).
pub const CLASS_PARENT_OFF: usize = 0x10;

pub fn parent(class: usize) -> Option<usize> {
    let p = safe::read_usize(class + CLASS_PARENT_OFF)?;
    (p >= 0x10000 && safe::readable_at(p)).then_some(p)
}

/// Walk the inheritance chain, starting with `class` itself.
pub fn chain(class: usize) -> Vec<usize> {
    let mut out = vec![class];
    let mut cur = class;
    for _ in 0..32 {
        match parent(cur) {
            Some(p) => {
                out.push(p);
                cur = p;
            }
            None => break,
        }
    }
    out
}

/// First method with this name anywhere in the class hierarchy.
pub fn method_in_chain(class: usize, name: &str) -> Option<(usize, usize)> {
    for c in chain(class) {
        for m in methods(c) {
            if m.name == name {
                return Some((c, m.entry));
            }
        }
    }
    None
}

/// Class name/namespace (this build: name at +0x98, namespace at +0x18).
pub fn class_name(class: usize) -> Option<String> {
    name_at(safe::read_usize(class + CLASS_NAME_OFF)?)
}

pub fn class_namespace(class: usize) -> Option<String> {
    name_at(safe::read_usize(class + CLASS_NS_OFF)?)
}

pub fn class_full_name(class: usize) -> String {
    let n = class_name(class).unwrap_or_default();
    let ns = class_namespace(class).unwrap_or_default();
    if ns.is_empty() {
        n
    } else {
        format!("{ns}.{n}")
    }
}

/// Every materialised class, in table order (index = typeDefinitionIndex).
pub fn classes(max: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let Some(table) = class_table() else {
        crate::log::log_tag("m3", "classes: no class table — enumerating nothing");
        return out;
    };
    for i in 0..max {
        let p = match safe::read_usize(table + i * 8) {
            Some(p) => p,
            None => break,
        };
        if p < 0x10000 || !safe::readable_at(p) {
            continue;
        }
        out.push((i, p));
    }
    out
}

/// Linear search by namespace/name. `name` empty = every class of that namespace.
///
/// A missing table is not fatal: the il2cpp export path runs first, then
/// [`discover_class`] returns the single confirmed hit.
pub fn find(ns: &str, name: &str, max: usize) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    if !name.is_empty()
        && let Some(class) = class_by_name_via_api(ns, name, "")
    {
        return vec![(usize::MAX, class)];
    }
    if let Some(table) = class_table() {
        for i in 0..max {
            let p = match safe::read_usize(table + i * 8) {
                Some(p) => p,
                None => break,
            };
            if p < 0x10000 || !safe::readable_at(p) {
                continue;
            }
            let Some(cname) = class_name(p) else {
                continue;
            };
            if !name.is_empty() && cname != name {
                continue;
            }
            let cns = class_namespace(p).unwrap_or_default();
            if !ns.is_empty() && cns != ns {
                continue;
            }
            hits.push((i, p));
            if hits.len() >= 64 {
                break;
            }
        }
        if !hits.is_empty() || name.is_empty() {
            return hits;
        }
        crate::log::log_tag(
            "m3",
            &format!("find {ns}.{name}: table hit nothing, falling back to discovery"),
        );
    } else {
        crate::log::log_tag(
            "m3",
            &format!("find {ns}.{name}: no class table — degrading to name discovery"),
        );
    }
    if !name.is_empty()
        && let Some(hit) = discover_class(ns, name)
    {
        crate::log::log_tag(
            "m3",
            &format!(
                "find {ns}.{name}: degraded discovery hit {:#x} (name_off {:#x})",
                hit.class, hit.name_off
            ),
        );
        return vec![(hit.index, hit.class)];
    }
    hits
}

/// Diagnostics only: every class whose **name contains** `needle`, up to
/// `limit` hits, with its full `Namespace.Name`. Used when a hook cannot find
/// its type by exact name after a game update — it says what the table really
/// holds instead of leaving a bare "not found".
pub fn find_contains(needle: &str, max: usize, limit: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let Some(table) = class_table() else {
        crate::log::log_tag("m3", "find_contains: no class table");
        return out;
    };
    for i in 0..max {
        let Some(p) = safe::read_usize(table + i * 8) else {
            break;
        };
        if p < 0x10000 || !safe::readable_at(p) {
            continue;
        }
        let Some(name) = class_name(p) else {
            continue;
        };
        if name.is_empty() || !name.contains(needle) {
            continue;
        }
        out.push((i, class_full_name(p)));
        if out.len() >= limit {
            break;
        }
    }
    out
}

pub struct Field {
    pub name: String,
    pub ty: usize,
    pub offset_or_index: u32,
    pub parent: usize,
}

fn field_offset(record: &[u8]) -> Option<u32> {
    let bytes = record.get(FIELD_OFFSET_OFF..FIELD_OFFSET_OFF + size_of::<u32>())?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Fields of a class through the verified Unity 6 `FieldInfo` record.
pub fn fields(class: usize) -> Vec<Field> {
    let mut out = Vec::new();
    let ptr = match safe::read_usize(class + CLASS_FIELDS_OFF) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    let count = safe::read_u16(class + CLASS_FIELDS_COUNT_OFF).unwrap_or(0) as usize;
    for i in 0..count.min(512) {
        let e = ptr + i * FIELD_STRIDE;
        let ty = safe::read_usize(e + FIELD_TYPE_OFF).unwrap_or(0);
        let name = safe::read_usize(e + FIELD_NAME_OFF)
            .and_then(name_at)
            .unwrap_or_default();
        let off = safe::read_bytes(e, FIELD_STRIDE)
            .as_deref()
            .and_then(field_offset)
            .unwrap_or(0);
        let parent = safe::read_usize(e + FIELD_PARENT_OFF).unwrap_or(0);
        out.push(Field {
            name,
            ty,
            offset_or_index: off,
            parent,
        });
    }
    out
}

pub struct Method {
    pub name: String,
    pub entry: usize,
    pub info: usize,
    /// Il2CppType.type from the return descriptor (1 = IL2CPP_TYPE_VOID).
    pub return_kind: u32,
    pub parameters_count: u8,
    /// Mono method attributes; 0x10 = METHOD_ATTRIBUTE_STATIC.
    pub flags: u16,
}

/// Methods of a class (`class+0x50` is an array of `MethodInfo*`).
pub fn methods(class: usize) -> Vec<Method> {
    let mut out = Vec::new();
    let arr = match safe::read_usize(class + CLASS_METHODS_OFF) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    for i in 0..4096 {
        let mi = match safe::read_usize(arr + i * 8) {
            Some(p) => p,
            None => break,
        };
        if mi < 0x10000 || !safe::readable_at(mi) {
            break;
        }
        let name = safe::read_usize(mi + METHOD_NAME_OFF)
            .and_then(name_at)
            .unwrap_or_default();
        if name.is_empty() {
            break;
        }
        let entry = safe::read_usize(mi).unwrap_or(0);
        let return_kind = method_return_kind_with(mi, safe::read_usize, safe::read_u32);
        let parameters_count = safe::read_u8(mi + 0x52).unwrap_or(u8::MAX);
        let flags = safe::read_u16(mi + 0x4c).unwrap_or(u16::MAX);
        out.push(Method {
            name,
            entry,
            info: mi,
            return_kind,
            parameters_count,
            flags,
        });
    }
    out
}

/// `Il2CppImage*` of an assembly. Images are packed with stride 0x48 and the
/// `il2cpp_get_corlib` result is *not* the first one (Assembly-CSharp sits four
/// entries earlier), so the search walks both directions. Exact short names win
/// over prefixes (`Assembly-CSharp` must not resolve to `Assembly-CSharp-firstpass`).
pub fn image_of(corlib_image: usize, assembly: &str) -> Option<usize> {
    let want = assembly.to_ascii_lowercase();
    let scan = |exact: bool| -> Option<usize> {
        for step in 0i64..768 {
            for sign in [1i64, -1] {
                let idx = step * sign;
                let Some(img) = corlib_image.checked_add_signed((idx * 0x48) as isize) else {
                    continue;
                };
                let name = match safe::read_usize(img + 0x08).and_then(name_at) {
                    Some(n) if !n.is_empty() => n,
                    _ => continue,
                };
                let low = name.to_ascii_lowercase();
                let hit = if exact {
                    low == want
                } else {
                    low.starts_with(&want)
                };
                if hit {
                    return Some(img);
                }
            }
        }
        None
    };
    scan(true).or_else(|| scan(false))
}

#[cfg(test)]
mod tests {
    use super::{
        CLASS_NAME_OFF, CLASS_NS_OFF, FIELD_STRIDE, IMAGE_NAME_OFF, MAX_NAME_LEN, NS_OFFSETS,
        TableScore, class_record_with, field_offset, identifier_like, method_return_kind_with,
        pick_offset, score_table_with,
    };
    use std::collections::HashMap;

    #[test]
    fn return_type_uses_slot_0x20_not_class_slot_0x28() {
        const METHOD_INFO: usize = 0x1000;
        const RETURN_TYPE: usize = 0x6fff_f8c6_2f60;
        const CLASS: usize = 0x3034_4d10;
        let kind = method_return_kind_with(
            METHOD_INFO,
            |address| match address {
                x if x == METHOD_INFO + 0x20 => Some(RETURN_TYPE),
                x if x == METHOD_INFO + 0x28 => Some(CLASS),
                _ => None,
            },
            |address| (address == RETURN_TYPE + 0x08).then_some(0x8001_0000),
        );
        assert_eq!(kind, 1, "LateUpdate must be accepted as void");
    }

    #[test]
    fn field_offset_comes_before_name_and_token_comes_after_it() {
        let mut record = [0u8; FIELD_STRIDE];
        record[0x08..0x0c].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        record[0x18..0x1c].copy_from_slice(&0x0400_002au32.to_le_bytes());
        assert_eq!(field_offset(&record), Some(0x1234_5678));
        assert_eq!(field_offset(&record[..0x0b]), None);
    }

    #[test]
    fn identifier_like_accepts_metadata_names_and_rejects_prose() {
        for good in [
            "Camera",
            "PhotonPeer",
            "<Module>",
            "<>c__DisplayClass1_0",
            "List`1",
            "System.Collections.Generic",
            "A/B",
            "n",
        ] {
            assert!(identifier_like(good), "{good:?} is a metadata name");
        }
        for bad in [
            "", "|#", "a b", "\u{1}", "\u{00e9}", "x\u{0}",
            "}", // closing brace without an opener is not emitted
        ] {
            assert!(!identifier_like(bad), "{bad:?} must be rejected");
        }
        let long = "x".repeat(MAX_NAME_LEN + 1);
        assert!(!identifier_like(&long));
        assert!(identifier_like(&"x".repeat(MAX_NAME_LEN)));
    }

    /// Tiny address space for the scoring tests: one qword map plus text.
    #[derive(Default)]
    struct FakeMemory {
        words: HashMap<usize, usize>,
        text: HashMap<usize, String>,
    }

    impl FakeMemory {
        fn put(&mut self, address: usize, value: usize) {
            self.words.insert(address, value);
        }

        fn put_text(&mut self, address: usize, value: &str) {
            self.text.insert(address, value.to_string());
        }

        fn read_usize(&self, address: usize) -> Option<usize> {
            self.words.get(&address).copied().or(Some(0)).filter(|_| {
                // Only mapped words answer; everything else is unreadable.
                self.words.contains_key(&address) || address.is_multiple_of(8)
            })
        }

        fn read_text(&self, address: usize) -> Option<String> {
            if address < 0x10000 {
                return Some(String::new());
            }
            self.text.get(&address).cloned()
        }
    }

    /// Build a table of `count` valid records; `name` and `namespace` are
    /// supplied so the "all `n`" garbage can be reproduced.
    fn build_table(mem: &mut FakeMemory, count: usize, names: impl Fn(usize) -> String) -> usize {
        const TABLE: usize = 0x0010_0000;
        const IMAGE: usize = 0x0020_0000;
        mem.put(IMAGE, 0);
        mem.put_text(0x0020_0010, "Assembly-CSharp");
        mem.put(IMAGE + IMAGE_NAME_OFF, 0x0020_0010);
        for index in 0..count {
            let class = 0x0100_0000 + index * 0x1000;
            mem.put(TABLE + index * 8, class);
            mem.put(class, IMAGE);
            let name = names(index);
            let name_ptr = class + 0x400;
            mem.put_text(name_ptr, &name);
            mem.put(class + CLASS_NAME_OFF, name_ptr);
            let ns_ptr = class + 0x500;
            mem.put_text(ns_ptr, "ExitGames.Client.Photon");
            mem.put(class + CLASS_NS_OFF, ns_ptr);
        }
        TABLE
    }

    #[test]
    fn a_real_table_is_accepted_and_all_n_garbage_is_rejected() {
        let mut real = FakeMemory::default();
        let table = build_table(&mut real, 32, |index| format!("Type{index}"));
        let score = score_table_with(
            table,
            32,
            CLASS_NAME_OFF,
            CLASS_NS_OFF,
            &|address| real.read_usize(address),
            &|address| real.read_text(address),
        );
        assert_eq!(
            score,
            TableScore {
                valid: 32,
                increasing: 32,
                unique: 32,
                long_names: 32
            }
        );
        assert!(score.accepts(32));

        // The 2026-11-17 false positive: every entry pointed at a record whose
        // name and namespace read back as "n".
        let mut garbage = FakeMemory::default();
        let table = build_table(&mut garbage, 32, |_| "n".to_string());
        let score = score_table_with(
            table,
            32,
            CLASS_NAME_OFF,
            CLASS_NS_OFF,
            &|address| garbage.read_usize(address),
            &|address| garbage.read_text(address),
        );
        assert_eq!(score.valid, 32, "names are identifier-like");
        assert_eq!(score.long_names, 0, "but none is a real name");
        assert!(!score.accepts(32), "all-\"n\" must be rejected");
    }

    #[test]
    fn duplicate_or_unsorted_entries_break_a_candidate() {
        let mut memory = FakeMemory::default();
        let table = build_table(&mut memory, 32, |index| format!("Type{index}"));

        let mut duplicated = FakeMemory::default();
        let table_dup = build_table(&mut duplicated, 32, |index| format!("Type{index}"));
        duplicated.put(table_dup + 8, duplicated.words[&table_dup]);
        let score = score_table_with(
            table_dup,
            32,
            CLASS_NAME_OFF,
            CLASS_NS_OFF,
            &|address| duplicated.read_usize(address),
            &|address| duplicated.read_text(address),
        );
        assert!(score.unique < score.valid);
        assert!(
            !score.accepts(32),
            "a repeated class pointer is not a table"
        );

        // Holes (a table that does not grow) drop below the 90% bar.
        for index in 1..5 {
            memory.put(table + index * 8, 0);
        }
        let score = score_table_with(
            table,
            32,
            CLASS_NAME_OFF,
            CLASS_NS_OFF,
            &|address| memory.read_usize(address),
            &|address| memory.read_text(address),
        );
        assert!(score.valid < 32, "{score:?}");
        assert!(score.increasing * 100 < 32 * 90, "{score:?}");
        assert!(!score.accepts(32));
    }

    #[test]
    fn offsets_are_chosen_by_data_not_by_hardcoded_layout() {
        // The record is laid out with name at +0x90 and namespace at +0x08;
        // `pick_offset` must find them by confirmation, not by the nominal
        // +0x98/+0x18 defaults.
        assert_eq!(
            pick_offset(&NS_OFFSETS, |offset| offset == 0x08),
            Some(0x08)
        );
        assert_eq!(pick_offset(&NS_OFFSETS, |_| false), None);
        assert_eq!(
            pick_offset(&NS_OFFSETS, |offset| offset == 0x18),
            Some(0x18)
        );

        let mut memory = FakeMemory::default();
        const CLASS: usize = 0x0100_0000;
        memory.put(CLASS, 0x0020_0000);
        memory.put(0x0020_0000 + IMAGE_NAME_OFF, 0x0020_0010);
        memory.put_text(0x0020_0010, "Assembly-CSharp");
        memory.put(CLASS + 0x90, 0x0100_0400);
        memory.put_text(0x0100_0400, "PhotonPeer");
        memory.put(CLASS + 0x08, 0x0100_0500);
        memory.put_text(0x0100_0500, "ExitGames.Client.Photon");
        assert_eq!(
            class_record_with(
                CLASS,
                0x90,
                0x08,
                &|address| memory.read_usize(address),
                &|address| memory.read_text(address)
            ),
            Some((
                "PhotonPeer".to_string(),
                "ExitGames.Client.Photon".to_string()
            ))
        );
        // The default offsets find nothing in this shifted record.
        assert_eq!(
            class_record_with(
                CLASS,
                CLASS_NAME_OFF,
                CLASS_NS_OFF,
                &|address| memory.read_usize(address),
                &|address| memory.read_text(address)
            ),
            None
        );
    }
}
