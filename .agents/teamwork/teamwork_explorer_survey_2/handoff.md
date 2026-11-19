# Survey Explorer 2 — Core, EAC, Features, Nuklear, DLL & Duplicate Subsystems Audit

**Core Finding:** Across `src/`, `hromatite-core`, `hromatite-dll`, `hromatite-eac`, `hromatite-features`, and `hromatite-nuklear` (82,882 total lines in `.rs/.c/.h/.toml`), `hromatite-core` acts as a monolithic hub pulled into both the CLI injector (`Hromatite.exe`) and `hromatite-eac`, creating 6+ Win32 FFI copies, 2 full x64 inline-hook engines, 5 PE parsers, 3 `VirtualQuery` memory walkers, duplicate SHA-256/CRC-32C implementations, duplicate `m3`/`telemetry`/`memory` command logic, and 2 overlapping BoringSSL EVP probes — plus confirmed dead code (`raw/m3dump.rs`, legacy GC mid-function guard in `raw/hook.rs`, unused `memscan` scanners, `hookat`/`unhook`/`hooks`/`tick` unreachable branches, and `gamebuild::is_supported`).

---

## 1. Observation

### 1.1 Current Workspace Dependency Graph (from `Cargo.toml` manifests)

- `Cargo.toml:2-11` (`Hromatite` CLI): depends on `hromatite-core`.
- `crates/hromatite-core/Cargo.toml:10-18`: depends on `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear`.
- `crates/hromatite-eac/Cargo.toml:11-14`: depends on `hromatite-core`.
- `crates/hromatite-dll/Cargo.toml:10-15`: depends on `hromatite-core`, `hromatite-eac`.
- `crates/hromatite-features/Cargo.toml:11-14`: depends on `hromatite-abi`, `hromatite-commands`, `hromatite-eac` (which transitively pulls in `hromatite-core` -> `hromatite-nuklear`!).
- `crates/hromatite-nuklear/Cargo.toml:11-14`: standalone (`cc = "1"` build-dep only).
- `crates/hromatite-console/Cargo.toml:23`: independent host `[workspace]` (depends on `hromatite-commands`, `unicode-width`).

### 1.2 Complete File Inventory & Target Crate Mapping

#### `src/` (247 lines)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `src/main.rs` | 247 | Injector CLI (`bridge`, `inject`, `procs`, `mods`, `hot-smoke`) | `hromatite-injector` | Currently depends on `hromatite-core` (`hot_runtime::smoke` pulls in `localmap`/`hostapi`/resident!). `hot-smoke` needs decoupling or moving to runtime/smoke test. |

