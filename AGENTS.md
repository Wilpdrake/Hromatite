# AGENTS.md — Hromatite

> Полный контекст проекта: что уже проверено руками, что сломано, что дальше.
> Документ для агентов (и людей), которые подхватят работу. Всё, что помечено
> «проверено» — подтверждено реальным запуском на этой машине, а не выведено
> логически. Всё, что не проверено, помечено явно.

Дата последнего обновления: 2026-11-14 (MSK).
Хост: NixOS 26.11, kernel 6.18.x, Intel i5-11300H, 15 GB RAM.
Пользователь пишет по-русски; предпочитает: сначала план → согласование → код.
Коммитить **каждое** изменение (просьба пользователя).

---

## 0. TL;DR состояния

| Что | Статус |
|---|---|
| Каркас workspace (Rust, windows-gnu) | ✅ собирается |
| Manual map в wine (релокации, импорты, TLS, entry) | ✅ проверено на подставном exe |
| Steam launch-options wrapper | ✅ проверено (инжект в подставной `VRChat.exe`) |
| L1 скрытность: `RW→RX`, стёртые заголовки | ✅ в коде, проверено по логу |
| L3: без своих потоков (inline `DllMain`) | ✅ в коде, проверено по логу |
| Module stomp | ⚠️ реализован, но slack мал (debug-DLL 6.1 МБ) → fallback |
| Attach-режим (env игры вместо `proton run`) | ⚠️ внутри контейнера не проверен; вне контейнера падает на 32-битном лоадере |
| Резолв таблицы экспортов il2cpp в рантайме | ⚠️ код есть, в живой VRChat не запускался |
| `il2cpp_domain_get` и вызовы il2cpp | ❌ не реализовано (M2 в процессе) |
| Чтение игроков/позиций (M3) | ❌ |
| Запись в игру (M5) | ❌ |
| Инцидент: EAC выкинул с «failed to verify installation integrity» (error 16) | ⚠️ причина не изолирована, есть два подозреваемых (см. §8.4) |

---

## 1. Железные факты об окружении (проверено)

```
OS           NixOS 26.11 (Zokor), kernel 6.18.x, x86_64
Rust         rustc 1.99.1, targets: x86_64-pc-windows-gnu, x86_64-unknown-linux-gnu
Mingw        x86_64-w64-mingw32-gcc 16.2.0 (NixOS), binutils 2.46
Steam        ~/.local/share/Steam (НЕ ~/.steam/steam — это симлинк-совместимость)
VRChat       ~/.local/share/Steam/steamapps/common/VRChat   (appid 438100)
Proton       "Proton - Experimental" 11.0-100 (есть также "Proton 10.0"; "Proton 9.0 (Beta)" НЕТ)
Prefix       ~/.local/share/Steam/steamapps/compatdata/438100/pfx
Container    SteamLinuxRuntime_4 (pressure-vessel, sniper)
EAC          EasyAntiCheat_EOS (EasyAntiCheat_EOS_Setup.exe), модуль `linux64`
ptrace_scope 1 по умолчанию → для внешнего чтения памяти нужен 0 (правка в nixos-config, см. §11)
protontricks не установлен (добавлен в nixos-config, требует rebuild)
```

Модули API игры:

```
/S/VRChat.exe                  — 64-bit PE, дочерний процесс launch.exe
UnityPlayer.dll                — 33 725 864 Б
GameAssembly.dll               — 252 339 712 Б
baselib.dll                    — 239 016 Б (кандидат на stomp)
VRChat_Data/globalgamemanagers — Unity 6000.0.67f1-DWR (строка в plaintext)
VRChat_Data/il2cpp_data/Metadata/global-metadata.dat — 40 745 428 Б, ЗАШИФРОВАН
GameAssembly sha256            — 648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b
```

Проверка отпечатка (команды):

```bash
G=~/.local/share/Steam/steamapps/common/VRChat
strings -n 6 $G/VRChat_Data/globalgamemanagers | head -3      # 6000.0.67f1-DWR
od -A x -t x1z -N 32 $G/VRChat_Data/il2cpp_data/Metadata/global-metadata.dat
x86_64-w64-mingw32-objdump -p $G/GameAssembly.dll | sed -n '/^\[Ordinal\/Name Pointer\] Table/,/^$/p' | head
```

