#!/bin/sh
# hromatite console — живой вывод лога игры отдельно от процесса игры.
#
# Почему файл, а не сокет: всё, что payload открывает внутри игры (TCP/UDP-порт,
# именованный канал), видно сканеру (хендлы, объекты \Device\NamedPipe, сокеты).
# Файл в префиксе уже есть, его пишет логгер, и он ничего не добавляет к
# поверхности детекта. Читаем его с хоста — для EAC это обычный read файла
# другим процессом, никаких объектов внутри игры.
#
#   tools/hromatite-console.sh [опции]
#
#   --prefix <dir>   префикс Proton (по умолчанию compatdata/438100/pfx)
#   --log <file>     конкретный лог
#   -n <N>           сколько последних строк показать сначала (default 30)
#   -f, --filter <s> фильтр: all | diag | dll | err | <подстрока>
#   --no-follow      показать и выйти
#   --status         состояние: процесс игры, регионы памяти, свежесть лога
#   --raw            без цветов и без префиксов времени
#
# Ctrl-C — выход.

set -u

PREFIX="${HROMATITE_PREFIX:-$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx}"
LOG=""
HIST=30
FILTER="all"
FOLLOW=1
STATUS=0
RAW=0
SEND=""
SHELL_MODE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        --log)    LOG="$2"; shift 2 ;;
        -n)       HIST="$2"; shift 2 ;;
        -f|--filter) FILTER="$2"; shift 2 ;;
        --no-follow) FOLLOW=0; shift ;;
        -s|--send) SEND="$2"; shift 2 ;;
        --shell) SHELL_MODE=1; shift ;;
        --status) STATUS=1; shift ;;
        --raw)    RAW=1; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "неизвестная опция: $1" >&2; exit 2 ;;
    esac
done

[ -n "$LOG" ] || LOG="$PREFIX/drive_c/users/steamuser/hromatite.log"
# командный канал — тот же файл, что читает payload (см. core/src/console.rs)
CMD_FILE="$PREFIX/drive_c/users/steamuser/hromatite-cmd.txt"
send_cmd() { printf '%s\n' "$1" >>"$CMD_FILE"; }

COL=1
[ -t 1 ] || COL=0

say() { printf '%s\n' "$*"; }

