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

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="$2"; shift 2 ;;
        --log)    LOG="$2"; shift 2 ;;
        -n)       HIST="$2"; shift 2 ;;
        -f|--filter) FILTER="$2"; shift 2 ;;
        --no-follow) FOLLOW=0; shift ;;
        --status) STATUS=1; shift ;;
        --raw)    RAW=1; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "неизвестная опция: $1" >&2; exit 2 ;;
    esac
done

[ -n "$LOG" ] || LOG="$PREFIX/drive_c/users/steamuser/hromatite.log"

COL=1
[ -t 1 ] || COL=0

say() { printf '%s\n' "$*"; }

# --- status -----------------------------------------------------------------
if [ "$STATUS" = 1 ]; then
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
        printf 'кандидаты в наш образ (анонимные rw-p, 512 KB..2 MB, заголовки стёрты):\n'
        # размер образа release-DLL ~0xef000; ищем именно такой регион
        awk '$6=="0" && $2=="rw-p" {print $1, $2}' "/proc/$GPID/maps" 2>/dev/null \
        | while read -r rng perm; do
            s=$((0x${rng%-*})); e=$((0x${rng#*-})); sz=$((e - s))
            if [ "$sz" -ge 524288 ] && [ "$sz" -le 2621440 ]; then
                b=$(dd if="/proc/$GPID/mem" bs=1 skip="$s" count=16 2>/dev/null | od -A n -t x1 | tr -s ' ')
                printf '  %s %s  %s KB  первые байты:%s\n' "$rng" "$perm" "$((sz / 1024))" "${b:- (нет доступа)}"
            fi
        done
        printf 'exec-страницы вне модулей (анонимные):\n'
        awk '$2 ~ /^r.x/ && $6=="0" {print "  " $1, $2}' "/proc/$GPID/maps" 2>/dev/null | head -8
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
wait $AWKPID
