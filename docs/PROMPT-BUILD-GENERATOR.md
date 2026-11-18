# Промпт для агента: генератор per-build данных для Hromatite

> Скопируй всё, что ниже разделителя, в другую ИИ (Claude Code / Codex / агент с Ghidra-MCP).
> Задача — сделать так, чтобы обновление игры стоило одного прогона, а не ручного
> реверса на 226 адресов, и чтобы рантайм сам подхватывал новую сборку.

---

## РОЛЬ И КОНТЕКСТ

Ты — инженер-реверсер и Rust-разработчик. Работаешь в репозитории
`~/RustroverProjects/Hromatite` (VRChat под Proton/Wine, Linux-хост, NixOS).

Цель проекта: DLL внутри `VRChat.exe`, которая через IL2CPP читает состояние игры
(камера, игроки, позиции) и обслуживает команды. Всё, что она делает, опирается на
адреса и смещения внутри `GameAssembly.dll` — а они меняются при каждом обновлении игры.

**Твоя задача: реализовать генератор per-build данных (`tools/buildgen.py` + рантайм-фоллбэк),
чтобы после обновления игры всё восстанавливалось одним прогоном, а не руками.**

## ФАКТЫ ОБ ОКРУЖЕНИИ (проверено, не перепроверяй заново)

```
игра:   ~/.local/share/Steam/steamapps/common/VRChat
файлы:  GameAssembly.dll (~253 МБ), UnityPlayer.dll, VRChat_Data/global-metadata.dat (зашифрован)
Unity:  6000.0.67f1-DWR
экспорты GameAssembly: 251 имя; из-за ICF-склейки — 225…226 различных адресов
имена экспортов переобфусцированы (Beebyte), при обновлении меняются ВСЕ
метадата на диске зашифрована (магии FAB11BAF нет) → офлайн-дамперы бесполезны
база образа: 0x180000000
хост-путь: PATH=$HOME/.cargo/bin:/run/current-system/sw/bin:$HOME/.nix-profile/bin:$PATH
тесты:  cargo test --workspace --target x86_64-unknown-linux-gnu --offline
сборка: cargo build --workspace --release --target x86_64-pc-windows-gnu --offline
линт:   cargo clippy --workspace --all-targets --release --offline   (должен быть 0 ошибок)
```

Что уже есть в репозитории (используй, не дублируй):

| путь | что делает |
|---|---|
| `crates/hromatite-abi/src/build.rs` | **единственный источник per-build чисел**: `GcAnchors`, `Il2cppAnchors`, `Layout`, таблица `KNOWN`, выбор по sha256 |
| `crates/hromatite-core/src/buildinfo.rs` | выбор дескриптора по sha из `symbols.rs`, иначе `raw::discover` |
| `crates/hromatite-core/src/raw/discover.rs` | уже умеет выводить GC/il2cpp-якоря из образа (см. правила ниже) |
| `tools/export_transfer.py` | перенос имён экспортов со старой сборки (якоря по телу + DP + изоморфизм графа вызовов) |
| `tools/symmap.py` | генерирует `crates/hromatite-core/src/symbols.rs` (251 `(name, rva)` + sha) |
| `tools/data/il2cpp-api-names.txt` | канонический список 232 имён il2cpp API |
| `tools/data/il2cpp-export-names-<sha12>.json` | карта адрес→имя для конкретной сборки |
| `docs/IL2CPP-EXPORT-NAMES.md`, `docs/HOT-RELOAD.md` | методика и замеры |
| `AGENTS.md` §17 | журнал; **каждый этап дописывается туда** |

## ЧТО ИМЕННО НУЖНО СГЕНЕРИРОВАТЬ

Офлайн (по файлам `GameAssembly.dll` + предыдущей карте) и/или в рантайме (при первом
запуске, из загруженного образа). Один артефакт — один раздел дескриптора.

### A. Экспортная карта и настоящие имена il2cpp

1. Разобрать PE: секции, таблицу экспорта (имя→RVA), таблицу релокаций.
2. Сгруппировать имена по адресам (ICF: один адрес = несколько API; 8–10 групп на сборку).
3. Перенести имена с предыдущей сборки: `tools/export_transfer.py` уже это делает, доработай:
   - якоря: тела функций ≤24 Б сравниваются побайтово с маской по таблице релокаций;
   - остальное — монотонный DP (порядок адресов сохраняется линкером), балл = доля
     совпавших немаскированных байт тела;
   - **приёмка: изоморфизм графа вызовов** — множество внутренних `call/jmp`-целей старой
     функции, прогнанное через перенос, должно совпасть с целями новой (на эталонных
     сборках совпадает 223/224; исключение `il2cpp_gc_set_mode` доказывается своим телом);
   - независимая проверка: экспорты, чьи имена не обфусцированы (`il2cpp_free`,
     `il2cpp_gchandle_get_target`, `il2cpp_native_stack_trace`) — перенос обязан предсказать их.
