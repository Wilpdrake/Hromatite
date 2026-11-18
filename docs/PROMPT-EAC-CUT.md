# Промпт-передача: «вырезать EAC из VRChat» (Hromatite, 2026-11-18)

Ты продолжаешь работу над проектом **Hromatite** — Rust-payload, который
инжектится в VRChat под Proton/Wine на NixOS. Задача этой фазы: **убрать EAC из
игры** (не «подменить трафик», а именно вырезать античит) и довести это до
измеримого результата. Тебе разрешено до **3 параллельных агентов**.

Ниже — всё, что уже доказано живыми прогонами и Ghidra-анализом. Не переоткрывай
это; если собираешься опровергнуть — сначала прочитай указанный файл/лог.

---

## 0. Жёсткие правила (нарушение = сломанная сессия пользователя)

- **Игру запускает и закрывает только пользователь.** Никаких kill/start/restart,
  никаких `pkill`. Один инстанс игры; живые прогоны — строго последовательно.
- Режим инжекта — файл `~/.hromatite-mode` (KEY=VALUE). Перед правкой — бэкап с
  sha256, после прогона вернуть байт-в-байт.
- **Коммит на каждое логическое изменение** + запись в `AGENTS.md` §17 (правило
  пользователя). Ветки не мержить в master без явного «ок».
- Любое утверждение о поведении EAC/il2cpp подкреплять логом/дампом, иначе
  помечать как гипотезу.
- Не трогать файлы игры в prefix при живом EAC (это ловится как error 16).
- Параллельные агенты — только в отдельных `git worktree`, с disjoint-списками
  файлов, сборки сериализовать `flock`, у каждого свой `CARGO_TARGET_DIR`.

## 1. Где что лежит

```
репо (master)        ~/RustroverProjects/Hromatite
ветка eac-emu        ~/RustroverProjects/Hromatite-eac      (эмулятор: forge/report/evp/photon/iat)
ветка eac-replace    ~/RustroverProjects/Hromatite-replace  (модель сообщения + self-replay, СВЕЖЕЕ)
артефакты EAC        ~/EAC-RE/{findings,evidence,tools,decoded,ghidra}
Ghidra-проект        /11.18  (открыты: eac_module_memory.so, blob3_client.dll,
                              stage1_module.so, elf_load4_nosec_vaddr969000.bin)
логи payload         <prefix>/drive_c/users/steamuser/hromatite.log
логи хуков           <prefix>/…/AppData/Local/Temp/hromatite_eac_hooks.log
логи игры            <prefix>/…/AppData/LocalLow/VRChat/VRChat/output_log_*.txt
prefix               ~/.local/share/Steam/steamapps/compatdata/438100/pfx
сборка               cargo build --workspace --release --target x86_64-pc-windows-gnu
тесты                cargo test --workspace --target x86_64-unknown-linux-gnu --offline
```

## 2. Что уже доказано (не переоткрывать)

### 2.1 Где живёт EAC под Linux/Proton

- **Нет ни процесса, ни модуля ядра.** Бутстрап (`start_protected_game.exe` →
  `EasyAntiCheat_EOS.exe`) мапит модуль в `VRChat.exe` и умирает через ~107 мс
  (`anticheatlauncher.log`: `Wine module mapping` → `loaded in-game` →
  `Terminating with: 0`). В `/proc/modules` — 0 совпадений.
- В памяти игры живёт **ELF64 SO 6 651 656 Б** по адресу вида
  `0x7081edfa1000`, backing `/memfd: (deleted)`. Это **не** тот файл, что
  декодируется из `.eac` на диске (12 015 192 Б — предыдущая стадия).
  Готовый образ: `~/EAC-RE/ghidra/eac_module_memory.so` (sha256 `2e5405f9…`).
- Внутри: BLAKE3 + Zydis, строки `Easy Anti-Cheat Hash Catalogue not found`,
  `Forbidden module`, `Corrupted memory`, `Forbidden tool`, `Cannot run under
  Virtual Machine.` Перечисление — **только ELF-модули через `dl_iterate_phdr`/
  `link_map`**, PE-образы игры туда не попадают. `ptrace`/`process_vm_readv`/
  `pread` — **нет**.