---

## 2. Билд VRChat: что именно мы имеем (проверено на этом билде)

- Unity **6000.0.67f1**. Игра обновляется → экспортные имена **переобфусцируются**
  (Beebyte), поэтому любые имена/адреса валидны только для конкретного sha256.
- `GameAssembly.dll`: **251 экспорт**. Имена — случайные 11-символьные
  (`AQIKwyWT_pM`, `A_eHSn_ViIs`, `MRRsoZbTqVa`, …). Часть имён выжила:
  `Flush`, `DllCanUnloadNow`, `CloseNLSocket`, `CreateNLSocket`,
  `il2cpp_free`, `il2cpp_gchandle_get_target`, `il2cpp_native_stack_trace`.
- `global-metadata.dat` **зашифрован**: магики `FAB11BAF` нет, первые байты
  `3a 08 77 f3`. Офлайн Il2CppDumper/Cpp2IL на диске бесполезны; в памяти после
  `il2cpp_init` метадата расшифрована — дампить надо рантаймом.
- `UnityPlayer.dll` содержит **блок имён экспортов GameAssembly** (тем самым он их
  и резолвит): 234–235 из 251 имени лежат плотным блоком в `.rdata`
  (наблюдённые диапазоны: ~`0x1b72xxx`–`0x1b78xxx` и второй плотный кластер ~`0x1ba11xx`).
  Имена — **inline-строки**, не указатели; 8/16-байтовое выравнивание, без релокаций.
- **Порядок имён в блоке НЕ совпадает** с порядком `il2cpp-api-functions.h` (Unity 2020).
  Два выживших «якоря»: `il2cpp_native_stack_trace` — индекс 71 в пуле против 83
  в хедере; `il2cpp_gchandle_get_target` — 128 против 116. Значит Unity 6 вставила
  функции в середину таблицы → **мапить имена по порядку нельзя**.

Вывод для нас: имя → адрес определяем **эмпирически в рантайме** (§6).

Инструмент: `tools/symmap.py` генерирует `crates/hromatite-core/src/symbols.rs`
(251 запись `(name, rva)` + `UNITY_VERSION` + `GAMEASSEMBLY_SHA256` + `POOL`)
и побочный артефакт `symbols-<sha12>.json`. Генерация обязательна после каждого
обновления игры.

---

## 3. Структура репозитория

```
Hromatite/
├─ Cargo.toml                 # пакет Hromatite (CLI) + workspace (core, dll, test-target)
├─ .cargo/config.toml         # target = x86_64-pc-windows-gnu, linker=mingw,
│                             # rustflags=-L/run/current-system/sw/lib (иначе нет -l:libpthread.a)
├─ src/main.rs                # CLI: bridge | inject | procs
├─ crates/hromatite-core/     # вся Win32/PE/il2cpp-механика (библиотека)
│  ├─ ffi.rs                  # ручные extern-декларации Win32 (без внешних крейтов)
│  ├─ log.rs                  # лог в %USERPROFILE%\hromatite.log (+ fallback C:\)
│  ├─ procs.rs                # Toolhelp32: find_pid, modules, own_module
│  ├─ remote.rs               # OpenProcess + RPM/WPM/VirtualAllocEx/CreateRemoteThread,
│  │                          # парсинг экспортов чужого модуля (с forwarder'ами)
│  ├─ pe.rs                   # парсер PE из файла: секции, reloc, imports, TLS
│  ├─ manualmap.rs            # сам маппер: релоки, импорты, TLS, стаб, опции stealth
│  ├─ il2cpp.rs               # рантайм-резолв таблицы экспортов + геттеры-синглтоны
│  └─ symbols.rs              # СГЕНЕРИРОВАННЫЙ symmap.py (не править руками)
├─ crates/hromatite-dll/      # cdylib "hromatite.dll": DllMain + inline recon
├─ crates/test-target/        # подставной VRChat.exe (tick-цикл) для тестов маппера
├─ tools/symmap.py            # генератор символов (uv run --no-project python)
├─ tools/hromatite-launch.sh  # Steam launch-options wrapper + инжект
├─ symbols-<sha12>.json       # побочный выход symmap
└─ AGENTS.md                  # этот файл
```

Сборка:

```bash
cd ~/RustroverProjects/Hromatite
cargo build --workspace            # → target/x86_64-pc-windows-gnu/debug/{Hromatite.exe,hromatite.dll,test-target.exe}
cargo build --workspace --release  # для stomp (см. §7.3)
uv run --no-project python tools/symmap.py     # обновить symbols.rs после патча игры
```

Замечание: `cargo build` в корне workspace'а с корневым пакетом собирает только
корневой пакет; для DLL нужен `--workspace`.

---

## 4. Что уже сделано и чем это подтверждено

### 4.1 Manual map работает в wine (проверено)

Лог успешного запуска на подставном `VRChat.exe` в отдельном префиксе:

```
remote_load: api-ms-win-core-synch-l1-2-0.dll not in module list, using HMODULE 0xffbf0000 as base
find_export_anywhere: WaitOnAddress in kernelbase.dll at 0xfffffc62a20
import: api-ms-win-core-synch-l1-2-0.dll -> WaitOnAddress lives in module 0xfffffbf0000
manualmap: ran 2 TLS callback(s)
[dll] DllMain(PROCESS_ATTACH), inline (no extra thread)
manualmap: ... mapped at 0x1990000 (relocs 1955), entry 0x1991230 returned
```

Порядок шагов маппера (`manualmap::inject`):

1. `PeImage::from_file` — заголовки, секции, reloc/imports/TLS.
2. `VirtualAllocEx(size_of_image, PAGE_READWRITE)` **или** `alloc_at` в slack модуля.
3. Локальная сборка образа: заголовки + секции по `VirtualAddress`.
4. `apply_relocs` (тип 10 DIR64; тип 0 игнор; прочие — в лог).
5. `resolve_imports`: имя DLL → база модуля в **целевом** процессе; forwarder'ы
   разворачиваются; apiset-заглушки ищутся по имени экспорта во всех модулях.
6. (опция) зануление `size_of_headers` в локальном образе.
7. Один `WriteProcessMemory` всего образа.
8. `VirtualProtectEx`: `.text` → `PAGE_EXECUTE_READ`, данные → `PAGE_READWRITE`.
9. Один стаб (`dll_stub`) со всеми TLS-колбэками + entry, `CreateRemoteThread`,
   `WaitForSingleObject`.

### 4.2 Wine-специфика, на которой мы уже обожглись (не переоткрывать)

| Симптом | Причина | Решение в коде |
|---|---|---|
| `CreateRemoteThread` возвращает NULL | в `OpenProcess` не было `PROCESS_CREATE_THREAD` (0x0002) | маска доступа в `Remote::open` |
| `DllMain` не выполняется, но exit code = 1 | wine не спец-обрабатывает DLL entry: `CreateRemoteThread(entry, param)` даёт `reason` = мусор | вызов entry через x64-стаб (`dll_stub`) с `rcx=base, edx=1, r8=0` |
| std-инициализация «полуживая» | у Rust-cdylib **есть** 2 TLS-колбэка (`__dyn_tls_init`, `_CRT_INIT`) | те же стабы вызывают их до entry |
| экспорт `kernel32!LoadLibraryW` указывает внутрь exported-dir | в wine `kernel32` — форвардеры в `kernelbase` | `Remote::export_in` разворачивает `"mod.Func"` (до 4 уровней) |
| apiset-импорт не резолвится (`api-ms-win-core-synch-l1-2-0.dll`) | модуля нет в списке, `LoadLibraryW` возвращает фейковый HMODULE `0xffbf0000` | `find_export_anywhere`: ищем экспорт по имени во всех PE-модулях (обычно `kernelbase.dll`) |
| «unresolved thunk (entry 0xf3c30)» | читали имя из массива thunk'ов, а не из `IMAGE_IMPORT_BY_NAME` | читать **значение** thunk'а, затем `+2` |
| пустой лог при успехе | `std::process::exit` не флашит stdout | `stdout().flush()` перед exit |

### 4.3 Скрытность, уже включённая

- Нет анонимной **RWX**: `RW` при записи → `RX` для кода.
- Заголовки PE в памяти занулены (`--keep-headers` отключает).
- TLS + entry — **один** стаб-поток; payload работает **inline** в `DllMain`
  (никаких `CreateThread` внутри игры).
- Маппинг не появляется в PEB (manual map), файла на диске рядом с игрой нет.

