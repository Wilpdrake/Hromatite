# EAC-FORGE — генерация собственных heartbeat-кадров

Цель: `EAC_FORGE=1` — выучить живой 512-байтный кадр сессии и вместо реплея
чужих кадров **собирать свои**: тот же `SessionId`, `Flags` и тело последнего
живого кадра, инкрементный `Sequence` и пересчитанный CRC-32C.

Основание (оффлайн, 451/451): `findings/frame_structure.md` §0/§3/§8 —
кадр нешифрован, `[0:2] Version=2`, `[2:4] Flags`, `[4:12] SessionId`,
`[12:20] Sequence`, `[20:24] CRC-32C(frame с нулями в 20..24, init=0,
xorout=0)`, `[24:]` фрагмент сообщения (488 Б для 512-байтного кадра).

## Файлы

| файл | что |
|---|---|
| `crates/hromatite-eac/src/digest.rs` | `crc32c_eac()` — тот же полином, но `init=0, xorout=0` (существующий `crc32c()` — стандартный init/xorout FFFFFFFF) |
| `crates/hromatite-eac/src/forge.rs` | новый модуль: `Template` (sid/flags/body/seq), `Template::from_frame`, `build_frame`, `Config` (after_ms, min_frames, interval_ms), парсинг env, счётчики |
| `crates/hromatite-eac/src/eos.rs` | ловля шаблона в `message_to_server_wrapper`, дроп живых кадров после старта, поток-генератор 2007 мс, логи `[forge]`, `Counters.forged`, `mode=forge` |
| `crates/hromatite-eac/src/lib.rs` | `pub mod forge;` |
| `AGENTS.md` | §17 запись (ветка `eac-forge`) |

Новых крейтов нет: только `std` + `std::thread`, как у `keyhunt`.

## Поведение

1. `EAC_FORGE=1` + `EAC_FORGE_AFTER_MS` (мс от `BeginSession`) +
   `EAC_FORGE_MIN_FRAMES` (сколько живых 512-Б кадров увидеть до старта).
2. До старта: каждый 512-байтный кадр (Version=2) переписывает шаблон
   (`SessionId`, `Flags`, тело 488 Б, последний `Sequence`).
3. `BeginSession` сбрасывает шаблон и счётчики: шаблон только текущей сессии.
4. Старт forge: живые кадры больше не форвардятся (дроп), поток раз в
   2007 мс собирает кадр `template + seq++` с новым CRC и отдаёт его в
   колбэк игры (`bound_callback`), т.е. сервер видит наш кадр в текущей сессии.
5. Лог на каждый кадр: `[forge] sid=0x… seq=N crc=0x… len=512 flags=0x… n=K`,
   плюс запись кадра в `forge_frames.bin` (формат capture) — оффлайн-проверка CRC.
6. `EAC_FORGE_BODY=stale|bump` (по умолчанию `stale`): `bump` дополнительно
   инкрементит `u32` по смещению `msg+0x18` (счётчик сообщения) в теле, если
   оно начинается с заголовка сообщения (`flags & 1`).

## API

```rust
pub const FRAME_LEN: usize = 512;      // 24 header + 488 body
pub const INTERVAL_MS: u64 = 2_007;    // медиана живого потока
pub struct Template { pub session_id: u64, pub flags: u16, pub body: Vec<u8>, pub last_seq: u64 }
impl Template { pub fn from_frame(frame: &[u8]) -> Option<Self>; }
pub fn build_frame(template: &Template, seq: u64, bump: bool) -> Vec<u8>;
pub struct Config { pub after_ms: u64, pub min_frames: u64, pub interval_ms: u64, pub bump: bool }
impl Config { pub fn from_env() -> Self; }   // 0 after_ms = выключено
```

## Команды

```sh
export PATH=$HOME/.cargo/bin:/run/current-system/sw/bin:$PATH
cargo test  -p hromatite-eac --target x86_64-unknown-linux-gnu --offline
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu --offline -- -D warnings
cargo build --workspace --release --target x86_64-pc-windows-gnu --offline
```

## Риски

- Вызов колбэка игры из своего потока (не из SDK-потока) — реплей вызывал только
  из обёртки; если игра не потокобезопасна, кадры не дойдут → в логе будет
  `[forge] emit`, но сервер промолчит. Признак — отсутствие ack'ов и кик по таймауту.
- Тело живого кадра — середина длинного отчёта; повтор одного фрагмента сервер
  может счесть дублем (так же выглядит ретрансмиссия при отсутствии ack'а).
- CRC-32C с `init=0/xorout=0` — проверяется оффлайн на живом кадре до прогона и
  на своих кадрах после (frame_analyze checksum).

## Верификация

1. Юнит-тесты: `crc32c_eac("123456789")`, round-trip `build_frame` → `Frame::parse`
   с `ChecksumKind::Crc32c` не подходит (другие init/xorout) — проверяем
   `crc32c_eac(build_frame(..))`, что поле совпало, и `forge`-проверку в Python
   на реальном кадре из `control_0009/capture.bin`.
2. Живой прогон 3–4 мин: EAC ON, вход в мир, ≥20 живых 512-Б кадров, forge,
   замер от старта forge до кика. Живая сессия — бесконечно, дроп — 120.5 с,
   реплей — 200 с; **≥300 с = forge держит**.
3. Отчёт `~/EAC-RE/findings/forge_test.md`, сырые логи `evidence/forge_<HHMM>/`.
