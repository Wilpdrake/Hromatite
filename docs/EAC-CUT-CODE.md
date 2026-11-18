# EAC: вырезание enforcement — код и живой прогон (2026-11-18)

База: `eac-cut` @ `533a803`. Ветка фазы: `cut-enforcement`
(worktree `~/RustroverProjects/cut-a3`).

Три части, каждая — самостоятельный кирпич:

1. **Инвариант эпохи.** Эмиттер кадров больше не может пережить сессию, из
   которой он выучен. Это починка подтверждённого дефекта (81 кадр старого
   `sid=0x4466` после `BeginSession` 07:16:07 и второй кик
   `Authentication with the VRChat Anti-Cheat system failed`).
2. **`eac.module`** — дешёвый read-only локатор in-process ELF-образа EAC
   (это memfd ELF 6.65 МБ, а не PE: `GetModuleHandleA` его не видит).
3. **`EAC_BLIND=1`** — подавление *локальных* вердиктов античита: игра не
   регистрируется на violation, `PollStatus` отвечает «нет нарушения», две
   read-only проверки требований отвечают управляемыми значениями.

Всё default-off, всё fail-closed, всё пишет строку в `hromatite_eac_hooks.log`
на каждое вмешательство. Новых потоков нет: `eac.module` и гейт работают в
существующем тике (managed `LateUpdate` / `Hooks::poll`) и в уже существующем
потоке генератора `eac-forge`.

---

## 1. Дефект «эмиттер не останавливается на смене эпохи»

### 1.1. Что наблюдалось

Живой прогон 2026-11-18: после первого кика игра открыла новую сессию
(`BeginSession` 07:16:07), генератор продолжил слать кадры **старого**
`sid=0x4466` — 81 штука — и сервер ответил
`Authentication with the VRChat Anti-Cheat system failed`.

`forge_reset_epoch()` в `eos.rs` сбрасывал `FORGE_STARTED` и шаблон, но
эмиттер продолжал работу. Причина: `hromatite-eac` компилируется **дважды** —
в резидента (`hromatite-dll`) и в образ фич (`hromatite-features`), — и у
каждой копии свои статики. Хуки ставит резидент из стартового потока, но после
того, как копия фич на первом тике переустанавливает те же экспорты, `BeginSession`
приходит в **одну** копию статиков, а поток-эмиттер читает **другую**. Событие
сброса до эмиттера просто не доходит.

### 1.2. Почему «сброс по событию» — неправильный инвариант

Сброс по `BeginSession` — это доставка события через состояние, которым владеет
не обязательно тот, кто принимает решение. Правильный инвариант должен
опираться на **свидетельство, которое эмиттер видит сам**: живой 512-байтный
кадр несёт `SessionId` той сессии, в которой реально находится модуль
античита. Пока этот `SessionId` совпадает с шаблоном и кадры продолжают
приходить — слать можно; первый кадр другой сессии (или тишина длиннее
нескольких каденций) — стоп.

### 1.3. Как сделано

Новый чистый модуль `crates/hromatite-eac/src/epoch.rs` — автомат `Gate`
(время передаётся параметром, поэтому проверяется на хосте):

```text
note_live(A) ─► arm(A) ─► verdict = Emit ──┐
                                 ▲         │ note_live(B)
                                 │         ▼
                       note_live(A)   Stop::ForeignSession (липкий)
```

* `arm(sid, now)` **отказывает**, если живого кадра этого `sid` не было в
  пределах бюджета: брать на себя сессию, в которой модуля уже нет, — ровно тот
  баг, от которого защищаемся.
* `verdict(now)` возвращает `Emit` только при совпадении `sid` и свежести;
  `Stop` бывает трёх видов: `ForeignSession { armed, live }`, `Stale { armed,
  age_ms }`, `EpochReset { .. }`.
* `Stop` **липкий**: одиночный «старый» кадр не может оживить мёртвый поток.
  Возобновление — только новое `arm` по свежему кадру новой сессии, а шаблон к
  этому моменту уже забыт.

Проводка в `eos.rs`:

| где | что |
|---|---|
| `static EPOCH: Mutex<Gate>` | состояние гейта; `try_lock` на потоке SDK, blocking на потоке генератора |
| `message_to_server_wrapper` | на каждом живом 512-Б кадре (в т.ч. **дропнутом** после захвата) → `epoch_note_live(sid, ts)` |
| `forge_learn` | любой выученный кадр подтверждает сессию (`epoch_note_live`) |
| `forge_begin` | `epoch_arm(sid, now)`; отказ = «продолжаем ждать живых кадров» (лог не чаще 1 раза в 5 с) |
| цикл `spawn_forge_loop` | `epoch_allows(now)` на **каждом** слоте, не только на захвате |
| `emit_forge_frame` / `emit_report_frame` | тот же гейт (wrapper-путь `EAC_REPLAY_FROM_WRAPPER=1` заходит сюда напрямую) |
| `forge_reset_epoch` | `epoch_reset()` + `forge_forget()` |
| `reset_state` / `install` | гейт сбрасывается, бюджет = `max(4 × interval, 8000)` мс |

Стоп логируется один раз и сопровождается забыванием шаблона:

```
[epoch] stop reason=foreign armed_sid=0x4466 live_sid=0x7a1b emitted=81 — no frame of a dead epoch is sent
```

Бюджет тишины: `EAC_FORGE_STALE_MS` (по умолчанию `max(4 × EAC_FORGE_INTERVAL_MS,
8000)`). Это же число закрывает кросс-копийный случай, где копия-эмиттер вообще
перестаёт видеть живые кадры: она замолкает через бюджет, а не через 81 кадр.

### 1.4. Чем это проверено (host)

* `epoch::tests::*` — 7 тестов модели: `Idle` без взвода, свежесть, ровно один
  бюджет, `ForeignSession`, липкость стопа, `reset` не возобновляет старый
  `sid`, отказ взвода без свежего свидетельства, минимальный бюджет.
* `eos::tests::the_emitter_stops_when_the_live_session_changes` — через
  настоящий `emit_forge_frame` + файл-capture: 2 кадра для `0x158b`, затем
  `epoch_note_live(0x7a1b)` → кадров больше нет, `FORGE_STARTED == false`,
  шаблон забыт, повторные тики и `epoch_arm(0x158b)` не помогают; после
  `forge_learn` кадра `0x7a1b` и `forge_begin` эмиссия возобновляется **с новым
  `sid`**.
* `eos::tests::a_silent_stream_stops_the_emitter_after_the_budget` — тишина
  дольше бюджета останавливает эмиттер (кросс-копийный сценарий).

---

## 2. `eac.module` — локатор in-process ELF-образа

### 2.1. Зачем

EAC под Proton — linux64 ELF (memfd, ~6.65 МБ), замапленный в `VRChat.exe`.
В списке модулей PE его нет, `GetModuleHandleA` его не назовёт, PE-обход
экспортов нечего обходить. Единственный дешёвый признак — `\x7fELF` в начале
отображения.

### 2.2. Как ищет (и почему не вешает GC)

`crates/hromatite-eac/src/eacimage.rs`:

1. `VirtualQuery` по возрастанию адреса, **только по началам регионов**;
2. на каждом читаемом (`MEM_COMMIT`, не `PAGE_GUARD`/`PAGE_NOACCESS`) регионе —
   **четыре байта** магии;
3. совпало — берётся ограниченный снимок региона (`read=<bytes>`, по умолчанию
   2 МиБ, максимум 64 МиБ) и разбирается чистыми функциями: ELF64-заголовок,
   program headers (`PT_LOAD`/`PT_DYNAMIC`/…), таблица секций, `.dynsym` →
   список **импортированных** (undefined) символов;
4. `pages` — необязательный проход по началам страниц внутри крупных
   исполняемых регионов: бюджет `MAX_PAGES = 4096`, `scan_yield()` каждые 256
   страниц;
5. жёсткие лимиты: `MAX_REGIONS = 65536` регионов за вызов, `MAX_SECTIONS`,
   `MAX_PROGRAMS`, лимит байт на снимок. «Бюджет кончился» печатается как
   `stopped_early=true`, а не выдаётся за «образа нет».

