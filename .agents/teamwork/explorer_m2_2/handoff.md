# Handoff Report — `explorer_m2_2`: Unified `crates/hromatite-hook` Design (Stage 2 / M2)

**Core finding:** Unify `crates/hromatite-core/src/raw/hook.rs:1-945`, `crates/hromatite-eac/src/hook.rs`, `midhook.rs`, `vmt.rs`, `iat.rs`, and `hookset.rs` into `crates/hromatite-hook` (depending **only** on `hromatite-win`) around a shared `decode` + 2-pass `tramp` + `mem` + `table` core that serves both typed frontends: `inline` (5-byte `jmp rel32` + relay page + `unhook`) and `abs` (12-byte `mov rax; jmp rax` + `Local\hromatite_rust_hooktable_v1`), while leaving `LateUpdate`/tick/GC-guard orchestration in `hromatite-core` during Stage 2.

---

## 1. Observation

### 1.1 Engine 1: `crates/hromatite-core/src/raw/hook.rs` (lines 1–945 vs 946–2569)
- **Lines 1–13**: imports `crate::console`, `crate::ffi`, `crate::log` (only `ffi` and `log` are used in 1–945; `console` is used only in `work`/`prepare_tick`/`command` starting at line 956).
- **Lines 26–299 (Decoder)**: `Insn { len: usize, rel: Option<(usize, u8)>, rip: Option<usize> }`, `modrm_len`, `decode_one`, `decode_span`, `prologue_len(bytes, min)`.
  - Handles `0x0F 0x1E 0xFA|0xFB` (`endbr64`/`endbr32`, lines 120–127), enforces `imm16 = opsize_66 && !rex_w` (line 110, tested at 2263–2282), rejects legacy prefix after `REX` (lines 87–100, tested at 2266), whitelists known `0x0F` opcodes (116–160).
- **Lines 305–450 (Near Allocator & Math)**: `REL32_SPAN = 0x7F00_0000`, `PAGE_SIZE = 0x1000`, `rel32_ok`, `adjusted_disp32`, `write_mode(o16)` (`0..=3 => Some(8)`, `4..=11 => Some(16)`, `12..=15 => None`), `branch_into_prologue`, `nearest_free_near` (region-walking `VirtualQuery` selecting minimum distance `MEM_FREE`), `enum_near` (`±k*64K` `VirtualAlloc` fallback), `alloc_near(target, size)` (`PAGE_READWRITE`).
- **Lines 457–945 (5-Byte Relay Engine)**:
  - `RELAY_OFF = 0x100` (580), `Prep { target, prologue, trampoline, relay, alloc_base, alloc_size }` (582–590), `Hook { target, detour, trampoline, prologue, alloc_base, alloc_size, orig32, base16, wmode, detached }` (457–471).
  - `write_window(base16, target, new32, wmode)` (483–556): aligned `write_volatile::<u64>` for `wmode == 8`, `lock cmpxchg16b` loop for `wmode == 16`, returns `false` for anything else.
  - `prepare(target)` -> `prepare_span(target, 5)` (595–751): checks `target >= 0x10000 && readable_range(target, 16)`, 1-pass relocator (refuses `branch_into_prologue` and out-of-range `rel8`), writes relocated copy + `ff 25 00000000 <target+prologue>` + relay slot `ff 25 00000000 <0>`, flips page to `RWX`.
  - `patch(p: &Prep, detour: usize) -> Option<Hook>` (754–861): checks `rel32` reach to `p.relay`, temporarily makes trampoline `RWX` (supports re-patching a retained `Prep`), writes `detour` at `p.relay + 6`, restores `PAGE_EXECUTE_READ`, calls `flush`, snapshots 32 bytes at `base16`, writes `E9 rel32` atomically via `write_window`, restores protection (`if old & 0xF0 != 0 { old } else { PAGE_EXECUTE_READ }`).
  - `hook_inline(target, detour) -> Option<Hook>` (874–877), `Hook::unhook(&mut self) -> bool` (881–930), `Hook::describe(&self)` (932–943).
- **Lines 946–2159 (Non-engine domain code)**: `tick`, `work`, `HOOKS` + `register`, legacy mid-function GC guard (`1069–1338`), `install_gc_context_guard` (1343), `install_gc_worker_trace` (1543), `late_update_detour` (1693), `materialise_late_class` (1733), `install_late_update` (1849), `late_update_status` (1922), `command` (2031).
- **Lines 2161–2568 (Unit tests)**: 8 engine tests (2173–2290) + 17 LateUpdate/GC-guard tests (2295–2567; line 2406 uses `prologue_len`).

