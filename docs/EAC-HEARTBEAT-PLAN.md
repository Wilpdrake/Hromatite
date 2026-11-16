# План: EAC heartbeat внутри Hromatite (payload-фича)

Ветка `eac-heartbeat` (worktree `~/RustroverProjects/Hromatite-eac`), master не трогаем.
Источник истины — Python-референс `/home/wilpdrake/EAC-RE/tool/eacheartbeat` (selftest
98/98). Порт, не изобретение. Непроверенное живым capture помечено `HYPOTHESIS`.

## Что меняется

| Файл | Что |
|---|---|
| `crates/hromatite-eac/` (новый) | чистая std-библиотека: кодек, сессии, JSONL, tailer |
| `crates/hromatite-features/src/eac.rs` (новый) | команды `eac.*`, конфиг, тик |
| `.../features/{Cargo.toml,feature.rs,lib.rs}` | маршрутизация, `tick()`, help |
| `crates/hromatite-eac/tests/epochs.rs` | тест: 2 эпохи + gap, чтение JSONL с диска |
| `Cargo.toml`, `AGENTS.md` | member + запись в §17 (дополнить, не переписать) |

## Крейт

`hromatite-eac`: только `std`, ноль внешних крейтов (offline registry не нужен).

- `codec` — 24-байтный заголовок EAC `{u16 Version=2, u16 Flags, u64 SessionId,
  u64 Sequence, u32 Checksum}` (ручная LE-сериализация, без `repr(C)`-паддинга) +
  payload 120 Б. `ChecksumKind::{Crc32c(default), Crc32, Fnv1a32}` — алгоритм
  **HYPOTHESIS**; контроль по региону `hdr[0..20] ++ payload`, как в
  `framing.EacMessage.try_parse` (магии в формате нет).
- `payload` — 120 Б: `seq u32 | ts_ms u64 | nonce u64 | flags u32 | sha256(integrity)
  | sha256(modules) | hmac-sha256(key, первые 88)`; раскладка один-в-один с
  `heartbeat.py` (`PAYLOAD_SIZE=120`, `_SEQ="<IQQI"`).
- `digest` — SHA-256 копирован из `hromatite-core/src/sha256.rs` с attribution
  (ядро Win32-специфично, тянуть его в payload-образ не хотим) + HMAC-SHA256,
  CRC-32/CRC-32C; сверено с эталонными векторами.
- `session` — epoch-модель: один AC-сеанс на комнату, монотонный `epoch`,
  `seq`/`hb_in_epoch` **на эпоху**, `last_hb_ms` **живёт между эпохами** → загрузка
  мира видна как `dt_ms`; `gap` при `dt > 2 * interval_ms`.
- `eventlog` — JSONL, одна запись на строку, flush после записи; жёсткая ротация на
  32 МБ, 2 файла (`x.jsonl` + `x.jsonl.1`).
- `tailer` — опрос `output_log_*.txt` через `std::fs` (offset, частичные строки,
  усечение): `Entering Room:`, `Joining wrld_`, `OnLeftRoom`, `OnDisconnected`,
  `[AntiCheatClient]` → `ev:"game"` с полем `line`.

Записи (имена как в `eventlog.py`):
`{"ts_ms":…,"ev":"session_begin","epoch":1,"sid":"0x…"}`;
`{"ts_ms":…,"ev":"heartbeat","epoch":1,"sid":"0x…","seq":1,"hb_in_epoch":1,"dt_ms":5000,"frame_len":144}`;
`{"ts_ms":…,"ev":"gap","epoch":1,"dt_ms":3120,"expected_ms":5000,"hb_in_epoch":…,"seq":…}`;
`{"ts_ms":…,"ev":"session_end","epoch":1,"heartbeats":12,"reason":…}`; `ev:"game"` + `line`.
`dt_ms` у первого heartbeat эпохи отсутствует (как в Python: `None` не пишется).

## Команды (через `Feature::execute`)

```
eac.status            — epoch, sid, seq, hb_in_epoch, dt, running, пути
eac.begin [sid]       — начать эпоху (sid по умолчанию выводится из времени)
eac.tick              — один heartbeat вручную (в обход интервала)
eac.end [reason]      — закрыть эпоху, записать session_end
eac.log <path>        — путь JSONL (открыть/сменить)
eac.tail <path>       — путь VRChat output_log для зеркалирования
```
`configure`: `eac=0|1`, `eac_auto`, `eac_autosession`, `eac_log`, `eac_tail`,
`eac_interval_ms` (200..60000, клампится). `tick()` из LateUpdate/PlayerLoop зовёт
`Feature::tick()` → поллинг tailer + heartbeat по расписанию при `eac_auto=1`;
только файловый I/O, без доступа к памяти игры.

## Фазы

1. **Сейчас:** кодек + сессии + JSONL + tailer + команды + тесты (собрано/прогнано).
2. **Не сейчас:** хуки в EOSSDK (`EOS_AntiCheatClient_BeginSession/EndSession`,
   `AddNotifyMessageToServer(25)`, `AddNotifyClientIntegrityViolated(26)`) — эпохи и
   тики будут приходить из игры. Требует живой игры и резолва обфусцированных экспортов.

## Риски

- Алгоритм checksum неизвестен → кодек помечен HYPOTHESIS; смена — одна функция.
- Запись JSONL из game-thread тика — риск залипания на медленном диске; flush на
  каждую запись, интервал по умолчанию 5000 мс, ротация 32 МБ.
- `std::fs` в payload: пути Windows (`Z:\…`, `%USERPROFILE%`) задаются командами.
- Межэпоховый `last_hb_ms` намеренный: при ручных `eac.begin` гигантский `dt_ms` — норма.
- Плейсхолдеры (mac-ключ, integrity/modules) — до фазы 2, помечены в doc-комментариях.

## Проверка (реальный вывод — §17, запись 2026-11-16 «EAC heartbeat»)

```bash
cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu     # 18 + 1 (2 эпохи, gap)
cargo test -p hromatite-features --target x86_64-unknown-linux-gnu # 14
cargo test --workspace --target x86_64-unknown-linux-gnu           # 119 core + 18 + 14 …
cargo build --workspace --release --target x86_64-pc-windows-gnu  # payload собирается
cargo clippy -p hromatite-eac -p hromatite-features -- -D warnings
python3 -c "from tool.eacheartbeat.selftest import run_all; raise SystemExit(run_all())"
```
Результат: все команды зелёные (offline, `--offline`), референс — `PASS: 98 checks`.
