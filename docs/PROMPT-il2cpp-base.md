# Промпт для ИИ: «база» разбора Unity IL2CPP для Hromatite

## Роль и задача

Ты — senior Rust/RE-инженер. Тебе дают живой проект **Hromatite** (внутриигровой
payload + хостовые инструменты для VRChat под Proton/Wine на Linux). Твоя задача —
построить **базу разбора IL2CPP**: парсеры метаданных, деобфускатор экспортов,
каталог классов/методов, генераторы символов — то, на чём потом делаются фичи
(чтение игроков, позиции, оверлей). Не ищи «камеру» и не пиши читы: нужен
фундамент, покрытый тестами и воспроизводимый после каждого апдейта игры.

Отвечай по-русски. Код и комментарии — как в репозитории (имена и типы
английские, комментарии допустимы английские; пользовательская документация —
русская). Каждый логический шаг — отдельный git-коммит с сообщением-фактом.
Каждое завершение этапа — запись в `AGENTS.md` §17 (дополнять, не переписывать).

## Железные факты (проверены на этой машине, не перепроверяй с нуля, но и не
## считай догмой — если факт противоречит коду игры, верь коду и напиши об этом)

Окружение: NixOS 26.11, x86_64, 15 ГБ RAM. Rust 1.99, target
`x86_64-pc-windows-gnu` (mingw). Хост без `python3` — скрипты запускаются
`uv run --no-project python tools/<x>.py`.

Игра: VRChat, appid 438100, Unity **6000.0.67f1**, Proton Experimental,
SteamLinuxRuntime_4. Каталог игры:
`~/.local/share/Steam/steamapps/common/VRChat`, префикс:
`~/.local/share/Steam/steamapps/compatdata/438100/pfx`.

Артефакты игры: `GameAssembly.dll` 253 МБ (SizeOfImage 0x0f440000, **251 экспорт**,
имена обфусцированы Beebyte: `wYRnnMQuvvH`, `AQIKwyWT_pM`, …), `UnityPlayer.dll`
34 МБ, `VRChat_Data/il2cpp_data/Metadata/global-metadata.dat` 40.7 МБ —
**зашифрован** (магики `FAB11BAF` нет, первые байты `3a 08 77 f3`). Значит
офлайн Il2CppDumper/Cpp2IL по файлу бесполезны — нужен **дамп из памяти** после
`il2cpp_init` (в RAM метадата расшифрована).

Репозиторий: `/home/wilpdrake/RustroverProjects/Hromatite`
workspace: `crates/hromatite-core` (вся Win32/PE/il2cpp-механика, `pub mod` в
`lib.rs`), `crates/hromatite-dll` (payload, `DllMain`), `crates/test-target`
(подставной exe), `src/main.rs` (CLI `inject|bridge|procs|mods`),
`tools/*.py|*.sh`, документация — `AGENTS.md` (читать обязательно, там журнал §17
со всеми граблями).

Доставка уже работает: launch-wrapper `tools/hromatite-launch.sh` (Steam →
Свойства → Параметры запуска), manual map, скрытность L1/L2/L2b, VEH-longjmp
(паника/AV не убивают игру), remote-консоль через файл `hromatite-cmd.txt`
(команды `q b s find findb asm entry call guard wq wb tick threads gct fix …`).
Консоль стабильна только в режиме тика в потоке игры
(`~/.hromatite-mode`: `HROMATITE_FRAME_HOOK=1`, `HROMATITE_LIVE=0`,
`HROMATITE_SEAL=0`); в stub-thread режиме (`LIVE>0`) поток иногда умирает.

Что уже найдено (используй как стартовые точки, не выводи заново):
- `il2cpp_domain_get` = экспорт `wYRnnMQuvvH` (RVA `0xc27f00`), домен
  `0x3027c1c0`; по `domain+0x8` лежит массив **216** записей сборок (stride 0x48),
  там же имена и `entry+0x28` = `Il2CppCodeGenModule`.
- `Il2CppCodeGenModule` в Unity 6 **не содержит** `typeInfoTable` (поля кончаются
  `staticConstructorTypeIndices@0x70, metadataRegistration@0x78,
  codeRegistaration@0x80`) — проверено живым дампом.
- `Il2CppCodeRegistration.codeGenModules` найдена живьём: таблица 216 указателей
  в `.data` GameAssembly.
- `s_TypeInfoTable` (`libil2cpp/vm/GlobalMetadata.cpp`) — `IL2CPP_CALLOC(typesCount*8)`,
  разреженный массив `Il2CppClass*`, заполняется лениво.
- `MethodInfo` в Unity 6: `methodPointer@0x00, virtualMethodPointer@0x08,
  invoker_method@0x10, name@0x18, klass@0x20`.
- `Il2CppClass` по заголовку 6000.0.3f1: `image@0x00, name@0x10, namespaze@0x18,
  …, fields@0x80, methods@0x98` — **но у нашего билда (6000.0.67f1) это не
  подтверждается**: линейный проход по куче находит записи, где `+0x10` — строка
  `Camera`, но ни у одной нет таблиц полей/методов по этой раскладке.
- В `UnityPlayer.dll` есть **232 ленивых стаба** (`48 8D 15 disp; E8 rel32;
  48 89 05 disp; 48 85 C0 75 0E 32 DB 48 8D 0D`), ровно столько же имён в
  каноническом списке API (`~/Downloads/message.txt`, 232 записи) и ровно
  столько же шаблонов в `~/Downloads/Il2CppKnownPatterns.inl`. Это и есть
  офлайн-путь к деобфускации.