### 1.2 Engine 2: `crates/hromatite-eac/src/hook.rs` (1,792 lines) & `tests/hook_engine.rs` (261 lines)
- **Constants & Decoder (38–348)**:
  - `PATCH_LEN = 12`, `NEAR_SPAN = 0x2000_0000` (±512 MiB), `TRAMP_PAGE = 0x1000`.
  - `Insn { len, rip_rel, disp_off, rel, rel_off, rel_size, op_off, cond }` (50–69).
  - `decode(code)` (114–314) adds `0x0F 0x38` (3-byte ModRM), `0x0F 0x3A` (3-byte ModRM+imm8), `0x0F 0x70..=0x73/0xBA/0xC2/0xC4/0xC5` (ModRM+imm8), `0x0F 0x0D/0x31/0x34/0x35/0x77/0xA2`, `0x69`/`0x6B` (`imul`), `0x04..=0x3D` (`alu al/eax, imm`), `0xF5/0xF8/0xF9/0xFC/0xFD/0xCE/0x9E/0x9F`.
  - Differences/defects found vs Engine 1:
    1. `eac/hook.rs:186,239` checks `opsz16` without `!rex_w` on `0x68` and `0xA9` (fails `[0x66, 0x48, 0x68, ...]` tested at `raw/hook.rs:2267`).
    2. `eac/hook.rs:179` has wildcard `_ => has_modrm = true` on `0x0F`, which would misdecode non-ModRM `0x0F` opcodes (`0x0F 0xC8..=0xCF` `bswap`, `0x0F 0xA0/0xA1` `push/pop fs`, `0x0F 0x06` `clts`) instead of rejecting unknown opcodes per `x64-trampoline-relocation` Rule 7.
    3. `eac/hook.rs:87` includes `0x62` in `one_byte_has_modrm` (32-bit `bound`, invalid/EVEX prefix in x64).
    4. `eac/hook.rs` does not explicitly recognize `0x0F 0x1E (0xFA|0xFB)` (`endbr64`/`endbr32`, though `0xFA` happens to parse as ModRM).
  - `prologue_span(code) -> Result<(usize, usize), String>` (319–342) and `prologue_len(code) -> Option<usize>` (346–348): enforces `insns <= 8` and `n <= 24` for 12-byte patches, names the first offending opcode & offset on failure.
- **2-Pass Trampoline Relocator (354–797)**:
  - `ABS_JUMP = 14` (`ff 25 00000000 <qword>`), `ABS_CALL = 16` (`ff 15 02000000 eb 08 <qword>`).
  - `abs_jmp(addr) -> [u8; 12]` (`48 b8 <addr> ff e0`) (383), `looks_patched(code) -> Option<usize>` (394).
  - `Reloc` (412), `Built` (438), `Form` (461–513), `Step` (518–528), `plan` (541–602), `map_target` (606–624), `build_trampoline(tramp, base, code, total) -> Result<Built, String>` (638–797).
  - Implements all 8 rules of `x64-trampoline-relocation`: 2-pass layout before emission, internal branches remapped to `tramp_addr + step.new` (mid-instruction targets rejected), inverted `jcc` (`^= 1`) over `jmp [rip+0]`, `loop`/`jrcxz` (`E0..E3`) `op +2 ; jmp +14 ; jmp [rip+0]`, register-free `jmp [rip+0]` / `call [rip+2]; jmp +8`, `rel8` never kept across pages, unknown opcodes rejected, per-detour `Reloc` metadata recorded.
