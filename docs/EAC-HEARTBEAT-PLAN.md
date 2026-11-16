# План: EAC heartbeat внутри Hromatite (payload-фича)

Ветка `eac-heartbeat` (worktree `~/RustroverProjects/Hromatite-eac`), master не трогаем.
Источник истины по раскладкам — Python-референс `/home/wilpdrake/EAC-RE/tool/eacheartbeat`
(selftest 98/98). Порт, не изобретение. Всё, что не подтверждено живым capture, помечено
`HYPOTHESIS`.

## Что меняется

| Файл | Что |
|---|---|
| `crates/hromatite-eac/` (новый) | чистая std-библиотека: кодек, сессии, JSONL, tailer |
| `crates/hromatite-features/Cargo.toml` | `+ hromatite-eac = { path = "../hromatite-eac" }` |
| `crates/hromatite-features/src/eac.rs` (новый) | команды `eac.*`, конфиг, тик |
| `crates/hromatite-features/src/feature.rs` | поле `eac`, маршрутизация `eac.*`, `help` |
| `crates/hromatite-features/src/lib.rs` | `tick()` → `Feature::tick_eac()` |
| `Cargo.toml` | `+ "crates/hromatite-eac"` в members |
| `AGENTS.md` | запись в §17 (дополнить, не переписать) |

## Крейт

`hromatite-eac`: только `std`, ноль внешних крейтов (offline registry не нужен).

- `codec` — 24-байтный заголовок EAC `{u16 Version=2, u16 Flags, u64 SessionId,
  u64 Sequence, u32 Checksum}` (ручная LE-сериализация, без `repr(C)`-паддинга) +
  payload 120 Б. `ChecksumKind::{Crc32c(default), Crc32, Fnv1a32}` — алгоритм
  **HYPOTHESIS**, поле битарейта не несёт; контроль над регионом `hdr[0..20] ++ payload`
  как в `framing.EacMessage.try_parse`.
- `payload` — 120 Б: `seq u32 | ts_ms u64 | nonce u64 | flags u32 | sha256(integrity)
  | sha256(modules) | hmac-sha256(key, первые 88)`; раскладка один-в-один с
  `heartbeat.py` (`PAYLOAD_SIZE=120`, `_SEQ="<IQQI"`).
- `digest` — SHA-256/ HMAC-SHA256 / crc32c; SHA-256 скопирован из
  `hromatite-core/src/sha256.rs` с attribution (ядро Win32-специфично, тянуть его в
  payload-образ не хотим) + сверка с эталонными векторами.
- `session` — epoch-модель: один AC-сеанс на комнату, монотонный `epoch`, счётчики
  `seq`/`hb_in_epoch`, `last_hb_ms` **живёт между эпохами** → загрузка мира видна как
  `dt_ms`; `gap` при `dt > 2 * interval_ms`.
- `eventlog` — JSONL, одна запись на строку, flush после записи; жёсткая ротация на
  32 МБ, хранится 2 файла (`x.jsonl` + `x.jsonl.1`).
- `tailer` — опрос `output_log_*.txt` через `std::fs` (offset, частичные строки,
  усечение/ротация): `Entering Room:`, `Joining wrld_`, `OnLeftRoom`, `OnDisconnected`,
  `[AntiCheatClient]` → `ev:"game"` с полем `line`.

Формат записей (имена как в `eventlog.py`):
`{"ts_ms":…,"ev":"session_begin","epoch":1,"sid":"0x…"}` — начало эпохи;
`{"ts_ms":…,"ev":"heartbeat","epoch":1,"sid":"0x…","seq":1,"hb_in_epoch":1,"dt_ms":5000,"frame_len":144}`;
`{"ts_ms":…,"ev":"gap","epoch":1,"dt_ms":3120,"expected_ms":5000,"hb_in_epoch":…,"seq":…}`;
`{"ts_ms":…,"ev":"session_end","epoch":1,"heartbeats":12,…}`; `ev:"game"` + `line`.
`dt_ms` у первого heartbeat эпохи отсутствует (как в Python: `None` не пишется).

## Поверхность команд (через `Feature::execute`)

```
eac.status            — epoch, sid, seq, hb_in_epoch, dt, running, пути логов
eac.begin [sid]       — начать эпоху (sid по умолчанию — счётчик/время)
eac.tick              — один heartbeat вручную (в обход интервала)
eac.end [reason]      — закрыть эпоху, записать session_end
eac.log <path>        — путь JSONL (открыть/сменить)
eac.tail <path>       — путь VRChat output_log для зеркалирования
```
`configure`: `eac=0|1` (вкл/выкл семейство), `eac_log=<path>`, `eac_tail=<path>`,
`eac_interval_ms=<200..60000>`, `eac_auto=0|1` (автотик из LateUpdate-тика).
`tick()` в `lib.rs` → `Feature::tick_eac()`: поллинг tailer + heartbeat по расписанию,
если `eac_auto=1`. Всё, что делает tick, ограничено по времени (один файловый write).

## Фазы

1. **Сейчас:** кодек + сессии + JSONL + tailer + команды + сборочные тесты.
2. **Не сейчас:** установка хуков в EOSSDK-экспорты (`EOS_AntiCheatClient_BeginSession`,
   `..._EndSession`, `AddNotifyMessageToServer(25)`, `...ClientIntegrityViolated(26)`) —
   эпохи/тики будут приходить из игры, а не из консоли. Требует живой игры.

## Риски

- Алгоритм checksum неизвестен → кодек помечен HYPOTHESIS; смена — одна функция.
- Запись JSONL из game-thread тика (`LateUpdate`) — риск залипания на медленном диске;
  буферизация + ротация, интервал по умолчанию 5000 мс.
- EOSSDK 1.19.2.1 у VRChat обфусцирован/стрипнут; имена экспортов — фаза 2.
- `std::fs` в payload: пути Windows (`Z:\…`, `%USERPROFILE%`) задаются командами.
- Межэпоховый `last_hb_ms` — намеренно (иначе world load не виден), но если эпохи
  стартуют вручную, гигантский `dt_ms` — норма.

## Проверка (реальный вывод — в §17)

```bash
cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu   # векторы + 2 эпохи + gap
cargo test -p hromatite-features --target x86_64-unknown-linux-gnu
cargo test --workspace --target x86_64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-gnu            # payload собирается
python3 ~/EAC-RE/tool/eacheartbeat/selftest.py                  # референс остаётся 98/98
```
