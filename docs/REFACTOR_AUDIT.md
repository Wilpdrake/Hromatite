# Ревизия кода и базовая линия (Stage 1 Audit — коммит `5064586`)

Документ фиксирует базовое состояние репозитория `Hromatite` на коммите `5064586` (ветка `master`), карту нарушений границ зависимостей, дублирующихся подсистем, подтверждённого мёртвого кода, декомпозиции монолитов, инвентаризацию `tools/` и целевой план этапов 2–5.

> Проверка выполнена 2026-11-18T22:24Z; все приведённые команды и подсчёты воспроизведены напрямую на текущем рабочем дереве (неотслеживаемые элементы на старте: `.agents/`, `PROJECT.md`, `hromatite-mode.launch-bak.1733464`).

---

## 1. Базовая линия (Baseline @ `5064586`)

### 1.1 Конфигурация сборки и линтов
- `.cargo/config.toml`: таргет по умолчанию `x86_64-pc-windows-gnu`, линкер `x86_64-w64-mingw32-gcc`, `ar = x86_64-w64-mingw32-ar`, `rustflags = ["-C", "link-arg=-L/run/current-system/sw/lib"]`.
- Корневой `Cargo.toml` (`edition = "2024"`, `resolver = "3"`): 9 пакетов (`Hromatite` + 8 крейтов в `crates/`), workspace-линты:
  - `[workspace.lints.rust] unsafe_op_in_unsafe_fn = "deny"`
  - `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`, `missing_safety_doc = "deny"`
- Автономные `[workspace]`-крейты вне корневого workspace:
  - `crates/hromatite-console/Cargo.toml` (Linux host, дублирует те же 3 workspace-линта)
  - `crates/hromatite-overlay/Cargo.toml` (Linux host, **секция `[lints]` отсутствует**)
  - `var/research/helpdump/Cargo.toml` (исследовательская утилита, зависит от `hromatite-commands`, без `[lints]`)

### 1.2 Матрица сборок, тестов и `clippy`

| Проверка | Точная команда | Exit | Фактический результат |
|---|---|---:|---|
| Debug-сборка workspace (Windows) | `cargo build --workspace` | `0` | Успешно, 0 предупреждений/ошибок |
| Release-сборка workspace (Windows) | `cargo build --workspace --release` | `0` | Успешно, 0 предупреждений/ошибок |
| Компиляция тестов workspace (Windows) | `cargo test --workspace --no-run` | `0` | Все 16 тестовых бинарников (`x86_64-pc-windows-gnu`) скомпилированы без ошибок |
| Запуск тестов workspace без `--target` | `cargo test --workspace` | `101` | Компиляция успешна; падает при запуске первого `.exe` (`target/x86_64-pc-windows-gnu/debug/deps/Hromatite-dd8f6c1f910d1c6e.exe`): `Exec format error (os error 8)` (Wine runner не настроен) |
| Юнит/интеграционные тесты workspace (Linux host) | `cargo test --workspace --target x86_64-unknown-linux-gnu` | `0` | **416 passed, 0 failed** + **1 doctest ok** (`hromatite-nuklear`); 3 предупреждения в `hromatite-core` (и 3 дубля для lib test) |
| `clippy` workspace (Windows по умолчанию) | `cargo clippy --workspace --all-targets` | `0` | **0 warnings, 0 errors** |
| `clippy` workspace (Linux host) | `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu` | `0` | **8 warnings** в `hromatite-core` (+ 8 дублей на lib test), **0 errors** |
| Тесты `hromatite-console` (Linux host) | `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu` | `0` | **67 passed, 0 failed** (`58` unit + `6` `tests/hot_reload.rs` + `3` `tests/snapshot.rs`) |
| `clippy` `hromatite-console` (Linux host) | `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` | `0` | **0 warnings, 0 errors** |
| Тесты `hromatite-overlay` (`nix-shell`) | `nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu"` | `0` | **0 tests** (компилируется, линкуется с X11/Xext, тестов нет) |
| `clippy` `hromatite-overlay` (`nix-shell`) | `nix-shell -p xorg.libX11 xorg.libXext --run "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu"` | `0` | **9 warnings** (+ дубли для bin/test), **0 errors** (без `[lints]` в манифесте) |
| `clippy` `hromatite-overlay` с workspace-линтами | `nix-shell -p xorg.libX11 xorg.libXext --run "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc"` | `101` | **15 errors** (`clippy::undocumented_unsafe_blocks` в `src/main.rs`) + **9 warnings** |
| Проверка `var/research/helpdump` | `cargo check --manifest-path var/research/helpdump/Cargo.toml --target x86_64-unknown-linux-gnu` | `0` | Успешно, 0 предупреждений/ошибок |

Дополнительно по замечаниям из Survey 1: запуск `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets` **без** `--target x86_64-unknown-linux-gnu` наследует Windows-таргет из `.cargo/config.toml` и падает (`std::os::unix` / `.dev()`/`.ino()` в `src/fileio.rs`), а `cargo test` для `hromatite-overlay` без `nix-shell` падает на линковке `-lX11`/`-lXext`.

### 1.3 Разбивка тестов по крейтам (`--target x86_64-unknown-linux-gnu`)
- `Hromatite`: `0`
- `hromatite-abi`: `8`
- `hromatite-commands`: `4`
- `hromatite-core`: `239` (`225` unit + `7` `tests/frames.rs` + `7` `tests/report_stream.rs`)
- `hromatite-dll`: `6`
- `hromatite-eac`: `122` (`118` unit + `0` `tests/eos_install.rs` + `1` `tests/epochs.rs` + `0` `tests/hook_engine.rs` + `3` `tests/keyhunt_scan.rs`)
- `hromatite-features`: `24`
- `hromatite-nuklear`: `13` (`3` unit + `10` `tests/frame.rs`) + `1` doctest
- `test-target`: `0`
- **Итого workspace:** `416` unit/integration + `1` doctest.
- Отдельные крейты: `hromatite-console` — `67`; `hromatite-overlay` — `0`.

### 1.4 Текущие предупреждения/ошибки `clippy`
**`hromatite-core` (8 предупреждений на `x86_64-unknown-linux-gnu`):**
1. `crates/hromatite-core/src/m3.rs:46` — `unused_imports`: `use crate::procs;`
2. `crates/hromatite-core/src/raw/m3dump.rs:36` — `clippy::duplicated_attributes`: `#![cfg_attr(not(windows), allow(dead_code))]` дублирует `#[allow(dead_code)]` в `crates/hromatite-core/src/raw/mod.rs:31`
3. `crates/hromatite-core/src/raw/d3d11_shaders.rs:5` — `dead_code`: `VERTEX_SHADER`
4. `crates/hromatite-core/src/raw/d3d11_shaders.rs:121` — `dead_code`: `PIXEL_SHADER`
5. `crates/hromatite-core/src/raw/hostapi.rs:122` — `clippy::needless_return`
6–8. `crates/hromatite-core/src/raw/kiero.rs:266, 348, 404` — `clippy::needless_return` (3 места)

**`hromatite-overlay` (`crates/hromatite-overlay/src/main.rs`):**
- 9 предупреждений: `clippy::upper_case_acronyms` (`type XID`, стр. 44), `clippy::too_many_arguments` (`fill` @ 803, `round_rect` @ 817, `line` @ 863, `circle` @ 871, `arc` @ 903), `clippy::unnecessary_cast` (`height as u32`, стр. 1176), `clippy::collapsible_if` (стр. 1552, 1627).
- 15 ошибок `clippy::undocumented_unsafe_blocks` при включении workspace-линтов (строки `1420, 1422, 1423, 1431, 1432, 1433, 1435, 1448, 1583, 1584, 1586, 1636, 1639, 1640, 1642`).

