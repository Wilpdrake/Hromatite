# Анатомия EAC-модуля и протокола (win64 + linux64)

Сжатая выжимка из RE. Всё ниже — наблюдённые факты с якорями (offset/RVA/hash),
если не помечено иначе.

## Контейнер модуля (CDN)
- URL: `https://modules-cdn.eac-prod.on.epicgames.com/modules/<pid>/<did>/{win64,linux64}`.
  win32/macos для VRChat отдают 403.
- win64 = 32 106 424 Б (etag = md5 `7770cf19…`), linux64 = 12 015 192 Б (etag `9ce44927…`).
  Байт-в-байт совпадает с кэшем клиента:
  `<prefix>/drive_c/users/steamuser/AppData/Roaming/EasyAntiCheat/<pid>/<did>/easyanticheat_x64.eac`
  (+ `.metadata` = `sha1=…` / `etag="…"`), т.е. скачанный файл можно сверять по sha1 из .metadata.
- Код — не шифр, а **обратный разностный**: `M[i] += (-3*i) - M[i+1]`, идти с конца.
  Проверка правильности: нулевые прогоны обязаны попасть ровно в границы PE-секций
  (0x400, 0x104e00, 0x1dd3000, …).
- После раскодирования: win64 → PE-образ (stage1), linux64 → **ELF64 DYN** (BuildID `9bf67e68…`).
  Декодер ожидает MZ и на ELF падает — это ограничение скрипта, не артефакта.
- Внутри `stage1` большая `.data` с блобами `[u32 uncompressed_size][Oodle/Kraken stream]`.
  win64 дал три: драйвер 40 411 288 Б, `EasyAntiCheat_EOS.exe` 1 041 336 Б (sha256 `0bbe6d48…`,
  = установленный файл), игровой клиент `client.dll` 20 593 664 Б.

## Игровой клиент (win64 `blob3_client.dll`)
- PE32+ x64, imagebase `0x180000000`, 8 секций, **нет таблицы импортов**;
  `.text` 0x25ACFA обычного кода + безымянная секция 0x1035000 (17.5 МБ) = байткод-VM.
- Экспорты: 4 штуки, **без имён** — ординалы 1..4 → RVA `0xBEA66`, `0x207FC0`, `0x2D3120`, `0xBE8BB`.
  `DllMain` = `31 C0 C3` (`xor eax,eax; ret`).
  #2 — шлюз в VM (сохраняет GP+XMM, `call` в VM, `jmp r10` при нуле);
  #3 — данные: 5 записей `{fnptr,0}` (арности 0/0/0/3/3) = интерфейс модуля.
- Диспетчер VM: стаб делает `mov eax,imm32; add rax,[r12+0x60]; jmp rax`,
  где `[r12+0x60] = imagebase − 0x7A7B` ⇒ **цель = imm32 − 0x7A7B** (проверено: 0x34CBA6 → 0x34512B).
  Таких стабов 2062, реальных целей 1603 (0x3452CE–0x3A32CB); плюс 557 прямых `e9` в VM.
- Кадр подтверждён в `FUN_1801acf01`: разбирает `rsi` как
  `{u16@0, u16@2, u64@4, u64@0xC, u32@0x14}` = 0x18 байт; сборка 0x1ADFC0..0x1AE01B,
  проверка checksum 0x1ADE73 (обнуляет поле, зовёт колбэк, восстанавливает, сравнивает).
  Алгоритм checksum — в VM, статически не извлечён.
- Внутри также полноценный TLS 1.2 стек (списки `TLS-ECDHE-…-GCM-SHA384`), SHA-256 K-таблица
  (`0x2B0FF0`), CRC-32C полином (`0x2B1F30`), строки `Hash Catalogue`, `Forbidden module`,
  `EAC index certificate revoked`, `Anti-Cheat Integrity CA1..CA11`.
- Хостов в открытом виде нет: имя устройства собирается в рантайме (`0x1C18BB`), API-указатели
  обфусцированы.

## Загрузчик и канал к драйверу
- `start_protected_game.exe` (4 МБ) качает/кэширует модуль и маппит stage1 (имена экспортов
  `a..e`; запрашиваются ординалы 1,2,3,5). Генерик PE-маппер (MZ/PE/machine, `VirtualAlloc`),
  call-order: init → map → вызвать экспорт `a` со структурой 0x38 байт.