#### `crates/hromatite-core/` (23,602 lines across 44 files)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `Cargo.toml` | 21 | Manifest | SPLIT / dissolve | Replaced by target crates |
| `src/lib.rs` | 47 | Re-exports `raw::*`, `GAME_EXE`/`GAME_ASSEMBLY`/`UNITY_PLAYER` | SPLIT | Constants -> `hromatite-abi` or `hromatite-il2cpp` |
| `src/buildinfo.rs` | 663 | Build selection, GC anchor validation, `discover` fallback + JSON cache | `hromatite-il2cpp` | Consumes `hromatite-abi::build`, `hromatite-win`, `discover` |
| `src/crc.rs` | 79 | EAC `crc32c_eac` + `seal_checksum` | DELETE (dedup into `hromatite-eac` / shared digest) | Exact duplicate of `hromatite-eac::digest::crc32c_eac` |
| `src/crypto.rs` | 166 | L2 `--seal` XOR cipher (`SEAL_KEY`) | DELETE (with `--seal`) or `hromatite-injector` | Only used by `manualmap::seal_image`; `hromatite-dll` rejects `HROMATITE_SEAL!=0` |
| `src/gamebuild.rs` | 23 | `SUPPORTED_SHA256` + unused `is_supported()` | `hromatite-il2cpp` (const only) or DELETE | `SUPPORTED_SHA256` used only by `lateupdate.rs:25`; `is_supported()` is dead |
| `src/gcthreads.rs` | 957 | Boehm GC thread table reader/reaper & worker provenance | `hromatite-il2cpp` | Uses `hromatite-win` (`safe`, `sys`, `procs`) + `buildinfo` |
| `src/hot.rs` | 944 | Pure hot-reload state machine (`HotReload`, `HotConfig`, `Phase`) | `hromatite-runtime` | Unify with `raw/hot.rs` (and share status/config schema with `console/src/hot.rs`) |
| `src/lateupdate.rs` | 345 | `Selector`, `AUTO_CLASS`, `resolve`, `candidates`, `retry_for` | `hromatite-il2cpp` | Pure selection logic over `m3` |
| `src/log.rs` | 117 | File logger, panic hook, `log::guard` (calls `veh::call_guarded`), unused `hlog!` | `hromatite-win` (or `hromatite-runtime` + `hromatite-win`) | `guard()` ties logging to `veh::call_guarded`; split pure file logger (`hromatite-win` / shared) vs guarded wrapper, delete `hlog!` |
| `src/m3.rs` | 492 | Unity 6 IL2CPP class/field/method/image reader | `hromatite-il2cpp` | Consumes `buildinfo`, `hromatite-win` |
| `src/manualmap.rs` | 576 | Remote PE manual mapper + `inject_loadlib` + `seal_image` | `hromatite-injector` | Remove `seal_image`/L2B if `--seal` retired; reuse `pe` |
| `src/pe.rs` | 253 | Safe PE32/PE32+ file parser (`PeImage`) | `hromatite-win` | Shared by `hromatite-injector` and `hromatite-runtime` (`localmap`) |
| `src/sha256.rs` | 217 | Pure SHA-256 + `hex_digest_file` | `hromatite-win` (or shared utility) | Duplicated in `hromatite-eac/src/digest.rs` |
| `src/symbols.rs` | 498 | Generated export RVAs & `GAMEASSEMBLY_SHA256` | `hromatite-il2cpp` | Used by `buildinfo`, `gamebuild`, `lateupdate`, `hostapi`, `hot` |
| `src/raw/mod.rs` | 42 | `raw` boundary module declarations | DELETE (dissolve `raw/`) | `m3dump` gated `#[allow(dead_code)]` here |
| `src/raw/console.rs` | 1287 | File console `Reader`, `dispatch`, built-in `m3`/`pos`/`who`/`esp`/`mem` commands | `hromatite-runtime` (reader/router) + `hromatite-il2cpp` (fallback cmds or prune dupes) | Duplicates `features/{m3,telemetry,memory}` & has `Action::Seal`/`Quit` stubs |
| `src/raw/context.rs` | 534 | VEH handler + per-thread `RtlCaptureContext`/`RtlRestoreContext` (`call0..call10`, `call_guarded`) | `hromatite-win` | Pure Win32/x64 fault boundary |
| `src/raw/d3d11.rs` | 1756 | COM vtable caller, `find_swapchain`, D3D11 `Backend` for Nuklear | `hromatite-render` | Uses `hromatite-win` (`safe`, `procs`, `veh`) |
| `src/raw/d3d11_shaders.rs` | 175 | Precompiled DXBC vertex/pixel shaders | `hromatite-render` | Used only by `d3d11.rs` |
| `src/raw/discover.rs` | 883 | Runtime signature discovery for GC/IL2CPP/PlayerLoop anchors | `hromatite-il2cpp` | Uses `hromatite-win` (`safe`, `pe`); `Image::data` has `#[allow(dead_code)]` |
| `src/raw/ffi.rs` | 299 | Hand-written Win32 FFI structs/consts/externs + `wide`/`cstr` | `hromatite-win` | Consolidate all duplicate Win32 externs here; prune unused decls |
| `src/raw/gcguard.rs` | 1025 | Boehm `GC_suspend_all`/`GC_remove_thread` 3-site detour guard | `hromatite-il2cpp` | Uses `hromatite-hook` + `hromatite-win` + `buildinfo` |
| `src/raw/hook.rs` | 2568 | **MIXED:** x64 inline hook engine (1–945) + `LateUpdate` tick/detour + legacy GC guard + GC worker trace + `hookat` | SPLIT: `hromatite-hook` (1–945) + `hromatite-il2cpp` (`LateUpdate` + GC trace) + `hromatite-runtime` (`tick`/`work`) + DELETE (legacy GC guard + unreachable `hookat`) | See §2.4 & §3 |
| `src/raw/hookset.rs` | 330 | `HookSet` registry (`Inline` + `Mid` overlap guard) | `hromatite-hook` | Wire into unified hook engine |
| `src/raw/hostapi.rs` | 295 | `HostApiV1` resident implementation (`host_log`, `host_read`, `host_write`, `host_guarded_call`) | `hromatite-runtime` | Uses `hromatite-win` + `hromatite-il2cpp` (`buildinfo`) |
| `src/raw/hot.rs` | 1396 | Resident hot-reload `Runtime`, manifest/config IO, `smoke` | `hromatite-runtime` | Unify with `core/src/hot.rs` |
| `src/raw/iat.rs` | 1043 | Pure PE IAT/export reader + `patch_module` / `module_imports` | `hromatite-hook` | Currently used ONLY by `hromatite-eac/src/eos.rs` |
| `src/raw/il2cpp.rs` | 46 | `find_domain()` (`il2cpp_get_corlib`) | `hromatite-il2cpp` | Replace hardcoded RVA `0x00c2_9c70` with `buildinfo` |
| `src/raw/kiero.rs` | 731 | `Kiero` swapchain `Present` VMT hook + `RenderFn` dispatch | `hromatite-render` | Uses `hromatite-hook` (`VmtHook` + reserve `Hook`) |
| `src/raw/localmap.rs` | 1622 | In-process `FeatureImage` PE mapper + `LiveModule` | `hromatite-runtime` | Deduplicate PE parsing against `hromatite-win::pe` |
| `src/raw/m3dump.rs` | 546 | `EAC_M3_DUMP` offline class-table byte dumper | **DELETE** | Confirmed dead code (`#[allow(dead_code)]`, 0 references) |
| `src/raw/memory.rs` | 765 | Safe `ReadProcessMemory`/`WriteProcessMemory` readers, `Span`, `ScanChunks`, host Win32 stubs | `hromatite-win` | Remove unused `*_in_span`/`find_qword_in_span` |
| `src/raw/memscan.rs` | 337 | `RegionMap`, `scan_ascii/qwords/words` (unused) + `stash_cstr`, `call3` (used by `photon`) | `hromatite-win` (or `hromatite-il2cpp` for `stash_cstr`) | Prune unused `RegionMap`/`readable_spans`/`spans_within`/`scan_*`/`call2` |
| `src/raw/midhook.rs` | 512 | Mid-function x64 inline hook (`plan`, `install`, `MidHook`) | `hromatite-hook` | Shares decoder/relocator with `hook` |
| `src/raw/photon.rs` | 2717 | Photon `SendOperation`/`OnEvent` decode + hook + template emulate | `hromatite-il2cpp` (split into `photon/{mod,decode,hook,emulate}`) | Uses `hromatite-hook`, `hromatite-win`, `m3`; calls `crc::seal_checksum` |
| `src/raw/playerloop.rs` | 786 | Native Unity `PlayerLoop` discovery & `player_loop_detour` | `hromatite-il2cpp` | Decouple from `raw::hook::tick` and `raw::hot::original_first_required` via callback/trait |
| `src/raw/procs.rs` | 341 | Toolhelp32 PIDs/modules, `unlink_from_peb`, `regions`, `claim_single_instance`, `own_module` | `hromatite-win` | Shared by `injector` and `runtime` |
| `src/raw/remote.rs` | 218 | Cross-process `Remote` (`OpenProcess`, RPM/WPM, `VirtualAllocEx`, `CreateRemoteThread`, export walk) | `hromatite-injector` (or `hromatite-win::remote`) | Needed only by injector (`src/main.rs` + `manualmap`) |
| `src/raw/sys.rs` | 121 | Safe Win32 wrappers (`live_thread_ids`, `export_address`, `resume_thread`, etc.) | `hromatite-win` | Merge with `ffi` / `win` facade |
| `src/raw/ui.rs` | 390 | Nuklear `Present` loop + User32 input polling (`GetAsyncKeyState`, etc.) | `hromatite-render` | Inline `user32` `extern "system"` -> `hromatite-win` |
| `src/raw/vmt.rs` | 717 | `TableMem`, `Live`, `Vmt`, `VmtHook` | `hromatite-hook` | Used by `kiero.rs` |
| `tests/frames.rs` | 333 | Tests `crc` + `photon::retarget_frame` | `hromatite-il2cpp` (or `hromatite-eac`) | Moves with `photon`/`crc` |
| `tests/report_stream.rs` | 476 | Tests `photon::parse_payload_records` | `hromatite-il2cpp` | Moves with `photon` |