---

## 2. Карта графа зависимостей и проблем изоляции

Фактическое дерево зависимостей (подтверждено `cargo tree` и манифестами `Cargo.toml`):

```text
Hromatite (bin Hromatite.exe)
└── hromatite-core
    ├── hromatite-abi
    ├── hromatite-commands
    └── hromatite-nuklear (+ cc -> nuklear_shim.c)

hromatite-dll (cdylib hromatite.dll)
├── hromatite-core -> {abi, commands, nuklear}
└── hromatite-eac -> hromatite-core

hromatite-features (cdylib hromatite_features.dll)
├── hromatite-abi
├── hromatite-commands
└── hromatite-eac -> hromatite-core -> hromatite-nuklear
```

### Ключевые нарушения архитектурных границ
1. **`Hromatite.exe -> hromatite-core -> hromatite-nuklear`**: инъектор-CLI (`src/main.rs`) компилирует и линкует Nuklear C-shim, D3D11/Kiero, IL2CPP, Photon, Boehm GC-guard и hot-reload runtime. Кроме того, `src/main.rs:200` вызывает `hot_runtime::smoke(&paths, rounds)` (`crates/hromatite-core/src/raw/hot.rs:1212`), привязывая CLI к резидентному `localmap`/`hostapi`.
2. **`hromatite-features -> hromatite-eac -> hromatite-core -> hromatite-nuklear`**: вопреки контракту изоляции (`crates/hromatite-features/src/lib.rs:3-4`), feature-DLL транзитивно тянет `hromatite-core` и `hromatite-nuklear` (раздутие `hromatite_features.dll` до ~24 MiB в debug). В дополнение `features/src/eac.rs` напрямую использует `hromatite_eac::eos::module_report` и `keyhunt::Hunter`, а сам крейт имеет глобальный `#![allow(unsafe_code)]` (`lib.rs:5`).
3. **`hromatite-dll -> hromatite-core`**: резидентная DLL (`hromatite.dll`) компилирует код кросс-процессного инъектора (`manualmap.rs`, `remote.rs`), который нужен только `Hromatite.exe`.
4. **`hromatite-eac/src/forge.rs -> photon`**: `forge::pump_photon` напрямую вызывает `hromatite_core::photon::{template, emulate_send}`, создавая обратную связку `eac -> il2cpp`. В целевой архитектуре зависимость должна быть инвертирована через callback/trait в `hromatite-runtime`.
5. **Отсутствие выделенных слоёв**: вся низкоуровневая Win32-механика, x64 inline-hooking, D3D11/Nuklear и Unity/IL2CPP смешаны в `crates/hromatite-core/src/raw/*` (27 файлов) и дублируются в `hromatite-eac`.

---

## 3. Карта дублей подсистем

### 3.1 Win32 FFI (6 независимых копий)
1. `crates/hromatite-core/src/raw/ffi.rs:173-282` — основной `unsafe extern "system"` (kernel32/ntdll) + структуры Win32.
2. `crates/hromatite-core/src/raw/localmap.rs:624-627` — встроенный `unsafe extern "system" { fn FreeLibrary(...) }`.
3. `crates/hromatite-core/src/raw/ui.rs:110-116` — встроенный `user32` (`GetForegroundWindow`, `GetClientRect`, `GetCursorPos`, `ScreenToClient`, `GetAsyncKeyState`).
4. `crates/hromatite-eac/src/hook.rs:865-917` (`mod win`) — дублирует `MemBasic` (`MEMORY_BASIC_INFORMATION`), `VirtualAlloc`, `VirtualQuery`, `VirtualProtect`, `FlushInstructionCache`, `GetCurrentProcess`, добавляя `CreateFileMappingA`/`MapViewOfFile`.
5. `crates/hromatite-eac/src/eos.rs:1517-1549` (`mod win`) — `GetModuleHandleA`, `DosHeader`, `ExportDirectory`.
6. `crates/hromatite-eac/src/keyhunt.rs:193-202, 278-306` — дублирует `ReadProcessMemory`, `GetCurrentProcess`, `VirtualQuery` (с `#[allow(clashing_extern_declarations)]` на стр. 301) и `MemoryBasic` (+ Linux `process_vm_readv`/`getpid`).
*(Дополнительно `crates/hromatite-core/src/raw/memory.rs:23-123` экспортирует `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` заглушки линковки для хост-тестов.)*

### 3.2 Два x64 inline-hook движка, спутники и связки
- **Движок 1:** `crates/hromatite-core/src/raw/hook.rs:14-945` — 5-байтный `jmp rel32` + relay-страница (`alloc_near` ±2 GiB), `cmpxchg16b` (`write_window`), `decode_one`/`decode_span`/`Hook`.
- **Движок 2:** `crates/hromatite-eac/src/hook.rs:1-1792` — 12-байтный `mov rax, imm64; jmp rax` (`alloc_near` ±512 MiB), `cas16`/`patch_entry`, `decode`/`decode_min`, инверсия коротких `jcc` / расширение `rel32`, межмодульная общая таблица в именованной секции `Local\hromatite_rust_hooktable_v1`.
- **Механизмы-спутники в `core/raw/`:** `midhook.rs` (512L, лезет во внутренности `raw::hook`), `vmt.rs` (717L, используется только `kiero.rs`), `iat.rs` (1043L, используется только `eac/eos.rs`), `hookset.rs` (330L, только тесты/реэкспорт).
- **Нарушения связности вокруг хуков:**
  - `raw/hook.rs` помимо движка содержит `LateUpdate`-тик (`tick`, `work`, `late_update_detour`, `install_late_update`, `late_update_status`), координацию `gcguard`, трассировку GC-воркеров, вызов `console::dispatch` и `raw::hot::*`.
  - `raw/playerloop.rs:282, 300` напрямую вызывает `raw::hot::original_first_required()` и `hook::tick()`.
  - `eac/eos.rs` и `eac/evp.rs` напрямую завязаны на `eac::hook`, а `eos.rs:1398` — на `hromatite_core::iat`.

### 3.3 Парсеры PE (5+ копий)
1. `crates/hromatite-core/src/pe.rs` — безопасный файловый парсер `PeImage` (секции, экспорты, reloc, imports, TLS).
2. `crates/hromatite-core/src/raw/remote.rs:143-207` — чтение экспортов чужого процесса (`export_in`, `export_ordinal`) через `ReadProcessMemory`.
3. `crates/hromatite-core/src/raw/localmap.rs:637-715` — `LiveModule::from_base` + собственный проход reloc/import/export/TLS.
4. `crates/hromatite-core/src/raw/iat.rs:127-250` — `image_nt`, `data_directory`, `descriptors`, `OrdinalNames`.
5. `crates/hromatite-core/src/raw/discover.rs:79-128, 270-330` — собственный парсер секций и таблицы экспортов.
6. Частичные копии: `crates/hromatite-eac/src/eos.rs:1557-1615` (`resolve_export`) и разбор `SizeOfImage` в `crates/hromatite-core/src/raw/procs.rs:301-306` (`own_module`).

### 3.4 Обход памяти через `VirtualQuery` (3+ реализаций)
1. `crates/hromatite-core/src/raw/{memory.rs, memscan.rs, procs.rs}` (`readable_at`, `readable_range`, `procs::regions`, `memscan::readable_spans`/`RegionMap`).
2. `crates/hromatite-eac/src/eacimage.rs:853-900, 1003-1025` (`live::walk`, `query_region`, смесь `hromatite_core::ffi::VirtualQuery` и `safe::read_bytes`).
3. `crates/hromatite-eac/src/keyhunt.rs:191-370` (`read_block`, `read_window`, `enumerate_regions`) + `crates/hromatite-eac/src/hook.rs:925-945` (`readable`).

