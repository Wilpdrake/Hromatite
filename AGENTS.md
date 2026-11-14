# AGENTS.md — Hromatite

> Полный контекст проекта: что уже проверено руками, что сломано, что дальше.
> Документ для агентов (и людей), которые подхватят работу. Всё, что помечено
> «проверено» — подтверждено реальным запуском на этой машине, а не выведено
> логически. Всё, что не проверено, помечено явно.

Дата последнего обновления: 2026-11-14 (MSK), последний коммит `0717d6a`.
Хост: NixOS 26.11, kernel 6.18.x, Intel i5-11300H, 15 GB RAM.
Пользователь пишет по-русски; предпочитает: сначала план → согласование → код.
Коммитить **каждое** изменение (просьба пользователя).

> **ПРАВИЛО (обязательное, просьба пользователя):** каждое завершение работы —
> этап, фича, починенный баг, удачный/неудачный эксперимент — агент **дописывает
> в этот файл**, в раздел §16 «Журнал работ»: дата, коммит, что сделано, чем
> подтверждено (лог/факт), что осталось. Не «переписать заново», а **дополнить**.
> Файл — единственная память проекта между сессиями.

---

## 0. TL;DR состояния

| Что | Статус |
|---|---|
| Каркас workspace (Rust, windows-gnu) | ✅ собирается (debug + release) |
| Manual map в wine (релокации, импорты, TLS, entry) | ✅ проверено на подставном exe и в живой игре |
| Steam launch-options wrapper | ✅ проверено (инжект в подставной и в живой VRChat) |
| Attach-режим (env игры вместо `proton run`) | ✅ внутри контейнера игры работает (лог 07:14); ❌ вне контейнера (`/lib/ld-linux.so.2` отсутствует) |
| L1 скрытность: `RW→RX`, стёртые заголовки, один стаб-поток | ✅ в коде, подтверждено логом живой игры |
| L3: без своих потоков (inline `DllMain`) | ✅ в коде, подтверждено логом |
| L1.5a: `--mode loadlib` (file-backed, не анонимная память) | ✅ реализовано, `DllMain` исполняется |
| L1.5b: PEB unlink (вычерк из Ldr-списка) | ⚠️ реализовано, оффсеты исправлены; успех ещё не подтверждён логом |
| Module stomp | ⚠️ реализован через slack; slack у всех модулей 2–4 КБ → нужен release+strip или перезапись мёртвой секции |
| Резолв таблицы экспортов il2cpp в рантайме | ⚠️ скан теперь по всем регионам процесса (235/251 имён, 236 указателей на имена найдены в куче), но поле с адресом рядом не лежит → layout неизвестен |
| L2 «шифр в покое» (`--seal`) | ✅ проверено в живой игре: регион `rw-p`, ни одной exec-страницы, содержимое — шифр |
| `il2cpp_domain_get` и вызовы il2cpp | ❌ не реализовано (M2 в процессе) |
| Чтение игроков/позиций (M3) | ❌ |
| Запись в игру (M5) | ❌ |
| L2 «двойная память» | ❌ спроектировано (§7.4), не написано |
| Инцидент: EAC выкинул с «failed to verify installation integrity» (error 16) | ⚠️ причина не изолирована (см. §8.4) |

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
ptrace_scope 1 по умолчанию → для внешнего чтения памяти нужен 0 (правка в nixos-config, §11)
protontricks не установлен (добавлен в nixos-config, требует rebuild)
```

Модули и файлы игры (размеры — с текущего билда 2026-11-14):

```
/S/VRChat.exe                  — 64-bit PE (672 256 Б), дочерний процесс launch.exe
UnityPlayer.dll                — 33 948 072 Б   SizeOfImage 0x02153000, хвост-slack 0x7c8
GameAssembly.dll               — 253 001 728 Б   SizeOfImage 0x0f440000, хвост-slack 0xbcc
baselib.dll                    — 239 016 Б       SizeOfImage 0x0003d000, хвост-slack 0x96c
libsodium.dll                  — 344 064 Б       хвост-slack 0xf54
VRChat.exe                     — 672 256 Б       хвост-slack 0x9a8
VRChat_Data/globalgamemanagers — Unity 6000.0.67f1-DWR (строка в plaintext)
VRChat_Data/il2cpp_data/Metadata/global-metadata.dat — 40 745 428 Б, ЗАШИФРОВАН
GameAssembly sha256            — 648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b
```

Важно: «хвостовой slack» (место после последней секции внутри `SizeOfImage`) у
**всех** модулей 2–4 КБ → stomp «в хвост» годится только для очень малого образа.

Наши артефакты (актуально):

```
debug   hromatite.dll  6 104 946 Б   Hromatite.exe 6 201 578 Б
release hromatite.dll  1 293 496 Б   Hromatite.exe 1 305 949 Б
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
├─ src/main.rs                # CLI: bridge | inject | procs | mods
├─ crates/hromatite-core/     # вся Win32/PE/il2cpp-механика (библиотека)
│  ├─ ffi.rs                  # ручные extern-декларации Win32 (без внешних крейтов)
│  ├─ log.rs                  # лог в %USERPROFILE%\hromatite.log (+ fallback C:\)
│  ├─ procs.rs                # Toolhelp32 + PEB unlink (unlink_from_peb)
│  ├─ remote.rs               # OpenProcess + RPM/WPM/VirtualAllocEx/CreateRemoteThread,
│  │                          # парсинг экспортов чужого модуля (с forwarder'ами)
│  ├─ pe.rs                   # парсер PE из файла: секции (+characteristics), reloc, imports, TLS
│  ├─ manualmap.rs            # маппер: manual-map и loadlib, stealth-опции, один стаб
│  ├─ il2cpp.rs               # рантайм-резолв таблицы экспортов (один проход) + геттеры
│  └─ symbols.rs              # СГЕНЕРИРОВАННЫЙ symmap.py (не править руками)
├─ crates/hromatite-dll/      # cdylib "hromatite.dll": DllMain + inline recon + PEB unlink
├─ crates/test-target/        # подставной VRChat.exe (tick-цикл) для тестов маппера
├─ tools/symmap.py            # генератор символов (uv run --no-project python)
├─ tools/hromatite-launch.sh  # Steam launch-options wrapper + инжект (attach/proton)
├─ tools/test-loadlib.sh      # стенд для loadlib-пути в отдельном префиксе
├─ symbols-<sha12>.json       # побочный выход symmap
└─ AGENTS.md                  # этот файл
```

Сборка:

```bash
cd ~/RustroverProjects/Hromatite
cargo build --workspace            # → target/x86_64-pc-windows-gnu/debug/{Hromatite.exe,hromatite.dll,test-target.exe}
cargo build --workspace --release  # для stomp/скорости скана (см. §7.3)
uv run --no-project python tools/symmap.py     # обновить symbols.rs после патча игры
```

Замечание: `cargo build` в корне workspace'а с корневым пакетом собирает только
корневой пакет; для DLL нужен `--workspace`.

CLI (`Hromatite.exe`):

```
bridge                     найти VRChat.exe, показать базы GameAssembly/UnityPlayer, прочитать PE-заголовок
inject --dll <Z:\path>     инжект; флаги:
      --mode manual|loadlib    manual = свой маппер (нет записи в PEB), loadlib = лоадер игры
                               (file-backed страницы + payload сам вычёркивается из PEB)
      --stomp <module>         попытаться занять хвостовой slack модуля
      --keep-headers           не занулять PE-заголовки
      --proc <name>            имя процесса (по умолчанию VRChat.exe)
      --pid <n>                готовый wineserver-pid (обходит Toolhelp, который умеет висеть)