4. Отдельно пересчитать **смещения структур** (см. D) и перепроверить имена-геттеры:
   если у функции изменилась константа смещения, перенос по байтам «схлопнет» её с чужой —
   такие адреса надо переименовать по раскладке, а не по подобию.

### B. GC-якоря Boehm (12 чисел)

Правила (проверены на двух сборках, воспроизводят значения, снятые вручную):

| что | как найти |
|---|---|
| `threads` | в функции удаления записи: перед `mov rbx,[r14+rsi*8]` (`49 8B 1C F6`) стоит `lea r14,[rip+d]` (`4C 8D 35`) — её цель и есть `GC_threads[]` (256 указателей). Второй такой `lea` в `GC_suspend_all` указывает на `threads+0x800` (конец массива) |
| `remove_thread` | начало функции, содержащей этот `49 8B 1C F6`: откатиться назад до байта `0xCC` (паддинг) |
| `skip_close` = remove+0x40 | `call qword [rip+d]` (`FF 15`) — вызов `CloseHandle` |
| `unlinked_next` = remove+0x46 | инструкция сразу после этого вызова |
| `after_unlink` = remove+0x68 | `lea rax,[rip+d]` (`48 8D 05`) — проверка sentinel |
| `suspend_entry` | в `.rdata` найти строку `"SuspendThread loop failed"` (**два** вхождения), собрать все `lea reg,[rip+d]`-xref на них; минимальный xref − `0x128` = вход `GC_suspend_all` |
| `context_call` = entry+0xb2 | `call [GetThreadContext]; test eax,eax; jne` внутри `GC_suspend_all` |
| `process_record` = entry+0x70 | метка обработки одной записи |
| `resume_abort` = entry+0xdb | `int3` после `ABORT("ResumeThread failed")` |
| `suspend_abort` = entry+0x145 | `int3` после `ABORT("SuspendThread loop failed")` |
| `advance_bucket` = entry+0x24a | `add r15,8` — переход к следующему бакету |
| `thread_list_cs` | критическая секция списка потоков. Мерить: `threads-0x40` — stop-the-world CS (её берёт `EnterCriticalSection` в начале `GC_suspend_all`); список — самая ссылаемая `lea`-цель в окне `[threads-0x200, threads)`, кроме `threads-0x40` |

Внутрифункциональные расстояния (+0x40/+0x46/+0x68 и +0x70/+0xb2/+0xdb/+0x145/+0x24a)
одинаковы в обеих сборках — это проверяй тестом, он ловит регрессию.

### C. il2cpp-якоря (3 числа)

| что | как найти |
|---|---|
| `class_table` | в `MetadataCache::GetTypeInfoFromTypeDefinitionIndex`: `mov rax,[rip+d]` (`48 8B 05`), сразу за ним `lea rsi,[rax+rdi*8]` (`48 8D 34 F8`) — глобал = `s_TypeInfoDefinitionTable` (слот, хранящий `Il2CppClass**`). Первое (минимальное) совпадение |
| `image_get_class` | тело `mov rax,[rcx+XX]; mov r8d,[rax]; add r8d,edx; cmp r8d,-1; jne …` — уникальное; адрес должен быть в таблице экспорта |
| `get_corlib` | среди экспортов вида `mov rax,[rip+d]; ret` (обычно 3) — тот, чей глобал указывает на образ, у которого `+0x8` == ASCII `"mscorlib"` |

### D. Раскладки структур (главный источник ошибок)

Смещения нельзя брать из заголовков Unity — они меняются между сборками той же версии.
Мерить по телам функций, которые обязаны эти поля читать:

| структура | как доказать |
|---|---|
| `Il2CppClass` | `class_get_fields` → `fields` + `count`; `class_get_properties`/`property_from_name` → `properties`, stride, `count`; `class_get_interfaces` → `interfaces` + `count`; `class_get_method_from_name` → `methods`, `count`, `MethodInfo.name`, `flags`, `param_count`, цепочка родителя; `class_get_name` → `name`; `class_get_static_field_data` → `static_fields`; `class_instance_size`/`get_flags`/`get_type_token`/`get_rank`/`set_userdata` → соответствующие |
| `FieldInfo` | `field_get_value` читает тип из `[field+?]` и флаги типа из `[type+8]`; `field_get_offset` — `movslq [field+?]`; stride — шаг итератора в `class_get_fields` |
| `MethodInfo` | `method_get_param` → `parameters` + `param_count`; `class_get_method_from_name` → `name` (strcmp), `flags`, `param_count` |
| `Il2CppImage` | `image_get_name` (`+8`), `image_get_filename` (`+0x40`), `image_get_class` берёт хэндл дефиниций через `[image+?]` |
| `Il2CppArray`/`Il2CppString`/`Il2CppType` | `array_length` (`+0x18`), `string_length` (`+0x10`), `string_chars` (`lea +0x14`), `type attrs@+8` (биты `16..23` = тип) |