#### `crates/hromatite-dll/` (442 lines across 2 files)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `Cargo.toml` | 18 | `cdylib` `"hromatite"` | `hromatite-dll` | Should depend on `hromatite-runtime`, `hromatite-win`, `hromatite-il2cpp`, `hromatite-render`, `hromatite-eac` (NO `manualmap`/`remote`) |
| `src/lib.rs` | 424 | `DllMain`, mode validation, AC/Photon/UI/GC/LateUpdate/PlayerLoop startup | `hromatite-dll` (thin entry) + `hromatite-runtime` (startup orchestration) | Currently imports `hromatite_core::{hook, procs, lateupdate, playerloop, photon, ui}` and `hromatite_eac` |

#### `crates/hromatite-eac/` (15,075 lines across 23 files)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `Cargo.toml` | 17 | Manifest (depends on `hromatite-core`) | `hromatite-eac` | Replace `hromatite-core` dep with `hromatite-win` + `hromatite-hook` (+ `hromatite-il2cpp` or decouple Photon/M3) |
| `src/lib.rs` | 140 | Public exports + `encode_beat` | `hromatite-eac` | Remove `#[allow(unsafe_code)]` on non-EAC `hook` |
| `src/capture.rs` | 224 | Binary capture format (`u32 len, u32 dt_ms, bytes`) | `hromatite-eac` | Pure `std` |
| `src/clock.rs` | 12 | `now_ms()` (`SystemTime`) | `hromatite-eac` (or `hromatite-win`/shared) | Tiny 12-line helper |
| `src/codec.rs` | 358 | 24-byte EAC wire header parser/encoder | `hromatite-eac` | Pure `std` |
| `src/digest.rs` | 297 | SHA-256, HMAC-SHA256, HKDF, CRC-32C (`crc32c_eac`) | `hromatite-eac` | Deduplicate SHA-256 with `hromatite-core/src/sha256.rs` & CRC-32C with `core/src/crc.rs` |
| `src/eacimage.rs` | 1658 | ELF64 parser + `/proc/self/maps` parser + Win32 `VirtualQuery` walker + `eac.module` report | SPLIT: `hromatite-win` (`VirtualQuery` walk) + `hromatite-eac` (`elf`, `locator`, `report`) | Currently calls `hromatite_core::{ffi::VirtualQuery, safe, veh}` |
| `src/eos.rs` | 3532 | **MONOLITH:** `Hooks`, `EOSSDK` detours, IAT wrapper, inline `evp`, `mod win` + `resolve_export`, forge/report pipeline | SPLIT: `hromatite-win` (`mod win` + PE export) + `hromatite-eac::eos::{hooks, pipeline, diag}` | Remove duplicate `install_evp` (or unify with `evp.rs`) |
| `src/epoch.rs` | 397 | `Gate` epoch state machine (`ForeignSession`/`Stale`/`EpochReset`) | `hromatite-eac` | Pure `std` |
| `src/eventlog.rs` | 290 | JSONL rotating event log (32 MiB / 2 files) | `hromatite-eac` | Pure `std` |
| `src/evp.rs` | 728 | BoringSSL `EVP_*` RVA probes in EOSSDK (`EAC_EVP=1`) | `hromatite-eac` | Overlaps `eos.rs::install_evp` (`EAC_KEYHUNT_EVP=1`); uses `hromatite-hook` |
| `src/forge.rs` | 1228 | 512-byte template builder, `StreamConfig`, Photon ring + `pump_photon` | SPLIT/REFACTOR in `hromatite-eac` | `pump_photon` calls `hromatite_core::photon::{template, emulate_send}` — invert via callback/trait so `hromatite-eac` doesn't force-link Photon |
| `src/hook.rs` | 1792 | Full x64 inline-hook engine (12-byte patch, shared hook table) + `mod win` + `readable` | `hromatite-hook` + `hromatite-win` | **Non-EAC responsibility**; merge with `core/raw/hook.rs` |
| `src/keyhunt.rs` | 1193 | `EAC_KEYHUNT=1` key/frame scanner + inline Win32 `ReadProcessMemory`/`VirtualQuery` | `hromatite-win` (region/read primitives) + `hromatite-eac` (key derivation & hunt) | Lines 185–370 duplicate Win32 FFI & region walk |
| `src/m3probe.rs` | 154 | `EAC_M3_PROBE=1` `s_TypeInfoDefinitionTable` scanner | `hromatite-il2cpp` (or DELETE if superseded by `discover.rs`) | **Non-EAC responsibility**; hardcoded 2026-11-17 RVA `0x0e66_f0d0` |
| `src/payload.rs` | 236 | 120-byte model heartbeat body | `hromatite-eac` | Pure `std` |
| `src/report.rs` | 1644 | `Report` parser + `Generator` + `Stream` + `retarget` | `hromatite-eac` (split `report/{parser,generator,stream}`) | Pure `std` |
| `src/session.rs` | 444 | `EacSession` epoch/cadence model | `hromatite-eac` | Pure `std` |
| `src/tailer.rs` | 210 | Polling tailer for VRChat `output_log_*.txt` | `hromatite-eac` | Pure `std` |
| `src/tap.rs` | 250 | Incremental reader over `hromatite_eac_capture.bin` | `hromatite-eac` | Pure `std` |
| `tests/*.rs` (4 files) | 820 | `eos_install.rs` (280), `epochs.rs` (165), `hook_engine.rs` (260), `keyhunt_scan.rs` (115) | `hromatite-eac` / `hromatite-hook` (`hook_engine.rs` -> `hromatite-hook`) | |