procs                      список процессов в текущей wine-сессии
mods                       список модулей целевого процесса (что видит модульный скан)
```

---

## 4. Что уже сделано и чем это подтверждено

### 4.1 Manual map работает в wine (проверено)

Лог на подставном `VRChat.exe`:

```
remote_load: api-ms-win-core-synch-l1-2-0.dll not in module list, using HMODULE 0xffbf0000 as base
find_export_anywhere: WaitOnAddress in kernelbase.dll at 0x6fffffc62a20
import: api-ms-win-core-synch-l1-2-0.dll -> WaitOnAddress lives in module 0x6fffffbf0000
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
| `peb unlink: false` (первый прогон) | `cur` в Ldr-списке указывает на `InMemoryOrderLinks` (**+0x10 внутри** `LDR_DATA_TABLE_ENTRY`) | `DllBase = cur+0x20`, `SizeOfImage = cur+0x28`, `BaseDllName = cur+0x48` |
| `mapped at 0x103` в loadlib-режиме | `0x103` = `STILL_ACTIVE`: поток ещё спит в `DllMain` (`wait_for_modules`) | ждать/игнорировать код; базой считать только значение > `0x10000` (TODO в `inject_loadlib`) |

### 4.3 Скрытность, уже включённая

- Нет анонимной **RWX**: `RW` при записи → `RX` для кода.
- Заголовки PE в памяти занулены (`--keep-headers` отключает).
- TLS + entry — **один** стаб-поток; payload работает **inline** в `DllMain`
  (никаких `CreateThread` внутри игры).