### 4.4 Launch wrapper (`tools/hromatite-launch.sh`)

Steam → VRChat → Свойства → Параметры запуска:

```
/home/wilpdrake/RustroverProjects/Hromatite/tools/hromatite-launch.sh %command%
```

Логика: запускает `"$@"` (это SLR+proton+launch.exe), затем ищет wine-процесс игры
по `/proc/*/cmdline` + `readlink /proc/<pid>/exe` (должно быть `*wine*`), ждёт 3 с,
делает до `HROMATITE_TRIES` (по умолчанию 3) попыток инжекта с паузой 12 с.

Переменные:

```
HROMATITE_DISABLE=1    — чистый запуск без инжекта
HROMATITE_MODE=proton  — старый путь через `proton run`
HROMATITE_TRIES=n      — число попыток
```

Режим по умолчанию — **attach**: env берётся из `/proc/<game-pid>/environ`
(`WINEPREFIX`, `WINEDLLPATH`, `WINEDLLOVERRIDES`, `LD_LIBRARY_PATH`, `PATH`, …),
значения экранируются (в `WINEDLLOVERRIDES` есть `;`), и запускается `wine`
напрямую, **без** повторного `proton run`. Причина — §8.4.

---

## 5. Wine/Proton: почему «извне» не работает

`wine` кладёт сокет сервера в `/tmp/.wine-<uid>/server-<dev>-<ino>`, где `<ino>` —
inode каталога `$WINEPREFIX`. Внутри pressure-vessel inode префикса другой, поэтому:

- игра (в контейнере): `/tmp/.wine-1000/server-27-783de1`
- наш `proton run`/`protontricks-launch` снаружи: `.../server-27-a14223`

→ **разные wineserver'ы** → `Toolhelp32` в нашем процессе видит только сам себя
(проверено: `Hromatite.exe bridge` → «VRChat.exe not found», хотя игра работала).

Как диагностировать:

```bash
pgrep -a wineserver
tr '\0' '\n' < /proc/<wineserver-pid>/environ | grep WINEPREFIX
readlink /proc/<wineserver-pid>/cwd      # = /tmp/.wine-1000/server-...
```

Следствия:
1. Инжект обязан выполняться **внутри** контейнера игры — даёт launch wrapper
   (`%command%` исполняется в контейнере) или DLL-прокси.
2. `protontricks-launch` снаружи контейнера для инжекта бесполезен (но годен для
   прочих задач).
3. `steam-runtime-launch-client --alongside-steam` — правильный «вход» в чужой
   контейнер, но хостовый бинарь требует glib из рантайма (`/proc/<pid>/root/usr/bin/...`).

---

## 6. il2cpp: стратегия резолва (M2)

Дано: имена экспортов обфусцированы, порядок в пуле не канонический, метадата на
диске зашифрована.

План, реализованный в `crates/hromatite-core/src/il2cpp.rs`:

1. Найти в образе `UnityPlayer.dll` каждую строку имени из `symbols::EXPORTS`.
2. Для каждой — найти слоты, хранящие указатель на эту строку (8-байтовое LE = VA строки).
3. Около слота искать значение, попадающее в диапазон `GameAssembly` (base..base+SizeOfImage):
   кандидатные смещения `DELTAS = [8, 16, 24, -8, 32, 40]`; выбирается смещение
   с наибольшим числом попаданий (голосование) — это и есть layout приватной структуры.
4. Итог: `Slot { name, name_off, addr_off, addr }` — эмпирическая карта
   «обфусцированное имя → реальный адрес функции».
5. `getter_candidates`: короткие геттеры-синглтоны (`48 8B 05 … C3` или `48 8D 05 … C3`).
   Оттуда берётся `il2cpp_domain_get` и прочие «получить глобал».
6. Дальше (не реализовано): вызвать `il2cpp_domain_get`, `il2cpp_domain_get_assemblies`,
   проверить `count > 1`, затем `il2cpp_class_from_name(img, "UnityEngine", "Camera")`.

Критерий готовности M2: в логе есть `diag: export table: N slots resolved` при N ≥ 200,
`il2cpp_domain_get` возвращает стабильный не-NULL, `domain_get_assemblies > 1`.