- **Win32 FFI & Shared Table (802–1339)**:
  - `Hook { target, tramp, copied, note, insns, relocs, mode }` (802–819) + `Hook::describe(&self)` (825–849).
  - `mod win` (865–917) duplicates `VirtualAlloc`/`VirtualQuery`/`VirtualProtect`/`FlushInstructionCache`/`GetCurrentProcess`/`MemBasic` and adds `CreateFileMappingA`/`MapViewOfFile`/`FILE_MAP_ALL_ACCESS`.
  - `readable(addr, len)` (925–945), `alloc_near` (953–1001, allocates **RWX** and falls back to `VirtualAlloc(NULL)`), `cas16` (1010–1051), `patch_entry` (1060–1121, uses `cas16` when `target % 16 <= 4` and `copy_nonoverlapping` fallback otherwise).
  - Shared named section `Local\hromatite_rust_hooktable_v1` (`RawEntry`, `RawTable`, `table_lookup`, `table_store` at 1124–1220, 64 slots).
  - `install(target, handler, name) -> Result<Hook, String>` (1232–1339): reuses/re-owns via shared table (`mode` 1/2/3); line 1337 calls `crate::eos::hook_log(...)` (the ONLY coupling to `EOSSDK`!).
- **Unit tests (1341–1791)** & **`tests/hook_engine.rs` (1–261)**: tests `hook::{install, PATCH_LEN, looks_patched, prologue_len}` with `global_asm!` probes (`tramp_probe`, `branch_probe`) and `hooked_add`.

### 1.3 Companion Modules in `crates/hromatite-core/src/raw/`
- **`midhook.rs` (513 lines)**:
  - Imports `crate::ffi`, `crate::log`, and 11 `pub(crate)` items from `super::hook` (lines 24–27).
  - `pub struct MidHook { pub target, pub detour, pub trampoline, pub prologue, ... }` with `unhook(&mut self) -> bool` and `describe(&self) -> String`; `pub fn install(target, detour) -> Option<MidHook>`.
  - Internal `pub(crate) fn plan(target, head, trampoline) -> Result<Plan, PlanError>` currently uses 1-pass relocation that rejects `rel8` and intra-window branches (tested at 431–462).
- **`hookset.rs` (331 lines)**:
  - `pub enum Kind { Inline, Mid }`, `pub enum HookError { Unreadable, DuplicateTarget, AddressOccupied, InstallFailed }`, `pub struct HookSet`.
  - Uses `crate::log`, `crate::safe::readable_range`, `super::hook::{hook_inline, Hook}`, `super::midhook::{install, MidHook}`.
  - Methods: `new`, `default`, `install_inline`, `install_mid`, `uninstall`, `clear`, `status`, `len`, `is_empty`. 6 unit tests.
- **`vmt.rs` (718 lines)**:
  - `pub trait TableMem: Send + Sync`, `pub struct Live`, `pub struct Vmt<'a>`, `pub struct VmtHook<'a>`.
  - Uses `crate::ffi`, `crate::log`, `crate::safe::{read_u64, write_usize}`.
  - `CLAIMED: Mutex<Vec<(usize, u32)>>` prevents double-hooking a shared vtable slot. 7 unit tests.
- **`iat.rs` (1,044 lines)**:
  - Pure PE reader layer: `Reader`, `same_dll`, `OrdinalNames`, `exporter_ordinals`, `Slot`, `Want<'a>`, `slots`, `first_slot`, `imports_dll`.
  - Live layer: `Outcome`, `status()`, `patch_module(module, dll, wanted)`, `replace(module, dll, func, replacement)`, `find_slot(module, dll, func)`, `module_imports(module, dll)`.
  - Uses `crate::ffi`, `crate::safe::{read_bytes, write_bytes, read_usize}`, `crate::procs::{own_module, module}`, `crate::raw::sys::{current_process_id, export_address}`. 8 unit tests.