- Manual map не попадает в PEB; loadlib-режим попадает, но payload вычёркивает
  свой `LDR_DATA_TABLE_ENTRY` (код есть, успех пока не подтверждён).

### 4.4 Launch wrapper (`tools/hromatite-launch.sh`)

Steam → VRChat → Свойства → Параметры запуска:

```
/home/wilpdrake/RustroverProjects/Hromatite/tools/hromatite-launch.sh %command%
```

Логика: запускает `"$@"` (SLR+proton+launch.exe), затем ищет wine-процесс игры
по `/proc/*/cmdline` + `readlink /proc/<pid>/exe` (должно быть `*wine*`), ждёт 3 с,
делает до `HROMATITE_TRIES` (по умолчанию 3) попыток инжекта с паузой 12 с.

Переменные:

```
HROMATITE_DISABLE=1    — чистый запуск без инжекта
HROMATITE_MODE=proton  — путь через `proton run` (вне контейнера)
HROMATITE_TRIES=n      — число попыток
```

Режим по умолчанию — **attach**: env берётся из `/proc/<game-pid>/environ`
(`WINEPREFIX`, `WINEDLLPATH`, `WINEDLLOVERRIDES`, `LD_LIBRARY_PATH`, `PATH`, …),
значения экранируются (в `WINEDLLOVERRIDES` есть `;`), и запускается `wine`
напрямую, **без** повторного `proton run`.

**Проверено на живой игре (2026-11-14, ~07:14):** attach внутри pressure-vessel
контейнера работает, `proton run` не понадобился (fallback не сработал):

```
[hromatite] game wine pid 79549 after 8s
[hromatite] inject attempt 1/3 (mode attach)
[hromatite] attach env: prefix=…/compatdata/438100/pfx/ wine=…/files/bin/wine
target pid 440 … erase_headers=true, stomp=None
[dll] DllMain(PROCESS_ATTACH), inline (no extra thread)
manualmap: … mapped at 0x6d770000 (relocs 1955, rx sections 1, tls 2, headers_erased true)
```

Вне контейнера тот же attach падает (`/lib/ld-linux.so.2: could not open` — нет
32-битного лоадера); wrapper ловит это и откатывается на `proton run`.

### 4.5 Доставка через лоадер игры (`--mode loadlib`, L1.5a)

`manualmap::inject_loadlib`: `CreateRemoteThread(kernel32!LoadLibraryW)`, путь к
DLL кладётся в чужой процесс. Плюсы: страницы **file-backed** (`/proc/<pid>/maps`
показывает файл, а не анонимный регион), секции `r-xp`, релокации/импорты делает
сам загрузчик. Минус: модуль попадает в PEB → payload вычёркивает себя
(`procs::unlink_from_peb`).

Проверено на подставном exe: `DllMain(PROCESS_ATTACH)` исполняется, `hinst` в
`DllMain` равен базе модуля. `peb unlink` в первом прогоне был `false`
(неверные оффсеты), после правки — нужен повторный прогон.

---

## 5. Wine/Proton: почему «извне» не работает

`wine` кладёт сокет сервера в `/tmp/.wine-<uid>/server-<dev>-<ino>`, где `<ino>` —
inode каталога `$WINEPREFIX`. Внутри pressure-vessel inode префикса другой, поэтому:

- игра (в контейнере): `/tmp/.wine-1000/server-27-783de1`
- наш `proton run`/`protontricks-launch` снаружи: `.../server-27-a14223`

