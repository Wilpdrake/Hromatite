# Допуск EAC: как игра решает, что «anti-cheat client доступен»

Дата: 2026-11-18. Ветка `cut-admission` (worktree `~/RustroverProjects/cut-a1`).
Метод: статика (Ghidra 12.1.2 через MCP-проект `/11.18`, capstone-скан, чтение строк/логов).
Игра агентом не запускалась.

Теги: **[П]** — проверено инструментом (адрес/байты/лог в тексте), **[Г]** — гипотеза,
**[Н]** — не проверено.

---

## 0. Вердикт коротко

1. **Локальная проверка «anti-cheat client доступен» — это не named object, не секция,
   не пайп и не файл.** Это **рукопожатие указателем на функцию**: EOSSDK зовёт
   `GetProcAddress((HMODULE)0xE08, L"")` и, если получила непустой указатель, вызывает
   его как «провайдера» анти-чита, передавая структуру запроса и 0xF0-байтный выход.
   Если указателя нет — возвращается `12000` (`EOS_AntiCheat_ClientProtectionNotAvailable`).
   **[П]** (декомпиляция `FUN_1803BE4D0`, см. §3).
2. **Обойти локальную проверку без модуля — да, можно** (и это уже сделано штатно:
   `EOS_USE_ANTICHEATCLIENTNULL=1` переводит игру на «null client», `BeginSession` →
   `Success`, вход в мир). **Но это не даёт допуска**: сервер выкидывает через ~3 с, потому
   что нет 512-байтных integrity-отчётов. Подмена провайдера даёт ровно тот же результат,
   что и env-переменная, только «нормальной» веткой SDK — **[П]** по коду + **[П]** по
   прошлым прогонам (`eac_off.md`, трек C: 193 с на настоящем реплее).
3. **Вердикт по вопросу задачи:** satisfiable без модуля — **для локального гейта да, для
   сессии нет**. Единственный путь к живой сессии — оставить модуль генератором отчётов
   (трек B), потому что содержимое отчётов не воспроизводится (доказано: 193 с).

---

## 1. Кто есть кто (уточнение постановки)

| файл | размер | что это | проверено |
|---|---|---|---|
| `start_protected_game.exe` | **3 993 896** | **боотстраппер** (в задаче назван «EasyAntiCheat_EOS.exe»); печатает `Starting Wine module mapping…`, `Launcher finished with: 301`, `Terminating with: 0` | **[П]** `ls -l`, строки в файле |
| `EasyAntiCheat/EasyAntiCheat_EOS_Setup.exe` | 985 896 | установщик/сервис-хост (PE32) | **[П]** |
| `decoded/blob2_usermode.exe` | 1 041 336 | сервис `EasyAntiCheat_EOS.exe` (байт-в-байт равен одноимённому на диске Windows) | **[П]** `module.md` §3 |
| кэш `easyanticheat_linux_x64.eac` | **12 015 192** | под Proton качается **linux64**-модуль (system `linux64`), sha256 `25ce4cd3…` | **[П]** лог + `sha256sum` кэша |
| `stage1_module.so` | 12 015 192 | он же после stage-1 декода; экспорты `a..e` | **[П]** `linux64.md` |
| `eac_module_memory.so` | **6 651 656** | **живой** in-game клиент из `/memfd:` в процессе игры | **[П]** AGENTS §17, 2026-11-18 |

**Новое (этого не было ни в одном прежнем findings):** in-game клиент (6 651 656 Б) **лежит
внутри** `stage1_module.so` — в `.rodata` с файлового смещения **0x23BDA0**, длиной ровно
6 651 656 Б, закрытый тем же difference-кодеком EAC (`EncryptModule`). **[П]**

```
decode(stage1[0x23BDA0 : 0x23BDA0+6651656])  ==  eac_module_memory.so
sha256 = 2e5405f903203f0d…  (совпал полностью, не только заголовок)
```
Тонкость: финальный шаг кодека `M[0] -= M[1]` применять **не нужно** (иначе первый байт
`0x3A` вместо `0x7F`); остальное — как в `tools/eac_module_decode.py:decrypt_module`.
Раньше этот участок `.rodata` описывался как «нерешённые ramp-регионы» (`linux64.md` §8).

