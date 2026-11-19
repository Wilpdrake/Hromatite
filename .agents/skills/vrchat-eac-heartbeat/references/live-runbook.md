# Live runbook: запуск VRChat под Proton и эксперименты с heartbeat

Всё ниже проверено на этой машине. Пути: Steam — `~/.local/share/Steam` (`~/.steam/steam` — симлинк),
prefix — `~/.local/share/Steam/steamapps/compatdata/438100/pfx`, логи игры —
`<prefix>/drive_c/users/steamuser/AppData/LocalLow/VRChat/VRChat/output_log_*.txt`.

## Подготовка (всегда)
```sh
export PATH=/run/current-system/sw/bin:$PATH       # coreutils/strings/objdump
cp ~/.hromatite-mode ~/.hromatite-mode.bak-$(date +%H%M)    # ОБЯЗАТЕЛЬНО, вернуть в конце
pgrep -af 'VRChat[.]exe'                          # игра не должна уже работать
```
Один инстанс за раз: параллельные прогоны путают логи и инжект.

## Запуск
- Обычный (EAC включён): в `~/.hromatite-mode` только `HROMATITE_DISABLE=1` (без инжекта)
  или `HROMATITE_DLL=Z:\\home\\wilpdrake\\EAC-RE\\shim\\eacshim.dll` (с инжектом).
- Старт: `steam -applaunch 438100 --no-vr` (в фоне; окно появится на десктопе).
- Враппер `tools/hromatite-launch.sh` читает KEY=VALUE из `~/.hromatite-mode` и **экспортирует**
  их в игру — так прокидывается и `EOS_USE_ANTICHEATCLIENTNULL=1`.
- Ждать в логе: `Logged In` → `[AntiCheatClient] [BeginSession-003] Mode: 1` →
  `AntiCheat Session Begin: Success` → `Entering Room: ...`.

## Эксперимент A: без EAC и без heartbeat
```sh
printf 'HROMATITE_DISABLE=1
EOS_USE_ANTICHEATCLIENTNULL=1
' > ~/.hromatite-mode
```
Ожидаемо (измерено): `Anti-cheat client not available, using null client`; `BeginSession` = Success;
вход в мир; через **~38 с** — `Lost connection to realtime network while waiting to enter room:
Joined - waiting on limits - The VRChat Anti-Cheat system was unable to establish or maintain your
VRChat installation's integrity…`, затем `OnLeftRoom` и connect-error state. Это базовый ответ
на «что будет, если heartbeat нет».

## Эксперимент B: EAC включён, heartbeat отрубается на ходу
Инжектить шим (`HROMATITE_DLL=…eacshim.dll`) и в нём либо (а) обернуть колбэк
`AddNotifyMessageToServer` и после `EACSHIM_CUT_AFTER_MS` дропать исходящие AC-сообщения
(чистая имитация остановки heartbeat), либо (б) в тот же момент вызвать настоящий
`EOS_AntiCheatClient_EndSession` с хэндлом из хука `BeginSession`, либо (в) заставить
`ReceiveMessageFromServer` возвращать ошибку. Вариант (а) — предпочтительный.
Серверный таймаут — порядка 30–40 с, так что после отсечки ждать ≥60 с и только потом судить.

## Что смотреть в собранных данных
- Поток AC-сообщений: размеры и таймстампы `ReceiveMessageFromServer`/`cbMessageData` —
  рукопожатие (48/144/156/512 Б), затем 24-байтные кадры каждые ~2.0–2.1 с (это keepalive).
- Смена мира: `OnLeftRoom → EndSession → BeginSession` в пределах 1–2 с (сессия на комнату).
- Кик: `Lost connection`, текст про integrity, `OnLeftRoom`, `set connect error state`.

## Сбор и уборка
- Копии: `~/EAC-RE/evidence/runX_<HHMM>/{eacshim.log, eacshim_frames.bin, output_log_copy.txt,
  hromatite_launch_tail.txt}`; выводы — в `~/EAC-RE/findings/*.md` (OBSERVED vs INFERRED).
- Закрыть игру: `pkill -f 'VRChat[.]exe'` (скобки обязательны, иначе убивает свой же shell).
- Вернуть `~/.hromatite-mode` из бэкапа и сверить, что там нет `HROMATITE_DLL`/`DISABLE=1`.
- Логи шима ротировать (32 МБ): при рекурсии в хуке лог раздувается до гигабайт за секунды.
