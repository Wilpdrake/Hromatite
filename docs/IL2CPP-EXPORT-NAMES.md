# Настоящие имена экспортов GameAssembly.dll (Unity 6000.0.67f1)

Beebyte переименовывает экспорты при каждой сборке игры: `GetProcAddress("il2cpp_class_get_name")`
возвращает NULL, а в таблице экспорта стоят `AopbJwKYHHH`, `GEsTfKaEfsh` и т.п.
Этот документ — как восстановить соответствие «обфусцированное имя → настоящая функция il2cpp»
**в самой Ghidra**, чтобы листинг и декомпилятор читались, и что при этом уже установлено.

Готовая карта: `tools/data/il2cpp-export-names.json` (адрес, RVA, экспортные имена, настоящее
имя, уверенность, доказательство). Она привязана к `GameAssembly.dll`
sha256 `648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b` — после патча игры
карту надо перегенерировать (имена экспортов снова поменяются).

## Что получилось на этом билде

- 251 экспорт, **225 различных адресов** (остальное — алиасы одного кода).
- **218 из 225 адресов** получили настоящие имена (после второго прохода — 214 подтверждены,
  4 исправлены); 208 из 232 канонических имён API привязаны к конкретному адресу,
  остальные 24 живут на тех же адресах через ICF-склейку (см. ниже).
- Независимая проверка: 232 ленивых стаба в `UnityPlayer.dll` выстраиваются в канонический
  порядок API, и **191 из 191** сопоставленного стаба указывает на адрес, чьё восстановленное
  имя совпадает с каноническим (0 расхождений). Это подтверждает имена помимо формы тела.

## Почему порядок стабов нельзя использовать как доказательство

Стабы в `UnityPlayer.dll` идут почти в порядке `il2cpp-api-functions.h`, но с локальными
вставками Unity 6 (`il2cpp_profiler_*`, `pool_chunk_*`) и ICF-склейками. Сдвиг не константа:
`get_corlib` = стаб 12 при каноне 11, `free` = 15/16, `native_stack_trace` = 83/84,
`gchandle_get_target` = 121/122, `image_get_class` = 211/211. Поэтому опознание — по форме
тела и декомпиляции, а порядок служит лишь проверкой.

## Рецепт в Ghidra (проверено на базе проекта `GameAssembly.dll`)

1. `rename_symbols_batch` принимает **строку адреса**:
   ```json
   {"renames":[{"target_type":"function","identifier":"0x180c2a060",
                "new_name":"il2cpp_type_get_type"}]}
   ```
   DoD: `get_function_info(0x180c2a060)` показывает новое имя.
2. Если Ghidra отвечает `Function not found` — по этому адресу функция не определена.
   **Не вызывайте `create_function`**: на неразобранных байтах она создаёт функцию длиной
   1 байт, и адрес остаётся без тела и без декомпиляции. В таком случае пробуйте
   `target_type:"data"` (переименование метки) и/или пометьте адрес как «нет функции».
3. `get_pseudocode` в этой базе **асинхронный**: возвращает `Task ID` → забирать
   `get_task_status({"task_id":…})` (готово за 2–6 с). Для скорости — пачками по 5–8 функций.
4. `get_xrefs_to` для rip-relative и абсолютных `lea` часто возвращает 0 ссылок — дискриминацию
   по глобальным переменным делайте через псевдокод/hexdump, а не по xrefs.

## Якоря, доказанные поведением в живой игре

| RVA | имя | чем доказано |
|---|---|---|
| `0xc27f00` | `il2cpp_get_corlib` | возвращает `Il2CppImage*` mscorlib (`0x3027c1c0`) |
| `0xc2a330` | `il2cpp_image_get_class` | `(image,167)` → класс `Camera` |
| `0xc28050` | `il2cpp_class_from_name` | thunk → `Class::FromName` (`0xc07560`) |
| `0xc28070` | `il2cpp_class_get_fields` | итератор `(klass, void** iter)`, `fields@0x80`, `fieldsCount u16@0x124` |
| `0xbde100` | `il2cpp_free` | имя пережило обфускацию |
| `0xc290a0` | `il2cpp_gchandle_get_target` | имя пережило обфускацию |
| `0xc28b10` | `il2cpp_native_stack_trace` | имя пережило обфускацию |

## Раскладки структур этого билда (измерены по коду, не по заголовкам)

