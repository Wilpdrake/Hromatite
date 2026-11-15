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
- **218 из 225 адресов** получили настоящие имена; 194 из 232 канонических имён API
  привязаны к конкретному адресу.
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
             +0x20 klass +0x28 return_type +0x30 parameters +0x38 metadataHandle
             +0x48 token +0x4c flags(u16) +0x4e iflags +0x50 slot +0x52 param_count(u8)
FieldInfo    stride 0x28: +0x00 type, +0x08 offset(int32), +0x10 name, +0x18 token, +0x20 parent
Il2CppType   +0x00 data, +0x08 attrs(u16), +0x0a type(int8, IL2CPP_TYPE_*)
Il2CppImage  +0x00 assembly, +0x08 short name, +0x14 typeStart, +0x20 metadata handle,
             +0x30 codegenModule, +0x40 "<name>.dll"
```

Типичные однострочные геттеры: `mov 0x98(%rcx),%rax` = `class_get_name`,
`lea 0x20(%rcx),%rax` = `class_get_type`, `movsbl 0xa(%rcx)` = `type_get_type`,
`movzwl 0x8(%rcx)` = `type_get_attrs`, `mov 0x48(%rcx),%eax` = `method_get_token`.

## ICF: почему часть имён API не имеет отдельного адреса

Линкер склеил идентичные тела, поэтому один адрес несёт несколько экспортных имён:
`0x3540` — 5 имён, `0x3dc0` — 8, `0x29200` — 4, `0x8acd0` — 4, `0x191870` — 3, `0x4cbd70`,
`0xc28040`, `0xc298e0`, `0x291f0`, `0x29240` — по 2. Из-за этого 38 канонических имён
(`il2cpp_property_get_*`, `il2cpp_field_get_name/type`, `il2cpp_method_get_name`,
`il2cpp_object_new` и др.) не имеют «своего» адреса: их код совпал с соседним.
При обращении через `GetProcAddress` это неважно — имя резолвится в тот же адрес.

## Что не опознано (5 адресов)

`0x0c4740` (+0x58 ptr), `0xa3950` (+0x20 u32), `0xaf7a0` (+0x1c u32), `0x161ec0`
(старший бит dword +0x28), `0xc2a6b0` (сеттер `0xe66c828`) — форма допускает 2+ канонических
варианта, поэтому имена не присвоены (в JSON у них `confidence: unknown`).

## Воспроизведение после обновления игры

1. `cargo build --workspace --release` не нужен; нужен только `tools/symmap.py` для новой
   карты экспортов и Ghidra-проект на новый `GameAssembly.dll`.
2. Открыть бинарь в Ghidra, прогнать анализ, затем пройти по `tools/data/il2cpp-export-names.json`:
   истинно то, что подтверждается формой тела, а не прежнее имя.
3. Обновить JSON и этот документ, зафиксировать новый sha256.