### 3.5 Прочие дубли (криптография, EVP, `m3`/телеметрия/утилиты)
- **SHA-256:** `crates/hromatite-core/src/sha256.rs` (217L) vs `crates/hromatite-eac/src/digest.rs` (297L).
- **CRC-32C:** `crates/hromatite-core/src/crc.rs` (79L, `crc32c_eac` + `seal_checksum`) vs `crates/hromatite-eac/src/digest.rs` (`crc32c_eac`).
- **BoringSSL EVP probe:** `crates/hromatite-eac/src/eos.rs:1093-1326` (`install_evp`, `EAC_KEYHUNT_EVP=1`) дублирует `crates/hromatite-eac/src/evp.rs` (`evp::install`, `EAC_EVP=1`) по тем же 5 RVA в `EOSSDK-Win64-Shipping.dll` (`evp.rs` полнее и имеет 6-ю цель).
- **`m3` / телеметрия / память / парсеры адресов:**
  - `core/src/m3.rs` (492L) vs `features/src/m3.rs` (624L, дублирует обход классов/полей/методов поверх `Host`).
  - `core/raw/console.rs:147-717, 918-1073` дублирует `pos`/`who`/`esp`/`m3*` из `features/src/{telemetry, m3}.rs`.
  - `features/src/host.rs:104-187` дублирует `ascii_at`, `ascii_lossy`, `utf16_lossy`, `module_tag` из `core/raw/{memory.rs, console.rs}`.
  - `features/src/memory.rs` (81L) дублирует `parse_addr`/`qwords`/`bytes`/`decode_hex` (которые есть и в `raw/console.rs`, `raw/hook.rs`, `eac/eos.rs`).
- **Разночтения `il2cpp` / `m3probe`:** `raw/il2cpp.rs:12` хардкодит `CORLIB_IMAGE_GETTER_RVA = 0x00c2_9c70` вместо `buildinfo`; `eac/m3probe.rs:20` хардкодит `LEGACY_TABLE_RVA = 0x0e66_f0d0` от 2026-11-17.

### 3.6 Три реализации `hot.rs` и подтверждённый дрейф контракта
1. `crates/hromatite-core/src/hot.rs` (944L) — чистая (`#![forbid(unsafe_code)]`) машина состояний `HotReload`, `Phase`, `HotConfig`, `parse_config`.
2. `crates/hromatite-core/src/raw/hot.rs` (1,396L) — чистые парсеры манифеста/запросов/имён (строки 9–223) + Windows-рантайм `Runtime`, `FeatureImage`, `smoke` (строки 225–1268).
3. `crates/hromatite-console/src/hot.rs` (98L) — хост-парсер строк `[hot] state=...` (`HotState`, `HotStatus`, `parse_hot_status`).

**Выявленные баги и рассинхронизации:**
- **Дрейф состояний (`Stable`/`Requested`/`Canary`):** `raw/hot.rs:751-763` пишет `"stable"`, `"requested"`, `"canary"`, тогда как `console/src/hot.rs:52-62` распознаёт только `idle|empty|ready`, `loading|reloading`, `draining`, `switching`, `active`, `rolling_back|rollback`, `retiring`, `error|failed` — в итоге `stable`/`requested`/`canary` сваливаются в `HotState::Other`.
- **Рассинхрон `ABI=1` vs `ABI=2`:** `hromatite-abi/src/lib.rs:12` задаёт `ABI_VERSION = 2` (и `features/src/feature.rs:226` печатает `abi=2`), однако `raw/hot.rs:178` отклоняет любой манифест, кроме `values[4] == "1"` (`unsupported manifest ABI`), а `tools/test-hot-smoke.sh:30` ищет `abi=1`.
- **Потерянная команда `"ui"` и захардкоженные списки:** `raw/hot.rs:1225-1247` (`resident_command`) и `features/src/feature.rs:374-380` дублируют ручной список из 13 имён (`gct`, `threads`, ..., `seal`, `quit`), забывая `"ui"`, хотя `hromatite-commands/src/lib.rs:170-177` объявляет `ui` с `Scope::Resident`.
- **Пропуск проверки `FeatureApiV1.frame`:** `raw/localmap.rs:306-327` (`feature_api_bytes_valid`) вручную проверяет 5 указателей (`init`, `dispatch`, `tick`, `configure`, `shutdown`), пропуская поле `frame` (добавленное в `ABI_VERSION = 2`), и не переиспользует `hromatite_abi::feature_header_valid` (которая вызывается только в unit-тестах `hromatite-abi`).

---

## 4. Карта подтверждённого мёртвого кода

### 4.1 Подтверждённый мёртвый код (кандидаты на удаление в Stage 5)
1. **`crates/hromatite-core/src/raw/m3dump.rs` (546L)** + `raw/mod.rs:31-32` (`#[allow(dead_code)] pub mod m3dump`): 0 вызовов в кодовой базе.
2. **Legacy mid-function Boehm GC guard в `crates/hromatite-core/src/raw/hook.rs`**: `install_legacy_gc_context_guard` (стр. 1269, `#[allow(dead_code)]`), `gc_context_detour` (1249), `gc_context_route` (1215), `complete_dead_gc_call_raw` (1195), `unlink_by_handle_raw` (1147), `write_synthetic_dead_context_raw` (1113), `note_inline_repair` (1092), `GC_CONTEXT_ORIG`/`GC_THREADS_BASE`/`INLINE_REPAIRS`/`INLINE_LAST_TID`/`GC_CONTEXT_CALL_PROLOGUE`/`WIN64_CONTEXT_*`/`valid_gc_context_call_site` (1075–1103) + тесты (2398–2543); `take_inline_repairs` (998) всегда возвращает `None`.
3. **Недостижимые ветки `hook::command` в `crates/hromatite-core/src/raw/hook.rs:1950-2018, 2081-2157`**: `raw/console.rs:1130` передаёт в `hook::command` только `"latec" | "latehook" | "late"`. Ветки `"unhook"`, `"hooks"`, `"hookat"`, `"tick"` недостижимы; `install_at`, `count_detour`, `HOOKAT_*` и `hook::installed()` (0 вызовов) — мёртвый код.
4. **Неиспользуемые сканеры `crates/hromatite-core/src/raw/memscan.rs` (`#![allow(dead_code)]`):** внешне используются только `stash_cstr` и `call3` (в `raw/photon.rs:1389-1391`). `ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `scan_ascii`, `scan_qwords`, `scan_words`, `call2` нигде не вызываются.
5. **Неиспользуемые `Span`-хелперы `crates/hromatite-core/src/raw/memory.rs`:** `find_qword_in_span` (676), `matches_in_span` (280), `read_usize_in_span` (265) — 0 внешних вызовов (только unit-тесты).
6. **`gamebuild::is_supported()` (`crates/hromatite-core/src/gamebuild.rs:21-23`)**: 0 вызовов (`SUPPORTED_SHA256` используется только в `lateupdate.rs:25`).
7. **`hlog!` (`crates/hromatite-core/src/log.rs:114-117`)**: 0 вызовов.
8. **`discover::Image::data` (`crates/hromatite-core/src/raw/discover.rs:51`)**: `#[allow(dead_code)] pub data: Section`, нигде не читается.
9. **Неиспользуемые `ffi`-экстерны (`crates/hromatite-core/src/raw/ffi.rs:237, 261, 262`)**: `CreateThread`, `RemoveVectoredExceptionHandler`, `SetUnhandledExceptionFilter` — 0 вызовов.
10. **Остатки `seal` / `L2B` / `crypto.rs`:** `hromatite-dll/src/lib.rs:72-77` и `raw/hook.rs:1852-1856` жестко отклоняют `HROMATITE_SEAL!=0` и `HROMATITE_L2B!=0`, а `raw/hook.rs:1055-1059` — `Action::Seal | Action::Quit`. Тем не менее остаются `core/src/crypto.rs` (166L), `manualmap::{seal_image, Options::seal}`, `--seal` в `src/main.rs:179` и `"seal"` в `console.rs`/`hot.rs`/`feature.rs`/`hromatite-commands` (нужно решение Stage 5: удалить целиком или оставить legacy-отказ в команде).
11. **Дублирующий `eos::install_evp` (`crates/hromatite-eac/src/eos.rs:1093-1326`)**: полностью перекрывается более полным `evp.rs`, но всё ещё вызывается при `EAC_KEYHUNT_EVP=1` на `eos.rs:1049` (в Stage 4/5 при удалении `install_evp` нужно перенаправить `EAC_KEYHUNT_EVP` на `evp::install` или явно вывести флаг из обращения).
12. **Мелкие clippy-находки:** `use crate::procs;` в `core/src/m3.rs:46`, `VERTEX_SHADER`/`PIXEL_SHADER` в `raw/d3d11_shaders.rs:{5,121}` (проверить необходимость при переносе в `hromatite-render`).