#### `crates/hromatite-features/` (5,375 lines across 11 files)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `Cargo.toml` | 17 | `cdylib` `"hromatite_features"` | `hromatite-features` | Currently depends on `hromatite-eac` -> `hromatite-core`! |
| `src/lib.rs` | 810 | Exported `FeatureApiV1` ABI entry (`hromatite_feature_v1`), `DllMain`, tests | `hromatite-features` | |
| `src/eac.rs` | 858 | `eac.*` feature commands, `EacFeature`, `CaptureTap`, `Hunter` | `hromatite-features` | Uses `hromatite-eac` |
| `src/feature.rs` | 391 | `Feature` command/configure router | `hromatite-features` | Duplicates resident-only command list from `hromatite-commands` |
| `src/game_fixture.rs` | 295 | Test-only sparse Unity 6000 fixture (only `#[cfg(test)]` in `lib.rs`) | `hromatite-features` (`#[cfg(test)]`) | |
| `src/host.rs` | 188 | `Host` safe wrapper over `HostApiV1` + `ascii_at`/`ascii_lossy`/`utf16_lossy`/`module_tag` | `hromatite-features` (or `hromatite-abi` helper) | String/tag helpers duplicate `raw/memory.rs` & `raw/console.rs` |
| `src/m3.rs` | 624 | Feature-side `Metadata` over `Host` + `m3*` command dispatch | `hromatite-features` (or shared `hromatite-il2cpp` reader trait) | Duplicates `hromatite-core/src/m3.rs` + `raw/console.rs` |
| `src/memory.rs` | 81 | `parse_addr`, `qwords`, `bytes`, `decode_hex` over `Host` | DELETE / share hex/addr utils with `hromatite-commands` or `core` | Duplicates `raw/console.rs`, `raw/hook.rs`, `eac/eos.rs` |
| `src/render.rs` | 1255 | In-game Unity `Graphics.DrawMesh` ESP (`Mesh`/`Material`/`Vector3[]` via IL2CPP) | `hromatite-features` (NOT `hromatite-render`!) | Pure IL2CPP gameplay feature via `Host`; no D3D11/Nuklear |
| `src/telemetry.rs` | 594 | `pos`, `who`, `esp`, `scene()` over `Metadata` | `hromatite-features` | Duplicates `raw/console.rs` `pos_probe`/`who_probe`/`esp_probe` |
| `src/test_support.rs` | 267 | Test-only fake `HostApiV1` | `hromatite-features` (`#[cfg(test)]`) | |

#### `crates/hromatite-nuklear/` (33,500 lines total; 2,029 non-vendored lines)
| File | Lines | Current Role | Target Crate | Notes |
|---|---:|---|---|---|
| `Cargo.toml` | 21 | Standalone crate (`cc = "1"`) | Merge into `hromatite-render` (or keep as sub-crate of `hromatite-render`) | |
| `build.rs` | 26 | Compiles `nuklear_shim.c` | `hromatite-render` | |
| `nuklear_shim.c` | 517 | C shim over `nuklear.h` | `hromatite-render` | |
| `src/lib.rs` | 602 | Safe `Nk`, `Ui`, `Input`, `Keys`, `Frame` API | `hromatite-render` | |
| `src/sys.rs` | 639 | Raw C shim FFI + `AlignedBox` | `hromatite-render` | |
| `tests/frame.rs` | 224 | Host tests for `Nk::frame` | `hromatite-render` | |
| `third_party/nuklear/nuklear.h` | 31471 | Vendored upstream header | `hromatite-render` | |

---

## 2. Duplicate Subsystems & Coupling Violations (R2–R5)

### 2.1 Win32 FFI Duplication (6 distinct locations)
1. `crates/hromatite-core/src/raw/ffi.rs:173-282` — primary kernel32/ntdll `unsafe extern "system"` block + `MEMORY_BASIC_INFORMATION`, `PROCESSENTRY32W`, `MODULEENTRY32W`, `THREADENTRY32`, `EXCEPTION_RECORD`, `EXCEPTION_POINTERS`.
2. `crates/hromatite-core/src/raw/localmap.rs:624-627` — inline `unsafe extern "system" { fn FreeLibrary(...) }`.
3. `crates/hromatite-core/src/raw/ui.rs:110-116` — inline `user32` `unsafe extern "system" { GetForegroundWindow, GetClientRect, GetCursorPos, ScreenToClient, GetAsyncKeyState }`.
4. `crates/hromatite-eac/src/hook.rs:865-917` (`mod win`) — duplicates `MemBasic` (`MEMORY_BASIC_INFORMATION`), `VirtualAlloc`, `VirtualQuery`, `VirtualProtect`, `FlushInstructionCache`, `GetCurrentProcess`, and adds `CreateFileMappingA`, `MapViewOfFile`.
5. `crates/hromatite-eac/src/eos.rs:1517-1549` (`mod win`) — declares `GetModuleHandleA` + duplicates `DosHeader` and `ExportDirectory`.
6. `crates/hromatite-eac/src/keyhunt.rs:193-202, 278-306` — duplicates `ReadProcessMemory`, `GetCurrentProcess`, `VirtualQuery` (with `#[allow(clashing_extern_declarations)]` at line 301) and `MemoryBasic` (`MEMORY_BASIC_INFORMATION`), plus Linux `process_vm_readv`/`getpid` at lines 229-239.
(Note: `crates/hromatite-core/src/raw/memory.rs:23-123` also exports `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` link stubs for `VirtualQuery`, `GetCurrentProcess`, `ReadProcessMemory`, `WriteProcessMemory`, `VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`.)

### 2.2 Memory Read/Write/Scan & VEH Guard Duplication
- **Core memory**: `core/src/raw/memory.rs` (`Span`, `ScanChunks`, `readable_at`, `readable_range`, `read_*`, `write_*`, `ascii_*`, `utf16_lossy`, `scan_yield`) + `core/src/raw/memscan.rs` (`RegionMap`, `ScanSpan`, `stash_cstr`, `call3`) + `core/src/raw/context.rs` (`veh`: `install`, `call_with`, `call_guarded`, `call0..10`).
- **`hromatite-eac` copies**:
  - `eac/src/hook.rs:925-945`: own `readable(addr, len)` via `win::VirtualQuery`.
  - `eac/src/eacimage.rs:853-900, 1003-1025`: own `VirtualQuery` walk (`live::walk`, `query_region`), mixing `hromatite_core::ffi::VirtualQuery` and `safe::read_bytes`.
  - `eac/src/keyhunt.rs:191-370`: own `read_block` (`ReadProcessMemory`), `read_window`, `enumerate_regions` (`VirtualQuery`).