Записывай в дескриптор **только доказанные** смещения. Неизвестное — `0` + запись в `note`,
и код должен уметь работать без него (например, `field_parent: 0` = «поле не хранится»).

### E. Остальные адреса рантайма

**Сделано (2026-11-18), правила ниже — это то, что реально сработало; держать их как описание
генератора, а не как TODO:** `runtime.get_current_player_loop` (`0x86cda10` → `0x86ea320`),
`runtime.worker_dispatch` (`0xbf5ce0` → `0xbf7a60`), `runtime.gc_unregister`
(`0xc41920` → `0xc43690`), `il2cpp.class_from_type` (`0xc28040` → `0xc29db0`).
Выводятся в `tools/buildgen.py` (секция `runtime`) и в рантайме (`raw::discover`):

* `GetCurrentPlayerLoop` — **не экспорт**, а скомпилированный managed-геттер: пролог
  33 байта обнуляет 0x28-байтовую структуру (`RCX` — скрытый out-указатель), а внутри есть
  `lea` на имя icall `UnityEngine.LowLevel.PlayerLoop::GetCurrentPlayerLoopInternal()`.
  Правило: сигнатура + обязательная ссылка на эту строку (иначе кандидат отбрасывается).
  Живёт в секции **`il2cpp`** (без точки), а не в `.text` — скан только `.text` не находит её
  никогда.
* диспетчер воркеров — уникальное 48-байтное тело, перекрёстно проверенное `lea` из
  создателя потока (в `CreateThread` уходит именно этот адрес).
* `GC_unregister_my_thread` — единственная функция, вызывающая `GC_remove_thread`; её
  **вход берётся из `.pdata`**, а не обратным ходом по `0xCC`: у функции несколько
  EH-фрагментов (`UNW_FLAG_CHAININFO`), и в байтах `rel32` полно `0xCC`, поэтому обратный
  ход останавливается в середине функции (даёт `0xc41924` вместо `0xc41920`).
* `gc.thread_list_cs` — **CS, которую входит `GC_suspend_all`** (`lea rcx` перед
  `call [EnterCriticalSection]`), а не «самый частый lea перед таблицей»: старая эвристика
  вернула строку `"CP1252"` и повесила игру на `EnterCriticalSection`.
`lateupdate::AUTO_BUILD_SHA256`/`AUTO_CLASS` остаются прибитыми к одному билду, но это уже
безопасно: индекс типа — лишь подсказка, имя перепроверяется, а при чужой сборке включается
`il2cpp_class_from_name`.

## КАКИЕ АРТЕФАКТЫ ДОЛЖНЫ ПОЛУЧИТЬСЯ

1. **`tools/buildgen.py`** (CLI, stdlib + objdump, без внешних зависимостей):
   ```
   python3 tools/buildgen.py --game <dir> [--previous-map tools/data/il2cpp-export-names-<sha12>.json] \
        --out-dir tools/data --update-descriptor
   ```
   Печатает человекочитаемый отчёт (что доказано, что выведено, что осталось неизвестным),
   пишет:
   - `tools/data/il2cpp-export-names-<sha12>.json` (схема уже есть, не меняй);
   - `crates/hromatite-core/src/symbols.rs` (через `tools/symmap.py`);
   - новый `Build { … }` в `crates/hromatite-abi/src/build.rs` (`verified`/`note` по факту).
2. **Рантайм-фоллбэк**: при старте резидент считает sha256 загруженного модуля; если сборки
   нет в `KNOWN` — берёт якоря из `raw::discover` (правила B, C, D), пишет кэш
   `%USERPROFILE%\hromatite-build-<sha12>.json`, логирует строкой `buildgen: …`.
   Любой неполученный якорь → **не ставить патч** (fail closed), а не «попробовать».
3. **Тесты**: `tools/test_buildgen.py` + юнит-тесты в Rust на чистых функциях (сканеры
   `lea`/паттернов, извлечение смещений из байтов).

## ПРИЁМКА (обязательна, это твой оракул)

Генератор обязан воспроизвести на **старой** сборке значения, снятые вручную, и на
**текущей** — зафиксированные ниже. Если не воспроизводит — правило неверное.