Альтернативы (если п.1–3 не сработают на живом процессе):
- читать таблицу резолвнутых указателей, которую заполняет сам Unity, напрямую;
- паттерн-скан по `.text` GameAssembly для нужных функций + версия-гейт;
- брать `il2cpp_*` адреса из in-memory метадаты (метадата расшифрована после `il2cpp_init`).

---

## 7. Скрытность: лестница и измерения

### 7.1 Уровни

| Уровень | Что | Статус |
|---|---|---|
| L0 | manual map в анонимный RWX, заголовки на месте, свой поток | было, выкинуло EAC |
| **L1** | `RW→RX`, стёртые заголовки, один стаб-поток, без файла рядом с игрой | ✅ реализовано |
| L1.5 | stomp: образ внутри slack уже загруженного модуля (в memory-map регион числится за модулем) | ⚠️ код есть, slack мал |
| **L2** | «двойная память»: pristine-копия + рабочая зашифрованная (RW, не executable), расшифровка на время вызова, XOR обратно; опционально `PAGE_GUARD`+VEH на pristine | ❌ |
| **L3** | ноль своих потоков: логика в существующем потоке игры (hook `PlayerLoop`/`Present`), инжект через APC | ⚠️ частично: `DllMain` inline, поток один |

### 7.2 Почему именно так

Под Linux EAC читает память с хоста (`/proc/<pid>/mem`, `process_vm_readv`), т.е.
**обходя page-протекции**. Поэтому «execute-only» страницы не спасают; спасают:
file-backed память (stomp), отсутствие RWX, отсутствие анонимных exec-регионов и
шифр в памяти в покое (L2).

### 7.3 Замеры

```
debug hromatite.dll   6 104 946 Б  (RX-секции, но образ большой)
baselib.dll           239 016 Б    slack — считанные килобайты
→ stomp baselib: "not enough slack, falling back to a fresh allocation"
```

Следующие шаги для stomp: `--release` + `strip` (ожидаемо 300–600 КБ) и/или stomp в
секцию, которая мертва после загрузки (`UnityPlayer.dll:.reloc` — релокации уже
применены), либо в модуль с большим `SizeOfImage`.

---

## 8. EAC: что известно, что нас касается

### 8.1 Как EAC запускается у нас

`start_protected_game.exe` → `EasyAntiCheat_EOS` → бутстрап (пишет `[Windows]` в лог,
но `System name: 'linux64'`) качает модуль `easyanticheat_linux_x64.eac` (~12 МБ) с
`modules-cdn.eac-prod.on.epicgames.com` и пишет `Starting Wine module mapping,
Wine version: 11.0.` → `Launcher finished with: 301, 'Easy Anti-Cheat successfully
loaded in-game'`.

Логи:

```
compatdata/438100/pfx/drive_c/users/steamuser/AppData/Roaming/EasyAntiCheat/
  a4a57ff548934dbeba0cc7c62cdf9f34/76151011566a4757ae955b380b05c378/anticheatlauncher.log
```

Важно: пользователь умеет запускать игру с `--no-eac` (в логах видели
`S:\steamapps\common\VRChat\VRChat.exe --no-vr --no-eac`).

### 8.2 Векторы детекта, которые применимы к internal-читу

Источники: `girther9399-gif/EAC-REVERSED-WRITEUP` (анализ драйвера EAC),
`aftermathlabs.net/blog/10/08/2021` (EAC мапит собственный модуль в игру),
UC-сниппеты (сам форум закрыт Cloudflare).

1. **VAD/private + executable + без file-backing** → «shellcode/manual map» → скан региона.
   (Linux-аналог: анонимный `rwxp`/`r-xp` регион в `/proc/<pid>/maps`.)
2. **RWX** — флаг сам по себе, даже без найденного PE.
3. **Скан на PE в приватной памяти**: характерные границы секций, массивы IAT,
   энтропия `.text`.
4. **Потоки**: `PsSetCreateThreadNotifyRoutine` → start address вне известных модулей
   = подозрительный поток. (Мы уменьшили: один стаб-поток, payload inline.)
5. **Дерево процессов / хендлы**: лишний процесс с открытыми хендлами к игре.
6. **Целостность установки** (интегральный чек, error 16): проверка файлов игры,
   а не памяти.

### 8.3 EAC Linux v2 (дедлайн проекта)

Анонс Epic 2026-10-14 (`onlineservices.epicgames.com/news/introducing-easy-anti-cheat-linux-v2`):