---

## 2. Вопрос 1: что делает боотстраппер на Wine и как ELF попадает в игру

### 2.1 Что печатает (verbatim из `anticheatlauncher.log`, 2026-11-18 04:59)

```
[04:59:10:560] System name: 'linux64'.
[04:59:10:685] Connecting to URL: https://modules-cdn.eac-prod.on.epicgames.com/modules/<pid>/<depid>/linux64
[04:59:11:553] Connect result: No error (0) Response Code: 304 … No update needed.
[04:59:11:612] Starting Wine module mapping, Wine version: 11.0.
[04:59:11:763] Launcher finished with: 301, 'Easy Anti-Cheat successfully loaded in-game'.
[04:59:11:763] Successfully initialized the Easy Anti-Cheat module, waiting for game window to become visible.
[04:59:11:795] Terminating with: 0.
```
**[П]** файл: `<prefix>/drive_c/users/steamuser/AppData/Roaming/EasyAntiCheat/<pid>/<depid>/anticheatlauncher.log`.

Между «Starting Wine module mapping» и «success» — **151 мс**, окна игры ещё нет. Значит
«success» — это не «игра увидела модуль», а **собственный return-code модуля** (301 =
`launcher_error.success_loaded`), который вернулся после того, как модуль сам запустил игру
и убедился, что попал в неё.

### 2.2 Кто печатает и где код

| строка | файловое смещение | RVA | где используется |
|---|---|---|---|
| `Starting Wine module mapping, Wine version: %s.` | 0x35C8F8 | 0x35D4F8 | `FUN_140035300` (RVA 0x35300), Wine-ветка |
| `Failed to map the anti-cheat module.` | 0x35C958 | 0x35D558 | там же, обработка NULL от маппера |
| `Launcher finished with: %u, '%s'.` | 0x35CEE8 | 0x35DAE8 | общий выход лаунчера |
| `Terminating with: %i.` | 0x35DDD0 | 0x35E9D0 | `main` лаунчера |
**[П]** (строки + xref-скан `tools/admission_xref*.py`)

`FUN_140035300` (проверено ранее, `loader_driver.md` §B.1–B.2, декомпиляция в
`~/EAC-RE/ghidra-loader2/dec1.txt`):

```
cVar1 = isWine();                       // FUN_14001cf80
if (!isWine) { ... ordinal-путь (1,2,3,5), in-memory PE mapper FUN_140016280 ... }
else {
    log("Starting Wine module mapping, Wine version: %s.", wine_version);
    lVar2 = FUN_14001d9f0(param_1);      // <-- вот он, Wine-маппер
    if (!lVar2) log("Failed to map the anti-cheat module.");
    *param_2 = dlsym(lVar2, "a"); *param_3 = dlsym(lVar2,"b");
    *param_4 = dlsym(lVar2,"c"); *param_5 = dlsym(lVar2,"d");
}
```
**[П]** (декомпиляция + байты в `dec1.txt`, строки `a/b/c/d` по RVA 0x35D550…0x35CDD8).

### 2.3 `FUN_14001D9F0` — собственно маппинг (RVA 0x1D9F0)

Полная декомпиляция (`dec1.txt`, строки 562–646) в переводе на псевдокод:

```c
void* wine_map_module(struct{void*begin; void*end;}* blob) {
    if (blob->begin == blob->end) return 0;
    // оба вызова — indirect через __guard_dispatch_icall, коды 0x13F и 1:
    idx = dispatch(0x13F, &local_88, 2);                         // создать объект, вернуть индекс
    if ((int)idx < 0) return 0;
    n   = dispatch(1, idx, blob->begin, blob->end - blob->begin);// записать ELF целиком
    if (n != len) return 0;
    sprintf(path, 0x40, "/proc/self/fd/%i", idx);                // <-- fd-путь
    if (!dlopen_ptr) {                                           // ленивый резолв
        libc = get_native_libc();                                // FUN_14001d5b0
        dlopen_ptr = GetProcAddress(libc, "__libc_dlopen_mode")
                  ?: GetProcAddress(libc, "dlopen");
    }
    return dlopen_ptr(path, 1 /*RTLD_LAZY*/);                    // FUN_14004bf36 — обёртка вызова
}
```
**[П]** — адреса и строки: `/proc/self/fd/%i` @RVA 0x35C4E0 (xref 0x1DACE),
`dlopen` @0x35C46C, `__libc_dlopen_mode` @0x35C480, `dlsym` @0x35C494,
`/libc-` @0x35C4B8, `libc.so` @0x35C4B0 — все **[П]**.
Механика ровно та же, что в публичных описаниях «PE грузит ELF через memfd + dlopen».

Вспомогательное:
* Wine определяется через `GetModuleHandleW(L"kernel32.dll")` +
  `GetProcAddress(…, "wine_get_unix_file_name")` — **[П]** (`0x1D995`–`0x1D9BC`);
* `wine_get_version` @0x35C440, `wine_get_host_version` @0x35C510, `wine_get_unix_file_name`
  @0x35C4F8 — **[П]**;
* `"/tmp/eac_wine_pid_%X"` @0x35C528 и `"/proc/%i/maps"` @0x35C540 — **[П]** (строки есть,
  xref на `/proc/%i/maps` — `0x1E1E2`).

### 2.4 Как ELF попадает **в игру** (а не только в лаунчер)

Твёрдо установлено:
* лаунчер делает `dlopen("/proc/self/fd/N")` **в своём процессе** — это 12 МБ `stage1`; **[П]**
* в процессе игры лежит **другой** файл — 6 651 656 Б, `/memfd:… (deleted)`,
  `/proc/<pid>/fd/3` (inode совпадал с `/proc/<pid>/maps`), база `0x7081edfa1000`; **[П]**
* этот файл **байт-в-байт** равен второму стейджу из `.rodata` `stage1` (§1). **[П]**

**[Г] (сильная, но не доказанная)** канал доставки второго стейджа в игру:
`memfd` создаёт `stage1` (в живом втором стейдже есть строка `memfd`, смещение 0x3658), пишет туда
6.65 МБ, **снимает `FD_CLOEXEC`**, формирует `LD_PRELOAD=/proc/self/fd/<N>` и запускает игру
через `CreateProcess`; `ld.so` игры сам мапит модуль на старте (отсюда file-backed страницы и
живой fd 3). Строки в пользу этого: `/proc/self/fd/%i` в лаунчере, `LD_PRELOAD` @0x23BA82 и
`easyanticheat_linux_x64.eac.trampoline.tmp` @0x28FA в `stage1`, а также `/proc/%i/maps`,
которым удобно **проверить**, что модуль действительно появился в карте игры (151 мс — как раз
покрывают `CreateProcess` + старт `ld.so`, но не загрузку Unity).
**[Н]** прямого подтверждения (`/proc/<gamepid>/environ` при живом запуске) пока нет.

Альтернатива, которую нельзя исключить: запись ELF в чужое адресное пространство через
`/proc/<pid>/mem` + запуск его инициализатора. Отличается одним наблюдением: если в
`environ` игры есть `LD_PRELOAD=/proc/self/fd/…`, верна первая версия.

---

## 3. Вопрос 2: как EOSSDK решает, что клиент «доступен»

Все адреса — VA, база `0x180000000`, PE `EOSSDK-Win64-Shipping.dll`
(sha256 `ab25af35…`, 19 548 600 Б).

### 3.1 Три строки и три логгера

| строка | VA | xrefs | логгер |
|---|---|---|---|
| `[AntiCheatClient] Anti-cheat client not available, using null client` | 0x180F7C950 | 0x1803BAFF4, 0x1803BB008 | `FUN_1803BAFF0` |
| `[AntiCheatClient] Anti-cheat client not available. Verify that the game was started using the anti-cheat bootstrapper…` | 0x180F7C9E0 | 0x1803BB024, 0x1803BB038 | `FUN_1803BB020` |
| `[AntiCheatClient] Anti-cheat client not available. Result code %i (%s).` | 0x180F7CB00 | `0x1803BB079/80` | `FUN_1803BB050` |

