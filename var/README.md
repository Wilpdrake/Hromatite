# var/ — вся «периферия» проекта в одном месте

Раньше логи, дампы, кеши, исследовательские скрипты и дерево EAC лежали в девяти
разных местах (`$HOME`, wine-префикс, `~/.cache`, `~/EAC-RE`, scratch агента).
Теперь они физически лежат здесь, а на старых путях остались **симлинки**, так
что payload, wrapper, TUI и старые скрипты продолжают работать без правок.
Каталог не коммитится (`.gitignore`: `/var/*` с исключением для этого README).

```
var/
├─ logs/                      448 файлов, ~30 MB
│  ├─ hromatite.log           ← C:\users\steamuser\hromatite.log ([con], [gct], [veh], [late], [playerloop])
│  ├─ hromatite-cmd.txt       ← канал команд (TUI/скрипты пишут, тик читает)
│  ├─ hromatite-eac.jsonl     ← eac.log / eac.tap, записи source=wire
│  ├─ hromatite-launch.log    ← wrapper: попытки инжекта, тайминги, fallback
│  ├─ hromatite-autoconsole.log
│  ├─ game-*.log (156)        ← stdout игры за каждый запуск
│  └─ gc-*.log (287)          ← сохранённые копии VRChat.gc.log (до/после)
├─ dumps/Crashes/             28 minidump'ов Unity (Crash_*/crash.dmp)
├─ mode/                      22 файла: история ~/.hromatite-mode (bak-*, launch-bak.*)
├─ modules/                   13 MB: поколения фич hot-reload (features-N-*.dll, current.manifest)
├─ cache/hromatite/           2 копии GameAssembly.dll по 242 MB + Il2CppDumper
├─ eac/EAC-RE/                2.0 GB исследовательского дерева EAC (ghidra, evidence, decoded)
└─ research/                  76 файлов, 30 MB — выжимка из project-scratch агента:
                              align*/callgraph*/newexport_map/margins (перенос имён экспортов
                              на 245768f2), find_pl/find_icall/probe3-5 (поиск PlayerLoop-геттера
                              и GC-сайтов), live-heartbeat.sh + live-heartbeat/ (живые срезы
                              heartbeat), eac-b/, eac-replace/, helpdump/,
                              eac-cut-untracked-*.tar.gz, eac-emu-worktree-dirty-*.patch
```

## Старые пути → новые

| Старый путь | Сейчас |
|---|---|
| `…/compatdata/438100/pfx/drive_c/users/steamuser/hromatite.log` | симлинк → `var/logs/hromatite.log` |
| `…/steamuser/hromatite-cmd.txt`, `…/hromatite-eac.jsonl` | симлинки → `var/logs/` |
| `…/AppData/Local/Temp/VRChat/VRChat/Crashes/` | симлинк → `var/dumps/Crashes/` |
| `…/steamuser/hromatite-modules/` (поколения фич) | симлинк → `var/modules/` |
| `~/.hromatite-logs/` | симлинк → `var/logs/` |
| `~/.hromatite-launch.log`, `~/.hromatite-autoconsole.log` | симлинки → `var/logs/` |
| `~/.cache/hromatite/` | симлинк → `var/cache/hromatite/` |
| `~/EAC-RE/` | симлинк → `var/eac/EAC-RE/` |

Перемещение делалось через `mv` (тот же inode), поэтому даже открытые на запись
хэндлы живого процесса продолжают писать в новый путь; симлинки — на случай
пути, зашитого в коде (`%USERPROFILE%\hromatite.log`, `~/.hromatite-logs` в TUI,
`~/.cache/hromatite` в `tools/il2cppdump.sh`).

`~/.hromatite-mode` **остаётся обычным файлом** в `$HOME`: wrapper делает
`cp -a` рядом (`~/.hromatite-mode.launch-bak.<pid>`) и возвращает файл на место,
поэтому симлинк там был бы заменён копией при первом же запуске. Здесь лежит
только история.

## Вернуть как было

```bash
R=~/RustroverProjects/Hromatite; P=~/.local/share/Steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser
rm ~/.hromatite-logs ~/.hromatite-launch.log ~/.hromatite-autoconsole.log ~/.cache/hromatite ~/EAC-RE \
   "$P/hromatite.log" "$P/hromatite-cmd.txt" "$P/hromatite-eac.jsonl" \
   "$P/AppData/Local/Temp/VRChat/VRChat/Crashes"
mv "$R/var/logs" ~/.hromatite-logs
mv "$R/var/cache/hromatite" ~/.cache/hromatite
mv "$R/var/eac/EAC-RE" ~/EAC-RE
mv "$R/var/dumps/Crashes" "$P/AppData/Local/Temp/VRChat/VRChat/Crashes"
mv "$R"/var/mode/.hromatite-mode* ~/
```

## Осторожно

* `git clean -xdf` в корне репозитория снесёт `var/` вместе с логами и деревом EAC.
* `var/logs/hromatite.log` уже >11 MB; payload пересоздаст файл при следующем
  запуске, старые `game-*`/`gc-*` можно чистить по вкусу.
* `var/cache/hromatite/builds/GameAssembly-<sha12>.dll` — копии игровых сборок для
  офлайн-разбора (`tools/buildgen.py`, Il2CppDumper), а не сама игра.
* `var/research/` — снимок scratch'а агента на 2026-11-18. Файлы `gc-forensics.md`,
  `gc-mechanism.md`, `gc-code-audit.md`, `review*.json`, `lateupdate-live-candidates.json`,
  на которые ссылается журнал `AGENTS.md`, были вычищены автоматическим pruning'ом
  scratch'а (idle > 24 ч) **до** этой уборки и в снимок не попали.
