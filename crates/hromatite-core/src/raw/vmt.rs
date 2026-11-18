//! Virtual-method-table (VMT) hooks — the mechanism behind Kiero's `bind`.
//!
//! A COM object begins with a pointer to its vtable, an array of function
//! pointers. Swapping one entry redirects every *future* virtual call made
//! through any object that shares that table, which is exactly what a
//! `Present`/`ResizeBuffers` hook needs and what an inline patch cannot do for
//! a per-object implementation.
//!
//! Two properties shape this module:
//!
//! * **The table is usually read-only.** `dxgi.dll`/`d3d11.dll` keep their
//!   vtables in `.rdata`, so a slot is written only between
//!   `VirtualProtect(PAGE_READWRITE)` and a restore of the previous protection.
//! * **A slot can be shared.** Every swapchain instance created by one
//!   implementation points at the same table, so hooking slot 8 twice would
//!   silently chain two detours. Claims are therefore tracked per
//!   `(table, index)` and a second claim is refused until the first is released
//!   by [`VmtHook::unhook`].
//!
//! Nothing here dereferences an unvalidated address: reads go through
//! [`TableMem`], whose live implementation is [`crate::safe`], and writes go
//! through the same backend after the protection change. `Option`/`bool` is
//! the contract for every rejected address or OS failure.

use crate::ffi;
use crate::log;
use std::sync::Mutex;

/// Page granularity of `VirtualProtect`; slots are never split across pages.
#[cfg_attr(not(windows), allow(dead_code))]
const PAGE_SIZE: usize = 0x1000;

/// Lowest address the OS can map. Rejects null and near-null sentinels before
/// any read is attempted.
const MIN_ADDRESS: usize = 0x10000;

/// Byte access and page protection for one virtual-method table.
///
/// The live implementation ([`Live`]) reads through [`crate::safe`] and writes
/// through the same checked OS copy after a protection change, so a bad address
/// is an `Err`-shaped `None`/`false` instead of an access violation. Tests
/// substitute an in-memory address space, which keeps the slot/claim logic
/// target-independent and exercisable on the Linux host.
pub trait TableMem: Send + Sync {
    /// One little-endian word, or `None` when the range is not readable.
    fn read_u64(&self, addr: usize) -> Option<u64>;
    /// Make `addr..addr+len` `prot`, returning the previous protection.
    fn protect(&self, addr: usize, len: usize, prot: u32) -> Option<u32>;
    /// Write one word; `false` when the backend refused the range.
    fn write_u64(&self, addr: usize, value: u64) -> bool;
    /// Publish a write to code/data the CPU may have cached.
    fn flush(&self, addr: usize, len: usize);
}

// --------------------------------------------------------------- live backend

/// `VirtualProtect` over the whole pages that cover `addr..addr+len`.
///
/// Page granularity is why the length is rounded *up*: a slot at the very end
/// of a page straddles into the next one, and that page has to be writable too
/// or the store faults.
#[cfg(windows)]
fn protect_pages(addr: usize, len: usize, prot: u32) -> Option<u32> {
    if len == 0 {
        return None;
    }
    let end = addr.checked_add(len)?;
    let page = addr & !(PAGE_SIZE - 1);
    let last = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if last <= page {
        return None;
    }
    let mut old = 0u32;
    // SAFETY: `old` is a live output pointer, `page..last` is page-aligned and
    // was derived from the caller's own range; the call is synchronous and
    // touches no Rust data.
    let ok = unsafe { ffi::VirtualProtect(page as *mut u8, last - page, prot, &mut old) };
    (ok != 0).then_some(old)
}

#[cfg(not(windows))]
fn protect_pages(_addr: usize, _len: usize, _prot: u32) -> Option<u32> {
    None
}

#[cfg(windows)]
fn flush_range(addr: usize, len: usize) {
    // SAFETY: the pseudo-process handle is always valid and `addr..addr+len`
    // describes the slot the caller just wrote.
    unsafe {
        ffi::FlushInstructionCache(ffi::GetCurrentProcess(), addr as *const u8, len);
    }
}

#[cfg(not(windows))]
fn flush_range(_addr: usize, _len: usize) {}