- **DeviceIoControl в загрузчике почти нет** (единственный вызов — HIDAPI от SDL, код 0xB0192).
  Реальный канал (UTF-16 строки в stage1): секция `Global\EasyAntiCheat_EOSBin` и события
  `Global\EasyAntiCheat_EOSEventDriver` / `…EventModule` / `…EventGame`;
  драйвер ставится через `sc.exe` (`EasyAntiCheat_EOS.sys`, start=demand), устройство `\??\…\EasyAntiCheat_EOS\`.
- Драйвер (виндовый, 40 МБ) — минифильтр, единственный импорт `FLTMGR.SYS!FltRegisterFilter`;
  `DriverEntry` делает только `jmp` в VM; в `.rdata` — Wintrust-политики и идентификация железа
  (`MBR:%08x`, `Part:%u`, `CD:%u`, `:lun=%llx`).
- CDN-вариант драйвера ≠ файлу с диска: ровно 69 байт в хвосте `.rdata` (file 0x30F577),
  PE-чексумма и подпись сходятся только у файла с диска → CDN-версия — пост-билдовая правка,
  вероятно функционально нейтральная.

## Инструмент: linux64 (то, что работает под Proton)
- linux64 — **один ELF64 DYN** (12 015 192 Б, BuildID `9bf67e68…`, stripped), без вложенных
  артефактов: ни PE, ни второго ELF, ни `.ko`, ни архива, ни `[u32][Kraken]`-фрейминга.
- Импорты: `dlopen/dlsym`, `ioctl`, `shm_open/shm_unlink`, `socket/connect/send/recv`, `mprotect`,
  `prctl`, `execv`; статический `libstdc++` и **статический OpenSSL** → TLS внутри модуля.
- **Скрытый 5-й `PT_LOAD`**: RX, file `0x962000` / vaddr `0x969000`, 2 165 888 Б, **не покрыт ни
  одной секцией**; в него ведут 62 прямых `jmp/call` из `.text` (`0x44FE8: jmp 0x96DADB`).
  Анализ только по section headers теряет 2.1 МБ кода — идти по program headers.
- К файлу дописан code-signing конверт с `0xB72C80`: PKCS#7 SignedData + DER-цепочка
  (Epic Games Inc. ← DigiCert G4 Code Signing CA1 ← Trusted Root G4) + метка
  `~Code signature appended~`; проверять `openssl smime -verify` над `file[0:0xB72C80]`.

## EOSSDK (клиентская часть античита)
- `EOSSDK-Win64-Shipping.dll` (19.5 МБ, 1.19.2.1) содержит классы
  `FAntiCheatClient`, `FAntiCheatClientSession`, `FAntiCheatP2PClient`, `FAntiCheatClientNull`,
  `FAntiCheatServer`, каждый с `Init/Shutdown/Tick`; env `EOS_USE_ANTICHEATCLIENTNULL`, лог
  `Anti-cheat client protection not active (NullClient)`.
- 25 экспортов `EOS_AntiCheatClient_*`. Строки `HEARTBEAT`/`HEARTBEAT-ACK`/`heartbeat-interval`
  принадлежат в основном встроенному WebRTC/dcsctp P2P-стеку — не путать с anti-cheat heartbeat.
- **Сокета у SDK нет вообще** (RE + живые логи): 25 экспортов → vtable `0x180F75A30` → сессия
  `client+0x2E8` (0x348 Б, 256-байтный ring `+0x1E8`, pump `0x1803C1300`, стоки `+0x1C8` to-server,
  `+0x1D0` to-peer); всё уходит как `(ctx, ptr, len)` в `AddNotifyMessageToServer`.
  Trampoline `0x1803E28C0` собирает документированный `{ClientData, MessageData, SizeBytes}`,
  указатель заимствованный, блоб уже EAC-фреймленный — заголовок не добавляется/не снимается.
  Эмулятор heartbeat'а воспроизводит **производство буфера**, а не сетевое соединение.
- `Tick` `0x1803BB0F0` = аккумуляция dt → очередь таймеров `0x180A7D280` → pump;
  `BeginSession` взводит пять таймеров **только в Mode 2** (`cmp [..],2 / jne` в `0x1803B7903`),
  в Mode 1 (VRChat) — ни одного: каденцию задаёт модуль EAC, SDK лишь отдаёт буферы.
- `GetProtectMessageOutputLength` `0x1803C1250` = `len + 0x16`: overhead ровно **22 Б**,
  конверт `ciphertext ‖ u32 seq ‖ u8 type(=1) ‖ u8 flags ‖ 16-Б tag`, AES-256-GCM через BoringSSL
  (`0x180405C40`/`0x180405DD0`).
- В конструкторе `0x180405AF0` захардкожен 32-байтный pre-session seed
  `b68d74704c4e9e43d491ef367345373db742514ae1eb3bb9a167a30b3a35c7ea` (гейтится флагом `+0x1C`);
  жив ли он после аутентификации — не проверено (нужен watchpoint).

## Измеренный провод (живые прогоны, EAC ON)
- Исходящие (клиент→сервер): **512 Б × 121** из 170 кадров, median **2007 мс**
  (114/118 в 1900–2150 мс), 512 Б = 87 % байт; 24 Б × 18 — событийные (рукопожатие/смена мира).
- Входящие: 24 Б × 154 (ack), 144 × 12, 48 × 2, 512 × 1, 156 × 1.
- Null-клиент: исходящие только 24 и 116 Б; 512-байтного кадра целостности нет.
- Сервер терпит молчание ~120 с (code 104), затем игра сама делает EndSession→BeginSession.

### Раскладка 512-Б кадра (проверено 451/451 из 5 сессий)
```
[0:2]   u16 Version = 2
[2:4]   u16 Flags   (бит0 StartData, бит1 EndData)
[4:12]  u64 SessionId      (в захватах 0x158b / 0x297e / 0x22d1 / 0x135d — на сессию)
[12:20] u64 Sequence       (сквозной, +1 на кадр)
[20:24] u32 CRC-32C(кадр с обнулёнными этими 4 Б, init=0, xorout=0)
[24:]   488 Б тела
```
- Тело — **не plaintext**: энтропия 7.5 бит/байт на кадр, в пуле 50 кадров 7.99, все 256 значений;
  между соседними heartbeat меняется 491/512 байт (min 485, max 493), монотонных u32/u64 нет.
- Т.е. защита тела сидит **внутри модуля** (linux64: статический OpenSSL), а не в каркасе;
  offline AES-256-GCM/ChaCha/Poly1305/HMAC по 6 кадрам двух сессий (27 ключей × 8 nonce × 5 AAD ×
  6 разбиений заголовка) — ни одного совпадения.
- `forge` (свой кадр с телом последнего живого, `Sequence+1`, пересчитанный CRC, каденция 2007 мс)
  проходит транспорт (`delivered=true`, CRC оффлайн 129/129), но сервер рвёт сессию через **39–75 с**
  (тот же `code 104`) — быстрее, чем при полной тишине (120.5 с): тело оценивается и устаревшее
  отвергается. `EAC_FORGE_BODY=bump` (растущий счётчик `msg+0x18`) не помогает.
- Формат файла захвата (общий для C-шима и Rust-payload): `u32 len | u32 dt_ms | bytes[len]`,
  без заголовка файла; парсер/форж — `~/EAC-RE/tools/frame_analyze.py` (команды `analyze`, `forge`).

## VRChat-релей (публичный `vrc-eac-emulator`)
- 1 WS-сообщение = 1 пакет: `u8 packet_id` + LE POD-поля; строки — nullable
  (`u8 is_valid`, затем `i32 len` + байты).
- id: 0 handshake, 2 create_platform, 3 initialize_eos, 4 begin_session,
  5 notify_message_to_server, 6 receive_message, 7 end_session.
- AC вверх: id 5 = `u8 require_bind | i32 api_version` + base64 + `u32 decoded_size`;
  вниз: id 6 = `i32 api_version` + base64 + size. Длины сверяются с заявленными.
- Порты 7777/7778 — LAN-протокол эмулятора, не транспорт VRChat; реальный канал игры
  к бэкенду публично не задокументирован (в il2cpp-репортах помечен «Unknown»).
- Реализация в инструменте: `eacheartbeat/vrchat_relay.py`, CLI `relay-encode`/`relay-decode`/`relay-serve`.