Все чтения — через `hromatite_core::safe::read_bytes` (`ReadProcessMemory`
после проверки региона), записи нет вообще.

### 2.3. Вывод (пример формы)

```
eac.module: walked 2341 region(s), 1893 committed/readable, 1 ELF magic hit(s), pages_walked=0, copied=32768 B, 41 ms, stopped_early=false
eac.module: ELF image at 0x7f3c2a000000 size=0x6a4000 protect=rw-
eac.module: snapshot budget 2097152 B, copied 32768 B
elf: class=ELF64 data=LE osabi=3 type=3 (ET_DYN) machine=62 (x86-64) entry=0x0
elf: phoff=0x40 phnum=13 phentsize=56 shoff=0x5f2c10 shnum=27 shentsize=64 shstrndx=26
ph[0] LOAD r-- off=0x0 va=0x0 filesz=0x5f2f40 memsz=0x5f2f40 align=0x1000
...
elf: 13 program header(s), 4 LOAD, 27 section(s)
elf: dynsym: 34 import(s)
elf: imports: ptrace, ioctl, mmap, mprotect, shm_open, shm_unlink, socket, ...
```

Строки идут и в `hromatite_eac_hooks.log`, и в ответ консоли
(`eac.module`). Аргументы: `pages`, `read=512k`, `imports=48`; неизвестный
токен игнорируется (диагностика не должна падать из-за опечатки).

---

## 3. `EAC_BLIND=1` — подавление локальных вердиктов

### 3.1. Что значит «локальный вердикт»

Игра узнаёт о нарушении двумя путями: колбэком
`OnClientIntegrityViolated` (регистрируется через
`EOS_AntiCheatClient_AddNotifyClientIntegrityViolated`) и опросом
`EOS_AntiCheatClient_PollStatus`. Плюс две read-only проверки
(`GetModuleBuildId`, `GetSystemRequirementStatus`), которыми игра решает, что
показывать игроку.

`EAC_BLIND=1` перехватывает **эти четыре** экспорта (плюс существующие восемь).
Серверная сторона не затрагивается: если сервер сам решит кикнуть, это не
наша команда.

### 3.2. Поведение по экспортам

| экспорт | `EAC_BLIND=1` | по умолчанию (`0`) |
|---|---|---|
| `AddNotifyClientIntegrityViolated(handle, opts, clientData, fn)` | колбэк игры **не передаётся** в SDK; возвращается синтетический ненулевой `EOS_NotificationId` (`0xB11D…` + счётчик) | честный trampoline |
| `PollStatus(handle, opts, *OutViolationType, OutMessage)` | `EOS_NotFound` (18) = «нарушения с прошлого вызова не было»; `*OutViolationType = 0` (`EOS_ACCVT_Invalid`) | trampoline |
| `GetModuleBuildId(handle, opts, *OutModuleBuildId)` | реальный вызов выполняется **всегда** (значение уходит в лог), затем при `EAC_BLIND_BUILD_ID=<u32>` в `*Out` пишется управляемое значение и возвращается `EOS_Success` | trampoline |
| `GetSystemRequirementStatus(handle, opts, *OutRequirementStatus, OutReason)` | реальный вызов выполняется (значение в лог), затем в `*OutRequirementStatus` пишется `EAC_BLIND_SYSREQ` (по умолчанию `2` = `EOS_ACCSRS_FullyMet`) и возвращается `EOS_Success` | trampoline |

Числа взяты из официального API-референса EOS SDK (`EOS_EResult`,
`EOS_EAntiCheatClientViolationType`, `EOS_EAntiCheatClientSystemRequirementStatus`),
не из догадок.

### 3.3. Fail-closed правила

* `EAC_BLIND` не задан/`=0` → все четыре хендлера просто вызывают trampoline;
* `AddNotify…` с нулевым колбэком → trampoline (подавлять нечего);
* `PollStatus`/`Get*` с нулевым out-указателем → без записи, честный ответ;
* паника в теле `AddNotify…` → trampoline (паника не пересекает границу
  `extern "system"`);
* `EAC_BLIND_BUILD_ID` не разобран (`0x…` и десятичное поддерживаются) → build id
  не подменяется, пишется строка «passthrough»;