→ **разные wineserver'ы** → `Toolhelp32` в нашем процессе видит только сам себя
(проверено: `Hromatite.exe bridge` → «VRChat.exe not found», хотя игра работала).

Вторая отдельная проблема: **второй `proton run` по тому же префиксу из другого
шелла зависает** (proton держит lock, пока жив его первый `run`). Рабочий рецепт
для тестов: оба `proton run` — в одном shell-вызове, либо убить первый
proton-процесс (python-обёртку), оставив живыми wineserver и игру.

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

Реализовано в `crates/hromatite-core/src/il2cpp.rs`:

1. `locate_names` — **один проход** по образу `UnityPlayer.dll`: собираем
   ASCII-раны (`[A-Za-z0-9_]`, длина 4..32) и сверяем с `HashMap` из
   `symbols::EXPORTS`. Раньше было 251 отдельных прохода по 34 МБ → в debug
   не укладывались в 60 с, и стаб-поток убивался по таймауту.
2. Для каждого найденного имени — ищем слоты, держащие указатель на строку
   (8-байтовое LE = VA строки).
3. Около слота ищем значение в диапазоне `GameAssembly` (`base..base+SizeOfImage`),
   кандидатные смещения `DELTAS = [8, 16, 24, -8, 32, 40]`, побеждает смещение
   с максимумом попаданий (голосование по layout приватной структуры).
4. Итог: `Slot { name, name_off, addr_off, addr }` — эмпирическая карта
   «обфусцированное имя → реальный адрес функции».
5. `getter_candidates`: короткие геттеры-синглтоны (`48 8B 05 … C3` / `48 8D 05 … C3`)
   — оттуда берём `il2cpp_domain_get` и прочие «получить глобал».
6. TODO (следующий шаг): вызвать `il2cpp_domain_get`,
   `il2cpp_domain_get_assemblies` → `count > 1`, затем
   `il2cpp_class_from_name(img, "UnityEngine", "Camera")`.

Критерий готовности M2: в логе `diag: export table: N slots resolved` (N ≥ 200),
`il2cpp_domain_get` стабилен и не NULL, `domain_get_assemblies > 1`.

Альтернативы, если п.1–3 не сработают на живом процессе:
- читать таблицу резолвнутых указателей, которую заполняет сам Unity;
- паттерн-скан по `.text` GameAssembly + версия-гейт;
- брать `il2cpp_*` из in-memory метадаты (она расшифрована после `il2cpp_init`).

---

## 7. Скрытность: лестница и измерения

### 7.1 Уровни

| Уровень | Что | Статус |
|---|---|---|
| L0 | manual map в анонимный RWX, заголовки на месте, свой поток | было, выкинуло EAC |
| **L1** | `RW→RX`, стёртые заголовки, один стаб-поток, payload inline | ✅ реализовано, подтверждено логом живой игры |
| **L1.5a** | `--mode loadlib`: file-backed образ (нет анонимного exec-региона) | ✅ работает (DllMain исполняется) |
| **L1.5b** | PEB unlink: вычерк из `InMemoryOrderModuleList` | ⚠️ оффсеты исправлены, подтверждения нет |
| L1.5c | stomp: образ внутри памяти уже загруженного модуля | ⚠️ хвостовой slack 2–4 КБ, нужен другой подход |
| **L2** | «двойная память»: pristine-копия + рабочая зашифрованная (RW, не exec), расшифровка на время вызова, XOR обратно; опционально `PAGE_GUARD`+VEH | ❌ спроектировано (§7.4) |
| **L3** | ноль своих потоков: логика в существующем потоке игры (hook `PlayerLoop`/`Present`), инжект через APC | ⚠️ частично: `DllMain` inline, поток один |

### 7.2 Почему именно так

Под Linux EAC читает память с хоста (`/proc/<pid>/mem`, `process_vm_readv`), т.е.
**обходя page-протекции**. Поэтому «execute-only» страницы не спасают; спасают:
file-backed память (loadlib/stomp), отсутствие RWX, отсутствие анонимных
exec-регионов и шифр в памяти в покое (L2).

### 7.3 Замеры