Все три — тонкие обёртки над `FUN_180AC36C0(0,0,&DAT_1811F8460,<level>,msg)` (логгер EOS). **[П]**

### 3.2 Решение принимает `FAntiCheatClient::Init` = `0x1803BAA70`

Декомпиляция (сокращённо, **[П]**):

```c
Init(client, opts) {
    ... скопировать 2 строки из opts в client+0xB0 / client+0xA0 ...
    FUN_1803BC380(client);
    status = client->vt[0xE8](client, opts, out = client-0x2F0);   // <-- ЗАПРОС МОДУЛЯ
    if (status == 0) return 1;                                     // модуль есть
    if (status != 12000) { if (level>1) FUN_1803BB050(status); return 0; }
    if (client->vt[0xD0](client) == 0) {                           // null не разрешён
        if (opts->vt[0x1A8]() == 0 && level>4) FUN_1803BB020();    // "Verify … bootstrapper"
        return 0;
    }
    if (level>2) FUN_1803BAFF0();                                  // "using null client"
    p = alloc(0x348); FUN_1803C0BB0(p); client[-0x200] = p;        // локальный null-сеанс
    return 1;
}
```
**[П]** — `cmp ebx,0x2EE0` @0x1803BAE77, вызовы слотов @0x1803BAE19 (`call r9`, где
`r9 = [[client]+0xE8]` @0x1803BADE6) и @0x1803BAE91 (`call [rax+0xD0]`).

### 3.3 Что такое слот `+0xE8` = `0x1803BE4D0` («провайдер»)

Ключевые куски декомпиляции (**[П]**):

```c
// шесть строк из объекта опций платформы
opts->vt[0x1E8](&s1); opts->vt[0x1F0](&s2); opts->vt[0x200](&s3);
opts->vt[0x208](&s4); opts->vt[0x1F8](&s5); opts->vt[0x1C8](&s6);

if (DAT_1811BECF8 == NULL)                    // кэш провайдера
    DAT_1811BECF8 = GetProcAddressAnsi((HMODULE)0xE08, L"");   // <== ВОТ ПРОВЕРКА
pcVar3 = DAT_1811BECF8;
if (pcVar3 == NULL) return 12000;             // EOS_AntiCheat_ClientProtectionNotAvailable

memset(out, 0, 0xF0);                          // out = client-0x2F0
req = { u32 2; u32 6; u32 0xF0; ... ; s1..s6; };
do {                                            // протокол "мало места -> повтори"
    rc = pcVar3(&req, out);
    ... селектор размера 0xD0/0xD8/0xE0/0xE8/0xF0 ...
} while (rc == 0x15 && --k > 1);
return rc;
```
Адреса: `mov ecx,0xE08` @0x1803BE5D1, `call 0x180B11C20` @0x1803BE5D6,
`mov [0x1811BECF8],rax` @0x1803BE5DE, `cmp pcVar3,0 / return 12000` @0x1803BEE…;
`GetProcAddress`-обёртка `FUN_180B11C20` (RVA 0xB11C20) — **[П]** (байты: `mov rsi,rcx`,
`call [KERNEL32!GetProcAddress]`), конвертирует wide-имя в ansi и зовёт `GetProcAddress`.
`L""` — это `0x180F7747C` (там 4 нулевых байта, далее `"false"`). **[П]**

Итог: **«модуль доступен» = «кто-то ответил на `GetProcAddress((HMODULE)0xE08, L"")`
непустым указателем и этот указатель, вызванный с нашим запросом, вернул 0»**. Никакой
криптографии, подписи или сверки версии в этой точке нет.

### 3.4 Что проверяется для «null client»