- без kernel-модуля, но: **systemd-сервис + eBPF + LSM BPF**;
- прямая цитата: *«This enables anti-cheat to restrict other processes from reading
  or writing protected game process memory without needing a kernel module»*
  — это приговор внешнему ридеру (вариант C);
- требования: подписанное ядро из well-known списка (Canonical, Fedora, Universal
  Blue, Debian, openSUSE, SUSE, Rocky, MS UEFI CA, Alma — **NixOS нет**), TPM 2.0,
  Secure Boot, measured boot, IOMMU, kernel lockdown integrity, kexec off, LSM BPF;
- Steam Deck официально не поддерживается;
- разработчик игры сам выбирает v1 или v2; пилот сначала в Fortnite;
- фичреквест VRChat «не включайте v2 на Linux» (14.10.2026, 41+ голосов, ответа нет).

Практический вывод: на NixOS v2-сессия скорее всего вообще не пройдёт аттестацию
(нет ключа NixOS), т.е. риск не «нас поймают», а «игра перестанет запускаться».
Горизонт — месяцы, не дни.

### 8.4 Инцидент: `Disconnected` / «Anti-Cheat failed to verify installation integrity» (error 16)

Что было: игра запущена через wrapper, инжект прошёл
(`mapped at 0x6d770000`, DllMain отработал), через несколько минут — кик.

Подозреваемые (не изолировано):

1. **Второй `proton run` внутри живой сессии** — Proton заново раскладывал файлы в
   префикс, в логе: `Proton: Error while copying to ".../system32/nvngx_update.exe":
   No such file or directory`. Это ровно «installation integrity».
2. **Детект памяти** — анонимный RWX + два потока со стартом в анонимной памяти
   (тогда был старый режим L0).

План изоляции (A/B, требует перезапусков игры):

- A: `HROMATITE_DISABLE=1` через wrapper → если чисто, wrapper сам по себе безвреден;
- B: wrapper + attach-инжект (без `proton run`, без RWX, без лишних потоков) → если
  чисто, виноват был `proton run`; если кик, виноват детект памяти;
- C: `--no-eac` как контроль (EAC вообще не работает).

---

## 9. milestone-план и текущий фокус

| M | Что | Статус |
|---|---|---|
| M0a | `ptrace_scope=0` в nixos-config (+ protontricks) | правка сделана, rebuild не выполнен |
| M0b | «мостик»: инжектор видит процесс игры | ✅ (в одной wine-сессии) |
| M1 | manual map + hello-world | ✅ |
| M1.5 | L1/L3 скрытность | ✅ код / ⚠️ тест в игре |
| M2 | резолв таблицы экспортов + `il2cpp_domain_get` | ⚠️ код есть, в игре не запускался |
| M3 | read-only телеметрия: локальный игрок, список игроков, позиции → JSON | ❌ |
| M4 | визуализация (TUI или X11-оверлей) без записи в игру | ❌ |
| M5 | запись в игру (FOV/timeScale/позиция) — только с явного согласия | ❌ |
| L2 | двойная память (shadow pages) | ❌ |

Текущий фокус: получить лог M2 из живой игры после перезапуска с wrapper'ом.

---

## 10. Логи и как их читать

```
~/.hromatite-launch.log                          — wrapper: попытки, fallback, env
<prefix>/drive_c/users/steamuser/hromatite.log   — инжектор + payload (тег [dll])
<prefix>/drive_c/users/steamuser/AppData/Roaming/EasyAntiCheat/<pid>/<depid>/anticheatlauncher.log
<prefix>/drive_c/dllmain-probe.txt               — маркер, что DllMain дошёл
```

Полезные грепы:

```bash
L=~/.local/share/Steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser/hromatite.log
grep -E "inject|stomp|manualmap|DllMain|mapped|modules ready" $L | tail -20
grep "^diag:" $L | tail -30          # M2: сколько слотов разрешилось, геттеры, hexdump
grep -iE "fallback|unusable" ~/.hromatite-launch.log
```

---

## 11. Изменения в системе вне репозитория

`~/nixos-config` (правки сделаны, **rebuild не выполнен**):

- `modules/base.nix`: `boot.kernel.sysctl."kernel.yama.ptrace_scope" = 0;`
- `modules/packages.nix`: `protontricks` в `environment.systemPackages`

