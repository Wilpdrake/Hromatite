# EAC emulator — шаг 3/3: эмулятор, который держит сессию
Дата: 2026-11-17. Ветка `eac-tests` (от `eac-emu` @ `d390043`). Продолжение
`docs/EAC-EMU-STEP2-PLAN.md`; модель потока — `~/EAC-RE/findings/report_stream.md`.

## 0. Цель
Сессия VRChat живёт **≥300 с** без Photon `code 104` (`could not maintain a
verified connection`) на потоке отчёта, который генерируем мы. Сейчас кик на
57–75 с (`findings/forge_test.md`): неверный `total_len`, неподвижный
`counter`, повторяющиеся 16-Б записи — три нарушения формата одновременно.

## 1. Что уже есть (проверено офлайн)
- `core::crc` — CRC-32C/EAC (init=0, xorout=0), `seal_checksum`, вектор `123456789 → 0x58e3fa20`; заголовок: sid@4, seq@12, crc@20.
- `core::photon` — декодер `Dictionary<byte,object>`, кольцо событий, хук `SendOperation`, `tick()` (`EAC_EVENT_CAP`/`_SEND*`), `retarget_frame`, `parse_payload_records`.
- `hromatite-eac` — `forge`, `report`, `capture`, `evp`, `m3probe`, inline-детуры 12 экспортов `EOS_AntiCheatClient_*`.
- Host-инварианты (`crates/hromatite-core/tests/`): 14 тестов на реальных артефактах — CRC 338/338 кадров, `retarget_frame`, парсер с обрезанным хвостом, сборка сообщений, 351 уникальная 16-Б запись, `Sequence`; без артефактов печатают `skip`.

## 2. Что осталось
**(a) Резолв таблицы классов после обновления игры.** `m3::CLASS_TABLE_RVA = 0xe66c870` — замер одного билда, `m3probe` лишь логирует кандидатов. Нужно: при промахе пересканировать ±8 МиБ, закешировать слот, переприменить managed-хуки; критерий — `dm.<class>` и `PhotonPeer` резолвятся после патча.

**(b) Managed-хук `PhotonPeer.SendOperation` + событие 66.** Вызов из managed-тика (`LateUpdate`), `RaiseEvent(253, {244:66, 245:byte[]})`; 66 должен уходить с нашим `sid/seq`, входящий `OnEvent` — декодироваться в `{244,245}`. Первые 1–2 отправки вручную, затем каденция.

**(c) Генератор потока отчёта.** Вместо «один фрагмент навсегда» — заголовок сообщения (`total_len`, `counter` +1 на сообщение) и уникальные 16-Б записи. Минимум: реплей полного записанного потока с пересчётом `counter`; инварианты — `tests/report_stream.rs` (`(total_len−78)%16`, одна `StartData` на сообщение, нет дублей записей).

**(d) IAT-хук EOSSDK.** Inline-детуры по RVA дрейфуют после патча игры; IAT-хук на `EOS_AntiCheatClient_*` переживает обновление (имена стабильны). Критерий — те же счётчики `installed/hits`, что у inline.

**(e) EVP-детуры/ключ.** `EAC_EVP=1` ставит детуры на BoringSSL-путь, но ключ сессии (AES-256-GCM, `self+0x1C` = «ключ пришёл») только считается. Нужно вытащить ключ/IV и проверить, что `ProtectMessage` расшифровывается в наш поток; трамплин обязан уметь `call rel32`.

**(f) Живая приёмка.** Сессия ≥300 с без `code 104`, лог ≤32 МБ, `evidence/<run>/{capture,forge_frames}.bin`, `~/.hromatite-mode` восстановлен (sha256 `81fad7f2…81f3`), `pkill -f 'VRChat[.]exe'`.

## 3. Env-переменные
```
EAC_M3_PROBE=1               лог кандидатов таблицы классов
EAC_EVENTHOOK=1 / _IN=1      хук исходящего SendOperation / входящего OnEvent
EAC_EVENT_CAP=<path>         дамп payload'ов 66 (capture-формат)
EAC_EVENT_SEND=<path>        свой поток (u32 len|u32 dt|bytes)
EAC_EVENT_SEND_{MS,START,LOOP,RETARGET,SID,SEQ}
EAC_REPLAY_REPORT=<path>     реплей записанного отчёта
EAC_FORGE=1                  синтез кадра из шаблона (диагностика)
EAC_EVP=1                    EVP-детуры (ключ сессии)
HROMATITE_EAC_CAPTURE=<path> evidence-захват EOS-кадров
HROMATITE_EAC_EVIDENCE=<dir> корень артефактов тестов (default ~/EAC-RE/evidence)
```

## 4. Риски
- Managed-вызов из managed-тика: скрытый `MethodInfo*`, GC-барьеры не наши → падение; сначала 1–2 отправки, откат по таймауту.
- `byte[]` фиксированной длины: короткий payload оставляет хвост живого кадра.
- Устаревший кеш раскладки/таблицы (GC, смена сессии) → сброс и один повторный probe, иначе хуки молча не встают.
- IAT-хук ломает импорт-таблицу: отдельный прогон, откат по несовпадению слота; EVP-трамплин без relocate `call rel32` → краш через ~40 с (наблюдалось).
- После патча игры дрейфуют RVA/сигнатуры: сверить прологи до включения эмуляции.

## 5. Верификация
```
cargo test --workspace --target x86_64-unknown-linux-gnu --offline
cargo clippy -p hromatite-core --all-targets -- -D warnings
cargo build --workspace --release --target x86_64-pc-windows-gnu --offline
python3 ~/EAC-RE/tools/frame_analyze.py checksum ~/EAC-RE/evidence/<run>/capture.bin
python3 ~/EAC-RE/tools/frame_analyze.py messages ~/EAC-RE/evidence/<run>/capture.bin
```
Живой прогон — по шаблону §9 `AGENTS.md`; host-тесты без артефактов печатают `skip` и не красят CI.

## 6. Порядок мержа
1. `eac-tests`: зелёные host-тесты + clippy (`-D warnings`), без правок `src/`.
2. `windows-gnu` release в `~/EAC-RE/build-eac` (билд игры не трогать).
3. Живой прогон на `eac-tests` по критерию §2(f); провал — журнал в `AGENTS.md`, возврат в (a)/(c), не мержим.
4. `git merge --no-ff eac-tests` в `eac-emu`; docs+journal — отдельным коммитом.
5. `master` — после двух подряд прогонов ≥300 с.