- Win64-клиент (`~/EAC-RE/decoded/blob3_client.dll`, 20.6 МБ) — **без таблицы
  импортов**, всё через прямые `syscall` (`0f 05`) с замаскированными номерами;
  блэклист имён модулей: `bassmidi.dll`, `bassmididrv.dll`, `apphelp.dll`.
- Наш payload после `manual map` в список модулей не попадает (`peb unlink: true`,
  заголовки стёрты). Ни один прогон не дал `Forbidden module` — детекта нашего
  модуля не наблюдалось.

### 2.2 Что происходит, если EAC убрать/заглушить

| конфигурация | исход |
|---|---|
| стаб `start_protected_game.exe` (без бутстрапа) | `Error World`, offline mode, **0 вызовов AC API** |
| `EOS_USE_ANTICHEATCLIENTNULL=1` | вход в мир, кик **~3 с**, далее 24–27 с |
| тишина (drop исходящих кадров) | **120.5 с**, `code 104` |
| реплей чужих кадров | 200 / 202 / 211 с |
| forge из шаблона (CRC ок) | **39–75 с** |
| реплей целого чужого отчёта | **51–65 с** |
| **реплей целого СВОЕГО сообщения** (self-replay, правильный формат) | **193 с** |

Вывод: **полное удаление модуля = отказ на входе** (3 с или offline), а любой
реплей умирает за 193 с максимум.

### 2.3 Формат кадра (измерено на 2106 кадрах / 17 сессиях)

- 24-Б заголовок: `u16 ver=2 | u16 flags | u64 sid | u64 seq | u32 crc32c`
  (CRC-32C, poly 0x1EDC6F41, `init=0/xorout=0`).
- Тело 488 Б = фрагмент длинного сообщения. Заголовок сообщения (в кадре с
  `flags&1`): `+0x00 total_len` (**константа на action**: 1592/13, 2302/4),
  `+0x04 ≡ 1`, `+0x08 nonce`, `+0x0C ≡ 5`, `+0x10 action`, `+0x14 ≡ 3`,
  `+0x18 u32 counter (+1 на сообщение)`, `+0x1C..0x4C` — 48-Б per-session blob,
  `+0x4C..0x4E` — 2 Б, дальше 16-Б записи.
- Сообщения живут **35–209 кадров** (≈70 с–7 мин при 2007 мс); `EndData` (bit1)
  не встречается вообще.
- Исходящий heartbeat: **512 Б каждые ~2007 мс**; входящий ack: **24 Б**.
- Реализация в `crates/hromatite-eac` (ветка `eac-replace`): `MessageHeader`
  с этими смещениями, `TotalMode::Recorded`, `EAC_REPLAY_SELF` (целое сообщение,
  deadline 90 с, min 20 кадров). **Это можно брать как базу.**

### 2.4 Чем это уже проверялось

- `EAC_REPLAY_SELF=1` + `EAC_FORGE_KEEP_SMALL=1` → захват состоялся
  (`sid=0x4466`, 118 кадров), 112 кадров ушли, кик на 193 с с executive-сообщением
  про integrity. После кика генератор продолжал слать старый `sid` — **дефект**:
  эмиттер обязан останавливаться на смене эпохи (`forge_reset_epoch`).
- Тесты: `cargo test -p hromatite-eac` — 82/82; clippy `-D warnings`; windows
  release собирается.

## 3. Что осталось неизвестным (это и есть работа)

1. **Как модуль подтверждает установку на входе.** Без этого «вырезать» нельзя:
   null-клиент кикают за 3 с. Нужно найти проверку, которую делает игра/EOSSDK
   (маркер бутстрапа, named section/event, поле в EOS-сессии) и понять, можно ли
   её удовлетворить без модуля.
2. **Откуда берутся 16-байтные записи** и почему отчёт «протухает» за ~193 с
   (гипотеза: хеши модулей с nonce/временем — тогда реплей невозможен в принципе).
3. **Сканирует ли win64-клиент анонимные регионы** (наш RX-регион) — в коде нет
   классического `gs:[0x60]`, но есть прямые syscall'ы.
4. **Можно ли «вырезать» enforcement, оставив генератор** (тогда сессия жива, а
   детекта нет) — это компромисс, если п.1 не решается.