```
debug   hromatite.dll  6 104 946 Б   — никуда не влезает, скан медленный
release hromatite.dll  1 293 496 Б   — быстрее, но slack модулей 2–4 КБ всё равно мал
slack:  baselib 0x96c, UnityPlayer 0x7c8, GameAssembly 0xbcc, VRChat.exe 0x9a8
→ "stomp: not enough slack, falling back to a fresh allocation"
```

Варианты stomp: (a) `--release` + `strip` + `opt-level="z"` — уменьшить образ;
(b) перезапись секции, мёртвой после загрузки (`UnityPlayer.dll:.reloc` —
релокации уже применены), с риском для file-hash проверок; (c) свой регион, но
file-backed через `NtCreateSection`/`NtMapViewOfSection` (SEC_IMAGE) — самый
«правильный» путь, требует удалённых вызовов ntdll.

### 7.4 L2 «двойная память» — проект (когда дойдём)

1. `pristine` — копия образа как есть (её и должен видеть любой скан; лучше
   file-backed через loadlib или SEC_IMAGE).
2. `working` — страницы с кодом, зашифрованные XOR-ключом постранично,
   `PAGE_READWRITE` (не executable).
3. Исполнение: стаб расшифровывает нужную страницу → `PAGE_EXECUTE_READ` → вызов →
   XOR обратно → `RW`. Между вызовами в памяти шифр, не PE, не код.
4. Опционально: `PAGE_GUARD` на pristine + VEH, чтобы замечать чтение снаружи и
   гарантированно отдавать «чистую» картинку.

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

Важно: игру можно запускать с `--no-eac` (в логах видели
`S:\steamapps\common\VRChat\VRChat.exe --no-vr --no-eac`).

### 8.2 Векторы детекта, применимые к internal-читу

Источники: `girther9399-gif/EAC-REVERSED-WRITEUP` (анализ драйвера EAC),
`aftermathlabs.net/blog/10/08/2021` (EAC мапит собственный модуль в игру),
UC-сниппеты (сам форум закрыт Cloudflare).

1. **VAD/private + executable + без file-backing** → «manual map» → скан региона.
   (Linux-аналог: анонимный `rwxp`/`r-xp` регион в `/proc/<pid>/maps`.)
2. **RWX** — флаг сам по себе, даже без найденного PE.
3. **Скан на PE в приватной памяти**: границы секций, массивы IAT, энтропия `.text`.
4. **Потоки**: `PsSetCreateThreadNotifyRoutine` → start address вне известных модулей
   = подозрительный поток. (Сейчас: один стаб-поток, payload inline.)
5. **Дерево процессов / хендлы**: лишний процесс с открытыми хендлами к игре.
6. **Целостность установки** (error 16): проверка файлов игры, а не памяти.

### 8.3 EAC Linux v2 (дедлайн проекта)

Анонс Epic 2026-10-14 (`onlineservices.epicgames.com/news/introducing-easy-anti-cheat-linux-v2`):

- без kernel-модуля, но: **systemd-сервис + eBPF + LSM BPF**;
- цитата: *«This enables anti-cheat to restrict other processes from reading or
  writing protected game process memory without needing a kernel module»* —
  приговор внешнему ридеру (вариант C);
- требования: подписанное ядро из well-known списка (Canonical, Fedora, Universal
  Blue, Debian, openSUSE, SUSE, Rocky, MS UEFI CA, Alma — **NixOS нет**), TPM 2.0,
  Secure Boot, measured boot, IOMMU, kernel lockdown integrity, kexec off, LSM BPF;
- Steam Deck официально не поддерживается;
- разработчик игры сам выбирает v1 или v2; пилот сначала в Fortnite;
- фичреквест VRChat «не включайте v2 на Linux» (14.10.2026, 41+ голосов, ответа нет).

Вывод: на NixOS v2-сессия скорее всего не пройдёт аттестацию (нет ключа NixOS) —
риск не «нас поймают», а «игра перестанет запускаться». Горизонт — месяцы.

### 8.4 Инцидент: `Disconnected` / «Anti-Cheat failed to verify installation integrity» (error 16)

Было: игра запущена через wrapper, инжект прошёл (`mapped at 0x6d770000`,
DllMain отработал), через несколько минут — кик.