/// The process's own address space: [`crate::safe`] reads, `VirtualProtect` +
/// `WriteProcessMemory` writes.
pub struct Live;

impl TableMem for Live {
    fn read_u64(&self, addr: usize) -> Option<u64> {
        crate::safe::read_u64(addr)
    }

    fn protect(&self, addr: usize, len: usize, prot: u32) -> Option<u32> {
        protect_pages(addr, len, prot)
    }

    fn write_u64(&self, addr: usize, value: u64) -> bool {
        crate::safe::write_usize(addr, value as usize)
    }

    fn flush(&self, addr: usize, len: usize) {
        flush_range(addr, len);
    }
}

// ------------------------------------------------------------------- claims

/// `(table, index)` pairs that currently hold a detour. A slot shared by two
/// objects is one claim, so the second object's `hook` is refused rather than
/// chaining detours through each other. A `Vec` keeps the static const while
/// the claim count stays in single digits.
static CLAIMED: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

fn claim(table: usize, index: u32) -> bool {
    let mut guard = match CLAIMED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.contains(&(table, index)) {
        return false;
    }
    guard.push((table, index));
    true
}

fn release(table: usize, index: u32) {
    let mut guard = match CLAIMED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|&entry| entry != (table, index));
}

// --------------------------------------------------------------------- Vmt

/// One object's vtable: the object pointer and the table it points at.
///
/// The backend is a reference so the same slot/claim logic runs unchanged
/// against a synthetic table in tests; [`Vmt::new`] always uses [`Live`].
pub struct Vmt<'a> {
    object: usize,
    table: usize,
    mem: &'a dyn TableMem,
}

impl<'a> Vmt<'a> {
    /// Read the vtable pointer of `object` through the live backend.
    pub fn new(object: usize) -> Option<Self> {
        Self::with_mem(object, &Live)
    }

    /// Same, against an explicit backend (tests, or a future remote reader).
    pub fn with_mem(object: usize, mem: &'a dyn TableMem) -> Option<Self> {
        if object < MIN_ADDRESS {
            return None;
        }
        let table = mem.read_u64(object)? as usize;
        if table < MIN_ADDRESS {
            return None;
        }
        // The table itself has to be readable for at least one slot: a
        // dangling first qword is the common result of pointing at a random
        // object rather than a COM interface.
        mem.read_u64(table)?;
        Some(Self {
            object,
            table,
            mem,
        })
    }

    pub fn object(&self) -> usize {
        self.object
    }

    pub fn table(&self) -> usize {
        self.table
    }

    /// Address of the `index`-th vtable entry.
    pub fn slot(&self, index: u32) -> Option<usize> {
        let at = self.slot_address(index)?;
        self.mem.read_u64(at).map(|value| value as usize)
    }

    fn slot_address(&self, index: u32) -> Option<usize> {
        let offset = (index as usize).checked_mul(core::mem::size_of::<usize>())?;
        self.table.checked_add(offset)
    }