## 4. План на 3 агента (каждый — свой worktree и свои файлы)

Сначала зафиксируй базу (она уже создана, см. §7) и работай от неё.
Каждому агенту — свой worktree от `eac-cut`.

**Агент 1 — «вход» (admission).**
Найти, что именно проверяет игра/EOSSDK при `BeginSession`, когда модуля нет:
стаб-бутстрап → offline; null-клиент → кик 3 с. Инструменты: Ghidra (EOSSDK
`/11.17/EOSSDK-Win64-Shipping.dll` уже в проекте; искать инициализацию AC,
проверку `EOS_AntiCheatClient_BeginSession`, любые named objects/маркеры), плюс
`~/EAC-RE/findings/{eac_off.md,research.md,module.md}`. Deliverable: отчёт
«admission-механизм + можно ли его удовлетворить», с адресами и логами.
Файлы: только `docs/` и `~/EAC-RE/findings/` (без кода payload'а).

**Агент 2 — «тело отчёта» (records).**
Определить, что такое 16-байтные записи и от чего зависит их валидность:
сравнить записи внутри сессии и между сессиями (артефакты в `~/EAC-RE/evidence`),
проверить гипотезу «BLAKE3 с nonce/временем» по коду linux-модуля
(`FUN_001566e6`, `FUN_0015425c`, `FUN_00154d24`, `FUN_00154406` в
`/11.18/eac_module_memory.so`). Deliverable: формула/или доказательство, что
записи невоспроизводимы без модуля. Файлы: `~/EAC-RE/findings/`, скрипты в
`~/EAC-RE/tools/`.

**Агент 3 — «вырезание» (enforcement).**
Если А1/А2 подтвердят, что без модуля нельзя — реализовать режим «EAC вырезан
как детектор»: оставить модуль генерировать отчёты, но забрать у него транспорт
и/или результат. База — `crates/hromatite-eac` в `eac-cut`; учесть дефект
«эмиттер не останавливается на смене эпохи». Deliverable: код + host-тесты +
windows release + инструкция живого прогона. Файлы: `crates/hromatite-eac/src/*`,
`crates/hromatite-dll/src/lib.rs`, `docs/`.

## 5. Критерии приёмки (что считается результатом)

- **A (полное вырезание):** игра стартует без модуля EAC, входит в мир и живёт
  **≥300 с** без `code 104`/integrity-сообщений. Если не достижимо — зафиксировать
  точный отказ (текст, время) и почему.
- **B (вырезание enforcement):** сессия жива ≥15 мин, `hromatite_eac_hooks.log`
  показывает живой трафик, при этом канал принадлежит нам (или модуль не может
  нам помешать), и в логах нет `Forbidden module`/`Corrupted memory`.
- В любом случае: `cargo test --workspace --target x86_64-unknown-linux-gnu
  --offline`, `clippy -D warnings`, `cargo build --workspace --release --target
  x86_64-pc-windows-gnu`, `git diff --check`, запись в §17, коммиты.

## 6. Чего НЕ делать

- Не повторять «реплей кадров» как основной путь — измерено: 193 с и кик.
- Не пытаться перехватывать ntdll в win64-клиенте: у него нет импортов и прямые
  syscall'ы; только инлайн-патч его собственного кода (и лишь зная функцию).
- Не трогать `~/.hromatite-mode` без бэкапа и не оставлять его на тестовой DLL.
- Не выдавать гипотезу за факт: в §17 и в отчётах писать «проверено/не проверено».

## 7. Первый шаг, который надо сделать сразу

1. База уже создана: ветка `eac-cut`, worktree `~/RustroverProjects/Hromatite-cut`
   (от `eac-replace`, включает исправленную модель сообщения и self-replay).
   Агентам — свои worktree от неё: `git worktree add <путь> -b <ветка> eac-cut`.
2. Прочитать `AGENTS.md` §17 (последние записи от 2026-11-18) и
   `docs/EAC-REPLACE-PLAN.md` §8.
3. Раздать трём агентам задачи из §4 с их собственными worktree.
4. Собрать базовые цифры ещё раз (`cargo test`, release) и только потом менять код.
