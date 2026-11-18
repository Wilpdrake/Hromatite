# UI на nuklear + живой хук Present (план)

Дата: 2026-11-18. Статус: в работе (ветки `feat/kiero`, `feat/safetyhook`, `feat/nuklear`).

## Задача
1. Живой хук `IDXGISwapChain::Present` (паттерн `Rebzzel/kiero`) — точка входа нового UI.
2. Переписать in-game UI на `nuklear` (единственный «проектный» UI в игре).
3. Прежний UI (векторная панель в `render.rs`) удалён; бэкап — `var/ui-draft-backup/`.

## Факты, на которых стоит план (проверено)
- Игра на **D3D11**: в процессе `d3d11.dll` (`0x6ffff9fa0000`) + `dxgi.dll` (`0x6ffffc840000`);
  d3d12 не загружен. `d3dcompiler_47.dll` есть в префиксе, но **не маппингован**.
- `/proc/<pid>/mem` читается (`ptrace_scope=0`). В куче игры найдены **21 указатель** на код
  `d3d11.dll` — это vtable-указатели; значит swapchain/device находится сканом по сигнатуре.
- `Rebzzel/kiero` (прочитан исходник): `init(D3D11)` создаёт **временное** окно +
  `D3D11CreateDeviceAndSwapChain`, копирует vtable в `g_methodsTable`
  (swapchain 18 + device 43 + context 144 записей) и освобождает объекты. `bind()` без
  MinHook — **заглушка** (`TODO: need own detour`). Вывод: «реализовать Kiero» = свой
  детур поверх этого механизма обнаружения.
- `nuklear` v4.13.4 (MIT/public domain) компилируется на хосте; ABI снят:
  `nk_context` 18744 Б, `nk_draw_command` 32 Б, `nk_user_font` 40 Б, `nk_handle` 8 Б.
  Бэкенд `demo/d3d11/nuklear_d3d11.h` (750 строк) — эталон рендера.

## Архитектура (три независимых слоя)
1. **`hromatite-core`: `raw/vmt.rs` + `raw/kiero.rs`** (ветка `feat/kiero`).
   - `Vmt::new(obj)` → читает первый qword объекта; `hook(i, detour)` — подмена слота.
   - **Способ подмены — копия vtable на объект**, а НЕ правка `.rdata` dxgi.dll:
     правка file-backed страницы модуля — это ровно то, что ищет EAC. Копия ложится в
     кучу, меняется только первый qword объекта.
   - `init()` повторяет Kiero: временный `D3D11CreateDeviceAndSwapChain` → эталон vtable
     (индексы Present=8, ResizeBuffers=13) → скан кучи за объектом с такой vtable.
   - `render(f)` — вызвать `f` из детура до/после оригинального Present.
2. **`hromatite-core`: `raw/midhook.rs` + `raw/hookset.rs`** (ветка `feat/safetyhook`).
   - Mid-function хук с переносом инструкций из середины + менеджер хуков (учёт/откат).
   - Переиспользует движок `raw/hook.rs` (декодер, `alloc_near`, `write_window`).
3. **`hromatite-nuklear`** (ветка `feat/nuklear`): vendored `nuklear.h` + C-шим + безопасная
   Rust-обёртка: `Nk::frame(&Input) -> Frame { commands, vertices, indices, texture }`.
   Панели: ESP/боксы/прицел/дистанция/список игроков/readout камеры. Вход — структура,
   которую резидент заполняет из Unity `Input`/`Windows`.

## Обвязка (после мержа; делает родитель)
- `Present`-детур: взять device/context из swapchain (`QueryInterface`/`GetDevice`),
  вызвать `Nk::frame`, отрисовать `Frame` бэкендом nuklear-d3d11 (шейдер, атлас шрифта,
  dynamic vertex/index буферы, сохранение/восстановление D3D11-состояния).
- Вход: Unity `Input.GetMousePosition` / `GetMouseButton*` / `GetKey*` из тика, плюс
  Win32-фолбэк (`GetCursorPos`, `GetAsyncKeyState`) — на выбор, по факту.

## Файлы
- `crates/hromatite-core/src/raw/vmt.rs`, `raw/kiero.rs`, `raw/midhook.rs`, `raw/hookset.rs`,
  правки `raw/mod.rs`, `src/lib.rs`.
- `crates/hromatite-nuklear/{Cargo.toml,build.rs,src/lib.rs,nuklear_shim.c,third_party/nuklear/*}`.
- Обвязка: `crates/hromatite-dll/src/lib.rs` (установка хука), возможно новый
  `crates/hromatite-features/src/ui.rs` (команда `draw ui`).
- Прежний UI в `render.rs` остаётся только как ESP-геометрия (DrawMesh) — панель уходит.

## Команды
- `draw [on|off|status|probe]` — без изменений; `draw ui` — открыть/закрыть окно nuklear.

## Риски (EAC)
- Любой патч в file-backed странице модуля = «Corrupted memory». Поэтому только копия vtable.
- Поток Present — игровой; в детуре нельзя блокировать/логировать в файл.
- Шейдеры компилируем заранее (байты в образе), не через `d3dcompiler` в рантайме.
- Вход с клавиатуры не перехватывает системные хоткеи — только чтение состояния.

## Верификация
- Офлайн: host-тесты в `hromatite-core` (vmt/midhook/hookset) и `hromatite-nuklear`
  (кадр без входа → команды непустые; клик меняет состояние; слайдер в границах).
- Сборка: `cargo test --workspace --target x86_64-unknown-linux-gnu --offline`,
  `cargo build --workspace --release --target x86_64-pc-windows-gnu --offline`, clippy `-D warnings`.
- Живое: пользователь запускает игру. Критерий: в логе `kiero: Present hooked …`,
  `nuklear: frame …`, в кадре окно; `draw ui` закрывает. Без кика/дампа.

## Границы честности
Это **не** «невидимый» UI: он виден в кадре. Скрытность — в отсутствии патча в
системных модулях и в отсутствии своего потока. Живую приёмку до запуска пользователем
считать непроверенной.