* `EAC_BLIND_SYSREQ` вне `0..=2` → `2`.

### 3.4. Чего здесь сознательно нет

* `OutReason` в `GetSystemRequirementStatus` **не пишется**: его ёмкость лежит в
  options-структуре, а угадывать смещение поля — это способ «безобидной»
  диагностикой испортить память игры. При `FullyMet` причина не нужна.
* `RemoveNotifyClientIntegrityViolated` не хукается: игра получила синтетический
  id, реальный SDK ответит на него `EOS_NotFound` — это штатный ответ, который
  игра обязана переносить.
* Локальные вердикты подавляются, но **не** подделываются сообщения в
  `MessageToServer` — это отдельная (кадровая) часть пайплайна.

### 3.5. Логи

```
[blind] armed: integrity_callback=suppressed poll_status=not_found build_id=passthrough sysreq=2
[blind] client-integrity callback NOT registered #1 handle=0x… clientData=0x… game_fn=0x… -> notification=0xb11d000000000001
[blind] PollStatus #37 -> EOS_NotFound (no violation) suppressed=37
[blind] GetModuleBuildId #2 real=4211 managed=4211 written=true -> EOS_Success
[blind] GetSystemRequirementStatus #1 real=0 managed=2 written=true real_ret=0x0 -> EOS_Success
```

`eac.hooks` печатает ещё две строки состояния:

```
eac.epoch: generation=3 armed=0x7a1b live=0x7a1b stale_ms=8028 stop=none
eac.blind: armed=true notify_suppressed=1 poll_no_violation=37 build_id=passthrough sysreq=2 (build_override=passthrough sysreq_value=2)
```

---

## 4. Проверено (команды и результат)

Все команды — из `~/RustroverProjects/cut-a3`, со своим `CARGO_TARGET_DIR` и
под `flock`:

```
flock /tmp/hromatite-cargo.lock -c 'cd ~/RustroverProjects/cut-a3 && \
  CARGO_TARGET_DIR=$HOME/RustroverProjects/cut-a3/target \
  cargo test --workspace --target x86_64-unknown-linux-gnu --offline'
#  -> ok: 183 + 97 + 15 + 7 + 7 + 6 + 3 + 1 (0 failed)

flock /tmp/hromatite-cargo.lock -c '… cargo build --workspace --release \
  --target x86_64-pc-windows-gnu --offline'
#  -> Finished `release` profile

flock /tmp/hromatite-cargo.lock -c '… cargo clippy -p hromatite-eac \
  -p hromatite-dll -p hromatite-features --all-targets --offline -- -D warnings'
#  -> Finished `dev` profile, без предупреждений
```

Windows-сборка обязательна: `mod win`/`mod live` (VirtualQuery-обход,
`hromatite_core::ffi`) host-тестами не компилируется вообще.

---

## 5. Живой прогон (инструкция; игру запускает только пользователь)

### 5.1. Сборка и доставка

```bash
cd ~/RustroverProjects/cut-a3
flock /tmp/hromatite-cargo.lock -c 'CARGO_TARGET_DIR=$PWD/target \
  cargo build --workspace --release --target x86_64-pc-windows-gnu --offline'
```

Дальше — обычный путь проекта: Steam launch-options wrapper
(`tools/hromatite-launch.sh`) или attach-режим. Ничего в способе доставки не
менялось.

### 5.2. Переменные окружения

Гейт эпохи включён всегда (он не «режим», а инвариант). Остальное — по вкусу:

| переменная | смысл | дефолт |
|---|---|---|
| `EAC_FORGE=1`, `EAC_FORGE_AFTER_MS=…` | существующий захват канала | выкл |
| `EAC_FORGE_INTERVAL_MS` | каденция эмиттера | 2007 |
| `EAC_FORGE_STALE_MS` | бюджет тишины гейта | `max(4×interval, 8000)` |
| `EAC_BLIND=1` | подавление локальных вердиктов | выкл |
| `EAC_BLIND_BUILD_ID=<n>` | управляемый build id (`0x` допускается) | не подменять |
| `EAC_BLIND_SYSREQ=0\|1\|2` | управляемый статус требования | 2 |