```
сборка 648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b (253 001 728 Б)
  gc.remove_thread 0xc417b0   skip_close 0xc417f0   unlinked_next 0xc417f6   after_unlink 0xc41818
  gc.suspend_entry 0xc41a20   context_call 0xc41ad2 process_record 0xc41a90  resume_abort 0xc41afb
  gc.suspend_abort 0xc41b65   advance_bucket 0xc41c6a  threads 0xe6bb180  thread_list_cs 0xe6bb100
  il2cpp.get_corlib 0xc27f00  image_get_class 0xc2a330  class_table 0xe66c870
  layout: parent 0x10, namespace 0x18, methods 0x50, fields 0x80, name 0x98, fields_count 0x124,
          field_stride 0x28 (type@0, offset@8, name@0x10, parent@0x20),
          method name 0x18, return_type 0x20, parameters 0x30, flags 0x4c, param_count 0x52

сборка 245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb (253 015 040 Б)
  gc.remove_thread 0xc43520   skip_close 0xc43560   unlinked_next 0xc43566   after_unlink 0xc43588
  gc.suspend_entry 0xc43790   context_call 0xc43842 process_record 0xc43800  resume_abort 0xc4386b
  gc.suspend_abort 0xc438d5   advance_bucket 0xc439da  threads 0xe6bd8a0  thread_list_cs 0xe6bd800 (выведен)
  il2cpp.get_corlib 0xc29c70  image_get_class 0xc2c0a0  class_table 0xe66f0d0
  layout: parent 0xa0, namespace 0x18, methods 0x58, fields 0x88, name 0x98, fields_count 0x124,
          field_stride 0x20 (name@0, offset@8, type@0x10, parent отсутствует),
          method name 0x20, return_type 0x28 (по сдвигу), parameters 0x30, flags 0x4c, param_count 0x52
  экспортная карта: tools/data/il2cpp-export-names-245768f2.json (222 из 226 названы)
```

Дополнительная проверка: `il2cpp_get_corlib` должен вернуть образ, у которого `+0x8` читается
как `"mscorlib"`; `il2cpp_image_get_class(corlib, i)` — вернуть класс с читаемым именем.

## ТРЕБОВАНИЯ К КАЧЕСТВУ (не нарушать)

1. **Не доверяй порядку стабов** в `UnityPlayer.dll` как доказательству личности функции:
   сдвиг непостоянный (12↔11, 122↔121, 211↔211). Порядок — только подсказка.
2. **ICF-склейка**: один адрес = несколько API. Храни группу имён, а не одно; не «ищи
   отдельное тело» для канонического имени, которого нет.
3. **Fail closed.** Нет доказательства — нет патча/имени. Каждое неизвестное значение —
   `0` в дескрипторе + `note` + отказ соответствующей фичи, а не «примерно так».
4. **Не создавай функции в Ghidra** (`create_function` на неразобранных байтах делает
   функцию в 1 байт и ломает адрес навсегда) — только `rename_symbols_batch` по адресу.
5. **Не читай и не патчи память игры** во время офлайн-генерации: работа только с файлами.
   Рантайм-часть — только чтения до момента, когда якоря проверены.
6. Байтовые «сигнатуры» не должны содержать дисплейсментов: `call [rip+disp]` проверяй
   как `FF 15`, `lea rax,[rip+disp]` как `48 8D 05`, пролог коллектора — как три
   `mov [rsp+d8], reg` (`48 89 XX 24 …`).
7. Каждое правило — с тестом на синтетическом буфере и с регрессией на двух эталонных сборках.
8. `cargo clippy --workspace --all-targets --release` — 0 замечаний; host-тесты зелёные;
   windows-release собирается.

## ФОРМАТ РАБОТЫ И ОТЧЁТА

- Сначала — короткий план (5–10 пунктов) и список файлов, которые создашь/изменишь.
- Коммить каждый завершённый шаг отдельным коммитом с сообщением-фактом
  (`feat(buildgen): …`), без «большого финального».
- В конце — отчёт: что доказано (с цифрами из прогона), что выведено эвристикой,
  что осталось неизвестным, какие тесты/сборки зелёные.
- Дописать запись в `AGENTS.md` §17: дата, коммиты, что сделано, чем подтверждено, что осталось.
- Язык отчёта — русский; код и имена — английские.

## ГРАНИЦЫ (не делать)

- Не менять файлы игры; доставка — только launch-wrapper.
- Не запускать `proton run` на живом префиксе (второй wineserver/блокировка).
- Не запускать/не убивать игру без явного разрешения: живые прогоны делает человек.
- Не «улучшать» интерфейс фич (`crates/hromatite-features/src/*`) сверх необходимого
  для новых полей дескриптора.