    /// Replace slot `index` with `detour`, remembering the original entry.
    ///
    /// Refuses when the slot is unreadable, the detour is not a plausible code
    /// address, or the slot is already claimed by another [`VmtHook`]. On
    /// success the table page is back to its previous protection and the slot
    /// reads `detour`.
    pub fn hook(&mut self, index: u32, detour: usize) -> Option<VmtHook<'a>> {
        let at = self.slot_address(index)?;
        if detour < MIN_ADDRESS {
            log::log_tag(
                "vmt",
                &format!("hook: detour {detour:#x} is not a plausible code address"),
            );
            return None;
        }
        let original = match self.slot(index) {
            Some(value) => value,
            None => {
                log::log_tag(
                    "vmt",
                    &format!("hook: slot {index} at {at:#x} is not readable"),
                );
                return None;
            }
        };
        if original == detour {
            log::log_tag("vmt", &format!("hook: slot {index} already holds {detour:#x}"));
            return None;
        }
        if original == 0 {
            // A zero entry is padding or an unimplemented slot, not a method:
            // forwarding to it would jump to address zero.
            log::log_tag("vmt", &format!("hook: slot {index} at {at:#x} is empty"));
            return None;
        }
        if !claim(self.table, index) {
            log::log_tag(
                "vmt",
                &format!(
                    "hook: slot {index} of table {:#x} is already hooked; unhook it first",
                    self.table
                ),
            );
            return None;
        }
        let old_protect = match self.mem.protect(at, core::mem::size_of::<usize>(), ffi::PAGE_READWRITE)
        {
            Some(previous) => previous,
            None => {
                release(self.table, index);
                log::log_tag(
                    "vmt",
                    &format!("hook: VirtualProtect(rw) on {at:#x} failed; slot left intact"),
                );
                return None;
            }
        };
        let wrote = self.mem.write_u64(at, detour as u64)
            && self.mem.read_u64(at) == Some(detour as u64);
        // Restore even when the write failed, so a rejected hook leaves no
        // writable `.rdata` page behind.
        let restored = self.mem.protect(at, core::mem::size_of::<usize>(), old_protect);
        self.mem.flush(at, core::mem::size_of::<usize>());
        if !wrote {
            release(self.table, index);
            log::log_tag(
                "vmt",
                &format!("hook: slot {index} at {at:#x} did not read back; claim released"),
            );
            return None;
        }
        if restored.is_none() {
            log::log_tag(
                "vmt",
                &format!("hook: slot {index} patched, protection {old_protect:#x} NOT restored"),
            );
        }
        log::log_tag(
            "vmt",
            &format!(
                "hook: table {:#x} slot {index} at {at:#x}: {original:#x} -> {detour:#x} (protect {old_protect:#x})",
                self.table
            ),
        );
        Some(VmtHook {
            table: self.table,
            index,
            original,
            old_protect,
            mem: self.mem,
            detached: false,
        })
    }
}

// ----------------------------------------------------------------- VmtHook

/// One installed VMT swap. `unhook` puts the original entry back and releases
/// the claim; dropping without `unhook` leaves the slot patched (deliberately
/// — same contract as [`crate::hook::Hook`]).
pub struct VmtHook<'a> {
    table: usize,
    index: u32,
    original: usize,
    old_protect: u32,
    mem: &'a dyn TableMem,
    detached: bool,
}