- **`hromatite-features` copies**:
  - `features/src/host.rs:104-187`: duplicates `ascii_at`, `ascii_lossy`, `utf16_lossy`, `module_tag` (vs `core/raw/memory.rs:602-674` & `core/raw/console.rs:105-121`).
  - `features/src/memory.rs:5-68`: duplicates `parse_addr` (also in `raw/console.rs:79`, `raw/hook.rs:2024`, `eac/eos.rs:1316`), `qwords` (vs `console.rs:123`), `bytes` (vs `console.rs:744`), `decode_hex` (vs `console.rs:796`).

### 2.3 PE Parsing & Process/Module/PEB Duplication
- **5 PE parsers**:
  1. `core/src/pe.rs` (`PeImage::from_file`/`from_bytes` — headers, sections, exports/reloc/imports/TLS).
  2. `core/src/raw/remote.rs:143-207` (`export_in`, `export_ordinal` via `ReadProcessMemory`).
  3. `core/src/raw/localmap.rs:637-715` (`LiveModule::from_base` + reloc/import/export/TLS walking duplicating `manualmap.rs` & `pe.rs`).
  4. `core/src/raw/iat.rs:127-250` (`image_nt`, `data_directory`, `descriptors`, `OrdinalNames` over `Reader`).
  5. `core/src/raw/discover.rs:79-128, 270-330` (section table & export directory parser).
  6. Plus a 6th partial copy in `eac/src/eos.rs:1557-1615` (`resolve_export`) and inline `SizeOfImage` parsing in `core/src/raw/procs.rs:301-306` (`own_module`).

### 2.4 Hook Engines & Couplings
- **Two x64 inline-hook engines**:
  1. `crates/hromatite-core/src/raw/hook.rs` (lines 14-945): 5-byte `jmp rel32` + relay page, `alloc_near` (±2 GiB), `cmpxchg16b` atomic write (`write_window`), `decode_one`/`decode_span`/`Hook`.
  2. `crates/hromatite-eac/src/hook.rs` (lines 1-1792): 12-byte `mov rax, imm64; jmp rax`, `alloc_near` (±512 MiB), `cas16`/`patch_entry`, `decode`/`decode_min`, inverted-`jcc` short-branch & `rel32` expansion, **plus** process-wide named shared section `Local\hromatite_rust_hooktable_v1` (`CreateFileMappingA`/`MapViewOfFile`).
- **Companion mechanisms in `core/raw/`**:
  - `midhook.rs` (512 lines): depends directly on `super::hook` internals (`decode_span`, `alloc_near`, `write_window`, `RELAY_OFF`, etc.).
  - `vmt.rs` (717 lines): `Vmt`/`VmtHook` with global `CLAIMED` mutex; used only by `kiero.rs`.
  - `iat.rs` (1043 lines): PE IAT patching; used only by `hromatite-eac/src/eos.rs`.
  - `hookset.rs` (330 lines): `HookSet` unifying `hook::hook_inline` & `midhook::install`; currently only used in unit tests and re-exported by `core/src/lib.rs`.
- **Coupling violations**:
  - `raw/hook.rs` embeds `LateUpdate` (`tick`, `work`, `late_update_detour`, `materialise_late_class`, `install_late_update`, `late_update_status`), `gcguard` coordination (`before_unhook`/`after_unhook`), `gcthreads` worker trace detours, `console::Reader`/`dispatch`, `raw::hot::{tick, frame, command, dispatch, resident_command, original_first_required}`, `playerloop::refresh_attachment`, `m3`, `il2cpp`, `buildinfo`.
  - `raw/playerloop.rs:282, 300` calls `crate::raw::hot::original_first_required()` and `crate::hook::tick()` directly from `player_loop_detour`.
  - `hromatite-eac/src/eos.rs` and `evp.rs` are coupled directly to `hromatite_eac::hook`, and `eos.rs:1398` directly calls `hromatite_core::iat`.

### 2.5 Render / UI
- `hromatite-nuklear` (`Nk`, `Ui`, `Input`, `Keys`, `Frame`, C shim) + `raw/kiero.rs` (`Present` VMT hook + `RenderFn`) + `raw/d3d11.rs` (`find_swapchain`, COM caller, D3D11 `Backend`, `SavedState`) + `raw/d3d11_shaders.rs` (DXBC) + `raw/ui.rs` (User32 input + thread-local `Nk`/`Backend` loop) belong together in `hromatite-render`.
- `raw/kiero.rs` currently depends on `raw/vmt.rs` and `raw/hook.rs` (`hook_inline`); after split, `hromatite-render` depends on `hromatite-hook` + `hromatite-win`.
- `hromatite-features/src/render.rs` is **not** D3D11/Nuklear — it is the Unity `Graphics.DrawMesh` ESP feature over `HostApiV1` and stays in `hromatite-features`.

### 2.6 Unity/IL2CPP & `photon.rs` Structure
- `core/src/m3.rs` vs `features/src/m3.rs`: `features/src/m3.rs` duplicates class/field/method/image traversal on top of `Host` (`HostApiV1`) rather than `safe` (`hromatite-win`).
- `core/raw/console.rs:147-717, 918-1073` vs `features/src/{telemetry,m3}.rs`: `console.rs` carries a full duplicate implementation of `pos`, `who`, `esp` (`pos_probe`, `who_probe`, `esp_probe`, `Sdk`, `CamApi`, `mul4`, `project`) and `m3f`/`m3s`/`m3l`/`m3c`/`m3i` as fallback when no feature DLL is loaded.
- `core/raw/il2cpp.rs:12` hardcodes `CORLIB_IMAGE_GETTER_RVA = 0x00c2_9c70` instead of reading `buildinfo::il2cpp_anchors`.
- `core/raw/photon.rs` (2,717 lines) internal layout:
  1. Constants & pure classification/formatting helpers (lines 47–431)
  2. **`decode`** (lines 435–740): `Dictionary<byte, object>` slot probing & `decode_event_params`
  3. **Hook state & capture ring** (lines 741–862): `SEND_ORIG`/`EVENT_ORIG`, `RING` (`Captured`)
  4. **`emulate`** (lines 863–1299): `Template`, `emulate_send`, `SendConfig`, `parse_payload_records`, `retarget_frame` (uses `crate::crc::seal_checksum`), `drain_capture`, `tick_send`, `tick()`
  5. **`hook` resolution** (lines 1300–1715): `resolve_send_operation`, `resolve_on_event`, `class_by_name_via_api` (uses `raw::memscan::{stash_cstr, call3}`), `resolve_peer_classes`
  6. **`hook` diagnostics, detours & install** (lines 1716–2101): `probe_report`, `status_line`, `send_operation_detour`, `on_event_detour`, `install_outgoing`, `install_incoming` (uses `raw::hook::{prepare, patch, register}`), `poll()`
  7. Unit tests (lines 2102–2718).
  - Note: grep confirmed `photon::poll()` and `photon::tick()` are **not currently called** in `raw/hook.rs::work()` (only `install_outgoing`/`install_incoming` are called from `hromatite-dll/src/lib.rs:185-213`).