```
Il2CppClass  +0x00 image   +0x10 parent   +0x18 namespaze   +0x20 byval_arg (Il2CppType)
             +0x30 declaringType  +0x40 element_class  +0x48 interfaces  +0x50 methods
             +0x80 fields (stride 0x28)  +0x88 events (0x38)  +0x90 properties (0x28)
             +0x98 name  +0xa8 nestedTypes  +0xb0 vtable  +0xb8 static_fields
             +0xc8 interfaceOffsets  +0xd0 userdata  +0xf8 instance_size
             +0x104 element_size  +0x10c static_fields_size  +0x118 flags  +0x11c token
             +0x120 method_count +0x122 properties_count +0x124 fields_count +0x126 events_count
             +0x128 nested_types_count +0x12c interfaces_count +0x12e vtable_count
             +0x130 depth  +0x132 rank  +0x133 minimumAlignment
             +0x135 биты: 1 initialized, 2 enumtype, 4 is_generic, 5 has_references
             +0x136 биты: 0 initialized_and_no_error, 3 import/windows-runtime, size_inited
MethodInfo   +0x00 methodPointer +0x08 virtualMethodPointer +0x10 invoker +0x18 name
             +0x20 return_type +0x28 klass +0x30 parameters +0x38 metadataHandle
             +0x48 token +0x4c flags(u16) +0x4e iflags +0x50 slot +0x52 param_count(u8)
FieldInfo    stride 0x28: +0x00 type, +0x08 offset(int32), +0x10 name, +0x18 token, +0x20 parent
Il2CppType   +0x00 data, +0x08 attrs(u16), +0x0a type(int8, IL2CPP_TYPE_*)
Il2CppImage  +0x00 assembly, +0x08 short name, +0x14 typeStart, +0x20 metadata handle,
             +0x30 codegenModule, +0x40 "<name>.dll"
```

Типичные однострочные геттеры: `mov 0x98(%rcx),%rax` = `class_get_name`,
`lea 0x20(%rcx),%rax` = `class_get_type`, `movsbl 0xa(%rcx)` = `type_get_type`,
`movzwl 0x8(%rcx)` = `type_get_attrs`, `mov 0x48(%rcx),%eax` = `method_get_token`.

## Второй проход: независимая проверка всех имён

218 имён перепроверены заново десятью ревьюерами (по 20–22 адреса, без пересечений): имя в
Ghidra считалось гипотезой, доказательство искалось в коде — во что уходит вызов, какие поля
какой структуры читаются, как используются аргументы, а не «совпадает ли форма с прошлой
записью».

Итог: **207 `confirm`, 7 `uncertain`, 4 `wrong`** — то есть 95 % имён выдержали сильную
проверку, а 4 оказались неверными и исправлены:

| адрес | было | стало | почему |
|---|---|---|---|
| `0xc28390` | `class_bitflag_136_bit3` | `il2cpp_class_is_blittable` | бит `+0x136.3`; порядок битов снят с распаковки `Il2CppTypeDefinition.flags` в `0xbcdbd0` и `Class::Init` `0xc19470` |
| `0xc283f0` | `il2cpp_cand_class_array_element_size` | `il2cpp_class_array_element_size` | switch по `byval_arg.type`, `Class::FromIl2CppType` `0xbcd7a0`, `-1` на недопустимом |
| `0xc28c10` | `field_get_object` | `il2cpp_field_get_object` | `(field, refclass) → 0xc0f590` строит `Il2CppReflectionField`; имя есть в шапке 2023.2+ |
| `0xc297c0` | `il2cpp_class_from_type` | `il2cpp_object_new` | форвардер в `Object::New` `0xc0e480`, `Il2CppType` не читается вовсе |

Плюс два имени переформулированы после разбора (`0xc2a490` → `unity_method_code_range_foreach_chunked`,
`0xc29400` → `unity_member_name_chunked`): прежние названия вводили в заблуждение — первое
обходит диапазоны кода методов, а не секции GC-кучи, второе эмитит имя **члена**, а не типа.

### Что проверка уточнила в раскладках

- `MethodInfo`: **`return_type@0x20`, `klass@0x28`, `parameters@0x30`**. Прежняя запись
  с переставленными `klass`/`return_type` опровергнута чтением живых структур 2026-11-16:
  у `VRCTweenPostLateUpdater.LateUpdate` и `AsyncLateUpdateTrigger.LateUpdate` `+0x20`
  ведёт к Il2CppType `void` (`type=1`), а у `Camera.get_main` — к class-типу (`type=0x12`);
  `+0x28` в обоих случаях равен указателю на собственный `Il2CppClass`. Тело
  `il2cpp_method_get_param` подтверждает только `parameters@0x30`, но не различает +0x20/+0x28.
- `Il2CppImage+0x14` — это **не** `typeStart`, а счётчик диапазонов custom-атрибутов
  (`typeStart` лежит в `imageDef+0x00`, куда ведёт `+0x20`).
- `Il2CppClass+0x136`: `bit3` = `is_blittable`, `bit4` = `import/windows-runtime` (в прежней
  записи они были сдвинуты).
- `FieldInfo.get_flags`/`is_literal` читают `FieldInfo.type->attrs` (два разыменования) —
  это проверено и осталось верным; `offset@0x08`, `token@0x18` не перепутаны.
- `il2cpp_get_top_frame`/`get_frame_at` в этом билде возвращают `bool` и пишут кадр в
  out-параметр (в шапке 2019 — указатель).
- `CreateNLSocket`/`ReadEvents`/`CloseNLSocket` — **throw-заглушки** (стоят `__FILE__` и уходят
  в конструктор исключения), не рабочие сокеты.