Пример «эпоха + слепой режим»:

```
EAC_FORGE=1 EAC_FORGE_AFTER_MS=45000 EAC_BLIND=1
```

### 5.3. Порядок и что смотреть

1. Запустить игру, войти в мир (обычные 40–60 с до `BeginSession`).
2. `eac.hooks` в консоли → ожидаем `exports=11/11`, строки
   `eac.epoch: … stop=none` и `eac.blind: armed=…`.
3. `eac.module` → одна-две строки `eac.module: ELF image at 0x… size=0x…` и
   разбор заголовка. Если `no ELF image found at a region start` — повторить
   `eac.module pages read=4m`.
4. Дождаться захвата: `[forge] start …` / `[report] start …`, затем в
   `hromatite_eac_hooks.log` кадры `[forge] sid=0x… seq=… n=…`.
5. **Главная проверка инварианта**: сменить мир (или дождаться кика и
   реконнекта). В логе должно быть ровно
   `[epoch] stop reason=… armed_sid=0x<старый> …` и **ни одного** `[forge]`/
   `[report]` кадра со старым `sid` после этой строки. Дальше — либо тишина до
   нового `[forge] start sid=0x<новый>`, либо (если модуль замолчал) тишина.
6. С `EAC_BLIND=1` — в логе `[blind] client-integrity callback NOT registered …`
   один раз (игра регистрирует колбэк один раз) и `[blind] PollStatus …`
   по мере опроса. Сообщений `ClientIntegrityViolated` в игре быть не должно.

### 5.4. Что будет означать провал

* кадр со старым `sid` после `[epoch] stop` — инвариант не сработал (баг);
* `[epoch] arm refused` каждые 5 с и захвата нет — модуль не шлёт 512-байтных
  кадров (проверить `EAC_FORGE_MIN_FRAMES`, `EAC_FORGE_AFTER_MS`);
* `exports=8/11` — три новых экспорта не найдены/не пропатчились (смотреть
  `[hook] … ok=false`), `EAC_BLIND` в этом случае будет частичным.

---

## 6. Что осталось непроверенным

* **Живой прогон.** Ни один пункт §5 не выполнялся: игра агентом не
  запускалась. Всё, что выше названо «проверено», — host-тесты, clippy и
  windows-сборка.
* **`eac.module` на реальном образе.** Парсер проверен на синтетическом ELF64
  (заголовок, два `PT_LOAD`, `.dynstr`/`.dynsym`, два импорта). Реальный
  memfd-образ может не иметь таблицы секций (тогда `shnum=0` и импортов не
  будет — это штатный, залогированный исход) или лежать не на границе региона
  (тогда нужен `pages`).
* **Кросс-копийный сценарий** (копия фич переустанавливает хуки) на живой игре
  не воспроизводился: тест `a_silent_stream_stops_the_emitter_after_the_budget`
  проверяет его следствие — остановку по тишине, — но не сам факт двух копий.
* **Реакция игры на синтетический `EOS_NotificationId`.** Ожидание: игра
  хранит id и при выходе зовёт `RemoveNotifyClientIntegrityViolated`, получает
  `EOS_NotFound` и это игнорирует. Не проверено.
* **`GetSystemRequirementStatus`**: не проверено, что игра вообще её зовёт и
  что `FullyMet` её устраивает; смещение `OutReason` намеренно не трогается.
* **Серверная сторона.** Подавление локальных вердиктов не отменяет серверных
  проверок: кик `Authentication … failed` приходит от сервера и этим режимом не
  лечится.

## 6. Живой прогон 2026-11-18: как payload впервые увидел модуль EAC

Прогон: сборка `cut-a3`, `FRAME_HOOK=1`, `LATE_UPDATE=auto`, `LIVE=0`, observe
(без `EAC_FORGE`/`EAC_BLIND`). Игра — pid 996382, uptime ≥15 мин.