### 2.7 `hromatite-eac` Structure & Non-EAC Responsibilities
- **Pure EAC protocol/session/log** (portable `std`): `codec.rs` (358), `payload.rs` (236), `session.rs` (444), `epoch.rs` (397), `capture.rs` (224), `tap.rs` (250), `eventlog.rs` (290), `tailer.rs` (210), `clock.rs` (12), `digest.rs` (297 — duplicates `core/sha256.rs` & `core/crc.rs`), `report.rs` (1,644: `Report` parser lines 1–300, `GenConfig`/`Generator` lines 301–700, `Stream` playback & `retarget` lines 701–1140, stream tests 1141–1644).
- **Non-EAC responsibilities inside `hromatite-eac`**:
  - `hook.rs` (1,792): entire x64 inline-hook engine + shared hook table + Win32 FFI (`mod win`) + `readable` -> belongs in `hromatite-hook` & `hromatite-win`.
  - `m3probe.rs` (154): IL2CPP class-table scanner (`LEGACY_TABLE_RVA = 0x0e66_f0d0`) -> belongs in `hromatite-il2cpp` (or delete if superseded by `discover.rs`).
  - `eacimage.rs` (1,658): Win32 `VirtualQuery` walker (`live::walk`, lines 797–1027) -> belongs in `hromatite-win`; ELF64 parser (lines 1–695) + `/proc/self/maps` parser (lines 695–795, 1028–1155) + `command` report (1156–1658) can be split into `elf`/`locator`/`report`.
  - `keyhunt.rs` (1,193): Win32/Linux memory reading & `VirtualQuery` region enumeration (lines 185–370) -> belongs in `hromatite-win`; seed/HKDF key & frame hunting -> stays in `hromatite-eac`.
  - `eos.rs` (3,532) internal sections:
    1. `Hooks` install/status/counters/config (1–1092)
    2. Duplicate BoringSSL EVP probe `install_evp` (1093–1326, overlaps `evp.rs`)
    3. `install_iat` wrapper calling `hromatite_core::iat` (1327–1456)
    4. `mod win` (`GetModuleHandleA`, `DosHeader`, `ExportDirectory`) + `resolve_export` PE parser (1515–1616) -> belongs in `hromatite-win`
    5. `EOS_AntiCheatClient_*` detours + `epoch_gate` (1617–2032, 2580–3000)
    6. Forge/report delivery pipeline & generator thread (2033–2579)
    7. Unit tests (3000–3532).
  - `forge.rs` (1,228): template builder (1–609), `StreamConfig` (610–720), Photon ring + `pump_photon` thread (721–900, directly calls `hromatite_core::photon::{template, emulate_send}`), tests (901–1228).

### 2.8 Three `hot` Implementations
1. `crates/hromatite-core/src/hot.rs` (944 lines): pure `HotReload` phase state machine + `HotConfig` parser.
2. `crates/hromatite-core/src/raw/hot.rs` (1,396 lines): resident `Runtime`, manifest/config file I/O, `FeatureImage` loading via `localmap`, `hostapi::current_host_api()`, `smoke` loop (invoked by `src/main.rs hot-smoke`). Note: `raw/hot.rs:178` checks `if values[4] != "1"` while `hromatite-abi/src/lib.rs:12` has `ABI_VERSION = 2`.
3. `crates/hromatite-console/src/hot.rs` (99 lines): host-side TUI parser (`parse_hot_status`, `HotState`, `HotStatus`) consuming `[hot] state=...` log lines produced by `raw/hot.rs::Runtime::status()`.

---

## 3. Verified Dead Code & `#[allow(dead_code)]` Audit

Every item below was checked via `grep_search` across the repository:

### 3.1 Confirmed Unused / Dead Code (Safe to DELETE)
1. **`crates/hromatite-core/src/raw/m3dump.rs` (546 lines)** + `raw/mod.rs:31-32` (`#[allow(dead_code)] pub mod m3dump`): 0 callers anywhere in the repository (`m3dump` only appears in `raw/mod.rs`, `raw/m3dump.rs`, and `AGENTS.md`).
2. **Legacy mid-function Boehm GC guard in `crates/hromatite-core/src/raw/hook.rs`** (comment at 1341 explicitly notes it is unused at runtime):
   - `install_legacy_gc_context_guard` (line 1268, `#[allow(dead_code)]`) — 0 callers.
   - `gc_context_detour` (1249), `gc_context_route` (1215), `complete_dead_gc_call_raw` (1195), `unlink_by_handle_raw` (1147), `write_synthetic_dead_context_raw` (1113), `note_inline_repair` (1092), `GC_CONTEXT_ORIG` (1085), `GC_THREADS_BASE` (1086), `INLINE_REPAIRS` (1087), `INLINE_LAST_TID` (1088), `GC_CONTEXT_CALL_PROLOGUE` (1075), `WIN64_CONTEXT_FLAGS` (1102), `WIN64_CONTEXT_SIZE` (1103), `valid_gc_context_call_site` (1077) — referenced only by `install_legacy_gc_context_guard` and unit tests (`2398–2543`). (`take_inline_repairs` at `998` always returns `None` because `note_inline_repair` is unreachable.)
3. **Unreachable `hook::command` subcommands in `crates/hromatite-core/src/raw/hook.rs:1950-2010, 2081-2157`**:
   - `raw/console.rs:1130` only forwards `"latec" | "latehook" | "late"` to `crate::hook::command(line)`, and `raw/hook.rs:1043` calls `console::dispatch` directly.
   - Therefore `"unhook"`, `"hooks"`, `"hookat"`, `"tick"` in `hook::command` are unreachable, making `install_at` (1992), `count_detour` (1957), `HOOKAT_ORIG`/`HOOKAT_HITS`/`HOOKAT_LOG` (1950–1952) dead code, and `hook::installed()` (2013) has 0 callers anywhere.