### 4.2 Не является мёртвым кодом (поддержка/инфраструктура/host-gating)
- `raw/hostapi.rs` и `raw/localmap.rs`: `#![allow(dead_code)]` подавляет предупреждения на Linux-сборке для `#[cfg(windows)]`-API, вызываемого из `raw/hot.rs`.
- `raw/hookset.rs`, `raw/midhook.rs`, `raw/kiero.rs::{init, render, shutdown, hook_module_export}`, `raw/photon.rs::{poll, tick}`: покрытая тестами инфраструктура, частично временно отвязанная от старта (`photon::poll`/`tick`) либо заготовка под единый `hromatite-hook`/`hromatite-render`.
- `hromatite-nuklear/src/lib.rs:36, 234` (`Nk::cmd_mem` держит буфер для C-структуры `nk_buffer`).

---

## 5. Карта декомпозиции монолитов

| Монолит | Строк | Внутренние логические блоки и целевой распил |
|---|---:|---|
| `crates/hromatite-eac/src/eos.rs` | 3,532 | (1) `Hooks` install/status/counters (1–1092) → `eos/hooks.rs`; (2) `install_evp` (1093–1326) → удалить в пользу `evp.rs`; (3) `install_iat` (1327–1456) → через `hromatite-hook::iat`; (4) `mod win` + `resolve_export` (1515–1616) → перенести в `hromatite-win`; (5) detours `EOS_AntiCheatClient_*` + `epoch_gate` (1617–2032, 2580–3000) → `eos/hooks.rs`; (6) pipeline генератора/репортов/forge (2033–2579) → `eos/pipeline.rs` + `eos/diag.rs`; (7) unit-тесты (3000–3532). |
| `crates/hromatite-core/src/raw/photon.rs` | 2,717 | Перенести в `hromatite-il2cpp::photon/`: `mod.rs` (константы, форматирование 47–431, тесты), `decode.rs` (`Dictionary` слоты, `decode_event_params` 435–740), `emulate.rs` (`Template`, `emulate_send`, `SendConfig`, `parse_payload_records`, `retarget_frame` 863–1299), `hook.rs` (резолв `SendOperation`/`OnEvent`, `RING`, детуры, `probe_report`, `install_*`, `poll` 741–862, 1300–2101). |
| `crates/hromatite-core/src/raw/hook.rs` | 2,568 | Расформировать: (1) x64 inline engine (14–945) → `hromatite-hook`; (2) `LateUpdate` + GC worker detours → `hromatite-il2cpp`; (3) `tick`/`work`/оркестрация → `hromatite-runtime`; (4) legacy GC guard + `hookat`/`count_detour`/`installed` → удалить. |
| `crates/hromatite-overlay/src/main.rs` | 2,023 | Добавить `src/lib.rs` и разбить на: `x11.rs` (FFI, `Gfx`, окна, `rounded_mask`, `shape_window`, `apply_opacity`), `draw.rs` (палитра, примитивы `fill`..`fit`), `config.rs` (`Config`, `prefix_from_args`), `telemetry.rs` (`Target`, `Telemetry`, `kv*`), `demo.rs` (`demo_feed`), `panel.rs` (`Ui`, `Row`, `draw_panel`, `draw_radar`), `esp.rs` (`draw_esp`), `main.rs` (event loop, tailing). Включить workspace-линты, исправить 15 `undocumented_unsafe_blocks` и 9 clippy-предупреждений, добавить unit-тесты для `config`/`telemetry`/`panel`. |
| `crates/hromatite-eac/src/eacimage.rs` | 1,658 | Вынести Win32 `VirtualQuery`-обход (`live::walk`, 797–1027) в `hromatite-win`; остальное разбить на `eacimage/{mod.rs, elf.rs, locator.rs, report.rs}` (ELF64-парсер 1–695, `/proc/self/maps` 695–795 / 1028–1155, команда отчёта 1156–1658). |
| `crates/hromatite-eac/src/report.rs` | 1,644 | Разбить на `report/{mod.rs, parser.rs, generator.rs, stream.rs}` (`Report` 1–300, `GenConfig`/`Generator` 301–700, `Stream` + `retarget` 701–1140, тесты 1141–1644). |
| `crates/hromatite-console/src/model.rs` | 1,332 | Разбить на `model/{mod.rs, buffers.rs, observe.rs, input.rs}` с сохранением публичных реэкспортов (`Action`, `ConsoleModel`, `Mode`, `field_manual`). |
| `crates/hromatite-eac/src/forge.rs` | 1,228 | Разбить builder шаблона (1–609), `StreamConfig` (610–720) и кольцо/поток `pump_photon` (721–900), инвертировав прямой вызов `hromatite_core::photon::{template, emulate_send}` через trait/callback из `hromatite-runtime`. |

---

## 6. Инвентаризация `tools/` (33 скрипта + `tools/data/`) и артефактов корня

### 6.1 Python-скрипты (19)