### 1.4 Complete Caller Audit
| Caller File | Exact Imports / Calls | Dependency on `LateUpdate` / `EOSSDK`? |
|---|---|---|
| `core/src/raw/gcguard.rs:9, 667–690, 819–870` | `Prep, prepare, patch` (needs 5-byte patch on 6B `skip` and 7B `after` sites! Caches `&'static Prep` in `AtomicUsize` across detach/reattach) + `register` (stays in `core::raw::hook` in Stage 2) | No (`register` stays in `core` for `HOOKS`) |
| `core/src/raw/playerloop.rs:421, 426, 439, 447` | `prepare`, `patch`, `register`, plus `tick()`/`prepare_tick()` (stay in `core::raw::hook` in Stage 2) | `tick()`/`prepare_tick()` stay in `core` |
| `core/src/raw/photon.rs:1973, 1980, 1990, 2011, 2015, 2024` | `prepare`, `patch`, `register` | No |
| `core/src/raw/kiero.rs:36, 163, 238, 312, 317, 362, 366, 374, 417, 592` | `Vmt, VmtHook, Live, TableMem` (`vmt`); `Hook, hook_inline, Hook::unhook` (`inline`) | No |
| `core/src/raw/hookset.rs:17–18, 125–126` | `hook::{hook_inline, Hook}`, `midhook::{install, MidHook}` | Moves into `hromatite-hook` |
| `core/src/raw/console.rs:1131` | `crate::hook::command(line)` (stays in `core::raw::hook` in Stage 2) | Stays in `core` |
| `hromatite-dll/src/lib.rs:9, 113, 125, 263, 274` | `hook::{materialise_late_class, install_late_update, install_gc_context_guard, install_gc_worker_trace}` (stay in `core::raw::hook` in Stage 2) | Stay in `core` |
| `hromatite-eac/src/eos.rs:75, 682, 1005, 1181, 1187, 1199, 1267, 1283, 1398, 1421, 1424` | `hook::install` (`tramp`, `mode`, `copied`, `note`), `hook::readable` (4 calls -> `hromatite_win::memory::readable_range`), `iat::{status, module_imports, patch_module}` | Only `eos.rs` calls `eos::hook_log` |
| `hromatite-eac/src/evp.rs:264, 282–290, 580` | `hook::install` (`tramp`, `copied`, `insns`, `note`, `describe()`), `hook::win::{MemBasic, VirtualQuery}` (-> `hromatite_win::memory::readable_range`) | `evp.rs` logs `installed.describe()` to `eos::hook_log` |
| `hromatite-eac/src/lib.rs:57, 70` | `pub mod hook; pub use hook::Hook;` | Re-exports `hromatite_hook::abs` |
| `hromatite-eac/tests/hook_engine.rs` | `hook::{install, PATCH_LEN, looks_patched, prologue_len}` | Moves to `hromatite-hook/tests/` |

---

## 2. Logic Chain

1. **Both patch formats are irreducibly required (Observation 1.1, 1.2, 1.4):**
   - `gcguard.rs` requires a **5-byte** patch (`SKIP_CLOSE_LEN = 6`, `AFTER_UNLINK_LEN = 7`) and two-phase `prepare` + `patch` with `&'static Prep` reuse across detach/reattach; a 12-byte patch cannot fit in a 6B/7B site.
   - `eos.rs`, `evp.rs`, and `hook_engine.rs` require a **12-byte** position-independent `mov rax, imm64; jmp rax` patch (`PATCH_LEN = 12`) with named shared table coordination (`Local\hromatite_rust_hooktable_v1`, `mode` 1/2/3) and non-16B-aligned fallback in `patch_entry` (needed for arbitrary DLL exports where `target % 16 > 4`).