Известные грабли (не наступай): `procs::regions()` — снапшот, страницы исчезают →
AV; сканы только чанками под `veh::call_with`; `PAGE_GUARD` даёт `0xC00000FD`;
сканы >~500 МБ на потоке игры подвешивают кадр (видно как `dt=5751ms`);
**вызовы чужих `il2cpp_*` из QPC-детура убивают игру** — из тика только чтения,
вызовы — в stub-thread режиме; `catch_unwind` в manual-map образе не работает,
только VEH-longjmp; метадата в памяти может быть разрезана по страницам.

## Что именно построить

1. **Хостовый офлайн-деобфускатор экспортов** (`tools/api_map.py`):
   - по 232 стабам в `UnityPlayer.dll` (универсальный префикс, дисплейсменты
     читать из байтов) достать обфусцированное имя на каждый стаб;
   - сопоставить с каноническим списком 232 API-имён (`message.txt`) и с
     шаблонами `Il2CppKnownPatterns.inl`; учесть, что порядок стабов = порядок
     API-списка (проверить и доказать: у 46 стабов имена читаются, сверить их
     индексы с индексами в списке);
   - на выходе — `crates/hromatite-core/src/api_symbols.rs`:
     `pub const API: &[(&str /*il2cpp_name*/, &str /*obf_name*/, u32 /*rva*/)]`
     + функция `rva(name) -> Option<u32>`;
   - отдельный отчёт `api_map.json` (для диффа между билдами).
2. **Парсер метаданных из памяти** (`crates/hromatite-core/src/metadata.rs` +
   зеркальный офлайн-парсер `tools/meta_parse.py` для дампов):
   - найти расшифрованный блок `global-metadata.dat` в памяти процесса: скан по
     магике `AF 1B B1 FA` (LE) и/или по структуре заголовка
     `Il2CppGlobalMetadataHeader` (sanity: `version` 29–31, `stringLiteralOffset`
     в границах файла, `stringOffset`, `typeDefinitionsOffset`, `methodsOffset`…);
   - разобрать заголовок, `string`-блоб, `typeDefinitions`, `methodDefinitions`,
     `fields`, `properties`, `images` (imageDefinitions), `assemblies`;
   - API: `Meta::locate()`, `Meta::types()`, `Meta::type(name, ns)`,
     `Meta::methods(type)`, `Meta::fields(type)`, все с `Result`, без паник;
   - юнит-тесты на синтетическом буфере + интеграционный тест на реальном дампе.
3. **Мост «метадата → рантайм»** (`runtime.rs` или расширение `meta.rs`):
   - по `Il2CppMetadataRegistration`/`s_TypeInfoTable` (и по
     `Il2CppCodeRegistration.codeGenModules`) связать тип из метадаты с
     `Il2CppClass*` в памяти; таблицу искать и кэшировать **один раз**,
     раскладку подтверждать структурными инвариантами (image back-pointer:
     `class+0x00` → `Il2CppImage`, у которого `+0x00` — C-строка `*.dll`);
   - дать команды консоли: `meta` (есть ли метадата, версия, счётчики),
     `typelist <filter>`, `typedump <ns.name>`, `meths <ns.name>`, `fields2`,
     `dumpmeta <path>` (выгрузить расшифрованный блок на диск для офлайн-разбора);
   - критерий: `typedump UnityEngine.Camera` печатает поля с типами и смещениями,
     `meths UnityEngine.Camera` — методы с RVA, **без вызовов `il2cpp_*`**.
4. **Дампер и деобфускатор имён** (`tools/dump.py` или команда `dumpmeta`):
   - выгруженный блок + `api_symbols.rs` → JSON «класс/метод/поле» с именами,
     пригодный для диффа между билдами игры;
   - утилита `tools/gamediff.py`: сравнить два JSON и вывести, что изменилось
     (переименования экспортов, сдвиги RVA, новые/удалённые типы).
5. **Регенерация после апдейта игры** — одна команда
   (`tools/refresh.sh`): sha256 `GameAssembly.dll` → `symmap.py` → `api_map.py` →
   обновить `symbols.rs`/`api_symbols.rs`, записать в `AGENTS.md` версию и хеш.

## Требования к качеству

- Никаких новых потоков/сокетов внутри игры (скрытность); всё чтение — через
  `safe::*`, все структуры — с проверками; длинные сканы — чанками под guard.
- Никаких `unwrap()` на внешних данных; `Result<T, String>` с понятными
  ошибками.
- Каждый модуль — с юнит-тестами на офлайн-данных; интеграционные проверки —
  в живой игре, результаты (строки лога) — в журнал.
- Публичные API — маленькие и стабильные, чтобы фичи потом не зависели от
  раскладок: `Meta`, `Runtime`, `Api`.
- Не трогать файлы игры. Не ломать существующие команды консоли.
- Никаких «магических» констант без комментария, откуда взяты (заголовок
  libil2cpp какой версии, живой дамп, офлайн-скрипт).
- Производительность: холодный `locate()` ≤ 2 с на потоке игры, повторные
  обращения — из кэша; полный dumpmeta — отдельной командой, не в тике по
  умолчанию.

## Формат ответа

1. Короткий план (5–9 шагов) с критериями «готово» на каждый.
2. Структура файлов/модулей (какие файлы создать, что изменить в `lib.rs`,
   `console.rs`, `Cargo.toml`).
3. Публичные типы и функции (сигнатуры Rust).
4. Команды/скрипты, которые появятся, и как ими пользоваться.
5. Риски и как их проверять (что может убить игру).
6. Затем — реализация, коммит за коммитом.

Если чего-то не хватает (например точной версии `global-metadata` заголовка для
Unity 6) — скажи это явно и предложи, как проверить на живом процессе, не
останавливая работу.