4. **Unused `memscan.rs` scanners (`crates/hromatite-core/src/raw/memscan.rs`, `#![allow(dead_code)]` at line 19)**:
   - Only `stash_cstr` and `call3` are called externally (`raw/photon.rs:1389-1391`).
   - `ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `scan_ascii`, `scan_qwords`, `scan_words`, `call2` have 0 callers outside `memscan.rs`.
5. **Unused `Span` helpers in `crates/hromatite-core/src/raw/memory.rs`**:
   - `find_qword_in_span` (676), `matches_in_span` (280), `read_usize_in_span` (265) have 0 callers outside `memory.rs` (only unit tests).
6. **Unused `gamebuild::is_supported()` (`crates/hromatite-core/src/gamebuild.rs:21-23`)**: 0 callers anywhere (`buildinfo` replaced static build gating).
7. **Unused `hlog!` macro (`crates/hromatite-core/src/log.rs:114-117`)**: 0 invocations anywhere.
8. **Unused `raw/discover.rs:51` `#[allow(dead_code)] pub data: Section`**: `Image::data` is never read (only constructed in `discover::image`).
9. **Legacy `seal` / `L2B` remnants**:
   - `hromatite-dll/src/lib.rs:72-77` and `raw/hook.rs:1852-1856` reject `HROMATITE_SEAL!=0` and `HROMATITE_L2B!=0` at startup; `raw/hook.rs:1055-1059` rejects `Action::Seal | Action::Quit`.
   - Yet `raw/console.rs:1133-1137` still accepts `"seal"`/`"quit"`, `manualmap.rs:34-73, 563-574` still implements `seal_image` + `Options::seal`, `src/main.rs:179` accepts `--seal`, and `core/src/crypto.rs` (166 lines) exists solely for `seal_image`.
10. **Unused `raw/ffi.rs` declarations** (covered by `#![allow(..., dead_code)]` at line 3): `CreateThread` (237), `SetUnhandledExceptionFilter` (262), `RemoveVectoredExceptionHandler` (261) have 0 callers anywhere.
11. **Duplicate BoringSSL probe**: `eos.rs:1093-1326` (`install_evp`, `EAC_KEYHUNT_EVP=1`) and `evp.rs` (`evp::install`, `EAC_EVP=1`) both hook the same 5 BoringSSL RVAs in `EOSSDK-Win64-Shipping.dll` (`evp.rs` adds 6th target `protector_ctor@405af0` and richer logging).

### 3.2 `#[allow(dead_code)]` That Are NOT Dead (Intentional / Host-Build Gating)
- `crates/hromatite-core/src/raw/hostapi.rs:6` (`#![allow(dead_code)]`): `current_host_api`, `smoke_host_api`, `game_sha_bytes` ARE called by `raw/hot.rs` under `#[cfg(target_os = "windows")]`; the blanket allow only suppresses warnings on Linux host builds (`#[cfg(target_os = "windows")]` gating would remove the need for `#![allow(dead_code)]`).
- `crates/hromatite-core/src/raw/localmap.rs:19` (`#![allow(dead_code)]`): `FeatureImage::{load, load_verified, api, base, retire}` ARE called by `raw/hot.rs` under `#[cfg(target_os = "windows")]`; `FeatureImage::size()` (581) is unused externally.
- `crates/hromatite-nuklear/src/lib.rs:36, 234`: `mod sys` covers the C widget shim boundary; `Nk::cmd_mem` at line 234 is an RAII buffer kept alive for C's `nk_buffer`.
- `crates/hromatite-core/src/raw/hookset.rs`, `midhook.rs`, `kiero.rs::{init, render, shutdown, hook_module_export}`: building blocks for `hromatite-hook` / `hromatite-render` tested via unit tests (`#[cfg(test)]`), currently unreferenced in live startup paths.
- `crates/hromatite-core/src/raw/photon.rs::{poll, tick}`: implemented and tested, currently unwired from `raw/hook.rs::work()`.

---

## 2. Logic Chain

1. Observation 1.1 shows `hromatite-core` is depended on by `Hromatite` (injector), `hromatite-dll`, and `hromatite-eac` (which is depended on by `hromatite-features`), while `hromatite-core` depends on `hromatite-nuklear`, `hromatite-abi`, and `hromatite-commands`. Therefore, `Hromatite.exe` transitively compiles Nuklear, D3D11, Kiero, IL2CPP, Photon, and hot-reload runtime; and `hromatite-features.dll` transitively pulls in `hromatite-core` via `hromatite-eac`.
2. Observation 2.1–2.3 establishes that Win32 FFI, `VirtualQuery` memory walking, safe reads, and PE parsing are duplicated across `core/raw`, `hromatite-eac`, and `hromatite-features` because `hromatite-eac` was initially written as "pure std + local Win32 declarations" before later adding `hromatite-core` for `m3probe`/`eacimage`/`forge`. Consolidating these into `hromatite-win` eliminates all 6 FFI copies, 3 memory walkers, and 5 PE parsers.
3. Observation 2.4 establishes two independent x64 inline-hook engines (`core/raw/hook.rs` 5-byte relay jump + `eac/hook.rs` 12-byte `mov rax; jmp rax` + shared section table) plus `midhook`, `vmt`, `iat`, `hookset`. Extracting `raw/hook.rs:1-945` + `eac/hook.rs` + `midhook` + `vmt` + `iat` + `hookset` into `hromatite-hook` (over `hromatite-win`) cleanly separates hooking from `LateUpdate` and `EOSSDK`.
4. Observation 2.5 confirms `hromatite-nuklear`, `raw/kiero`, `raw/d3d11`, `raw/d3d11_shaders`, and `raw/ui` form a self-contained D3D11 `Present` overlay stack (`hromatite-render`), whereas `hromatite-features/src/render.rs` is a pure IL2CPP `Graphics.DrawMesh` feature using only `HostApiV1` and must stay in `hromatite-features`.
5. Observation 2.6–2.7 shows `buildinfo`, `discover`, `symbols`, `m3`, `il2cpp`, `gcthreads`, `gcguard`, `lateupdate`, `playerloop`, `photon`, and `eac/m3probe.rs` form the Unity/IL2CPP domain (`hromatite-il2cpp`), while `hromatite-eac` contains pure protocol/session/report/forge logic alongside non-EAC Win32/hook/IL2CPP modules (`hook.rs`, `m3probe.rs`, `mod win`, `live::walk`). Note that `eac/forge.rs::pump_photon` creates an `eac -> il2cpp(photon)` edge unless inverted via a callback/sink trait.
6. Observation 2.8 shows the three `hot.rs` files have distinct roles (`core/hot.rs` state machine, `core/raw/hot.rs` runtime loader/router, `console/hot.rs` host log parser), which should be unified into `hromatite-runtime`'s hot router (and shared config/status contract if desired) rather than duplicated.
7. Observation 3.1 provides grep-verified dead code (546 lines in `m3dump.rs`, ~300 lines of legacy GC guard + `hookat` in `raw/hook.rs`, unused `memscan`/`memory` helpers, `gamebuild::is_supported`, `hlog!`, unused `ffi` externs, and `seal`/L2B remnants if `--seal` is retired).

