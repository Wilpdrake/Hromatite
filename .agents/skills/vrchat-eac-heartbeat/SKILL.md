---
name: vrchat-eac-heartbeat
description: Use when probing VRChat EAC heartbeat or Hromatite.
metadata:
  version: "2"
---

# VRChat EAC/EOS heartbeat + Hromatite

Работа идёт в двух местах: `~/EAC-RE` (артефакты и разбор EAC) и репозиторий
`~/RustroverProjects/Hromatite` (Rust-инжектор, куда логика должна в итоге попасть).

## Куда кладём результат (правило пользователя)
- Итоговый продукт живёт **внутри Hromatite** (Rust-крейт/фича в payload'е), а не как
  отдельный C-шим или python-скрипт. Python в `~/EAC-RE/tool` и C `eacshim.dll` —
  исследовательские щупы, их код переносится, а не подключается как зависимость.
- Работа в **git worktree на отдельной ветке**:
  `git worktree add ~/RustroverProjects/Hromatite-eac -b <фаза>`.
  Мерж в master — только после явного «ок» пользователя, `--ff-only`; §17 в master
  дописывается отдельным коммитом после мержа. После мержа следующая фаза стартует
  **новой веткой от master** (стaraя ветка больше не нужна). После мержа проверять
  самому: `cargo test --workspace --target x86_64-unknown-linux-gnu --offline`,
  `cargo build --workspace --release --target x86_64-pc-windows-gnu --offline`,
  `clippy -D warnings` (cargo лежит в `~/.cargo/bin`, его нет в PATH по умолчанию).
- `AGENTS.md` может быть защищён от записи тулзой: если patch блокируется — не ретраить
  и не обходить, а оставить изменение и сказать пользователю (запись уже в ветке может быть).
- Сначала `docs/*-PLAN.md` (≤80 строк: файлы, API, команды, риски, верификация), затем код,
  затем коммит на каждое логическое изменение, затем запись в §17 `AGENTS.md`.

## Hromatite (обязательные правила проекта)
- Rust workspace (edition 2024), цель `x86_64-pc-windows-gnu`, `panic="unwind"`.
  Пакеты: `Hromatite` (bin, инжектор/CLI), `crates/hromatite-core` (manualmap, pe, log,
  lateupdate, gcthreads, m3, hot, sha256, crypto), `hromatite-dll` (payload),
  `hromatite-features`, `hromatite-abi`, `hromatite-console`, `test-target`.
- Запуск: Steam Launch Options → `tools/hromatite-launch.sh %command%`.
  Файл `~/.hromatite-mode` (KEY=VALUE, по строке): `HROMATITE_DLL=Z:\\путь\\x.dll`
  инжектит ЛЮБУЮ dll; `HROMATITE_DISABLE=1` — чистый запуск без инжекта.
- `AGENTS.md` (§17 «Журнал работ»): сначала план → согласование → код,
  **коммитить каждое изменение**, дописывать запись в §17 — требование пользователя.
- Цепочка: Proton Experimental → `VRChat/launch.exe --no-vr` → `start_protected_game.exe` → `VRChat.exe`.

## Факты про EAC EOS (проверено)
- Модуль: `https://modules-cdn.eac-prod.on.epicgames.com/modules/<pid>/<did>/{win64,linux64}`
  (VRChat: `a4a57ff548934dbeba0cc7c62cdf9f34` / `76151011566a4757ae955b380b05c378`).
  win64 = 32 106 424 Б, linux64 = 12 015 192 Б (реально работает под Proton).
- Контейнер — не шифр, а **дифференциальный код**: `M[i] += (-3*i) - M[i+1]` с конца.
  win64 → PE (внутри Oodle-блобы: драйвер, `EasyAntiCheat_EOS.exe`, игровой клиент ~20.6 МБ);
  linux64 → stripped ELF64 (BuildID `9bf67e68ac97e9571ba01b47f6b5c6f101d46daf`).
  Декодер: `~/EAC-RE/tools/venv/bin/python ~/EAC-RE/tools/eac_module_decode.py <file> --out dir`.
- Античит — в процессе игры: EOSSDK 1.19.2.1 (C#) зовёт `EOS_AntiCheatClient_BeginSession`
  (Mode 1), регистрирует `AddNotifyMessageToServer` (25) и `AddNotifyClientIntegrityViolated` (26);
  транспорт сообщений — сама игра.
- **Сессия античита — на каждую комнату**: в реальном логе 18 переходов = 18 BeginSession /
  17 EndSession; пара идёт `OnLeftRoom → EndSession → BeginSession`. Kill процесса = без EndSession.
- **EOSSDK не имеет своего транспорта**: 25 экспортов → vtable → сессия `client+0x2E8` с
  256-байтным ring; всё уходит как `(ctx, ptr, len)` в `AddNotifyMessageToServer`, блоб уже
  EAC-фреймленный (ABI `{ClientData, MessageData, SizeBytes}`, 24 Б). В Mode 1 SDK **не заводит
  таймеров** — каденцию даёт модуль EAC, поэтому свой генератор обязан сам держать 2 с.
  `ProtectMessage` добавляет ровно **22 Б** (AES-256-GCM: ciphertext‖u32 seq‖u8 type(=1)‖u8 flags‖16-Б tag).
- Каркас кадра: 24 байта `{u16 Version=2, u16 Flags, u64 SessionId, u64 Sequence, u32 Checksum}`;
  **открыта только обвязка, тело — нет**. Checksum = обычный **CRC-32C по кадру с обнулёнными
  этими 4 байтами, init=0, xorout=0** (не VM) — проверено на 451/451 кадре из 5 сессий, и
  `frame_analyze.py forge <capture.bin>` собирает кадр с `verify=True`. НО `[24:]` — 488 Б
  непрозрачного потока: энтропия 7.5 бит/байт на кадр (в пуле 50 кадров 7.99), между соседними
  heartbeat меняется **491 из 512 байт** — но это **не шифр** (см. раздел «Тело кадра — поток
  сообщений»): меняются сами 16-Б записи. AES-256-GCM/ChaCha/HMAC «не сидят» именно потому, что
  AEAD там нет: offline-перебор 27 ключей × 8 nonce × 5 AAD = 0 — ожидаемый результат, а не
  доказательство, что тело закрыто статическим OpenSSL.
- **Поток сообщений измерен живьём, направления не путать** (захват через обёртку
  `AddNotifyMessageToServer` + хук `ReceiveMessageFromServer`):
  * **клиент→сервер heartbeat = 512 Б каждые ~2007 мс** (median 2007 мс; 114/118 в 1900–2150 мс);
    это и есть кадр, который надо генерировать;
  * **сервер→клиент ack = 24 Б каждые ~2050 мс** — ровно заголовок; его легко принять
    за «heartbeat», но это входящее направление;
  * рукопожатие: всплеск 24/144/152/156/214/274/356/372/464/512 Б, дальше ровный ритм.
  Номинальный интервал = **2000 мс** (не 5000). Null-клиент сам шлёт только 24 и 116 Б и
  **никогда** 512-байтный кадр целостности — по этому признаку видно, что сессии нет.
- **Тайминги наказаний (измерено):**
  * null-клиент (`EOS_USE_ANTICHEATCLIENTNULL=1`): кик **~3 с** после входа в мир, реджойны и
    кик каждые **24–27 с**, текст «unable to establish or maintain … integrity»;
  * сессия была и замолкла (drop исходящих): сервер терпит **~120 с**, затем
    `DisconnectByDisconnectMessage`, Photon **code 104**, «could not maintain a verified
    connection», после чего VRChat сам делает `EndSession → BeginSession` и пробует снова;
  * «полностью без EAC» (стаб вместо `start_protected_game.exe`): `Anti-cheat client not
    available. Verify that the game was started using the anti-cheat bootstrapper…` →
    offline mode и **Error World**, кика как такового нет.
- **Реплей чужих кадров не спасает:** 60 снятых кадров, влитых в колбэк игры, не удержали
  сессию — кадры привязаны к `SessionId`/`Sequence`/checksum. Значит нужна своя живая сессия,
  а не копия трафика; и «одного heartbeat» не хватает.
- **Полностью без EAC** (`EOS_USE_ANTICHEATCLIENTNULL=1`): лог пишет «Anti-cheat client not
  available, using null client», `BeginSession` рапортует **Success**, игра входит в мир —
  и выкидывает с «The VRChat Anti-Cheat system was unable to establish or maintain your
  VRChat installation's integrity…». Отсчёт вести **от входа в мир**: в воспроизводимых
  прогонах это **~3 с** (и реджойн/кик каждые 24–27 с); «~38 с» из первого наблюдения — это
  от старта процесса. `BeginSession: Success` при null-клиенте ничего не гарантирует.
- **На Linux/Proton у EAC нет ядра**: в `/proc/modules` ни одного `eac` (0 из ~293). Вся защита —
  user-mode внутри Wine-процесса, поэтому детект-поверхность слабее, чем на Windows
  (там драйвер-минифильтр с колбэками). Инжект с активным EAC наблюдался в мире ~2.5 минуты
  без кика — но это «детекта не наблюдали», а не «детекта нет».
- Драйвер (win64): минифильтр (импорт только `FLTMGR.SYS!FltRegisterFilter`), 40 МБ, `.text` ~2.5 МБ,
  безымянная секция 37 МБ = байткод-VM; IOCTL статически не виден. Связь с модулем — не IOCTL, а
  секция `Global\EasyAntiCheat_EOSBin` + события `Global\EasyAntiCheat_EOSEventDriver|Module|Game`.
- Логи игры: `<prefix>/drive_c/users/steamuser/AppData/LocalLow/VRChat/VRChat/output_log_*.txt`.
- Подробная анатомия (блобы, экспорты клиента, диспетчер VM `imm32 − 0x7A7B`, канал секции,
  дифф CDN/диск, классы EOSSDK, релей id 5/6): `references/module-and-protocol.md`.

## Тело кадра — поток сообщений, а не шифр (проверено на 14 сессиях)
- 512-Б кадры — **фрагменты логических сообщений**: `StartData` (бит0) стоит только на первом
  кадре сообщения, `EndData` (бит1) — на последнем; в кадре 488 Б тела.
- Заголовок сообщения открытый и читается на фиксированных смещениях:
  `u32 total_len | u32 1 | u32 inner_ck | u32 5 | u32 action | u32 3 | u32 counter |`
  `48-Б per-session блоб` (одинаков внутри сессии, разный между сессиями) `| 2 Б | N × 16 Б записей`.
  `action=11` — handshake, `action=13` — первый отчёт, `action=4` — отчёты; `counter` растёт +1
  на сообщение, `total_len` = полная длина сообщения в байтах.
- Признаки «не шифр»: открытые поля на фиксированных смещениях, `(len − 78) % 16 == 0`, XOR-тест
  без переиспользования keystream, 16-Б записи уникальны внутри сессии (похоже на хеши памяти).
- Читать это, а не переоткрывать: `~/EAC-RE/findings/frame_structure.md` §4–§8,
  `~/EAC-RE/findings/report_stream.md`; инструменты `tools/report_extract.py` (режет capture на
  сообщения), `tools/make_report_capture.py` (вырезает отчёт в capture-формат),
  `tools/frame_analyze.py messages|cross|forge`.
- Следствие: «валидный по CRC кадр» ≠ «пройденная проверка». Реплей/форж обязаны воспроизводить
  **поток сообщений** (`total_len`, растущий `counter`, уникальные записи, per-session блоб), а не
  один и тот же кадр; повтор одного фрагмента ломает формат сразу по трём пунктам.

## Инструмент `~/EAC-RE/tool` (python, stdlib)
- `eacheartbeat`: framing (EACH v2), crypto (AES-GCM/CBC/CTR, HMAC, HKDF), session
  (BeginSession→RegisterPeer→RUNNING, таймауты), transport, pcap, `vrchat_relay`
  (id 5/6 = notify_message_to_server / receive_message, base64 + `u32 decoded_size`).
- `python3 -m eacheartbeat --selftest` → 98 проверок PASS, не ломать.
- Логи: `--log FILE` (JSONL `epoch/seq/dt_ms/gap`) у `demo-session`/`serve`;
  просмотр — `python3 -m eacheartbeat logs FILE`; `gap` считается между сессиями.

## Живые прогоны (рецепт)
Пошаговые команды запуска/инжекта/null-клиента/отрубания на ходу и сбор доказательств —
`references/live-runbook.md`. Коротко: `~/.hromatite-mode` (KEY=VALUE) → `HROMATITE_DLL=Z:\\...`
для инжекта, `HROMATITE_DISABLE=1` для чистого запуска, env прокидывается и в игру;
запуск `steam -applaunch 438100 --no-vr`; логи игры в prefix
`drive_c/users/steamuser/AppData/LocalLow/VRChat/VRChat/output_log_*.txt`.
Автоматизация одного прогона (бэкап и восстановление `~/.hromatite-mode` по sha256, ожидание
`Entering Room`, сбор логов игры и `hromatite_eac_hooks.log`, `pkill -f 'VRChat[.]exe'`):
`scripts/live_run.sh <dll-win-path> <out-dir> <seconds> [KEY=VALUE ...]`.

## Измеренные тайминги и кадры (2026-11-17, живые прогоны)
- **heartbeat клиент→сервер = 512 Б каждые ~2007 мс** (median по 121 кадру, 114/118 в 1900–2150 мс).
  Входящий keepalive — **24 Б каждые ~2050 мс** (это ack сервера, не наш кадр). Событийные 24 Б —
  рукопожатие/смена мира; рукопожатие = 24/144/152/156/214/274/356/372/464/512 Б.
- **Кики:** нет EAC-клиента (null) → ~3 с после входа в мир, далее реджойн и кик каждые 24–27 с;
  живая сессия замолкла (дроп исходящих) → **120.5 с**, `DisconnectByDisconnectMessage`, Photon **code 104**,
  текст «could not maintain a verified connection»; реплей чужих кадров → **~200 с**, тот же 104.
- **Полное отключение EAC** = подменить `start_protected_game.exe` стабом, который стартует `VRChat.exe`.
  Только стаб → «Verify that the game was started using the anti-cheat bootstrapper» → offline mode/**Error World**;
  стаб + `EOS_USE_ANTICHEATCLIENTNULL=1` → «using null client», BeginSession=Success, кик ~3 с.
- **Forge (свои кадры) транспорт проходит, но сессию не держит:** `EAC_FORGE=1` в ветке `eac-forge`
  собирает 512-Б кадры (sid/flags/тело из последнего живого, seq+1, CRC-32C init=0/xorout=0,
  каденция 2007 мс, `delivered=true`, CRC 129/129 оффлайн), но сервер выкидывает через
  **39–75 с** с тем же `code 104`/«could not maintain a verified connection» — раньше тишины
  (120.5 с). Значит транспорт принимается, а тело (16-Б записи-хеши) сервер считает устаревшим;
  `EAC_FORGE_BODY=bump` (растущий счётчик `msg+0x18`) не помогает.
- **Кадры сессионно-привязаны:** реплей не работает — кадры сессионно-привязаны (`SessionId` чужой ≠ ожидаемый, `Sequence`/checksum),
  сервер их не подтверждает. Без ключа сессии/алгоритма checksum (VM) свой heartbeat не подставить
  в готовом виде; self-replay требует перезапуска (capture читается один раз в `install()`).

## EOSSDK (статический RE, findings/eossdk.md)
- SDK **не открывает сокетов**: отдаёт готовые EAC-фреймленные буферы через `AddNotifyMessageToServer`
  (ABI `{ClientData, MessageData, SizeBytes}`); значит генератор должен воспроизводить продукцию буферов.
- В Mode 1 (VRChat) SDK **не заводит таймеров** — каденцию даёт модуль EAC; `Protect/Unprotect` в Mode 1
  не вызываются (22-байтный overhead живьём не наблюдался).
- Крипта: AES-256-GCM (BoringSSL), overhead ровно `+0x16`; в конструкторе захардкожен seed-ключ
  `b68d74704c4e9e43d491ef367345373db742514ae1eb3bb9a167a30b3a35c7ea` (pre-session, не подтверждён вживую).
- **Seed-ключ в памяти не ищется побайтово**: конструктор `0x180405AF0` собирает его из `movl`
  иммедиатов на стеке, поэтому в `.rdata`/куче нет 32-байтной константы — скан памяти даёт только
  self-hits (наши же DLL и таблица needle). Искать надо хук на сам конструктор (12-Б пролог, без
  `rel32`) либо уже готовый ключ в контексте шифра.
- **Кольцо кадров модуля найдено в памяти**: шаг слота **0x220** (32 Б заголовок + 512 Б кадр),
  десятки слотов в двух аренах ~11 МБ и ~16 МБ. Это самый быстрый способ читать живые кадры без
  хуков; адреса искать по `SessionId` из свежего кадра.

## Hromatite: EAC-фича уже в master
- Крейт `crates/hromatite-eac` (epoch-модель, кодек 24 Б + payload 120 Б, JSONL с ротацией, tailer),
  команды `eac.status|begin|tick|end|log|tail|hooks`, `configure eac*`.
- Фаза 2 (хуки) тоже в master: `hook.rs` (порт C-движка: трамплин ±512 МБ, rel8 `jcc`), `eos.rs`
  (8 хуков, capture `u32 len|u32 dt|bytes`, `EAC_REPLAY=1`, `EACSHIM_DROP_AFTER_MS`/`_DROP_IN`), установка
  из стартового потока резидента (иначе `AddNotify` проскакивает). Живой результат: `hooks 8/8`, 512 Б ×121.
- **Грабля:** `EACSHIM_DROP_AFTER_MS=cmd` (формат C-шима) раньше молча включал drop через 20 с →
  churn Begin/End каждые 24–30 с и только мелкие кадры. Исправлено: нечисловое = выключено (`78d8122`).
- Проверки: `cargo test --workspace --target x86_64-unknown-linux-gnu --offline` (3/119/5/32+1/15),
  `cargo build --workspace --release --target x86_64-pc-windows-gnu --offline`, `clippy -D warnings`.

## После обновления игры: переопределить il2cpp-API (без запуска игры)
- Beebyte меняет только **имена** экспортов; сами функции остаются в том же порядке и
  размере в блоке il2cpp-API (`0xc20000..0xc30000`). Значит последовательности «адрес →
  группа экспортов» старого и нового `symbols-<sha12>.json` выравниваются один-к-одному:
  `scripts/export_rank_align.py <old.json> <new.json> old_name=0xRVA …`. Сходимость
  проверять тремя числами: одинаковое число групп (типично 185/185), одинаковый span
  (`0x8d30`), выжившие якоря (`il2cpp_gchandle_get_target`, `il2cpp_native_stack_trace`)
  отображаются сами в себя. Без этих проверок выравнивание — догадка.
- Обновлять после выравнивания: `CLASS_*_RVA`/`IMAGE_GET_CLASS_RVA` в `m3.rs`,
  `CORLIB_IMAGE_GETTER_RVA` в `core/raw/il2cpp.rs` **и** те же константы в
  `hromatite-features/src/{m3.rs,game_fixture.rs}` — иначе feature-образ и его тесты
  расходятся с core.
- Таблицу классов (`s_TypeInfoDefinitionTable`) искать не надо: это глобал в `.data`
  образа, который ссылается на массив **в куче**, поэтому «поиск массива в образе» находит
  заголовок модуля или мусор. Рабочий путь — вызвать `il2cpp_class_from_name(image, ns, name)`,
  fallback — перебор `il2cpp_image_get_class(image, i)` с проверкой `name`/`namespace`;
  сканер таблицы оставлять только диагностикой.
- Кандидатов проверять по **именам**, а не по «непустой строке»: в живом логе мусорные
  таблицы давали имена `"n"`, `">"`, `"ISE"`. Требуй ≥90% идентификатороподобных имён,
  уникальные и монотонно возрастающие указатели, заполненность массива; все окна скана
  клампить в `[base, base+size)` — fine-pass ниже базы модуля даёт лучший score из заголовка.
- Детали, порядок вызовов и что считать успехом в логе: `references/il2cpp-class-lookup.md`.

## Грабли
- `pkill -f 'VRChat.exe'` убивает **собственную** сессию агента: шаблон матчит строку
  `bash -c ... VRChat.exe ...`. Всегда `pkill -f 'VRChat[.]exe'`; то же для pgrep.
- Хук-трамплин возвращался в `entry` вместо `entry+len` → рекурсия и лог 8.7 ГБ за 35 с:
  всегда ротация лога (32 МБ) и проверка «одна строка на вызов».
- На стенде хуки вставали 12/12, в живой игре 0/12 — логировать причину отказа по каждой функции.
  Причина была в прологе реального EOSSDK: `48 83 ec 28 / 48 85 c9 / 74 13` — rel8 `jcc` на +7,
  а трамплин был в ~4 ГБ. Либо аллоцировать трамплин в пределах ±512 МБ, либо переписывать
  rel8-переход в `inverse jcc+0x0C / mov rax,imm64 / jmp rax`.
- Хуки на `AddNotifyMessageToServer` надо ставить **из стартового потока резидента**, а не из
  первого тика: игра регистрирует колбэк один раз и рано, тик проигрывает гонку и молча
  остаётся без исходящих сообщений.
- Мерить трафик можно только в **нормальной сессии**: сначала `Entering Room:` + `OnPlayerJoined`,
  потом смотреть кадры; в кик-лупе видно только 24/116 Б и легко сделать неверный вывод о протоколе.
- Переменные окружения шима живут в общем `~/.hromatite-mode` — нечисловое значение
  (`EACSHIM_DROP_AFTER_MS=cmd`) не должно означать «резать через 20 с по умолчанию»: парсить
  через `and_then(parse().ok())` и дефолт **0/off**, иначе payload молча работает в режиме drop
  и игра крутится в `BeginSession/EndSession` каждые 24–30 с.
- REA MCP — одна глобальная сессия (параллельным агентам `mcp__rea__*` не давать);
  `mcp__ghidra__*` привязан к GUI с GameAssembly.dll — для EAC не годится.
- NixOS: `export PATH=/run/current-system/sw/bin:$PATH` перед coreutils/strings/objdump.
- Декодер модуля требует numpy → запускать только `~/EAC-RE/tools/venv/bin/python`, иначе
  `ModuleNotFoundError`. Он исторически ждёт MZ: linux64 распаковывается в ELF, и без приёмки
  ELF-magic скрипт останавливается на «stage1 did not produce MZ» — это не ошибка артефакта.
- Файлы в prefix нельзя менять при живом EAC: Proton, дописывающий файлы, ловится проверкой
  целостности (`failed to verify installation integrity`, error 16) — сначала закрыть игру.
- `~/.hromatite-mode` указывает на тестовую DLL после эксперимента — **обязательно вернуть**
  исходное содержимое (иначе пользователь запустит игру с шимом). Бэкапить и сверять sha256.
- Нельзя создавать поток (`std::thread::spawn`) внутри `DllMain`/`install()` payload'а: SDK
  перестаёт вызывать обёртку (0 исходящих кадров, «Authentication with the VRChat Anti-Cheat
  system failed»). Стартовать генератор лениво из первого вызова обёртки — тогда кадры идут.
- Второй инстанс игры одновременно не поднять: живые прогоны (EAC on/off) — строго
  последовательно, иначе логи и инжект перепутываются.
- Проверять инжект без игры: `shimhost.exe` через Proton/wine с тем же префиксом — быстрее и
  безопаснее, чем сразу лезть в VRChat.
- В JSONL-логе эпоха должна жить **в объекте лога, а не в сессии**, а `dt_ms`/`gap` считаться
  между сессиями — иначе переход мира (новая сессия) не даёт гэпа и разница «по действиям» не видна.
- Трамплин должен перелокализовать не только `rel8 jcc`, но и **`call rel32`**: если просто скопировать
  пролог, детур на `EVP_*` (BoringSSL) ставится «успешно», а через ~40 с игра падает — симптом
  «детуры встали 5/5, но живой прогон не живёт». Для первого прохода проще хук на пролог из 12 байт
  без `rel32` (например конструктор).
- `ReadProcessMemory` возвращает успех, но **не число прочитанных байт**: в обёртке из Rust/Python
  проверка «результат == 0» инвертирует статус и сканы дают «миллионы МиБ, 0 попаданий» — возвращать
  lpNumberOfBytesRead и уважать частичное чтение.
- Перед анализом тела сначала читать `findings/frame_structure.md` §4 и `report_stream.md`: модель
  «кадр — фрагмент сообщения» там уже описана. Переоткрывать её заново — потерянный час и те же выводы.
- Управляющие кадры (24/116 Б) тоже несут бит `StartData` в своём поле флагов → при разборе
  сообщений **сначала фильтровать записи по размеру 512**, иначе каждый управляющий кадр
  открывает «новое сообщение» и таблица разваливается (34 «отчёта» вместо 2).
- Длинные python-пробы через heredoc в `terminal` могут не пройти lifecycle-guard («script the
  command executes must be scannable») → писать пробу файлом в `~/EAC-RE/tools/*.py` и запускать
  `~/EAC-RE/tools/venv/bin/python <file>`; заодно остаётся воспроизводимый артефакт.
- Параллельные рабочие потоки по одному репозиторию разводить по **отдельным git worktree**
  (`git worktree add ~/RustroverProjects/<repo>-<тема> -b <ветка> <база>`): агенты в одном index
  коммитят друг другу поверх и затирают правки.
- Рецепт на 8+ агентов (работает): (1) закоммитить WIP в базовую ветку, чтобы все стартовали
  от одного коммита; (2) каждому агенту — свой worktree, **своя ветка** и **свой список
  файлов**, никаких пересечений (мерж потом без конфликтов); (3) `delegate_task` принимает
  массив задач — один агент на элемент, одиночный элемент в массиве запускает **одного**
  агента; (4) сборки сериализовать локом (`flock /tmp/hromatite-cargo-A.lock -c 'cargo …'`)
  и разными `CARGO_TARGET_DIR` на агента — иначе 4 ядра и 15 ГБ RAM уходят в своп;
  (5) живой прогон агенту не давать: игра — синглтон, прогоны строго последовательно и
  только родителем после мержа; (6) `~/.hromatite-mode`, `~/EAC-RE/build-eac` и `AGENTS.md`
  агентам трогать запрещено (журнал §17 дописывает родитель); (7) long-running `cargo test`
  запускать фоном с notify, а не в foreground с таймаутом больше лимита.
- `/proc/self/maps` из вине-процесса описывает **wineserver**, а не игру (110 регионов / 51 МиБ):
  регионы для скана памяти брать через `VirtualQuery` (≈1000–1200 регионов, 3–4 ГБ).
