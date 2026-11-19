Выполнить полную ревизию кода и поэтапный архитектурный рефакторинг репозитория Hromatite (Этапы 1–5): выделить общие слои (`hromatite-win`, `hromatite-hook`, `hromatite-il2cpp`, `hromatite-render`, `hromatite-eac`), разделить инъектор и резидентный runtime, декомпозировать монолитные модули, унифицировать `hot`-роутер, очистить подтверждённый мёртвый код и `tools/`, закоммитить каждый этап и обновить `AGENTS.md`.

Working directory: /home/wilpdrake/RustroverProjects/Hromatite
Integrity mode: development

## Requirements

### R1. Этап 1 — Ревизия и базовая линия
Зафиксировать базовое состояние через `cargo test --workspace`, `cargo clippy --workspace --all-targets` и проверки `hromatite-console` и `hromatite-overlay`. Составить карту мёртвого кода, дублей подсистем и скриптов в `tools/` для последующих этапов.

### R2. Этап 2 — Выделение фундамента (`hromatite-win` и `hromatite-hook`)
Выделить единый низкоуровневый крейт `hromatite-win`: Win32 FFI (`ffi`, `sys`), безопасное чтение/запись памяти (`VirtualQuery`-проверки, `memscan`), VEH-guard (`context`), PE-парсер и процессы/модули/PEB (устранив дубли FFI и работы с памятью в `core/raw`, `hromatite-eac` и `hromatite-features/src/memory.rs`). Объединить два x64 inline-hook движка (`crates/hromatite-core/src/raw/hook.rs` и `crates/hromatite-eac/src/hook.rs`) и механизмы перехвата (`midhook`, `vmt`, `iat`, `hookset`) в единый крейт `hromatite-hook` поверх `hromatite-win` без привязки к `LateUpdate` или `EOSSDK`, отделив `LateUpdate`-тик.

### R3. Этап 3 — Разделение точек входа и вынос рендера (`hromatite-render`)
Отделить код инъектора (`manualmap`, `remote`) от резидента: `hromatite-injector` (`Hromatite.exe`) должен использовать только `hromatite-win` + `manualmap`/`remote` + CLI, а `hromatite-runtime` + `hromatite-dll` — содержать резидент (`DllMain`, `console`, `hot`-роутер, связку `il2cpp`/`render`/`eac`). Объединить `hromatite-nuklear`, `kiero`, `d3d11`, `d3d11_shaders` и `ui` в `hromatite-render` с изоляцией COM/DXGI/D3D11 и Nuklear от остального рантайма. Очистить `hromatite-features` от дублирования `memory.rs` и утилит, оставив чистые фичи через `hromatite-abi` и `hromatite-commands`.

### R4. Этап 4 — Декомпозиция крупных модулей и доменов (`hromatite-il2cpp`, `hromatite-eac`, host-инструменты)
Перенести Unity/IL2CPP-модули (`symbols`, `buildinfo`, `m3`, `gcthreads`, `playerloop`, `lateupdate`, `photon`) в `hromatite-il2cpp` поверх `hromatite-win` + `hromatite-hook` + `hromatite-abi`, разбив `photon.rs` на `decode`, `hook` и `emulate`. В `hromatite-eac` оставить только протокол, сессию и хуки `EOSSDK` поверх `hromatite-win` и `hromatite-hook`, разбив `eos.rs` на `hooks`, `pipeline`, `diag` (и проведя декомпозицию крупных файлов вроде `report.rs`, `eacimage.rs`, `forge.rs` по необходимости). Разбить `hromatite-overlay/src/main.rs` и `hromatite-console/src/model.rs` на логические модули.

### R5. Этап 5 — Унификация `hot`-роутера, чистка мёртвого кода, `tools/` и документация
Унифицировать три реализации `hot.rs` (`core/src/hot.rs`, `core/src/raw/hot.rs`, `console/src/hot.rs`) в единый `hot`-роутер. Удалить подтверждённый мёртвый код (включая `m3dump.rs` с `#[allow(dead_code)]` и старые диагностические ветки), разобрать `tools/` (способ разбора — удаление или архивация — на усмотрение команды) и обновить `AGENTS.md` (§3 и §17).

### R6. Линты, Git-дисциплина и язык
Соблюдать workspace-линты (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`). Коммитить каждый завершённый этап и значимое изменение в текущую ветку и выполнить `git push` после завершения (в окружении без SSH-агента использовать `git -c credential.helper='!gh auth git-credential' push`). Вопросы совместимости внешних интерфейсов/протоколов и дополнительных проверок сверх базовых остаются на усмотрение команды. Общение с пользователем — на русском языке.

## Acceptance Criteria

### Сборка и проверки
- [ ] `cargo test --workspace` и `cargo clippy --workspace --all-targets` проходят без ошибок и нарушений workspace-линтов (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`).
- [ ] Сборка и проверки `hromatite-console` и `hromatite-overlay` проходят без ошибок (и с соблюдением применимых линтовых ограничений).
- [ ] Workspace собирается после рефакторинга (включая таргеты инъектора и DLL).

### Архитектура и декомпозиция
- [ ] Свалка `hromatite-core/src/raw/` расформирована по целевым слоям (`hromatite-win`, `hromatite-hook`, `hromatite-il2cpp`, `hromatite-render`, `hromatite-runtime`).
- [ ] В графе зависимостей `Cargo.toml` / `Cargo.lock` разделены точки входа: `hromatite-injector` (`Hromatite.exe`) не зависит от `hromatite-render`/`hromatite-il2cpp`/`hromatite-eac`, а `hromatite-dll` не зависит от кода `manualmap`/`remote`.
- [ ] Существует единый движок перехвата `hromatite-hook` (без привязки к `LateUpdate` и `EOSSDK`) и единый `hot`-роутер; дублирование FFI/работы с памятью в `hromatite-eac` и `hromatite-features` устранено.
- [ ] Монолиты `eos.rs`, `photon.rs`, `hromatite-overlay/src/main.rs` и `hromatite-console/src/model.rs` разбиты на логические модули; подтверждённый мёртвый код (в т.ч. `m3dump.rs`) удалён; `tools/` разобран.

### Git и документация
- [ ] Каждый завершённый этап (1–5) зафиксирован отдельным git-коммитом в текущей ветке, а итоговое состояние отправлено в удалённый репозиторий через `git push`.
- [ ] `AGENTS.md` (§3 «Структура репозитория» и §17 «Журнал работ») обновлён по фактическим результатам и коммитам.