```c
bool IsNullClientForced(client) {                     // 0x1803BB250, слот +0xD0
    wchar_t name[] = L"EOS_USE_ANTICHEATCLIENTNULL";
    client->vt[0xC8](&val, name);                      // 0x1803BB230 -> 0x180B0F330
    return wcscmp(val, L"1") == 0;                     // L"1" @0x180F7CBF8
}
```
`FUN_180B0F330` — **[П]** декомпилирован: `GetEnvironmentVariableW(name, buf, cap)` в цикле
(двухпроходное чтение с реаллокацией). То есть это **переменная окружения процесса**, а не
файл, не секция и не опция платформы. Сравнение — с точной строкой `"1"` (без trim,
регистрозависимо).

### 3.5 Чего в этой проверке НЕТ (важные отрицания)

* **[П]** В `EOSSDK-Win64-Shipping.dll` **ноль** вхождений `EasyAntiCheat`/`easyanti`/`EAC_`
  в ASCII и UTF-16. Значит проверка не может быть «найти модуль/секцию по имени».
* **[П]** В AC-объекте SDK нет обращений к named-объектам: `eossdk.md` §7 — все 25 функций
  трогают только реестр нотификаций (`client+0x1C8`), сессию (`client+0x2E8`) и шифр;
  сокетов/`CreateThread`/`CreateFileMapping` в этом пути нет.
* **[П]** `EOS_AntiCheatClient_GetModuleBuildId` (`0x1803B8F30`) — **не** проверка, а геттер:
  `ApiVersion!=1 → 0xD`, иначе `client->vt[0x160]()` (провайдер → сессия).
* **[П]** Константа `12000` производится в двух местах: `mov eax,0x2EE0; ret` @0x1803C0F60
  (стаб «недоступно») и как возврат `FUN_1803BE4D0` при NULL-провайдере.

---

## 4. Вопрос 3: три пути и чем они отличаются

| | **модуль есть** (провайдер ответил) | **null client** (`12000` + env=1) | **модуля нет** (`12000`, env нет) |
|---|---|---|---|
| ветка `Init` | `status==0 → return 1` | `vt[0xD0]==1` → лог + `alloc(0x348)` | `vt[0xD0]==0` → лог + `return 0` |
| лог игры | — | `using null client` | `Verify that the game was started using the anti-cheat bootstrapper…` |
| объект сессии | ставит модуль (`client+0x2E8`) | локальный `FUN_1803C0BB0`, vt `0x180F75E58` | нет |
| интерфейс `EOS_Platform_GetAntiCheatClientInterface` | валиден | валиден | **NULL** |
| `BeginSession` | `Success`, Mode 1 | `Success`, Mode 1 | **не вызывается вообще** (0 AC-вызовов) |
| игровой результат | мир, сессия живёт | мир → кик ~3 с (`unable to establish or maintain … integrity`) | offline testing mode → `Error World` |
| исходящие кадры | 24/116/… + **512 Б каждые ~2007 мс** | только 24 Б и 116 Б | нет |

Источники: **[П]** код §3.2–3.4 + **[П]** прогоны `runA_eac_on` / `runB1_stub_only` /
`runB2_null_replay` (`eac_off.md` §2, §3).
Функция, которая возвращает «not available», — это `FUN_1803BE4D0` (слот `+0xE8`);
функция, которая решает, что с этим делать, — `FAntiCheatClient::Init` (`0x1803BAA70`).

---

## 5. Вопрос 4: можно ли удовлетворить проверку без модуля — честная оценка

### 5.1 Технически — да

Нужно ровно три вещи (все в своём процессе, наш payload это умеет):

1. **Перехватить `GetProcAddress`** (inline-хук или правка IAT EOSSDK) и на пару
   `(hModule==0xE08, name=="")` вернуть адрес нашей функции. **[Г]** — реализуемо, но
   требует, чтобы хук стоял **до** первого `Init` AC-интерфейса (то есть до создания
   EOS-платформы в игре).
2. **Наш «провайдер»** должен: заполнить 0xF0-байтный `out`, вернуть 0 (и корректно
   отработать код `0x15` = «дай больше»), иначе `Init` уйдёт в ветку ошибки.