---

## 3. Caveats

- `crates/hromatite-console` and `crates/hromatite-overlay` were only checked for `console/src/hot.rs` interaction and file line counts (`6443` lines total); full internal decomposition of `console/model.rs` and `overlay/main.rs` is covered by Survey Explorer 1.
- Some modules (`hookset`, `midhook`, `kiero::{init, render, shutdown, hook_module_export}`, `photon::{poll, tick}`) have 0 production callers today, **not** because they are obsolete, but because they are unintegrated infrastructure or temporarily unwired (`photon::poll`/`tick`); they are flagged separately from dead code in §3.2.
- Removing `seal_image` / `crypto.rs` / `--seal` assumes `--seal` on `Hromatite.exe` CLI (`src/main.rs:179`) is no longer needed, consistent with `hromatite-dll` rejecting `HROMATITE_SEAL!=0`. Verify with the user/orchestrator before deleting `crypto.rs` if CLI backward compatibility for standalone non-LateUpdate tests is desired.
- Removing `raw/console.rs` fallback `m3`/`telemetry`/`memory` commands assumes a feature DLL is always loaded (or kept as a deliberate minimal fallback in `hromatite-runtime`).

---

## 4. Conclusion

1. **Target Crate Architecture (R2–R5):**
   - `hromatite-win`: `raw/{ffi, sys, memory, context, procs}` + `pe.rs` + `sha256.rs` + `raw/memscan::stash_cstr` + Win32 parts extracted from `eac/{hook, eos, eacimage, keyhunt}` and `raw/ui`.
   - `hromatite-hook`: unified x64 inline engine (`raw/hook.rs:1-945` + `eac/hook.rs` engine & shared hooktable) + `midhook` + `vmt` + `iat` + `hookset` (0 Unity/EOS deps).
   - `hromatite-render`: `hromatite-nuklear` + `raw/{kiero, d3d11, d3d11_shaders, ui}`.
   - `hromatite-il2cpp`: `symbols`, `buildinfo`, `raw/discover`, `raw/il2cpp`, `m3`, `gcthreads`, `raw/gcguard`, `lateupdate`, `raw/playerloop`, `raw/photon` (split into `decode`/`hook`/`emulate`), `eac/m3probe` (or delete).
   - `hromatite-eac`: `capture`, `clock`, `codec`, `digest` (deduped), `eacimage` (ELF/locator/report only), `eos` (split into `hooks`/`pipeline`/`diag`, deduped `evp`), `epoch`, `eventlog`, `evp`, `forge` (decoupled from `photon` via callback/trait), `payload`, `report` (split into `parser`/`generator`/`stream`), `session`, `tailer`, `tap`.
   - `hromatite-injector`: `src/main.rs` + `manualmap` + `remote` (depending only on `hromatite-win`; move `hot-smoke` out of `Hromatite.exe` or gate behind runtime test binary).
   - `hromatite-runtime`: unified `hot` (`hot.rs` + `raw/hot.rs`), `localmap`, `hostapi`, `console` reader/router, `LateUpdate` tick/detour orchestration (extracted from `raw/hook.rs`), startup sequence (extracted from `hromatite-dll`).
   - `hromatite-dll`: thin `DllMain` + `attach` entry delegating to `hromatite-runtime`.
   - `hromatite-features`: `lib.rs`, `feature.rs`, `host.rs`, `m3.rs`, `render.rs` (Unity `Graphics.DrawMesh`), `telemetry.rs`, `eac.rs` (cleanup `memory.rs` & duplicate hex/addr/resident-list utilities; depend only on `hromatite-abi`, `hromatite-commands`, and protocol-only `hromatite-eac` without `hromatite-core`!).
2. **Immediate Deletions:** `raw/m3dump.rs`, legacy GC guard block in `raw/hook.rs`, unreachable `hook::command` subcommands (`hookat`, `count_detour`, `install_at`, `hook::installed`), unused `memscan`/`memory` span helpers, `gamebuild::is_supported()`, `hlog!`, unused `ffi` externs (`CreateThread`, `SetUnhandledExceptionFilter`, `RemoveVectoredExceptionHandler`), `discover::Image::data`, and deduplicate `eos::install_evp` vs `evp.rs` and `core/crc.rs` vs `eac/digest.rs`.

---

## 5. Verification Method

To independently verify these findings before refactoring:
1. Run baseline checks:
   - `cargo test --workspace`
   - `cargo clippy --workspace --all-targets`
2. Verify `m3dump.rs` and legacy GC guard have zero active callers:
   - `rg "m3dump|install_legacy_gc_context_guard|gc_context_detour|install_at|hookat|gamebuild::is_supported|hlog!" crates/ src/`
3. Verify duplicate FFI & `VirtualQuery` copies:
   - `rg 'unsafe extern "system"' crates/`
   - `rg 'VirtualQuery' crates/`
4. Verify dependency graph violations:
   - `cargo tree -p Hromatite` (shows `hromatite-nuklear` & `raw/` linked into injector)
   - `cargo tree -p hromatite-features` (shows `hromatite-core` & `hromatite-nuklear` linked into `hromatite_features.dll` via `hromatite-eac`)
5. **Invalidation condition:** Any new commits to `main` adding callers to `m3dump`, `install_legacy_gc_context_guard`, `hook::command` (`hookat`), or changing `Cargo.toml` dependencies invalidate the corresponding dead-code/dependency claim.