### Протокол одного ревью (что реально смотрели)

`add_internal_call`/`resolve_icall` — одна `std::map` (`0xe66bb78`), `resolve` при промахе
обрезает имя по `'('`; `gc_enable`/`gc_disable` — уменьшение/увеличение счётчика `GC_dont_gc`
(`0xe66ccb8`) под одной CS, третий голос — `gc_set_mode`, который зовёт ровно нужную из двух;
`stop_gc_world`/`start_gc_world` — вход в тело `GC_suspend_all` (`0xc41a20`) против
`GC_resume_all` (`0xc41cb0`); `gchandle_new_weakref` содержит дословную строку
«IL2CPP does not support resurrection for weak references»; `is_debugger_attached` — вызов
`KERNEL32!IsDebuggerPresent` через IAT `0x18b709170`; `register_log_callback` — единственное
вхождение `"[libil2c"` в образе; `custom_attrs_from_class/method/field` — один и тот же
bsearch `0xc53f7c` с токенами `+0x11c` / `+0x48` / `+0x18`.

## ICF: почему часть имён API не имеет отдельного адреса

Линкер склеил идентичные тела, поэтому один адрес несёт несколько экспортных имён, и один и
тот же код обслуживает несколько разных API. Разобрано по коду (какие смещения читаются):

| адрес | API, которые за ним стоят |
|---|---|
| `0x3540` (`mov (%rcx)`) | `object_get_class`, `class_get_image`, `image_get_assembly`, `assembly_get_image`, `property_get_parent` |
| `0x191870` (`mov $0x10`) | `object_header_size`, `allocation_granularity`, `offset_of_array_bounds_in_array_object_header` |
| `0x29200` (`mov 0x18`) | `class_get_namespace`, `method_get_name` |
| `0x291f0` (`mov 0x28`) | `method_get_class` |
| `0x29240` (`mov 0x20`) | `field_get_parent` + `method_get_return_type` |
| `0x8acd0` (`mov 0x10`) | `class_get_parent`, `field_get_name` |
| `0xc47b0` (`mov 0x40`) | `image_get_filename`, `class_get_element_class` |
| `0xc28040` (`mov $1,%dl; jmp 0xc15a20`) | `class_from_il2cpp_type`, `class_from_type` |
| `0xc298e0` (strlen + utf8) | `string_new`, `string_new_wrapper` |
| `0x3dc0` | `_guard_check_icall` (8 экспортных имён) |

Из-за этого 24 канонических имени остаются без собственного адреса
(`il2cpp_property_get_*` — 5, `method_get_declaring_type`, `method_get_from_reflection`,
`method_has_attribute`, `field_get_type`, `class_get_declaring_type`, `class_is_valuetype`,
`class_get_bitmap*`, `gc_set_external_*_tracker`, `custom_attrs_free`,
`debug_get_method_info`, `debugger_set_agent_options`, `register_debugger_agent_transport`,
`thread_get_all_attached_threads`, `image_get_class_count`, `set_default_thread_affinity`,
`unity_install_unitytls_interface`, `unity_set_android_network_up_state_func`,
`runtime_unhandled_exception_policy_set`). Для вызова через `GetProcAddress` это неважно —
имя резолвится в тот же адрес.

`tools/data/il2cpp-api-names.txt` при этом сам неполон: в нём нет `il2cpp_field_get_object`,
`il2cpp_field_get_from_reflection` и семи `il2cpp_profiler_*`, которые в этом билде есть.

## Что не опознано (5 адресов)

`0x0c4740` (+0x58 ptr), `0xa3950` (+0x20 u32), `0xaf7a0` (+0x1c u32), `0x161ec0`
(старший бит dword +0x28), `0xc2a6b0` (сеттер `0xe66c828`) — форма допускает 2+ канонических
варианта, поэтому имена не присвоены (в JSON у них `confidence: unknown`).

## TODO: адреса берём из файла, паттерны — потом

Пока источник истины — **адреса из `tools/data/il2cpp-export-names.json`** (они валидны ровно
для текущего sha256 `GameAssembly.dll`). Ничего другого в коде на них не завязано.

Когда выйдет обновление игры (или когда специально займёмся этим) — сделать **wildcard-подпись
на каждую функцию**: набор байт из её тела, где операнды-смещения/адреса заменены масками,
чтобы функция находилась в новом билде сама, без ручного разбора 225 адресов. Тогда обновление
игры будет стоить одного прогона «пройтись подписями по новому образу», а не новой волны
ревью. До этого момента карта остаётся адресной.

## Воспроизведение после обновления игры

1. `cargo build --workspace --release` не нужен; нужен только `tools/symmap.py` для новой
   карты экспортов и Ghidra-проект на новый `GameAssembly.dll`.
2. Открыть бинарь в Ghidra, прогнать анализ, затем пройти по `tools/data/il2cpp-export-names.json`:
   истинно то, что подтверждается формой тела, а не прежнее имя.
3. Обновить JSON и этот документ, зафиксировать новый sha256.
