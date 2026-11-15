#!/bin/sh
# Автозапуск консоли при старте игры.
#
# Консоль живёт на хосте (читает файл лога), поэтому её не надо тащить внутрь
# Proton-контейнера. Wrapper, отработав инжект, обновляет маркер
# ~/.hromatite-live; этот сторож видит маркер и открывает терминал с консолью.
#
#   tools/hromatite-autoconsole.sh            # следить (Ctrl-C — выход)
#   tools/hromatite-autoconsole.sh --open     # открыть окно сейчас и выйти
#   tools/hromatite-autoconsole.sh --once     # один проход проверки
#
# Переменные:
#   HROMATITE_TERM      терминал (по умолчанию ищется: konsole, xterm, kitty, …)
#   HROMATITE_MARKER    путь к маркеру (default ~/.hromatite-live)
#   HROMATITE_PREFIX    префикс Proton (default compatdata/438100/pfx)

set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CONSOLE="$HERE/hromatite-tui.sh"
MARKER="${HROMATITE_MARKER:-$HOME/.hromatite-live}"
PREFIX="${HROMATITE_PREFIX:-$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx}"
LOGF="$HOME/.hromatite-autoconsole.log"

log() { printf '[%s] %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOGF"; }

# Терминал ищем по PATH, а если сессия без env (агент/systemd) — берём
# DISPLAY/WAYLAND_DB и адрес шины у уже запущенного терминала.
pick_term() {
    if [ -n "${HROMATITE_TERM:-}" ]; then echo "$HROMATITE_TERM"; return; fi
    for t in konsole xterm kitty alacritty foot wezterm gnome-terminal xfce4-terminal st; do
        p=$(command -v "$t" 2>/dev/null) && { echo "$p"; return; }
    done
}

borrow_env() {
    src=""
    for cand in $(pgrep -f 'konsole|alacritty|kitty|gnome-terminal|xfce4-terminal' 2>/dev/null); do
        [ -r "/proc/$cand/environ" ] && { src=$cand; break; }
    done
    [ -z "$src" ] && for cand in $(pgrep -f 'VRChat[\\/]VRChat[.]exe' 2>/dev/null); do
        [ -r "/proc/$cand/environ" ] && { src=$cand; break; }
    done
    [ -z "$src" ] && return
    for k in DISPLAY WAYLAND_DISPLAY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS XAUTHORITY \
             XDG_SESSION_TYPE XDG_CURRENT_DESKTOP XDG_DATA_DIRS; do
        v=$(tr '\0' '\n' <"/proc/$src/environ" 2>/dev/null | sed -n "s/^$k=//p" | head -1)
        [ -n "${v:-}" ] && export "$k=$v"
    done
    log "env borrowed from pid $src (DISPLAY=${DISPLAY:-} WAYLAND=${WAYLAND_DISPLAY:-})"
}

console_running() {
    # Either the native TUI binary or its launcher already owns the console.
    pgrep -f '[h]romatite-console([[:space:]]|$)|[h]romatite-tui[.]sh' >/dev/null 2>&1
}

open_console() {
    term=$(pick_term)
    if [ -z "${term:-}" ]; then
        log "no terminal emulator found (HROMATITE_TERM not set)"
        return 1
    fi
    borrow_env
    log "opening $term with $CONSOLE"
    case "$term" in
        *konsole) setsid -f "$term" --hold -p tabtitle="hromatite · VRChat" \
                      -e "$CONSOLE" >/dev/null 2>&1 ;;
        *gnome-terminal) setsid -f "$term" --title "hromatite · VRChat" -- \
                      "$CONSOLE" >/dev/null 2>&1 ;;
        *) setsid -f "$term" -T "hromatite · VRChat" -e \
                      "$CONSOLE" >/dev/null 2>&1 ;;
    esac
    sleep 2
    if console_running; then log "console started"; else log "console did not start"; fi
}

case "${1:-}" in
    --install-autostart)
        d="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
        mkdir -p "$d"
        cat >"$d/hromatite-autoconsole.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=hromatite auto-console
Comment=Открывает консоль hromatite, когда игра запускается с wrapper-ом
Exec=$HERE/hromatite-autoconsole.sh
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
        echo "written: $d/hromatite-autoconsole.desktop"
        exit 0
        ;;
    --open)
        open_console
        exit $?
        ;;
    --once)
        [ -f "$MARKER" ] || { log "no marker yet: $MARKER"; exit 1; }
        console_running && { log "console already running"; exit 0; }
        open_console
        exit $?
        ;;
esac

log "watcher started (marker $MARKER)"
last=""
while :; do
    if [ -f "$MARKER" ]; then
        cur=$(sed -n '1p' "$MARKER" 2>/dev/null)
        if [ "$cur" != "$last" ]; then
            last="$cur"
            log "marker changed: $cur"
            console_running || open_console
        fi
    fi
    sleep 2
done