3. **Поставить объект сессии** в `client+0x2E8`. Иначе первый же `Tick` пойдёт по
   `FUN_1803BEF30`: `if (client+0x2E8)==0 → jmp [client+0x200]`, а `+0x200` в
   «успешной» ветке **не заполняется** (его заполняет только null-ветка `Init`).
   То есть наивная подделка «успеха» даёт **переход по нулевому указателю** — краш игры,
   а не сессию. **[П]** по коду `0x1803BEF30` + `Init`.

### 5.2 Но это не даёт допуска

Чтобы сессия жила, нужен объект сессии, который **умеет производить 512-байтные сообщения**.
Единственный, кто их производит, — модуль. Альтернатива «взять null-объект» — это ровно то,
что уже делает `EOS_USE_ANTICHEATCLIENTNULL=1`, с тем же исходом (кик ~3 с).
Подмена провайдера даёт максимум переход «offline → join+кик», то есть **ничего нового**.

Численно (все **[П]**, из `EAC-REPLACE-PLAN.md` §8 и `eac_off.md`):

| сценарий | сколько живёт сессия |
|---|---|
| настоящий модуль | без ограничений (283 с и 6+ мин в контроле) |
| null client | ~3 с, далее 24–27 с циклами |
| реплей настоящих 512 Б (своя сессия, новый Sequence, живой blob, CRC-32C) | **193 с** |
| чужие кадры | 200–211 с |
| тишина | 120.5 с |

### 5.3 Что было бы нужно, чтобы «провайдер» действительно помог

Если сервер на входе сверяет **module build id / deployment id**, которые SDK получает из
0xF0-структуры провайдера, то подделка провайдера с *настоящими* значениями сдвинет отказ с
3 с на ~120–200 с (до момента, когда сервер поймёт, что отчётов нет). Проверяемо только
живым прогоном и только после реализации хука. **[Г]**

**Практический вывод:** локальный гейт — не стена. Стена — содержимое отчётов (трек B:
отключить «зрение» модуля, оставив его генератором).

---

## 6. Отрицательные результаты (что проверил и не нашёл)

1. **[П]** Три wide-строки «not available» имеют **только** по одной паре xref — на
   логгеры `FUN_1803BAFF0` / `FUN_1803BB020` / `FUN_1803BB050`. Никакая «решающая» функция
   их напрямую не грузит; решение принимается по коду возврата, а текст выбирается уровнем лога.
2. **[П]** В EOSSDK нет ни одной строки `EasyAntiCheat`/`easyanti`/`EAC_` — гипотеза
   «ищет named section/pipe/файл по имени» **опровергнута**.
3. **[П]** `EOS_USE_ANTICHEATCLIENTNULL` читается **только** через `GetEnvironmentVariableW`
   (`0x180B0F330`); в SDK одно вхождение строки. В лаунчере — своё вхождение (RVA 0x35C628)
   и своя ветка «Debug environment variable (%s) was present, launching with null client»
   (0x35C6E0) — то есть при выставленной переменной **лаунчер вообще не грузит модуль**;
   это объясняет `runB2`, где модуля не было.
4. **[П]** В лаунчере нет ни одного `CreateFileMappingW`/`MapViewOfFile`/named-pipe и ровно
   один `DeviceIoControl` (в SDL HID) — версия «передаёт модуль через named-секцию» **неверна
   для Wine-пути**; передача идёт через `memfd` + `dlopen`.
5. **[П]** В in-game клиенте (6.65 МБ ELF) **нет** строк `GetProcAddress`, `ntdll`,
   `kernel32`, `LdrGetProcedureAddress`, `PEB` — значит он не хукает Win32 API по именам;
   кто именно отвечает на sentinel `0xE08` — **[Н]**.
6. **[П]** В `blob3_client.dll` (win64 EAC-клиент) нет `cmp ecx,0xE08`; четыре
   `cmp edx,0xE08` лежат в VM-секции (17.5 МБ, энтропия 6.6) и к делу не относятся.