| Файл | Строк | Назначение | Активные ссылки | Решение | Обоснование |
|---|---:|---|---|---|---|
| `tools/buildgen.py` | 1,499 | Генерация `hromatite-abi/src/build.rs`, `tools/data/build-*.json`, `il2cpp-export-names-*.json` | `AGENTS.md`, `test_buildgen.py`, `build.rs`, `buildinfo.rs` | **KEEP** | Основной генератор per-build смещений/якорей |
| `tools/test_buildgen.py` | 358 | Синтетические и oracle-тесты для `buildgen.py` | `AGENTS.md`, `build.rs` | **KEEP** | Набор приёмки `buildgen.py` |
| `tools/export_transfer.py` | 338 | Перенос обфусцированных имён экспортов IL2CPP между патчами | `AGENTS.md`, `buildgen.py`, `discover.rs` | **KEEP** | Используется `buildgen.py` |
| `tools/symmap.py` | 161 | Генерация `symbols.rs` и `symbols-<sha12>.json` | `AGENTS.md`, `refresh.sh`, `symbols.rs` | **KEEP** | Обновить путь назначения на `hromatite-il2cpp` в Stage 4 |
| `tools/api_map.py` | 428 | Парсинг lazy-резолверов `UnityPlayer.dll` → `api_map.json` | `refresh.sh`, `AGENTS.md`, `test_api_map.py` | **KEEP** | Вызывается из `refresh.sh --check` (проверить `DEFAULT_OUT`) |
| `tools/test_api_map.py` | 97 | Офлайн-тесты `api_map.py` | `AGENTS.md` | **KEEP** | Тесты к `api_map.py` |
| `tools/test_gc_detour_abi.py` | 136 | Проверка Win64 ABI `gc_context_detour` в release DLL | `AGENTS.md` | **REFACTOR (Stage 4–5)** / **ARCHIVE** | На `5064586` уже падают 2/4 теста (`test_gc_guard_takes_no_lock_across_a_collection`, `test_suspend_call_site_stub_keeps_the_router_abi`), захардкожен путь `crates/hromatite-core/src/raw/gcguard.rs`, а 1-й тест требует удаляемый в Stage 5 `gc_context_detour` |
| `tools/test-console-logs.py` | 199 | PTY-тест переключения логов в `hromatite-console` | `AGENTS.md` | **KEEP** | Активный TUI-тест |
| `tools/test-console-wheel.py` | 147 | PTY-тест прокрутки колесом мыши в `hromatite-console` | `AGENTS.md` | **KEEP** | Активный TUI-тест |
| `tools/host_scan.py` | 152 | Внешний `/proc/<pid>/mem` сканер `Il2CppClass` | `AGENTS.md` | **KEEP** (или **ARCHIVE**) | Полезный RE-скрипт при `ptrace_scope=0` |
| `tools/gamediff.py` | 84 | Сравнение `api_map.json` и выходов `dump.py` | `AGENTS.md`, `docs/baselines/README.md` | **ARCHIVE** (или **KEEP** без `dump.py`) | Частично завязан на отложенный `dump.py` |
| `tools/gct_table.py` | 95 | Старый искатель RVA `GC_threads[]` | `AGENTS.md` | **DELETE** (или **ARCHIVE**) | Полностью заменён `buildgen.py` (`GcAnchors::threads`) |
| `tools/resolve_api.py` | 129 | Ранний матчер паттернов (`~/Downloads/Il2CppKnownPatterns.inl`) | `AGENTS.md` | **DELETE** (или **ARCHIVE**) | Заменён связкой `api_map.py` + `buildgen.py` + `export_transfer.py` |
| `tools/meta_parse.py` | 306 | Python-парсер расшифрованной `global-metadata.dat` | `AGENTS.md`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Относится к архивному `IL2CPP-DUMP` |
| `tools/dump.py` | 74 | Экспорт метаданных через `meta_parse.py` | `gamediff.py`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Относится к архивному `IL2CPP-DUMP` |
| `tools/il2cpp_methods.py` | 189 | Конвертер `Il2CppDumper/script.json` | `il2cppdump.sh`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Относится к архивному `IL2CPP-DUMP` |
| `tools/test_dumps.py` | 49 | Тесты `dump.py` + `gamediff.py` | `docs/HROMATITE-SKILLS-AND-MEMORY.md` | **ARCHIVE** / **DELETE** | Тестирует мёртвый `dump.py` |
| `tools/test_il2cpp_methods.py` | 142 | Тесты `il2cpp_methods.py` | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **ARCHIVE** / **DELETE** | Тестирует мёртвый `il2cpp_methods.py` |
| `tools/test_meta_parse.py` | 122 | Тесты `meta_parse.py` | `docs/HROMATITE-SKILLS-AND-MEMORY.md` | **ARCHIVE** / **DELETE** | Тестирует мёртвый `meta_parse.py` |

### 6.2 Shell-скрипты (14)