Подозреваемые (не изолировано):

1. **Второй `proton run` внутри живой сессии** — Proton заново раскладывал файлы в
   префикс; в логе: `Proton: Error while copying to ".../system32/nvngx_update.exe":
   No such file or directory`. Это ровно «installation integrity».
2. **Детект памяти** — анонимный RWX + два потока со стартом в анонимной памяти
   (тогда был режим L0).

План изоляции (A/B, требует перезапусков игры):

- A: `HROMATITE_DISABLE=1` через wrapper → если чисто, wrapper безвреден;
- B: wrapper + attach-инжект (без `proton run`, без RWX, без лишних потоков) → если
  чисто, виноват был `proton run`; если кик, виноват детект памяти;
- C: `--no-eac` контроль (EAC выключен).

Частично проверено: последний запуск (attach-инжект, L1) прошёл без немедленного
кика; длительного наблюдения не было.

---

## 9. milestone-план и текущий фокус

| M | Что | Статус |
|---|---|---|
| M0a | `ptrace_scope=0` в nixos-config (+ protontricks) | правка сделана, rebuild не выполнен |
| M0b | «мостик»: инжектор видит процесс игры | ✅ (в одной wine-сессии) |
| M1 | manual map + hello-world | ✅ |
| M1.5 | L1 / L1.5a / L3 скрытность | ✅ код, подтверждено в живой игре (кроме PEB unlink) |
| M2 | резолв таблицы экспортов + `il2cpp_domain_get` | ⚠️ код ускорен, нужен лог из игры |
| M3 | read-only телеметрия: локальный игрок, список игроков, позиции → JSON | ❌ |
| M4 | визуализация (TUI или X11-оверлей) без записи в игру | ❌ |
| M5 | запись в игру (FOV/timeScale/позиция) — только с явного согласия | ❌ |
| L2 | двойная память (shadow pages) | ❌ спроектировано (§7.4) |

Текущий фокус: release-сборка готова → нужен запуск игры с wrapper'ом, чтобы снять
`diag:` (M2) и подтвердить PEB unlink.

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
grep -E "inject|stomp|manualmap|DllMain|mapped|modules ready|peb" $L | tail -20
grep "^diag:" $L | tail -30          # M2: слоты, геттеры, hexdump
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

1. **`CreateToolhelp32Snapshot` иногда висит минуты** (в логе разрыв ~390 с между
   заголовком инжекта и «target pid»). Обход: `--pid N` (без Toolhelp) или
   `/proc/*/cmdline` на Linux-стороне (так делает wrapper).
2. **Второй `proton run` по тому же префиксу из другого шелла зависает** (lock):
   оба `proton run` должны быть в одном shell-вызове, либо первый proton-процесс
   надо убить, оставив wineserver+игру.
3. **`proton run` из хоста создаёт отдельный wineserver** (§5).
4. **Вне SLR-контейнера `wine` не запускает даже `cmd`**: `/lib/ld-linux.so.2:
   could not open` (32-битный лоадер есть только в Steam runtime).