7. **[П]** `stage1_module.so` (12 МБ) **не** содержит отдельных ELF/MZ-артефактов, но
   содержит второй стейдж, закрытый **тем же difference-кодеком** — прежний поиск
   (`linux64.md` §4) его не находил, потому что искал магики, а не diff-поток.
8. **[П]** Ни `XDG`-переменных, ни файлов-маркеров в проверке нет: путь «модуль есть»
   полностью определяется ответом провайдера в памяти.

---

## 7. Сводка

| утверждение | статус |
|---|---|
| «anti-cheat client доступен» = ответ `GetProcAddress((HMODULE)0xE08, L"")` + вызов провайдера | **проверено** |
| нет провайдера → `12000` → `Init` возвращает 0 (нет интерфейса) → offline mode | **проверено** |
| `12000` + `EOS_USE_ANTICHEATCLIENTNULL=1` → локальный null-сеанс, `BeginSession` Success, кик ~3 с | **проверено** |
| проверка **не** использует named object/секцию/файл/имя модуля | **проверено** |
| подделка провайдера без объекта сессии → `jmp [client+0x200]` = NULL → краш | **проверено по коду**, живьём не проверялось |
| второй стейдж (6 651 656 Б) лежит в `stage1[0x23BDA0:+6 651 656]` под diff-кодеком | **проверено** (sha256 совпал) |
| доставка стейджа в игру = `LD_PRELOAD=/proc/self/fd/N` при `CreateProcess` | **гипотеза** |
| кто отвечает на sentinel `0xE08` (win64-клиент / ELF через IAT / сам лаунчер) | **не проверено** |
| сервер сверяет module build id из 0xF0-структуры на входе | **не проверено** |

---

## 8. Самые дешёвые следующие эксперименты (без запуска игры агентом)

1. **`/proc/<gamepid>/environ` + `/proc/<gamepid>/maps` при следующем обычном запуске**
   (пользователь запускает, агент только читает): если в `environ` есть
   `LD_PRELOAD=/proc/self/fd/N`, гипотеза §2.4 подтверждена; в `maps` видно, какой файл
   соответствует `memfd`-региону. Стоимость — ноль перезапусков.
2. **Тот же запуск: прочитать `[EOSSDK + 0x11BECF8]`** (кэш провайдера). Ненулевое значение
   при живом EAC ⇒ sentinel кто-то обслуживает; ноль ⇒ провайдер подставляется иначе
   (например, IAT правится до `Init`). Это одно чтение памяти из нашего payload.
3. **Статически (офлайн, без игры):** найти в `stage1_module.so` код, который пишет
   `LD_PRELOAD` (строка @0x23BA82 — прямых xref нет, значит ссылка идёт через таблицу
   строк/`getenv`-хелпер; искать по `setenv`/`putenv`-подобным PLT-вызовам и по строке
   `easyanticheat_linux_x64.eac.trampoline.tmp` @0x28FA).
4. **Статически:** в `blob3_client.dll` найти обработчик `GetProcAddress`/`LdrGetProcedureAddress`
   (у него нет таблицы импортов, всё через прямые syscall — искать перехват
   `LdrGetProcedureAddress` по паттернам сравнения `hModule` с малым magic и возврату
   указателя на таблицу экспортов). Если найдётся `0xE08` — вопрос «кто отвечает» закрыт.

---

## 9. Артефакты

```
~/EAC-RE/tools/admission_xref.py        # capstone-xref по строкам (медленный, полный)
~/EAC-RE/tools/admission_xref_fast.py   # numpy-вариант: --utf16/--ascii/--rva/--fileoff
~/EAC-RE/tools/ghidra_scripts/admission_XrefsTo.java  # xrefs + декомпиляция в headless Ghidra
```
Ghidra-программы брались из общего проекта `/11.18` (`EOSSDK-Win64-Shipping.dll`,
`stage1_module.so`); боотстраппер разбирался capstone'ом + ранее снятой декомпиляцией
`~/EAC-RE/ghidra-loader2/dec1.txt` (проект `ghidra-loader2` открывается только на чтение:
его `.gpr` нулевого размера, headless его не признаёт — обход: capstone).