| Файл | Строк | Назначение | Активные ссылки | Решение | Обоснование |
|---|---:|---|---|---|---|
| `tools/hromatite-launch.sh` | 476 | Steam Launch Options wrapper + инжект | `hromatite-mode`, `AGENTS.md`, `eac_probe.sh` | **KEEP** | Основная точка запуска игры и инжекта |
| `tools/hromatite-tui.sh` | 20 | Сборка и запуск `hromatite-console` | `hromatite-autoconsole.sh`, `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** | Основной лаунчер TUI |
| `tools/hromatite-autoconsole.sh` | 118 | Наблюдатель `~/.hromatite-live` с автозапуском TUI | `hromatite-launch.sh`, `AGENTS.md` | **KEEP** | Активный вотчер консоли |
| `tools/hromatite-overlay.sh` | 51 | Сборка и запуск `hromatite-overlay` через `nix-shell` | `AGENTS.md`, `hromatite-overlay/src/main.rs` | **KEEP** | Основной лаунчер оверлея |
| `tools/hromatite-reload.sh` | 181 | Атомарная публикация `features-N-<sha12>.dll` и `current.manifest` | `hromatite-watch.sh`, `test-hromatite-hot.sh`, `docs/HOT-RELOAD.md` | **KEEP** (**обновить путь в Stage 4 и ABI в Stage 5**) | Основной паблишер hot-reload; строка `99` читает `crates/hromatite-core/src/symbols.rs` (обновить на `hromatite-il2cpp` в Stage 4), строка `67` (`[ "$OLD_ABI" = 1 ]`) и строки `125, 174, 180` (`ABI=1`) обновляются на `ABI=2` вместе с `raw/hot.rs:178` в Stage 5 |
| `tools/hromatite-watch.sh` | 102 | Polling-вотчер для `hromatite-reload.sh` | `test-hromatite-hot.sh`, `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** | Активный вотчер hot-reload |
| `tools/test-hromatite-hot.sh` | 340 | Герметичные тесты `hromatite-reload.sh` и `hromatite-watch.sh` | `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** (**обновить путь в Stage 4 и ABI в Stage 5**) | Активный регрессионный тест; строка `225` читает `crates/hromatite-core/src/symbols.rs` (обновить на `hromatite-il2cpp` в Stage 4), строки `73, 88` ожидают `ABI=1` и обновляются на `ABI=2` синхронно с `hromatite-reload.sh` и `raw/hot.rs:178` в Stage 5 |
| `tools/test-hot-smoke.sh` | 49 | Proton smoke-тест `Hromatite.exe hot-smoke` | `AGENTS.md` | **KEEP** (**починить**) | **Дефект на стр. 30:** проверяет `abi=1` вместо `abi=2` (и учесть перенос `hot-smoke` из `Hromatite.exe` в Stage 3) |
| `tools/eac_probe.sh` | 312 | Пост-проверка PASS/FAIL/UNKNOWN для EAC/Photon | `hromatite-launch.sh` | **KEEP** | Активный верификатор логов EAC |
| `tools/refresh.sh` | 38 | Обновление `symmap.py` + `api_map.py --check` после патча игры | `AGENTS.md` | **KEEP** | Активный скрипт обновления (поправить путь `symbols.rs` в Stage 4) |
| `tools/test-loadlib.sh` | 48 | Изолированный стенд `--mode loadlib` + PEB-unlink | `AGENTS.md` | **ARCHIVE** (или **KEEP**) | Ручной стенд проверки `--mode loadlib` |
| `tools/hromatite-console.sh` | 188 | Устаревший shell tailer + `--status` по `SEAL_SIG` | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **DELETE** | Заменён Rust `hromatite-console` и отказом от `--seal` |
| `tools/test-seal.sh` | 91 | Стенд для `inject --seal` | `AGENTS.md` | **DELETE** | Устарел: L2 `--seal` отклонён в рантайме |
| `tools/il2cppdump.sh` | 146 | Хост-половина удалённого `dumpmeta` → `Il2CppDumper` | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **ARCHIVE** / **DELETE** | `dumpmeta` удалён из payload |

### 6.3 Данные `tools/data/` (8 файлов) и артефакты корня
- **Активные данные `buildgen.py` / `test_buildgen.py`:** `build-648f61b13e4f.json`, `build-245768f25214.json`, `il2cpp-export-names.json`, `il2cpp-export-names-245768f2.json`.
- **Ранние/справочные входы (`api_map.py` и RE):** `il2cpp-api-anchors.json`, `il2cpp-api-names.txt`, `Il2CppKnownPatterns.inl`, `message.txt`.
- **Мусорный файл в корне:** `hromatite-mode.launch-bak.1733464` (неотслеживаемый бэкап от прерванного запуска `tools/hromatite-launch.sh:126`, идентичен `hromatite-mode`) → **удалить в Stage 5**.

---

## 7. Целевой план этапов 2–5

### 7.1 Жёсткие инварианты графа зависимостей
1. `hromatite-injector` (`Hromatite.exe`) зависит **только** от `hromatite-win` (+ `std`) — никаких `render`, `il2cpp`, `eac`, `runtime`.
2. `hromatite-dll` (`hromatite.dll`) **не** зависит от `manualmap`/`remote` (`hromatite-injector`).
3. `hromatite-hook` зависит **только** от `hromatite-win` — 0 ссылок на `LateUpdate`, `EOSSDK`, `il2cpp`, `render`, `hot`.
4. `hromatite-eac` **не** зависит от `hromatite-il2cpp` (вызов `photon` из `forge` инвертируется через callback/trait в `hromatite-runtime`).
5. `hromatite-features` **не** тянет транзитивно `hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp` или `hromatite-runtime`: `hromatite-eac` подключается с `default-features = false` (чистый протокол/сессия/tap/чистый `eos::state`/чистый парсер `eacimage/{elf,locator}`), а всё ОС-зависимое в `hromatite-eac` выносится под feature `"runtime"`.
6. `hromatite-core` и `src/raw/` полностью расформированы к концу Stage 4.

### 7.2 Маппинг файлов в целевые крейты

| Целевой крейт | Содержимое и источники миграции |
|---|---|
| **`hromatite-win`** | `core/src/raw/{ffi.rs, sys.rs, memory.rs, context.rs, procs.rs}`, `core/src/pe.rs`, `core/src/sha256.rs`, `core/src/log.rs` (чистый файловый логгер + guarded helper, без мёртвого `hlog!`), `core/src/lib.rs` константы (`GAME_EXE`, `GAME_ASSEMBLY`, `UNITY_PLAYER`), `core/src/raw/memscan::{stash_cstr, call3}`, `core/src/raw/remote.rs` (под feature `"remote"` или в `hromatite-injector`), Win32 FFI/обход регионов/PE из `eac/{hook.rs, eos.rs, eacimage.rs, keyhunt.rs}` и `raw/ui.rs`. |
| **`hromatite-hook`** | Единый x64 inline-движок (`raw/hook.rs:1-945` + `eac/hook.rs` + общая таблица хуков `Local\hromatite_rust_hooktable_v1`), `raw/midhook.rs`, `raw/vmt.rs`, `raw/iat.rs`, `raw/hookset.rs`, `eac/tests/hook_engine.rs`. |
| **`hromatite-render`** | `crates/hromatite-nuklear/*` (`build.rs`, `nuklear_shim.c`, `third_party/`, `src/{lib,sys}.rs`, `tests/frame.rs`), `raw/kiero.rs`, `raw/d3d11.rs`, `raw/d3d11_shaders.rs`, `raw/ui.rs` (чистый UI-цикл поверх callback/поверхности `Present` и ввода из `hromatite-win`). |
| **`hromatite-il2cpp`** | `core/src/{symbols.rs, buildinfo.rs, gamebuild.rs (const SUPPORTED_SHA256), gcthreads.rs, lateupdate.rs, m3.rs}`, `raw/{discover.rs, il2cpp.rs, gcguard.rs, playerloop.rs}`, `raw/photon.rs` → `photon/{mod, decode, hook, emulate}.rs` (CRC-32C через `hromatite_abi::crc`), `LateUpdate`-часть из `raw/hook.rs`, реэкспорт/использование `GAME_ASSEMBLY`/`UNITY_PLAYER` из `hromatite-win`, `core/tests/{frames.rs, report_stream.rs}` (`frames.rs` переводится на `hromatite_abi::crc`). (`eac/m3probe.rs` не переносится — удаляется в Stage 4 как перекрытый `discover::il2cpp_anchors` + `buildinfo`.) |
| **`hromatite-eac`** | Зависимость от `hromatite-abi` (для `crc32c_eac` и общего контракта). Всегда компилируемое чистое ядро (`default-features = false`): `capture.rs`, `clock.rs`, `codec.rs`, `digest.rs` (реэкспорт `hromatite_abi::crc::crc32c_eac` + SHA-256), `epoch.rs`, `eventlog.rs`, `payload.rs`, `session.rs`, `tailer.rs`, `tap.rs`, `report/{mod, parser, generator, stream}.rs`, чистые типы/хелперы `forge` (`MessageHeader`, `Template`, `StreamBody`, `StreamConfig`, `TotalMode`, `Via`, `INTERVAL_MS`, `seal_frame`, `push_frame`, `drain_frames`, `clear_ring`, `ring_pushed`, `note`, `take_log` и остальные чистые хелперы кольца/заголовка, используемые в `report.rs` и его тестах) **плюс** вызываемые `Delivery` и `forge::pump_photon()` с `#[cfg(not(feature = "runtime"))]`-заглушкой, возвращающей `Delivery { unarmed: true, ..Delivery::default() }` (либо с зарегистрированным callback-слотом, по умолчанию unarmed), чтобы `report.rs:644`, `report.rs:1553` и `forge.rs:1210` собирались и проходили без `hromatite-core`, `eos/state.rs` (чистые `Hooks`, `HookEvent`, `Counters`, `Report`, `EXPORTS`, `EVENTS`, `SINK`, 26 глобальных атомиков для `Hooks::counters`/`Hooks::status_lines` — `OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `CALLS`, `PROTECT_*`, `OVERHEAD_*`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`, а также `EPOCH`/`epoch_line()`; на внутренних хелперах `hook_log`/`epoch_*`, вызываемых только из `runtime`-веток, ставится `#[cfg_attr(not(feature = "runtime"), allow(dead_code))]`; `Hooks::install` переводится с `#[cfg(windows)]`/`#[cfg(not(windows))]` (`eos.rs:744, 1087`) на `#[cfg(all(windows, feature = "runtime"))]`/`#[cfg(not(all(windows, feature = "runtime")))]` с возвратом `Err` при отсутствии `"runtime"`), чистые парсеры `eacimage/{elf, locator}.rs`, чистые таблицы/парсеры `keyhunt`. Под feature `"runtime"` (по умолчанию, тянет `hromatite-win` + `hromatite-hook`, 0 зависимостей от них при `default-features = false` уже в Stage 3): в `eac/src/lib.rs` закрываются `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;` (или `hromatite_hook`) и `pub use hook::Hook;` (`lib.rs:51-57, 70`), `eos/{mod, hooks, pipeline, diag}.rs` (`use crate::hook` (`eos.rs:75`), `iat::status()` (`eos.rs:682`), блок `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`), `install_iat` (`1367`), `module_report` (`1387`), `eos::tests` (`2958-3532`), а также `m3probe` и вызов `m3probe::probe()` (`eos.rs:1065`) закрыты `#[cfg(feature = "runtime")]` в Stage 3 и удаляются в Stage 4), `evp.rs` (без дубля `install_evp`; `EAC_KEYHUNT_EVP` перенаправляется на `evp::install` или выводится), `eacimage/{live, report}.rs` (`NativeMemRead`, `live`, `locate_native` (`1294-1357`, вызывает `NativeMemRead::new` на `1312`), `locate_unix`/`UnixMemRead`/`find_process_pid`/`maps_of` (`1040-1194`), `command` и inline-тесты `eacimage.rs:1568, 1633` под `#[cfg(feature = "runtime")]`), тело `forge::pump_photon` с `hromatite_core::photon` (под `#[cfg(feature = "runtime")]` в Stage 3, в Stage 4 инвертируется через callback/trait), `keyhunt::Hunter` + ОС-функции чтения/обхода памяти (`read_maps` (`167-180`), `read_block` (`190-256`), `read_window` (`259-272`), `enumerate_regions` (`276-369`), `find_in_region` (`1102-1114`) поверх `hromatite-win`, с сохранением `#[cfg(not(windows))]` `process_vm_readv` внутри `"runtime"` для Linux host-тестов `tests/keyhunt_scan.rs`; inline-тест `keyhunt.rs:1160` закрыт `#[cfg(feature = "runtime")]`, чтобы `default-features = false` на Windows не касался `hromatite-win`). Тесты/фикстуры остаются в крейте: `eac/tests/{eos_install.rs, epochs.rs, keyhunt_scan.rs}` + `tests/fixtures/` (`required-features = ["runtime"]` в `Cargo.toml` для `tests/keyhunt_scan.rs` и `tests/eos_install.rs`; `hook_engine.rs` переносится в `hromatite-hook`). |
| **`hromatite-injector`** | `src/main.rs` (`Hromatite.exe`: `bridge`, `inject`, `procs`, `mods`), `core/src/manualmap.rs`, `core/src/raw/remote.rs`, временно в Stage 3 — `core/src/crypto.rs` (для `manualmap::seal_image` до удаления `--seal`/`crypto.rs` в Stage 5); зависимость строго от `hromatite-win`. (`hot-smoke` переносится в `hromatite-runtime` как отдельный бинарник/тест.) |
| **`hromatite-runtime`** | Единый `hot`-роутер (`core/src/hot.rs` + `raw/hot.rs`; временно в Stage 3 в `raw/hot.rs::resident_command` (`1225-1247`) добавляется `"eac.module"` до унификации роутинга через `hromatite-commands` в Stage 5), `raw/localmap.rs` (с проверкой `FeatureApiV1.frame` + `feature_header_valid`), `raw/hostapi.rs`, `raw/console.rs` (чтение/маршрутизация команд без дублей фич; диспетчеризация `"eac.module"` через `hromatite_eac::eacimage::command`), оркестрация старта из `hromatite-dll/src/lib.rs`, тик `keyhunt::Hunter` (при `EAC_KEYHUNT=1`), `LateUpdate`/PlayerLoop tick wiring, связка Photon↔EAC forge, бинарник `hot_smoke`. |
| **`hromatite-dll`** | Тонкий `cdylib` (`hromatite.dll`): `DllMain` + вызов `hromatite_runtime::startup`, ноль зависимостей на `manualmap`/`remote`. |
| **`hromatite-features`** | Существующие `lib.rs`, `feature.rs` (проверка resident-only команд, включая `"eac.module"`, выполняется **до** `command == "eac" || command.starts_with("eac.")` на `feature.rs:172`), `host.rs`, `m3.rs`, `render.rs` (Unity `Graphics.DrawMesh`), `telemetry.rs`, `eac.rs` (без `hunter: Hunter`, без строк статуса `self.hunter` в `eac.rs:614-619` и без `"eac.module"`), а также `#[cfg(test)]` модули `game_fixture.rs` и `test_support.rs` — без модуля `memory.rs` (с переносом `&Host`-хелперов `qwords`/`bytes`, нужных парсеров `parse_addr`/`decode_hex` и сохранением unit-теста `memory.rs:73` `addresses_and_hex_writes_validate_their_format` в `host.rs` или `feature.rs`, чтобы сохранились все 24 теста), без дублей resident-списка, зависимость только от `hromatite-abi`, `hromatite-commands` и `hromatite-eac` (`default-features = false`); заменить крейтовый `#![allow(unsafe_code)]` на `#![deny(unsafe_code)]` + локальные `#[allow(unsafe_code)]`. |
| **`hromatite-abi` & `hromatite-commands`** | `hromatite-abi` (`#![forbid(unsafe_code)]`): добавить `crc` (`crc32c_eac`, `seal_checksum` из `core/src/crc.rs`) и `hot` wire/config/status контракт для единого использования в `hromatite-runtime` и `hromatite-console`. `hromatite-commands`: перевести `"eac.module"` в `Scope::Resident` (Stage 3) и добавить предикат `Scope`/маршрутизации (`is_resident_forwarded` / `is_router_command`), включающий `"ui"`. |
| **`hromatite-console` & `hromatite-overlay`** | `hromatite-console`: `model.rs` → `model/{mod, buffers, observe, input}.rs`, `hot.rs` перевести на общий контракт `hromatite-abi::hot`. `hromatite-overlay`: `lib.rs` + `x11, draw, config, telemetry, demo, panel, esp, main` + workspace-линты + unit-тесты. |
| **`test-target`** | `crates/test-target` остаётся в workspace без изменений (тестовый процесс-мишень для инъектора). |

### 7.3 Поэтапный план работ
- **Stage 2 (M2) — Фундамент (`hromatite-win` и `hromatite-hook`):**
  1. Создать `crates/hromatite-win` и перенести FFI, `memory`/`memscan` (`stash_cstr`, `call3`), `context` (VEH), `pe`, `procs`/PEB, `sys`, `sha256`, `log`, константы `GAME_EXE`/`GAME_ASSEMBLY`/`UNITY_PLAYER` (без переноса неиспользуемых `memscan`/`memory` span-хелперов, `hlog!` и лишних FFI-экстернов).
  2. Создать `crates/hromatite-hook` поверх `hromatite-win`, объединив два inline-движка (`raw/hook.rs:1-945` + `eac/hook.rs`) и `midhook`/`vmt`/`iat`/`hookset` без ссылок на `LateUpdate`/`EOSSDK` (перенести `eac/tests/hook_engine.rs`).
  3. Перевести `hromatite-core` и `hromatite-eac` на `hromatite-win` и `hromatite-hook`, удалив дубли FFI, `VirtualQuery` и второго движка.
- **Stage 3 (M3) — Точки входа, рендер и очистка `features`:**
  1. Выделить `crates/hromatite-render` (`hromatite-nuklear` + `kiero` + `d3d11` + `d3d11_shaders` + `ui`).
  2. Разделить `crates/hromatite-injector` (`Hromatite.exe`: CLI + `manualmap` + `remote` + временный `crypto.rs` до удаления `--seal` в Stage 5) и `crates/hromatite-runtime` (+ тонкий `hromatite-dll`), вынеся `hot-smoke` из `Hromatite.exe`.
  3. Очистить `hromatite-features` (удалить `memory.rs`, перенеся `&Host`-хелперы `qwords`/`bytes`, чистые `parse_addr`/`decode_hex` и тест `memory.rs:73` `addresses_and_hex_writes_validate_their_format` в `host.rs` или `feature.rs` для сохранения всех 24 тестов; сузить `unsafe_code`; в `feature.rs::execute` проверять resident-only команды, включая `"eac.module"`, **до** `command == "eac" || command.starts_with("eac.")` на `feature.rs:172`) и перевести `hromatite-features` на `hromatite-eac = { default-features = false }` с **нулевой** зависимостью `hromatite-eac` (`default-features = false`) от `hromatite-core` / `hromatite-win` / `hromatite-hook` уже в Stage 3:
     - оставить чистые типы/хелперы `forge` (`MessageHeader`, `seal_frame`, `push_frame`, `INTERVAL_MS`, `note` и хелперы кольца для `report.rs` и его тестов) **плюс** вызываемые `Delivery` и `forge::pump_photon()` в чистом ядре с `#[cfg(not(feature = "runtime"))]`-заглушкой `Delivery { unarmed: true, ..Delivery::default() }` (или зарегистрированным callback-слотом, по умолчанию unarmed), закрыв только `hromatite_core::photon`-тело `pump_photon` (до инверсии в Stage 4) и `m3probe` (+ вызов `eos.rs:1065`) под `#[cfg(feature = "runtime")]` уже в Stage 3, чтобы `report.rs:644`, `report.rs:1553` и `forge.rs:1210` оставались зелёными без `hromatite-core`;
     - в `eac/src/lib.rs` закрыть `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;` (или `hromatite_hook`) и `pub use hook::Hook;` (`lib.rs:51-57, 70`) под `#[cfg(feature = "runtime")]`; вынести в всегда компилируемый `eac::eos::state` чистые `Hooks`/`HookEvent`/`Counters`/`Report`/`EXPORTS`/`EVENTS`/`SINK`, все 26 глобальных атомиков для `Hooks::counters`/`Hooks::status_lines` (`OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`) и `EPOCH` (`epoch_line()`), поставив `#[cfg_attr(not(feature = "runtime"), allow(dead_code))]` на внутренние хелперы `hook_log`/`epoch_*`, а `eos/{hooks, pipeline, diag}`, `use crate::hook` (`eos.rs:75`), `evp`, `iat::status()` (`eos.rs:682`), блок `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`), `install_iat` (`1367`), `module_report` (`1387`) и `eos::tests` (`2958-3532`) закрыть под `#[cfg(feature = "runtime")]`, переведя `Hooks::install` (`eos.rs:744, 1087`) с `#[cfg(windows)]`/`#[cfg(not(windows))]` на `#[cfg(all(windows, feature = "runtime"))]`/`#[cfg(not(all(windows, feature = "runtime")))]` (возвращает `Err` без `"runtime"`);
     - закрыть `keyhunt::Hunter` + ОС-функции чтения/обхода памяти (`read_maps` (`167-180`), `read_block` (`190-256`), `read_window` (`259-272`), `enumerate_regions` (`276-369`), `find_in_region` (`1102-1114`) поверх `hromatite-win`, сохраняя `#[cfg(not(windows))]` `process_vm_readv` из `keyhunt.rs:223-256` внутри `"runtime"` для Linux host-тестов) и inline-тест `keyhunt.rs:1160` под `#[cfg(feature = "runtime")]`, чтобы на Windows `default-features = false` не тянул `hromatite-win`; добавить `required-features = ["runtime"]` в `Cargo.toml` для `tests/keyhunt_scan.rs` и `tests/eos_install.rs`, убрать `hunter` и строки статуса `self.hunter` (`crates/hromatite-features/src/eac.rs:614-619`) из `features/src/eac.rs` и тикать `Hunter` в `hromatite-runtime`;
     - перевести `"eac.module"` в `hromatite-commands` с `Scope::Feature` на `Scope::Resident`, временно добавить `"eac.module"` в `raw/hot.rs::resident_command` (`1225-1247`) наряду с диспетчеризацией в `raw/console.rs` в Stage 3 (до унификации роутинга через `hromatite-commands` в Stage 5), оставив чистый парсер ELF/`maps` в `eacimage/{elf, locator}` и закрыв `live`/`NativeMemRead`/`locate_native` (`1294-1357`, вызывает `NativeMemRead::new` на `1312`)/`locate_unix`/`UnixMemRead`/`find_process_pid`/`maps_of` (`1040-1194`)/`command` + inline-тесты `eacimage.rs:1568, 1633` под `#[cfg(feature = "runtime")]`.
  4. Починить `localmap::feature_api_bytes_valid` (`frame` + `feature_header_valid`).
- **Stage 4 (M4) — Домены (`hromatite-il2cpp`, `hromatite-eac`), роспуск `core` и хост-инструменты:**
  1. Вынести `crc32c_eac` и `seal_checksum` из `core/src/crc.rs` в `hromatite-abi::crc`; перевести `hromatite-il2cpp::photon` + `tests/frames.rs` на `hromatite_abi::crc`, добавить `hromatite-abi` в `hromatite-eac` с реэкспортом `crc32c_eac` в `digest.rs` и удалить `core/src/crc.rs`.
  2. Создать `crates/hromatite-il2cpp` (перенести `symbols`, `buildinfo`, `discover`, `il2cpp`, `m3`, `gcthreads`, `gcguard`, `lateupdate`, `playerloop`, `photon/{mod, decode, hook, emulate}`, `tests/{frames.rs, report_stream.rs}`); убрать захардкоженный RVA в `il2cpp.rs`; обновить путь к `symbols.rs` в `tools/symmap.py`, `tools/refresh.sh`, `tools/hromatite-reload.sh:99` и `tools/test-hromatite-hot.sh:225`.
  3. Полностью расформировать `crates/hromatite-core` и `src/raw/`, удалив при переносе/роспуске `core`-специфичный мёртвый код (`raw/m3dump.rs`, `gamebuild::is_supported`, `discover::Image::data`, неперенесённые мёртвые хелперы `log`/`ffi`/`memscan`/`memory`).
  4. Разгрузить и декомпозировать `hromatite-eac` (`eos` → `state`/`hooks`/`pipeline`/`diag`, окончательно удалить `eac/m3probe.rs` и вызов `m3probe::probe()` в `eos.rs:1065`, `report` → `parser`/`generator`/`stream`, `eacimage` → `elf`/`locator`/`live`/`report`, инвертировать `forge::pump_photon` через callback/trait, убрать `install_evp` с перенаправлением `EAC_KEYHUNT_EVP` на `evp::install` или выводом флага).
  5. Декомпозировать `hromatite-overlay/src/main.rs` (с включением 3 workspace-линтов и добавлением unit-тестов) и `hromatite-console/src/model.rs`.
- **Stage 5 (M5) — Единый `hot`, чистка мёртвого кода, `tools/` и документация:**
  1. Унифицировать три `hot.rs` через общий контракт в `hromatite-abi` (закрыв `Stable`/`Requested`/`Canary` и синхронизировав `ABI_VERSION = 2` в `raw/hot.rs:178` вместе с `tools/hromatite-reload.sh:67,125,174,180` (`OLD_ABI = 1` и `ABI=1`) и `tools/test-hromatite-hot.sh:73,88`) и маршрутизацию команд через `hromatite-commands` (починив `"ui"`).
  2. Удалить оставшийся подтверждённый мёртвый код (§4.1: legacy GC guard / `gc_context_detour`, `hookat`/`unhook`/`hooks`/`tick`, `--seal`/`L2B`/`seal_image`/временный `crypto.rs` из `hromatite-injector`).
  3. Разобрать `tools/` (удалить/архивировать 11 устаревших скриптов; решить судьбу `test_gc_detour_abi.py` — рефакторинг под `hromatite-il2cpp::gcguard` и удаление `gc_context_detour` либо архивация; починить `abi=2` в `tools/test-hot-smoke.sh:30`, проверить актуальность путей в `symmap.py`/`refresh.sh`, удалить `hromatite-mode.launch-bak.1733464`).
  4. Обновить `AGENTS.md` (§3 и §17), прогнать полный цикл проверок (`cargo test`, `cargo clippy`, `cargo tree`, `hromatite-console`, `hromatite-overlay`) и сделать финальный `git push`.