2. **Decoder, 2-pass trampoline relocator, and OS/atomic primitives unify cleanly underneath both frontends (Observation 1.1, 1.2, 1.3):**
   - Merging the superset of valid x64 opcodes into `decode.rs` (preserving `imm16 = opsize_66 && !rex_w`, `endbr64/32`, `0F 38`/`0F 3A`/`imul`/`al/eax`, while removing `0x62` and wildcard `_ => has_modrm = true` on `0F`) fixes both decoders' edge-case bugs and satisfies `x64-trampoline-relocation` Rule 7.
   - Moving `build_trampoline` (2-pass planner/emitter) into `tramp.rs` allows `abs::install`, `inline::prepare_span`, AND `midhook::install` to share one battle-tested relocator complying with all 8 rules of `x64-trampoline-relocation` (fixing `raw/hook.rs` and `midhook.rs`'s previous failure on `jcc rel8` and intra-prologue branches).
   - Unifying `alloc_near` (region-walking `nearest_free_near` picking the closest boundary of `MEM_FREE`, plus `enum_near`, allocating `PAGE_READWRITE` and flipping to `PAGE_EXECUTE_READ` after write + `FlushInstructionCache`) eliminates steady-state RWX trampoline pages in `abs` and `inline` alike, and shares one `cas16` (`lock cmpxchg16b`) implementation between `write_window` and `patch_entry`.
3. **Zero domain coupling in `hromatite-hook` (Observation 1.1, 1.2, 1.4):**
   - `LateUpdate`, `tick()`, `HOOKS`/`register()`, `gcguard` detours, and console `command()` remain in `hromatite-core::raw::hook` during Stage 2 (and dissolve into `hromatite-il2cpp`/`hromatite-runtime` in Stages 3–4).
   - `eos::hook_log` is removed from `abs::install`; `hromatite-hook` logs through `hromatite_win::log::log_tag("hook", ...)` and returns `Hook::describe()`, which `eos.rs`/`evp.rs` log to `eos::hook_log`.
   - `eos.rs` (`readable`) and `evp.rs` (`hook::win::VirtualQuery`) switch directly to `hromatite_win::memory::readable_range` (removing Win32 probing from `hromatite-hook`).
4. **Shared `table.rs` OS dependency requires `hromatite-win::ffi` alignment (Observation 1.2):**
   - `eac/hook.rs:893-915` is the only place using `CreateFileMappingA`, `MapViewOfFile`, and `FILE_MAP_ALL_ACCESS`. Placing these declarations in `hromatite-win::ffi` (under `#[cfg(windows)]`) keeps `hromatite-hook` 100% free of its own `extern "system"` Win32 block.

---

## 3. Proposed Design for `crates/hromatite-hook`

### 3.1 `Cargo.toml` & Crate Root
```toml
[package]
name = "hromatite-hook"
version = "0.1.0"
edition = "2024"

[lib]
name = "hromatite_hook"
path = "src/lib.rs"

[dependencies]
hromatite-win = { path = "../hromatite-win" }

[lints]
workspace = true
```
Add `"crates/hromatite-win"` and `"crates/hromatite-hook"` to root `Cargo.toml` `members`.

`crates/hromatite-hook/src/lib.rs`:
- `#![deny(unsafe_code)]` (with `#[allow(unsafe_code)]` only on `mem`, `table`, `inline`, `abs`, `midhook`, `vmt`, `iat` where OS/asm/raw-ptr ops exist; `decode`, `tramp`, `hookset` stay safe).
- Module tree and public exports:
  ```rust
  pub mod decode;
  pub mod tramp;
  #[allow(unsafe_code)]
  pub mod mem;
  #[allow(unsafe_code)]
  pub mod table;
  #[allow(unsafe_code)]
  pub mod inline;
  #[allow(unsafe_code)]
  pub mod abs;
  #[allow(unsafe_code)]
  pub mod midhook;
  pub mod hookset;
  #[allow(unsafe_code)]
  pub mod vmt;
  #[allow(unsafe_code)]
  pub mod iat;

  pub use abs::{Hook as AbsHook, install as install_abs};
  pub use hookset::{HookError, HookSet, Kind as HookKind};
  pub use inline::{Hook as InlineHook, Prep, hook_inline, patch, prepare, prepare_span};
  pub use midhook::{MidHook, install as install_mid};
  pub use vmt::{Live as VmtLive, TableMem, Vmt, VmtHook};
  ```

### 3.2 Exact Module Responsibilities & Public Signatures

1. **`decode.rs`** (pure Rust, host-testable):
   - `pub struct Insn { pub len: usize, pub op_off: usize, pub rip_rel: bool, pub disp_off: usize, pub rel: bool, pub rel_off: usize, pub rel_size: u8, pub cond: bool }`
     - Helpers: `pub fn rel_field(&self) -> Option<(usize, u8)>`, `pub fn rip_field(&self) -> Option<usize>`.
   - `pub fn decode(code: &[u8]) -> Option<Insn>` (alias/re-export `decode_one` for compatibility).
   - `pub fn decode_span(code: &[u8], min: usize, max_insns: usize, max_bytes: usize) -> Result<(usize, usize, Vec<Insn>), String>`
   - `pub fn prologue_span(code: &[u8], min: usize) -> Result<(usize, usize), String>` (names offending byte & offset per Rule 7; defaults: `max_insns = 32, max_bytes = 32` for `min == 5`, `max_insns = 8, max_bytes = 24` when called via `abs::prologue_span`).
   - `pub fn prologue_len(code: &[u8], min: usize) -> Option<usize>`

2. **`tramp.rs`** (pure Rust, host-testable, implements `x64-trampoline-relocation` Rules 1–8):
   - `pub const ABS_JUMP: usize = 14;`
   - `pub const ABS_CALL: usize = 16;`
   - `pub struct Reloc { pub kind: &'static str, pub at: usize, pub target: i64, pub distance: i64 }` (`impl Reloc { pub fn label(&self) -> String }`)
   - `pub struct Built { pub len: usize, pub note: String, pub insns: usize, pub relocs: Vec<Reloc> }`
   - `pub fn rel32_ok(from: usize, to: usize) -> bool`
   - `pub fn adjusted_disp32(original: i32, delta: i64) -> Option<i32>`
   - `pub fn branch_into_prologue(target: usize, prologue: usize, off: usize, len: usize, disp: i64) -> bool`
   - `pub fn build_trampoline(tramp: &mut [u8], base: usize, code: &[u8], total: usize) -> Result<Built, String>`
   - `pub fn build_trampoline_at(tramp: &mut [u8], tramp_addr: usize, base: usize, code: &[u8], total: usize) -> Result<Built, String>`

3. **`mem.rs`** (OS memory/protection/cache/atomic patching via `hromatite_win::ffi` + `hromatite_win::log`):
   - `pub const PAGE_SIZE: usize = 0x1000;`
   - `pub const REL32_SPAN: usize = 0x7F00_0000;`
   - `pub const NEAR_SPAN: usize = 0x2000_0000;`
   - `pub fn alloc_near(target: usize, size: usize) -> Option<usize>` (calls `alloc_near_within(target, size, REL32_SPAN, false)` — finds closest 64K-aligned end of `MEM_FREE` via `VirtualQuery`, falls back to `enum_near`; allocates `PAGE_READWRITE`).
   - `pub fn alloc_near_within(target: usize, size: usize, max_span: usize, allow_anywhere: bool) -> Option<usize>` (for `abs`, passes `NEAR_SPAN` + `true`).
   - `pub fn set_range_protect(addr: usize, len: usize, prot: u32) -> Option<u32>`
   - `pub fn flush(addr: usize, len: usize)`
   - `pub fn write_mode(offset_in_16: usize) -> Option<u8>`
   - `pub unsafe fn cas16(base16: usize, window16: &[u8; 16]) -> bool`
   - `pub unsafe fn write_window(base16: usize, target: usize, new32: &[u8; 32], wmode: u8) -> bool`

4. **`table.rs`** (named shared section `Local\hromatite_rust_hooktable_v1`, `#[cfg(windows)]` with no-op host stubs):
   - `pub struct TableEntry { pub target: u64, pub tramp: u64, pub handler: u64 }`
   - `pub fn lookup(target: usize) -> Option<TableEntry>`
   - `pub fn store(target: usize, tramp: usize, handler: usize, name: &str)`

5. **`inline.rs`** (5-byte `jmp rel32` + relay page + `unhook`):
   - `pub const RELAY_OFF: usize = 0x100;`
   - `pub struct Prep { pub target: usize, pub prologue: usize, pub trampoline: usize, pub relay: usize, alloc_base: usize, alloc_size: usize }`
   - `pub struct Hook { pub target: usize, pub detour: usize, pub trampoline: usize, pub prologue: usize, ... }`
   - `pub fn prepare(target: usize) -> Option<Prep>`
   - `pub fn prepare_span(target: usize, min: usize) -> Option<Prep>` (uses `tramp::build_trampoline` + checks `built.len <= RELAY_OFF`, logs relocation summary via `hromatite_win::log::log_tag("hook", ...)` per Rule 8, leaves page `PAGE_READWRITE`).
   - `pub fn patch(p: &Prep, detour: usize) -> Option<Hook>` (sets `PAGE_READWRITE` on trampoline page — handles retained `Prep` previously published as `RX` — writes `detour` into `p.relay + 6`, sets `PAGE_EXECUTE_READ`, flushes, atomically patches `E9 rel32` at `p.target` via `write_window`).
   - `pub fn hook_inline(target: usize, detour: usize) -> Option<Hook>`
   - `impl Hook`: `pub fn unhook(&mut self) -> bool`, `pub fn is_detached(&self) -> bool`, `pub fn describe(&self) -> String`.

6. **`abs.rs`** (12-byte `mov rax, imm64; jmp rax` + shared hook table):
   - `pub const PATCH_LEN: usize = 12;`
   - `pub struct Hook { pub target: usize, pub tramp: usize, pub copied: usize, pub note: String, pub insns: usize, pub relocs: usize, pub mode: u8 }` (`#[derive(Clone, Debug, PartialEq, Eq)]`)
   - `impl Hook { pub fn describe(&self) -> String }`
   - `pub fn prologue_span(code: &[u8]) -> Result<(usize, usize), String>` (enforces `min = 12`, `max_insns = 8`, `max_bytes = 24`)
   - `pub fn prologue_len(code: &[u8]) -> Option<usize>`
   - `pub fn abs_jmp(addr: usize) -> [u8; PATCH_LEN]`
   - `pub fn looks_patched(code: &[u8]) -> Option<usize>`
   - `pub unsafe fn patch_entry(target: usize, patch: &[u8; PATCH_LEN]) -> Result<(), String>` (uses `mem::cas16` if `target % 16 <= 4`, fallback `copy_nonoverlapping`)
   - `pub unsafe fn install(target: usize, handler: usize, name: &str) -> Result<Hook, String>` (checks `hromatite_win::memory::readable_range(target, 32)`, `table::lookup`/`store`, `tramp::build_trampoline`, writes into `PAGE_READWRITE` page then flips to `PAGE_EXECUTE_READ` + `flush`, logs via `hromatite_win::log::log_tag("hook", ...)` — zero `EOSSDK` reference).

7. **`midhook.rs`**, **`hookset.rs`**, **`vmt.rs`**, **`iat.rs`**:
   - Keep their exact existing public APIs (`MidHook`/`install`, `HookSet`/`Kind`/`HookError`, `TableMem`/`Live`/`Vmt`/`VmtHook`, `Reader`/`OrdinalNames`/`Slot`/`Want`/`Outcome`/`same_dll`/`exporter_ordinals`/`slots`/`first_slot`/`imports_dll`/`status`/`patch_module`/`replace`/`find_slot`/`module_imports`).
   - `midhook` uses `tramp::build_trampoline` + `mem::{alloc_near, write_window, set_range_protect, flush, RELAY_OFF}` + `hromatite_win::memory::{read_bytes, read_upto}`.
   - `hookset` uses `hromatite_win::memory::readable_range`, `hromatite_win::log::log_tag`, `inline::{hook_inline, Hook}`, `midhook::{install, MidHook}`.
   - `vmt` uses `hromatite_win::ffi::PAGE_READWRITE`, `hromatite_win::memory::{read_u64, write_usize}`, `hromatite_win::log::log_tag`, `mem::{set_range_protect, flush}`.
   - `iat` uses `hromatite_win::ffi::{VirtualProtectEx, GetCurrentProcess, GetLastError, PAGE_READWRITE, PAGE_READONLY}`, `hromatite_win::memory::{read_bytes, read_usize, write_bytes}`, `hromatite_win::procs::{own_module, module}`, `hromatite_win::sys::{current_process_id, export_address}`.

### 3.3 Caller Migration Plan (Stage 2)
- **`crates/hromatite-core`**:
  - Delete `crates/hromatite-core/src/raw/{midhook.rs, vmt.rs, iat.rs, hookset.rs}`.
  - Remove lines 1–945 and engine tests (2168–2290) from `crates/hromatite-core/src/raw/hook.rs`; import `Hook, Prep, prepare, patch, hook_inline` from `hromatite_hook::inline` and `prologue_len` from `hromatite_hook::decode`.
  - Re-export in `crates/hromatite-core/src/lib.rs`: `pub use hromatite_hook::{hookset, iat, midhook, vmt};` (and keep `pub use raw::hook;` for `LateUpdate`/`tick`/`register`/`install_gc_*` in Stage 2).
  - Update `raw/gcguard.rs`, `raw/playerloop.rs`, `raw/photon.rs`, `raw/kiero.rs` to import hook primitives from `hromatite_hook::{inline, vmt}` (keeping `register` from `crate::raw::hook` in Stage 2).
- **`crates/hromatite-eac`**:
  - Delete `crates/hromatite-eac/src/hook.rs`.
  - Move `crates/hromatite-eac/tests/hook_engine.rs` -> `crates/hromatite-hook/tests/hook_engine.rs` (switch `use hromatite_eac::hook;` -> `use hromatite_hook::abs as hook;`).
  - `crates/hromatite-eac/src/lib.rs`: replace `pub mod hook; pub use hook::Hook;` with `pub use hromatite_hook::abs as hook; pub use hromatite_hook::abs::Hook;`.
  - `crates/hromatite-eac/src/eos.rs`: use `hromatite_hook::abs as hook` (or `crate::hook`), `hromatite_hook::iat`, `hromatite_win::memory::readable_range` (lines 1181, 1187, 1199, 1267), and emit `hook_log(format!("[hook] {name}: {}", installed.describe()))` at line 1007.
  - `crates/hromatite-eac/src/evp.rs`: replace `hook::win::{MemBasic, VirtualQuery}` in `imp::readable` (lines 276–299) with `hromatite_win::memory::readable_range`.

---

## 4. Caveats

1. **`hromatite-win` coordination**: `hromatite-hook/src/table.rs` requires `CreateFileMappingA`, `MapViewOfFile`, and `FILE_MAP_ALL_ACCESS` (and `iat.rs` requires `VirtualProtectEx`, `GetCurrentProcess`, `GetLastError`, `PAGE_READWRITE`, `PAGE_READONLY`) in `hromatite-win::ffi` (or safe wrappers in `hromatite-win`). At the time of this report, `explorer_m2_1` has not yet emitted its `handoff.md`, so exact module path names in `hromatite-win` (`memory` vs `safe`, `ffi`) must be aligned with `explorer_m2_1`'s report before implementation.
2. **Host-test behavior change in `midhook::plan`**: Currently `midhook.rs:431–462` has two internal unit tests asserting `plan()` refuses `BranchIntoWindow` and `Rel8OutOfRange`. Once `midhook` uses unified `tramp::build_trampoline`, `jcc rel8` and instruction-boundary intra-window branches are *relocated* rather than refused (matching `x64-trampoline-relocation` Rules 2 & 6), so those two internal `midhook::tests` must be updated to assert relocation of `rel8`/instruction-boundary branches and refusal of *mid-instruction* targets and `Rel8` when intentionally testing `Form::Rel` limits.
3. **`abs` trampoline memory protection**: Changing `eac/hook.rs`'s `alloc_near` from `PAGE_EXECUTE_READWRITE` to `PAGE_READWRITE` -> `PAGE_EXECUTE_READ` (RX) improves L1 stealth; verified by reading all callers (`eos.rs`, `evp.rs`, `hook_engine.rs`, `eos_install.rs`) that none write to a trampoline after `abs::install` returns, though live Wine execution of `hook_engine.rs` and `eos_install.rs` must confirm no unexpected write-back.
4. **Stage 2 boundary**: `LateUpdate`, `tick()`, `HOOKS`/`register()`, `gcguard` detours, and `command()` stay in `hromatite-core::raw::hook` in Stage 2; removing them from `hromatite-core` is scoped to Stages 3–5 per `PROJECT.md`.

---

## 5. Verification Method

After implementing Stage 2 (`hromatite-win` + `hromatite-hook`):
1. **Boundary checks (`cargo tree`)**:
   - `cargo tree -p hromatite-hook` -> contains **only** `hromatite-win` (+ `std`); **0** occurrences of `hromatite-core`, `hromatite-eac`, `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear`, `LateUpdate`, or `EOSSDK`.
2. **Linux host tests & lints**:
   - `cargo test --workspace --target x86_64-unknown-linux-gnu` -> all existing 416+ tests pass (including migrated `decode`, `tramp`, `mem`, `midhook`, `hookset`, `vmt`, `iat`, `abs` tests inside `hromatite-hook`).
   - `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu` -> 0 errors, 0 warnings in `hromatite-hook`.
3. **Windows target build, test-compile & clippy**:
   - `cargo build --workspace` -> 0 errors, 0 warnings.
   - `cargo test --workspace --no-run` -> `hook_engine` integration test and `eos_install` compile cleanly under `x86_64-pc-windows-gnu`.
   - `cargo clippy --workspace --all-targets` -> 0 errors, 0 warnings (`unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc` satisfied).
4. **Optional Wine live smoke (if stub/Wine runner available per `x64-trampoline-relocation`)**:
   - Run the compiled `hook_engine-*.exe` test binary under Proton/Wine in a throwaway `WINEPREFIX` to verify `inline_hook_redirects_calls_and_the_trampoline_preserves_the_result`, `a_rel8_jcc_pointing_into_the_patch_window_is_relocated_and_still_branches`, and `a_prologue_with_a_rel32_call_is_relocated_and_the_callee_still_runs` (plus an `inline::hook_inline` + `unhook()` probe).