Применить: `sudo nixos-rebuild switch` (нужно для внешнего чтения памяти, вариант C,
и для protontricks; для пути «wrapper + attach» не блокирует).

---

## 12. Известные баги и хрупкие места

1. **`CreateToolhelp32Snapshot` иногда висит ~390 с** при конфликте wine-сессий
   (в логе виден разрыв 1794627388 → 1794627776 между заголовком и «target pid»).
   Лечение: перечисление процессов через `/proc/*/cmdline`+`/proc/*/exe` (в wrapper'е
   уже так) или таймаут/повтор в `procs::find_pid`.
2. **`proton run` из хоста создаёт отдельный wineserver** (§5) — тестовые сценарии
   надо строить в одной сессии.
3. **stdout не флашится** при `std::process::exit` — исправлено в `main.rs`.
4. **Экранирование env в shell** — `WINEDLLOVERRIDES` содержит `;`: без
   single-quote-обёртки `export` ломается (исправлено в wrapper'е).
5. **Debug-DLL 6.1 МБ** — не влезает в slack обычных модулей; для stomp нужен release.
6. **`pkill -f pattern`** убивает собственную оболочку, если паттерн есть в её
   командной строке; использовать `[p]attern`.
7. **Вне контейнера `wine` не запускает даже `cmd`**: `/lib/ld-linux.so.2: could not
   open` (32-битный лоадер есть только в Steam runtime) → attach-режим вне контейнера
   падает, wrapper это детектит и откатывается на `proton run`.

---

## 13. Открытые вопросы / решения, ожидающие пользователя

1. Изолировать причину error 16 (A/B/C из §8.4) — нужен перезапуск игры.
2. Attach-режим внутри контейнера: работает ли — узнаем из лога после перезапуска;
   если нет — вариант B (DLL-прокси) или `steam-runtime-launch-client`.
3. Цель stomp: release+strip и куда именно мапить (slack `baselib.dll` vs мёртвая
   секция `UnityPlayer.dll`).
4. Нужен ли M4-оверлей (X11) или достаточно TUI/лога.

---

## 14. Конвенции работы агента в этом репозитории

- Пользователь — русскоязычный; план → согласование (нумерованные варианты
  1/2/3/4) → реализация → короткий отчёт с фактами из логов.
- **Коммит на каждое изменение** (git; remote не настроен, коммиты локальные).
- Не трогать файлы игры: доставка — launch wrapper (или DLL-прокси по согласованию).
- Тестировать маппер на `crates/test-target` в отдельном префиксе
  (`STEAM_COMPAT_DATA_PATH=/…/scratch/pmtestN`), не на живой игре.
- Любое утверждение о поведении EAC/il2cpp подкреплять логом или файлом; иначе
  помечать как гипотезу.
- После обновления VRChat — перегенерировать `symbols.rs` и зафиксировать новый sha256.

---

## 15. Источники

- Epic, EAC Linux v2: <https://onlineservices.epicgames.com/news/introducing-easy-anti-cheat-linux-v2>
- Разбор детекта EAC (VAD, RWX, PE-in-private, потоки): <https://github.com/girther9399-gif/EAC-REVERSED-WRITEUP>
  (`detection_methods.md`, `internal_cheats_and_injectors.md`, `usermode_techniques.md`)
- EAC self-integrity bypass (драйвер): <https://github.com/gmh5225/Bypassing-EasyAntiCheat-Integrity-check>
- EAC инжектит свой модуль в игру: <https://aftermathlabs.net/blog/10/08/2021/>
- Module stomping (техника): <https://github.com/MaorSabag/NaX/blob/main/wiki/Module-Stomping.md>
- VRChat фичреквест «не включайте EAC v2 на Linux»:
  <https://feedback.vrchat.com/feature-requests/p/do-not-require-easyanticheat-v2-for-linux-secure-boot-tpm-attestation-root-lockd>
- VRChat IL2CPP реверс (отдельный билд, только для перекрёстных проверок имён):
  <https://github.com/dwgx/vrchat-il2cpp-re>
- Порт мода на Unity 6 (наблюдения по обфускации/метадате):
  <https://github.com/kawaiistudio/VRChatArchiveMod/blob/main/docs/UNITY6-PORT.md>