5. **`stdout` не флашится** при `std::process::exit` — исправлено в `main.rs`.
6. **Экранирование env в shell** — `WINEDLLOVERRIDES` содержит `;`: без
   single-quote-обёртки `export` ломается (исправлено в wrapper'е).
7. **debug-DLL 6.1 МБ и медленный скан** — использовать `--release`.
8. **`pkill -f pattern`** убивает собственную оболочку, если паттерн есть в её
   командной строке; использовать `[p]attern`.
9. **`inject_loadlib` возвращает `STILL_ACTIVE` (0x103)** вместо базы, если поток
   ещё работает в `DllMain` — TODO: считать базой только значения > `0x10000`.
10. **PEB unlink в wine не подтверждён**; при отладке полезен лог
    `peb: walked N entries, self base … not found`.

---

## 13. Открытые вопросы / ожидания

1. Снять `diag:` из живой игры (нужен запуск с wrapper'ом) → закрыть M2.
2. Подтвердить PEB unlink в wine (после правки оффсетов).
3. Чем стомпить образ 1.2 МБ: release+strip, перезапись `.reloc` или SEC_IMAGE.
4. Нужен ли M4-оверлей (X11) или достаточно TUI/лога.
5. Изолировать причину error 16 (A/B/C из §8.4).

---

## 14. Конвенции работы агента в этом репозитории

- Пользователь — русскоязычный; план → согласование (нумерованные 1/2/3/4) →
  реализация → короткий отчёт с фактами из логов.
- **Коммит на каждое изменение** (git; remote не настроен, коммиты локальные).
- **Каждое завершение дописывать в §16** этого файла (правило пользователя).
- Не трогать файлы игры: доставка — launch wrapper (или DLL-прокси по согласованию).
- Маппер тестировать на `crates/test-target` в отдельном префиксе
  (`STEAM_COMPAT_DATA_PATH=/…/scratch/pmtestN`), не на живой игре.
- Любое утверждение о поведении EAC/il2cpp подкреплять логом/файлом; иначе —
  помечать как гипотезу.
- После обновления VRChat — перегенерировать `symbols.rs` и зафиксировать sha256.

---

## 15. Источники

- Epic, EAC Linux v2: <https://onlineservices.epicgames.com/news/introducing-easy-anti-cheat-linux-v2>
- Разбор детекта EAC (VAD, RWX, PE-in-private, потоки): <https://github.com/girther9399-gif/EAC-REVERSED-WRITEUP>
  (`detection_methods.md`, `internal_cheats_and_injectors.md`, `usermode_techniques.md`)
- EAC self-integrity bypass (драйвер): <https://github.com/gmh5225/Bypassing-EasyAntiCheat-Integrity-check>
- EAC инжектит свой модуль в игру: <https://aftermathlabs.net/blog/10/08/2021/>
- Module stomping: <https://github.com/MaorSabag/NaX/blob/main/wiki/Module-Stomping.md>
- VRChat фичреквест «не включайте EAC v2 на Linux»:
  <https://feedback.vrchat.com/feature-requests/p/do-not-require-easyanticheat-v2-for-linux-secure-boot-tpm-attestation-root-lockd>
- VRChat IL2CPP реверс (другой билд, только для перекрёстных проверок имён):
  <https://github.com/dwgx/vrchat-il2cpp-re>
- Порт мода на Unity 6 (наблюдения по обфускации/метадате):
  <https://github.com/kawaiistudio/VRChatArchiveMod/blob/main/docs/UNITY6-PORT.md>

---

## 16. Журнал работ (дополнять после каждого завершения)

Формат записи: `### YYYY-MM-DD — <кратко>`, затем: коммиты, что сделано, чем
подтверждено (лог/факт), что осталось. Ничего не удалять — только дописывать снизу.

### 2026-11-14 — Разведка билда и каркас проекта

- Коммит `eb46af3`: workspace (`hromatite-core`, `hromatite-dll`, `test-target`),
  `tools/symmap.py`, генерация `symbols.rs` (251 экспорт, sha256 `648f61b1…`).
- Подтверждено: Unity 6000.0.67f1, метадата зашифрована, 234–235 имён экспортов
  лежат блоком в `.rdata` `UnityPlayer.dll`, порядок не совпадает с таблицей
  Unity 2020 (якоря 71↔83, 128↔116).
- Manual map прогнан на подставном `VRChat.exe`: релокации, импорты
  (forwarder'ы + apiset), TLS-колбэки, entry через x64-стаб — работает.

### 2026-11-14 — Доставка в живую игру, скрытность L1/L3

- Коммиты `78b4dad`, `9b7ece2`.
- Wrapper с ожиданием `VRChat.exe` и ретраями; attach-режим (env игры из
  `/proc/<pid>/environ`): внутри pressure-vessel работает, вне его откатывается
  на `proton run`.
- L1: `RW→RX`, зануление PE-заголовков; L3: TLS+entry одним стаб-потоком,
  payload inline в `DllMain`. Подтверждено логом живой игры:
  `rx sections 1, tls 2, headers_erased true`, `mapped at 0x6d770000`.
- Инцидент: EAC выкинул с error 16 («installation integrity») — подозреваются
  повторный `proton run` (копирование в `system32`) и/или детект памяти (§8.4).

### 2026-11-14 — Документация

- Коммит `2efe872`: создан этот файл (15 разделов).

### 2026-11-14 — L1.5 (loadlib + PEB unlink) и ускорение скана

- Коммит `0717d6a`.
- `--mode loadlib` (лоадер игры: file-backed страницы) + `procs::unlink_from_peb`
  (оффсеты исправлены: `DllBase = entry+0x20`), CLI `--pid`, команда `mods`,
  стенд `tools/test-loadlib.sh`.
- `il2cpp::locate_names` — один проход вместо 251 (debug не укладывался в 60 с).
- Замеры slack: `baselib 0x96c`, `UnityPlayer 0x7c8`, `GameAssembly 0xbcc`,
  `VRChat.exe 0x9a8`; release-DLL 1 293 496 Б → хвостового stomp недостаточно.
- Не подтверждено: PEB unlink в wine, `diag:` из живой игры (нужен перезапуск
  с release-DLL).

### 2026-11-14 — M2: скан всего адресного пространства; L2: `--seal` (шифр в покое) в живой игре

- Коммиты `69c59fa`, `8ead986`.
- **M2, почему прошлый прогон дал `0 slots`.** Оффлайн по файлу `UnityPlayer.dll`:
  235 имён лежат в `.rdata` пулом строк (`0x1ba11d8`…`0x1ca01b9`, зазоры 0x38–0x80,
  нули между ними), но **ни одно из 4 243 509 выровненных 8-байтовых слов образа не
  равно `ImageBase+RVA(имя)`** → таблица «имя → адрес» строится в рантайме вне образа.
  Старый код искал указатели только внутри образа ⇒ пусто.
- Новый `il2cpp::resolve`: один проход по **всем читаемым регионам процесса**
  (`VirtualQuery`, `procs::regions`), два оракула: (1) слоты с указателем на строку
  имени, (2) непрерывные массивы указателей в GameAssembly. Замеры в живой игре:
  `names: 235/251 (236 occurrences), regions 759, scanned 1560 MB in 2.5s`,
  `name pointers: 236`, но `delta votes [0…0]` — рядом с указателем на имя **нет**
  адреса в GameAssembly (ни ±0x80); значит таблица не `{name, fn}`. Самые длинные
  «раны» — 430 976 / 165 113 слов (это метадата il2cpp, а не таблица API), поэтому
  oracle 2 теперь берёт ран, длина которого ближе к числу имён.
- **L2 (`--seal`) проверен в живой игре.** После возврата payload'а в стаб-поток
  инжектор XOR'ит все страницы образа и оставляет `PAGE_READWRITE`. Лог:
  `seal: 236 pages XORed, 0 failed, size 0xec000` (до: нули — заголовки стёрты;
  после: `c6 2f ed 2c …`). С хоста, по `/proc/<pid>/maps`:
  `6bdd0000-6bebc000 rw-p 00000000 00:00 0` — одна анонимная RW-область, **ни одной
  exec-страницы**, первые 16 байт = `c6 2f ed 2c 76 06 ec 7b 89 6e 68 a4 39 1b 9f f8`.
  Оговорки: (а) seal делается после `WaitForSingleObject`, ожидание увеличено до
  600 с и пропускается при `STILL_ACTIVE`; (б) в loadlib-режиме модуль остаётся в
  загрузчике и thread-detach вызовет `DllMain` из зашифрованного образа.
- Wrapper переведён на release-бинарники и передаёт `--seal` (`HROMATITE_SEAL=0` для A/B).
- Стенд: `tools/test-seal.sh`; вне SLR-контейнера `proton run` второго процесса
  висит/молчит, а `wine` падает на `/lib/ld-linux.so.2` — проверять только из игры.
- Осталось: (1) снять окрестности слотов (в лог добавлен дамп ±0x20/0x40) и понять
  реальный layout таблицы; (2) перезапуск игры для нового `diag:`; (3) L2b —
  pristine-копия/`PAGE_GUARD`+VEH и расшифровка на время вызова.

### 2026-11-14 — Правило журнала + актуализация документа

- Дописан §16 и правило «каждое завершение — сюда»; обновлены TL;DR, замеры
  slack, статусы L1.5a/L1.5b, добавлен §4.5 (loadlib), §7.4 (проект L2),
  баги 9–10 (STILL_ACTIVE, PEB unlink), CLI-флаги.