**Что подтвердилось сразу.** `eac: hooks installed 11/11 on
EOSSDK-Win64-Shipping.dll` (три новых экспорта пропатчились), GC-guard'ы на
месте (`ordering=true context_call=true suspend_call=true`), ни одного
`SuspendThread loop failed`, ни одного нового `crash.dmp`, ни одной паники и ни
одного AV в этом прогоне (`[veh] caught` = 0 после старта). Живой трафик: за
прогон 170 кадров по 512 Б, 246 исходящих, 540 входящих.

**Три новых экспорта игра не вызывает вообще.** `AddNotifyClientIntegrityViolated`,
`GetModuleBuildId`, `GetSystemRequirementStatus` дали по 0 вызовов за прогон
(`[call] …` в логе отсутствует). Значит local-verdict подавление (`EAC_BLIND`)
нечего подавлять в этом режиме, а хуки на них можно не ставить — patch surface
возвращается с 11 к 8.

**Главное открытие — адресное пространство раздвоено.** Первая версия
`eac.module` (обход `VirtualQuery`) нашла **0** ELF-образов по 1803 регионам,
хотя образ в процессе есть. Хост подтвердил: в `/proc/996382/maps` он лежит по
`0x7c4eb05a1000` (`/memfd:b2e3ac3c-… (deleted)`, 7 сегментов, 6.6 МБ). Причина —
`VirtualQuery` показывает только то, что создал сам Wine; модуль замаплен
нативным кодом лаунчера. Прямое чтение по адресу при этом работает:
`q 0x7c4eb05a1000` отвечает «(unmapped)» только потому, что `q` сначала
спрашивает `VirtualQuery`, а не потому, что память недоступна.

**Вторая ловушка: `proc/self` в Wine — это wineserver.** Чтение
`\\?\unix\proc\self\maps` вернуло 111 маппингов, первый путь —
`.../Proton - Experimental/files/bin/wineserver`; у wineserver (pid 996212) ровно
111 записей, у игры — 2472. То есть `\\?\unix`-файл открывает сервер, и `self`
указывает на него. Абсолютные пути `/proc/<pid>/…` работают нормально.

**Рабочий путь (реализован в `eacimage.rs`):**

1. pid игры — перебор `/proc/<pid>/cmdline`, ищем аргумент, кончающийся на
   `VRChat.exe`;
2. `/proc/<pid>/maps` → memfd-регионы (`offset == 0`, читаемые) → база образа и
   все его сегменты;
3. таблица «файловое смещение → адрес» из `maps`: последний LOAD имеет
   `p_offset 0x219000`, но `p_vaddr 0x222000`, поэтому заголовки программы
   (`phoff 0x655258`) лежат по адресу `base + 0x65e258`, а не `base + 0x655258`;
4. чтение — `copy_nonoverlapping` внутри `veh::call_with`, границы берутся из
   `maps` (AV превращается в `None`, а не в мёртвую игру), бюджет байт
   соблюдается.

**Результат живьём:** `native ELF image at 0x7c4eb05a1000 span=0x65f000`,
сегменты `r--p 0x20000 / r-xp 0x143000 / r--p 0xa9000 / ---p / r--p / rw-p /
r-xp 0x43d000`, `ET_DYN`, `machine x86-64`, 13 program headers (5 LOAD) —
побайтово те же поля, что в офлайн-дампе `~/EAC-RE/ghidra/eac_module_memory.so`.
Секции (`.shdr`) не читаются: они лежат вне загруженных сегментов — это не
ошибка парсера.

**Что из этого следует.**

* payload **может читать** образ античита, но только напрямую по адресу; ни
  `VirtualQuery`, ни `ReadProcessMemory` этот регион не видят.
* запись/патч потребуют смены защиты страницы (`mprotect`) — из PE это возможно
  только сырым `syscall`; это отдельная задача и отдельный риск.
* невидимость двусторонняя: модуль перечисляет только ELF (`link_map`), наш
  PE-образ в него не попадает, поэтому `Forbidden module` по нашему DLL
  невозможен в принципе — что согласуется с отсутствием таких вердиктов во
  всех прогонах.

**Команды.** `eac.module` — native-взгляд (по умолчанию), `eac.module win` —
старый обход `VirtualQuery`, `eac.module self` — взгляд через `proc/self` (он и
есть доказательство того, что `self` = wineserver).