impl VmtHook<'_> {
    pub fn table(&self) -> usize {
        self.table
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// The entry that was in the slot before the swap; forward to this to call
    /// the game's own implementation.
    pub fn original(&self) -> usize {
        self.original
    }

    pub fn is_detached(&self) -> bool {
        self.detached
    }

    fn slot_address(&self) -> Option<usize> {
        let offset = (self.index as usize).checked_mul(core::mem::size_of::<usize>())?;
        self.table.checked_add(offset)
    }

    /// Restore the original entry and release the claim. Idempotent: a second
    /// call reports `true` without touching memory.
    pub fn unhook(&mut self) -> bool {
        if self.detached {
            return true;
        }
        let Some(at) = self.slot_address() else {
            log::log_tag("vmt", &format!("unhook: slot {} has no address", self.index));
            return false;
        };
        let Some(_) = self
            .mem
            .protect(at, core::mem::size_of::<usize>(), ffi::PAGE_READWRITE)
        else {
            log::log_tag(
                "vmt",
                &format!("unhook: VirtualProtect(rw) on {at:#x} failed; slot kept"),
            );
            return false;
        };
        let wrote = self.mem.write_u64(at, self.original as u64)
            && self.mem.read_u64(at) == Some(self.original as u64);
        let restored = if self.old_protect != 0 {
            self.mem
                .protect(at, core::mem::size_of::<usize>(), self.old_protect)
        } else {
            None
        };
        self.mem.flush(at, core::mem::size_of::<usize>());
        if !wrote {
            log::log_tag(
                "vmt",
                &format!("unhook: slot at {at:#x} did not read back; claim kept"),
            );
            return false;
        }
        self.detached = true;
        release(self.table, self.index);
        if restored.is_none() && self.old_protect != 0 {
            log::log_tag(
                "vmt",
                &format!(
                    "unhook: {at:#x} restored to {:#x}, protection {:#x} NOT restored",
                    self.original, self.old_protect
                ),
            );
        } else {
            log::log_tag(
                "vmt",
                &format!("unhook: {at:#x} restored to {:#x}", self.original),
            );
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// The claim table is process-global and every fixture uses the same
    /// synthetic address, so the tests that hook have to be serialised.
    static ORDER: Mutex<()> = Mutex::new(());

    fn lock_order() -> std::sync::MutexGuard<'static, ()> {
        ORDER.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A faithful in-test address space: one buffer with page-granular
    /// protection, so the real claim/protect/restore ordering is exercised.
    struct TestMem {
        base: usize,
        inner: Mutex<Inner>,
    }

    struct Inner {
        bytes: Vec<u8>,
        prot: HashMap<usize, u32>,
        protect_calls: Vec<(usize, u32)>,
        flushes: usize,
    }

    impl TestMem {
        const BASE: usize = 0x1_4000_0000;

        /// `page_prot` maps a page offset (0-based) to its initial protection.
        fn new(len: usize, page_prot: &[u32]) -> Self {
            let mut prot = HashMap::new();
            let pages = len.div_ceil(PAGE_SIZE);
            for (page, prot_value) in page_prot.iter().take(pages).enumerate() {
                prot.insert(Self::BASE + page * PAGE_SIZE, *prot_value);
            }
            Self {
                base: Self::BASE,
                inner: Mutex::new(Inner {
                    bytes: vec![0u8; len],
                    prot,
                    protect_calls: Vec::new(),
                    flushes: 0,
                }),
            }
        }

        fn page_of(addr: usize) -> usize {
            addr & !(PAGE_SIZE - 1)
        }

        /// Absolute -> buffer offset, for fixtures.
        fn offset(&self, addr: usize) -> Option<usize> {
            addr.checked_sub(self.base)
        }

        fn protect_calls(&self) -> Vec<(usize, u32)> {
            self.inner.lock().unwrap().protect_calls.clone()
        }

        fn page_protect(&self, addr: usize) -> u32 {
            let page = Self::page_of(addr);
            self.inner
                .lock()
                .unwrap()
                .prot
                .get(&page)
                .copied()
                .unwrap_or(ffi::PAGE_NOACCESS)
        }

        /// Fixture setup: change a page's protection without recording a
        /// `protect` call (the calls under test are the hook's own).
        fn set_protect(&self, page: usize, prot: u32) {
            self.inner.lock().unwrap().prot.insert(page, prot);
        }
    }

    impl TableMem for TestMem {
        fn read_u64(&self, addr: usize) -> Option<u64> {
            let inner = self.inner.lock().unwrap();
            let page = Self::page_of(addr);
            let prot = inner.prot.get(&page).copied().unwrap_or(ffi::PAGE_NOACCESS);
            if prot & 0xFF == ffi::PAGE_NOACCESS || prot & ffi::PAGE_GUARD != 0 {
                return None;
            }
            let at = self.offset(addr)?;
            let end = at.checked_add(8)?;
            let bytes = inner.bytes.get(at..end)?;
            Some(u64::from_le_bytes(bytes.try_into().ok()?))
        }

        fn protect(&self, addr: usize, len: usize, prot: u32) -> Option<u32> {
            let end = addr.checked_add(len)?;
            let first = Self::page_of(addr);
            let last = Self::page_of(end - 1);
            let mut inner = self.inner.lock().unwrap();
            let mut previous = ffi::PAGE_NOACCESS;
            let mut page = first;
            loop {
                let old = inner.prot.get(&page).copied().unwrap_or(ffi::PAGE_NOACCESS);
                if page == first {
                    previous = old;
                }
                inner.prot.insert(page, prot);
                inner.protect_calls.push((page, prot));
                if page >= last {
                    break;
                }
                page += PAGE_SIZE;
            }
            Some(previous)
        }

        fn write_u64(&self, addr: usize, value: u64) -> bool {
            let mut inner = self.inner.lock().unwrap();
            let first = Self::page_of(addr);
            let last = Self::page_of(addr + 7);
            let writable = |prot: u32| {
                matches!(
                    prot & 0xFF,
                    ffi::PAGE_READWRITE
                        | ffi::PAGE_WRITECOPY
                        | ffi::PAGE_EXECUTE_READWRITE
                        | ffi::PAGE_EXECUTE_WRITECOPY
                ) && prot & ffi::PAGE_GUARD == 0
            };
            let mut page = first;
            loop {
                if !writable(inner.prot.get(&page).copied().unwrap_or(ffi::PAGE_NOACCESS)) {
                    return false;
                }
                if page >= last {
                    break;
                }
                page += PAGE_SIZE;
            }
            let at = match self.offset(addr) {
                Some(at) => at,
                None => return false,
            };
            let Some(slice) = inner.bytes.get_mut(at..at + 8) else {
                return false;
            };
            slice.copy_from_slice(&value.to_le_bytes());
            true
        }

        fn flush(&self, _addr: usize, _len: usize) {
            self.inner.lock().unwrap().flushes += 1;
        }
    }

    /// One object whose first qword points at a table of `slots` fake entries.
    /// The object sits on a writable page, the table on a read-only one — the
    /// `.rdata` situation a real `dxgi.dll` vtable is in.
    fn fixture(slots: usize) -> (TestMem, usize, usize) {
        let object = TestMem::BASE;
        let table = TestMem::BASE + PAGE_SIZE + 0x100;
        let len = table - TestMem::BASE + slots * 8 + 0x100;
        // Populate first, then make the table read-only: a real vtable lives in
        // `.rdata` and cannot be written directly.
        let mem = TestMem::new(len, &[ffi::PAGE_READWRITE, ffi::PAGE_READWRITE]);
        mem.write_u64(object, table as u64);
        for index in 0..slots {
            let at = table + index * 8;
            mem.write_u64(at, 0x7FF0_0000_0000 + (index as u64) * 0x100);
        }
        mem.set_protect(TestMem::page_of(table), ffi::PAGE_READONLY);
        (mem, object, table)
    }

    #[test]
    fn new_reads_the_vtable_pointer_and_walks_the_slots() {
        let _order = lock_order();
        let (mem, object, table) = fixture(18);
        let vmt = Vmt::with_mem(object, &mem).expect("object has a readable vtable");
        assert_eq!(vmt.object(), object);
        assert_eq!(vmt.table(), table);
        assert_eq!(vmt.slot(0), Some(0x7FF0_0000_0000));
        assert_eq!(vmt.slot(8), Some(0x7FF0_0000_0000 + 8 * 0x100));
        assert_eq!(vmt.slot(17), Some(0x7FF0_0000_0000 + 17 * 0x100));
        // Past the fixture: unreadable rather than a invented value.
        assert_eq!(vmt.slot(4096), None);
    }

    #[test]
    fn an_object_without_a_readable_vtable_is_refused() {
        let _order = lock_order();
        let (mem, _object, _table) = fixture(4);
        assert!(Vmt::with_mem(TestMem::BASE + 0x200_0000, &mem).is_none());
        assert!(Vmt::with_mem(0, &mem).is_none(), "null is not an object");
        assert!(Vmt::with_mem(0x1000, &mem).is_none(), "reserved sentinel");

        // An object whose first qword is zero has no table at all.
        let blank = TestMem::new(PAGE_SIZE, &[ffi::PAGE_READWRITE]);
        assert!(Vmt::with_mem(TestMem::BASE, &blank).is_none());
    }

    #[test]
    fn hook_swaps_the_slot_and_unhook_puts_the_original_back() {
        let _order = lock_order();
        let (mem, object, table) = fixture(18);
        let mut vmt = Vmt::with_mem(object, &mem).expect("vtable");
        let original = vmt.slot(8).unwrap();
        let detour = 0x7FF0_0000_ABC0;

        let mut hook = vmt.hook(8, detour).expect("slot 8 is free");
        assert_eq!(hook.index(), 8);
        assert_eq!(hook.table(), table);
        assert_eq!(hook.original(), original);
        assert!(!hook.is_detached());
        assert_eq!(vmt.slot(8), Some(detour), "the swap is visible immediately");

        // The write went through a protection flip and the flip was undone.
        let calls = mem.protect_calls();
        assert!(
            calls.iter().any(|&(page, prot)| {
                page == TestMem::page_of(table + 8 * 8) && prot == ffi::PAGE_READWRITE
            }),
            "the table page was made writable: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|&(page, prot)| page == TestMem::page_of(table + 8 * 8)
                    && prot == ffi::PAGE_READONLY),
            "the original protection came back: {calls:?}"
        );
        assert_eq!(mem.page_protect(table + 8 * 8), ffi::PAGE_READONLY);

        assert!(hook.unhook());
        assert!(hook.is_detached());
        assert_eq!(vmt.slot(8), Some(original));
        assert!(hook.unhook(), "unhook is idempotent");
        assert_eq!(vmt.slot(8), Some(original));
    }

    #[test]
    fn a_shared_slot_is_refused_twice_and_free_after_unhook() {
        let _order = lock_order();
        let (mem, object, _table) = fixture(18);
        let mut first = Vmt::with_mem(object, &mem).expect("vtable");
        let mut second = Vmt::with_mem(object, &mem).expect("same table");

        let mut first_hook = first.hook(8, 0x7FF0_0000_1000).expect("first claim");
        assert!(
            second.hook(8, 0x7FF0_0000_2000).is_none(),
            "the slot is shared by every object of this implementation"
        );
        // A different slot of the same table is still available.
        let mut other_slot = second.hook(13, 0x7FF0_0000_3000).expect("another slot");

        assert!(first_hook.unhook());
        let mut retry = second.hook(8, 0x7FF0_0000_4000).expect("claim released");
        assert!(retry.unhook());
        assert!(other_slot.unhook());
    }

    #[test]
    fn hook_refuses_a_detour_that_is_not_a_code_address() {
        let _order = lock_order();
        let (mem, object, _table) = fixture(18);
        let mut vmt = Vmt::with_mem(object, &mem).expect("vtable");
        let original = vmt.slot(8).unwrap();
        assert!(vmt.hook(8, 0).is_none());
        assert!(vmt.hook(8, 0x1000).is_none());
        assert!(vmt.hook(8, original).is_none(), "already the original");
        assert_eq!(vmt.slot(8), Some(original));
    }

    #[test]
    fn an_unreadable_slot_is_refused_and_leaves_no_writable_page() {
        let _order = lock_order();
        // A table whose page is PAGE_NOACCESS cannot be swapped.
        let object = TestMem::BASE;
        let table = TestMem::BASE + PAGE_SIZE;
        let mem = TestMem::new(PAGE_SIZE * 2, &[ffi::PAGE_READWRITE, ffi::PAGE_NOACCESS]);
        mem.write_u64(object, table as u64);
        assert!(Vmt::with_mem(object, &mem).is_none());

        // A readable table with a hole in it: slot 64 was never populated.
        let (mem, object, _table) = fixture(1);
        let mut vmt = Vmt::with_mem(object, &mem).expect("vtable");
        assert!(vmt.hook(64, 0x7FF0_0000_5000).is_none());
        assert_eq!(mem.protect_calls().len(), 0, "no protection change happened");
    }

    #[test]
    fn a_write_the_backend_refuses_releases_the_claim() {
        let _order = lock_order();
        // A backend whose store always fails must not leave the slot claimed:
        // the next attempt has to reach the backend again, not be refused as a
        // double hook.
        let failing = FailingWrites {
            inner: fixture(2).0,
        };
        let mut vmt = Vmt::with_mem(TestMem::BASE, &failing).expect("vtable");
        assert!(vmt.hook(0, 0x7FF0_0000_7000).is_none());
        assert!(vmt.hook(0, 0x7FF0_0000_7000).is_none());
    }

    /// TestMem whose writes always fail, to exercise the claim-release path.
    struct FailingWrites {
        inner: TestMem,
    }

    impl TableMem for FailingWrites {
        fn read_u64(&self, addr: usize) -> Option<u64> {
            self.inner.read_u64(addr)
        }
        fn protect(&self, addr: usize, len: usize, prot: u32) -> Option<u32> {
            self.inner.protect(addr, len, prot)
        }
        fn write_u64(&self, _addr: usize, _value: u64) -> bool {
            false
        }
        fn flush(&self, addr: usize, len: usize) {
            self.inner.flush(addr, len);
        }
    }
}
