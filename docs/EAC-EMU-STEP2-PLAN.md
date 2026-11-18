# EAC emulator — шаг 2/3: декодер RaiseEvent + свой heartbeat через Photon

Дата: 2026-11-17. База: ветка `eac-emu` (`2a6dd7e`), worktree
`~/RustroverProjects/Hromatite-eac`. Продолжение `docs/EAC-EMU-PLAN.md` (шаг 3).

## 0. Что закрываем

Шаг 1 (наблюдение `SendOperation`/`OnEvent`) сделан и проверен офлайн; в живом
прогоне осталась заглушка `decode_event_params` → в логе только `params_ptr`.
Без декодера нельзя ни опознать событие 66, ни подставить в него свой отчёт.
Шаг 2+ = «дописать эмулятор»:

1. декодер `Dictionary<byte, object>` (слоты 244/245) — реальный, а не заглушка;
2. захват payload события 66 в файл (сравнение с EOSSDK-кадром 1:1);
3. генератор: свой отчёт (записанный или свой кадр) отправляется **нами** как
   `RaiseEvent(253, {244:66, 245:byte[]})` через тот же `SendOperation`, из
   managed-тика (игровой поток). Транспорт, не зависящий от EOSSDK.

## 1. Модель раскладок (без магии)

`Dictionary<byte,object>` (il2cpp):
`obj_klass@0`, `monitor@8`, затем поля; имена полей в Unity — `entries`/`count`
(Mono) либо `_entries`/`_count` (.NET Core) — **читаем из `m3::fields`**, адреса
полей не хардкодим. Массив il2cpp: `max_length` = u32 @ `+0x18`, данные @ `+0x20`.
Запись `Entry`: кандидаты `(stride, key_off, value_off)` = `(0x18, 8, 0x10)`,
`(0x20, 8, 0x10)`, `(0x10, 8, 0x0C)`; побеждает та, при которой в одном массиве
найдены ключи `244` **и** `245` (валидация по данным, а не по докам).
Код события — бокс-байт по значению слота 244 (`value+0x10`), payload — объект
`byte[]` слота 245.

## 2. Файлы и API

- `crates/hromatite-core/src/crc.rs` (новый): `crc32c_eac` (init=0/xorout=0),
  вектор `123456789 → 0x58e3fa20`. Нужен для перецелевой привязки кадра.
- `crates/hromatite-core/src/raw/photon.rs`:
  * `trait Mem { fn read(&self, addr, len) -> Option<Vec<u8>> }` — декодер
    тестируется на fake-памяти (host-тесты), прод-реализация = `safe::read_bytes`;
  * `dictionary_offsets(fields)` — выбор `(count_off, entries_off)` по именам;
  * `decode_with(mem, dict, layout, dict_offsets)` + `decode_event_params`;
  * кольцо захвата событий (`EVENT_RING`, cap 256) + `drain_events()`;
  * `Template { this, dict, opts, payload, capacity }` — снимается с первого
    живого события 66; `emulate_send(&[u8])` пишет байты в живой `byte[]` и
    вызывает оригинал `SendOperation(253, dict, opts)`;
  * `tick()` — каденция 2000 мс из managed-тика: `EAC_EVENT_SEND=<file>`
    (`u32 len|u32 dt|bytes`, иначе файл целиком = один payload),
    `_SEND_MS`, `_SEND_START`, `_SEND_LOOP=0`, `_SEND_RETARGET=0`,
    `_SEND_SID`, `_SEND_SEQ`; `EAC_EVENT_CAP=<file>` — дамп payload'ов 66.
- `crates/hromatite-core/src/raw/hook.rs`: `work()` вызывает `photon::tick()`
  рядом с `photon::poll()`.

## 3. Env-сводка (новое)

```
EAC_EVENTHOOK=1            хук исходящего SendOperation (было)
EAC_EVENTHOOK_IN=1         хук входящего OnEvent (было)
EAC_EVENT_CAP=<path>       писать payload'ы события 66 в capture-формат
EAC_EVENT_SEND=<path>      источник payload'ов для своей отправки
EAC_EVENT_SEND_MS=2000     каденция отправки
EAC_EVENT_SEND_RETARGET=1  переписать SessionId/Sequence/CRC-32C в 512-Б кадре
EAC_EVENT_SEND_SID=0x..    чужой/свой SessionId (иначе из последнего живого)
EAC_EVENT_SEND_SEQ=n       стартовый Sequence; дальше +1 на кадр
EAC_EVENT_SEND_LOOP=0      не начинать поток заново по исчерпании
```

## 4. Риски

- Вызов managed-метода из managed-тика: тот же поток, что `LateUpdate`, но
  скрытый `MethodInfo*` на стеке и GC-барьеры не наши → игра может упасть;
  сначала 1–2 отправки, потом каденция.
- `byte[]` в словаре имеет фиксированную длину: если наш payload короче —
  хвост остаётся от живого кадра (логируем `payload=обычно 512`).
- Кэш раскладки устаревает (GC/new session) → при провале кэш сбрасывается и
  probe повторяется один раз.
- Живого прогона в этой сессии может не быть: тогда всё подтверждено host-тестами
  (fake-память) + сборкой windows-gnu, а живой результат — следующий шаг.

## 5. Верификация

```
cargo test --workspace --target x86_64-unknown-linux-gnu --offline
cargo clippy -p hromatite-core -p hromatite-eac --all-targets -- -D warnings
cargo build --workspace --release --target x86_64-pc-windows-gnu --offline
```
Живой прогон — по шаблону §9 AGENTS.md, `~/.hromatite-mode` вернуть (sha256
`81fad7f2…81f3`), лог ≤32 МБ, `pkill -f 'VRChat[.]exe'`.
