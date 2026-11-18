# Runtime IL2CPP dump → offline names, types and method RVAs

Задача: получить имена и RVA **всех** managed-методов и смещения полей, не
разгадывая в рантайме ни одной личности `il2cpp_*`-экспорта. Решение — то же, что
используют `Zygisk-Il2CppDumper` (Perfare) и вся модовская экосистема: **снять
расшифрованную метадату из памяти и разобрать её офлайн** штатным
`Il2CppDumper` (v6.7.46, поддерживает metadata v16–31, т.е. Unity 6).

## Почему именно так

| Факт (проверено на этой машине) | Следствие |
|---|---|
| `global-metadata.dat` на диске зашифрован: первые байты `a1 d8 28 14 …`, магии `AF 1B B1 FA` нет | офлайн-дампа «как есть» не бывает |
| `Il2CppDumper` v6.7.46 запускается под dotnet 8 (roll-forward) и на диске отвечает `Metadata file not found or encrypted` | нужен дамп из памяти, инструмент готов |
| `Il2CppDumper` требует магию в первых 4 байтах — по ней он и отличает метадату от бинаря | если в памяти магии нет, её надо дописать (скрипт делает это сам) |
| `GameAssembly.dll` на диске не шифруется (`sha256 648f61b1…`) | дампить нужно только метадату, ~40 МБ |

Вторая половина пайплайна (`Il2CppInterop-Unity6`, форк `BepInEx/Il2CppInterop`
с правкой `Class::Init` для Unity 6) на этом шаге не нужна: она нужна позже, когда
мы будем **вызывать** managed-методы (generic-и, статические конструкторы).

## Пайплайн

```
игра (payload, тик)                     хост
────────────────────                   ──────────────────────────────────────
dumpmeta Z:\...\global-metadata.dat →  tools/il2cppdump.sh
                                       ├─ sha256 GameAssembly → sha12
                                       ├─ magic AF1BB1FA, если нет (патч копии)
                                       ├─ Il2CppDumper v6.7.46 (кэш ~/.cache)
                                       └─ dump.cs, script.json, il2cpp.h, DummyDll
                                            │
                                            ├─ tools/il2cpp_methods.py
                                            │    → methods-<sha12>.json
                                            │    → game_methods.rs (RVA-таблица)
                                            └─ dump.cs → смещения полей
```

### 1. Дамп метадаты из игры

Консоль (или `tools/hromatite-console.sh --send`):

```
dumpmeta Z:\home\wilpdrake\.hromatite-dumps\648f61b13e4f\global-metadata.dat
```

`Z:` в Proton указывает на `/`, поэтому хостовый путь записывается как `Z:\home\…`.
Команда уже есть в payload (`crates/hromatite-core/src/runtime.rs`), она пишет
`meta.dump()` — снапшот, который наш `metadata.rs` признал расшифрованной
метадатой. Ограничения те же, что у любого чтения из игры: файл пишется один раз,
~40 МБ; при `HROMATITE_FRAME_HOOK=1` это делается на потоке игры (кадр подвиснет).

### 2. Офлайн-дамп

```bash
tools/il2cppdump.sh                       # метадата по умолчанию из ~/.hromatite-dumps/<sha12>/
tools/il2cppdump.sh --metadata FILE --out DIR
```

Скрипт сам: скачивает `Il2CppDumper-net7-v6.7.46.zip` (928 КБ) в
`~/.cache/hromatite/il2cppdumper/`, выключает `RequireAnyKey`, берёт `dotnet` из
`PATH` или из `nixpkgs#dotnet-sdk_8`, дописывает магию в копию метадаты, если её
нет, и печатает лог дumper'а.

### 3. Таблица методов

```bash
uv run --no-project python tools/il2cpp_methods.py \
    ~/.hromatite-dumps/648f61b13e4f/dump/script.json \
    --game-dir ~/.local/share/Steam/steamapps/common/VRChat \
    --out ~/.hromatite-dumps/648f61b13e4f/methods.json \
    --rust crates/hromatite-core/src/game_methods.rs
```

`script.json` содержит по записи на каждый managed-метод:
`{"Address": <VA>, "Name": "Namespace.Type$$Method", "Signature": …}`.
`Address - ImageBase` = RVA внутри `GameAssembly.dll`; проверяется тестами
(`tools/test_il2cpp_methods.py`, 11 тестов, `uv run --no-project python -m unittest`).

## Риски и что делать, если не сработает

1. **Метадата в памяти лежит не одним блоком.** Наш `metadata.rs` ищет
   структурный заголовок v29–31 без магии; на живом прогоне это ещё не
   подтверждено (`Meta::locate` пока не находил кандидатов). Тогда `dumpmeta`
   упадёт с `decrypted global-metadata.dat not found`, и первым шагом будет
   диагностика: адрес первого вхождения строки `mscorlib`/`UnityEngine.CoreModule.dll`
   и hex-дамп ±0x40 вокруг него (`q <addr> 16`).
2. **`Il2CppDumper` не найдёт `Il2CppCodeRegistration`** в 253-МБ PE (авто-поиск
   рассчитан на «обычные» сборки). Запасные варианты: указать адреса вручную
   (`config.json` + ручной режим), либо дать ему таблицу `codeGenModules`,
   которую мы уже находили в живом процессе (`Il2CppCodeRegistration.codeGenModules`
   — 216 указателей в `.data` GameAssembly).
3. **Unity 6 vs v31.** `Il2CppDumper` знает layout v31 (Unity 2023.1); 6000.x
   может отличаться в отдельных таблицах. Если разбор падает на конкретной
   секции — сверять с нашим `metadata.rs` (он писался по тем же `MetadataClass.cs`)
   и с `tools/meta_parse.py`.
4. **DummyDll не соберётся** под .NET-версию игры — на RVA и имена это не влияет
   (`script.json` пишется независимо), dummy-сборки нужны только для декомпилятора.

## Что даёт результат сразу

- `dump.cs` — все типы, поля со смещениями (`// 0x10`), методы с RVA.
- `script.json` — машинная карта «имя метода → RVA» (232k+ записей на этом билде).
- `il2cpp.h` — структуры для IDA/Ghidra.
- Далее: `M3` (классы/поля/позиции) перестаёт зависеть от личности экспортов —
  остаётся найти `Il2CppClass*` по type index в `s_TypeInfoTable`
  (это уже делает `runtime.rs`).