# --- status -----------------------------------------------------------------
if [ "$STATUS" = 1 ]; then
    TMPF=$(mktemp "${TMPDIR:-/tmp}/hromatite-status.XXXXXX")
    trap 'rm -f "$TMPF"' EXIT
    printf 'лог: %s\n' "$LOG"
    if [ -f "$LOG" ]; then
        printf 'размер: %s, изменён: %s\n' "$(wc -c <"$LOG")" "$(date -r "$LOG" '+%H:%M:%S')"
        printf 'сейчас: %s\n' "$(date '+%H:%M:%S')"
    else
        printf 'лога нет — игра ещё не запускалась с wrapper-ом\n'
    fi
    # в командной строке wine путь windows-ный: VRChat\VRChat.exe
    GPID=$(for p in $(pgrep -f 'VRChat[\\/]VRChat[.]exe' 2>/dev/null); do
        case "$(readlink -f "/proc/$p/exe" 2>/dev/null)" in *wine*) echo "$p"; break ;; esac
    done)
    if [ -n "${GPID:-}" ]; then
        printf 'процесс игры: pid %s\n' "$GPID"
        # Подпись запечатанного образа: keystream нулевой страницы постоянен,
        # поэтому первые 8 байт шифра всегда одни и те же. Так наш регион
        # находится без знания его адреса.
        SEAL_SIG="c6 2f ed 2c 76 06 ec 7b"
        found=0
        printf 'наш образ (поиск по подписи шифра):\n'
        awk '$5=="0" && $2=="rw-p" {print $1}' "/proc/$GPID/maps" 2>/dev/null | head -200 \
        | while read -r rng; do
            s=$((0x${rng%-*})); e=$((0x${rng#*-})); sz=$((e - s))
            [ "$sz" -ge 262144 ] || continue
            # /proc/<pid>/mem может блокироваться (ptrace) — таймаут обязателен
            b=$(timeout 3 dd if="/proc/$GPID/mem" bs=1 skip="$s" count=8 2>/dev/null | od -A n -t x1 | tr -s ' ' | sed 's/^ *//;s/ *$//')
            case "$b" in
                "$SEAL_SIG")
                    printf '  ЗАПЕЧАТАННЫЙ %s  %s KB  (ни PE, ни кода; RW, не exec)\n' "$rng" "$((sz / 1024))"
                    echo "1" >>"$TMPF" ;;
                4d\ 5a*)
                    printf '  PE-в-анонимной-памяти %s  %s KB  (MZ: заголовки НЕ стёрты)\n' "$rng" "$((sz / 1024))" ;;
            esac
        done
        [ -s "$TMPF" ] || printf '  не найден (игра запущена без --seal или образ ещё открыт)\n'
        printf 'анонимные exec-страницы (кандидаты в детект):\n'
        awk '$5=="0" && $2 ~ /^r.x/ {print "  " $1, $2}' "/proc/$GPID/maps" 2>/dev/null | head -6
    else
        printf 'процесс игры не найден\n'
    fi
    exit 0
fi

# --- stream -----------------------------------------------------------------
[ "$COL" = 1 ] && { C_RST=$(printf '\033[0m'); C_DIM=$(printf '\033[2m'); C_CYAN=$(printf '\033[36m');
                    C_MAG=$(printf '\033[35m'); C_RED=$(printf '\033[31m'); C_YEL=$(printf '\033[33m'); }
[ "$COL" = 1 ] || { C_RST=; C_DIM=; C_CYAN=; C_MAG=; C_RED=; C_YEL=; }

if [ ! -f "$LOG" ]; then
    say "лога пока нет: $LOG"
    say "жди запуска игры (wrapper пишет сюда же)…"
fi

# follow: tail -F переживает пересоздание файла (игра перезапустилась)
if [ "$FOLLOW" = 1 ]; then
    TAILCMD="tail -n $HIST -F"
else
    TAILCMD="tail -n $HIST"
fi

$TAILCMD "$LOG" 2>/dev/null | awk \
    -v col="$COL" -v filt="$FILTER" \
    -v rst="$C_RST" -v dim="$C_DIM" -v cyan="$C_CYAN" -v mag="$C_MAG" -v red="$C_RED" -v yel="$C_YEL" '
BEGIN { prev = 0 }
{
    line = $0
    sub(/\r$/, "", line)
    if (line == "") next

    ts = ""; human = ""
    if (match(line, /^\[[0-9]+\]/)) {
        ts = substr(line, 1, RLENGTH)
        line = substr(line, RLENGTH + 1)
        sub(/^ +/, "", line)
        t = substr(ts, 2, RLENGTH - 2) + 0
        if (prev > 0 && t - prev > 5) human = sprintf("[+%ds] ", t - prev)
        prev = t
    }

    # фильтр
    kind = "log"
    if (line ~ /diag:/) kind = "diag"
    else if (line ~ /\[dll\]/) kind = "dll"
    if (line ~ /(failed|error|FAILED|not found|unresolved)/) kind = "err"
    if (filt != "all" && filt != "") {
        if (filt == "diag" && kind != "diag") next
        if (filt == "dll"  && kind != "dll")  next
        if (filt == "err"  && kind != "err")  next
        if (filt != "diag" && filt != "dll" && filt != "err" && index(tolower(line), tolower(filt)) == 0) next
    }

    c = ""
    if (kind == "diag") c = cyan
    else if (kind == "dll") c = mag
    else if (kind == "err") c = red
    if (human != "" && col == 1) line = yel human rst line
    printf "%s %s%s%s\n", dim, c, line, rst
    # stdout is a pipe when the output is captured: tanpa flush каждая строка
    # висит в буфере awk до выхода процесса
    fflush()
}' &
AWKPID=$!

trap 'kill $AWKPID 2>/dev/null; exit 0' INT TERM

# одна команда и выход («консоль» без интерактива — удобно из скрипта)
if [ -n "$SEND" ]; then
    send_cmd "$SEND"
    printf '%s→ %s%s\n' "$C_DIM" "$SEND" "$C_RST"
    sleep 1
    kill $AWKPID 2>/dev/null
    exit 0
fi

# интерактив: строки stdin уходят в файл команд, ответы приходят в этот же поток
if [ "$SHELL_MODE" = 1 ]; then
    printf '%sконсоль: команда + Enter (help, asm, q 0x…, domain, seal). Ctrl-D — выход.%s\n' "$C_DIM" "$C_RST"
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        send_cmd "$line"
        printf '%s→ %s%s\n' "$C_DIM" "$line" "$C_RST"
    done
fi

kill $AWKPID 2>/dev/null
exit 0
