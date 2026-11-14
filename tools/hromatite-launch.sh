#!/bin/sh
# Steam launch-options wrapper: %command% is executed *inside* the proton
# container, so this script shares the game's wineserver. (Started from outside,
# wine keys its server socket on the prefix inode and spawns a second server.)
#
# Steam -> VRChat -> Properties -> Launch Options:
#   /home/wilpdrake/RustroverProjects/Hromatite/tools/hromatite-launch.sh %command%
#
# Injection uses the *game's own* wine environment (/proc/<pid>/environ) instead
# of `proton run`: re-running proton's setup while the game is live copies files
# into system32, which trips EAC's installation-integrity check (error 16).
#
#   HROMATITE_DISABLE=1    boot clean
#   HROMATITE_MODE=proton  fall back to `proton run`
#   HROMATITE_TRIES=n      injection attempts (default 3)

set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/.." && pwd)
PROTON="${HROMATITE_PROTON:-$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton}"
DIST=$(dirname "$PROTON")/files
BIN="$ROOT/target/x86_64-pc-windows-gnu/release"
EXE="$BIN/Hromatite.exe"
DLL_WIN='Z:\home\wilpdrake\RustroverProjects\Hromatite\target\x86_64-pc-windows-gnu\release\hromatite.dll'
# L2 (seal): after the payload returns, the injector XORs the whole mapping and
# leaves it RW. HROMATITE_SEAL=0 disables it for an A/B run.
INJECT_ARGS="inject --dll $DLL_WIN"
if [ "${HROMATITE_SEAL:-1}" = "1" ]; then
    INJECT_ARGS="$INJECT_ARGS --seal"
fi
LOG="$HOME/.hromatite-launch.log"
GAME_LOG="$HOME/.steam/steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser/hromatite.log"

echo "[hromatite] wrapper: $*" >>"$LOG"
"$@" &
GAME_PID=$!

if [ "${HROMATITE_DISABLE:-0}" = "1" ]; then
    wait "$GAME_PID"
    exit 0
fi

# The game's wine process (not the shell whose command line mentions it).
find_game_pid() {
    for p in $(pgrep -f "VRChat.exe" 2>/dev/null); do
        exe=$(readlink -f "/proc/$p/exe" 2>/dev/null || true)
        case "$exe" in
            *wine*) echo "$p"; return 0 ;;
        esac
    done
    return 1
}

(
    i=0
    GPID=""
    while [ "$i" -lt 120 ]; do
        if GPID=$(find_game_pid); then
            echo "[hromatite] game wine pid $GPID after ${i}s" >>"$LOG"
            break
        fi
        i=$((i + 1))
        sleep 1
    done
    sleep 3

    mode="${HROMATITE_MODE:-attach}"
    tries="${HROMATITE_TRIES:-3}"
    t=1
    while [ "$t" -le "$tries" ]; do
        echo "[hromatite] inject attempt $t/$tries (mode $mode)" >>"$LOG"
        if [ "$mode" = "proton" ] || [ -z "$GPID" ]; then
            "$PROTON" run "$EXE" $INJECT_ARGS >>"$LOG" 2>&1
        else
            ENVF=$(mktemp)
            # Values contain ';', spaces and '=' (WINEDLLOVERRIDES), so every
            # assignment is single-quoted with inner quotes escaped.
            tr '\0' '\n' <"/proc/$GPID/environ" 2>/dev/null \
                | grep -E "^(WINEPREFIX|WINEDLLPATH|WINEDLLOVERRIDES|WINEDEBUG|LD_LIBRARY_PATH|LD_PRELOAD|PATH|HOME|USER|TMPDIR|XDG_RUNTIME_DIR|DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|STEAM_COMPAT_DATA_PATH|STEAM_COMPAT_CLIENT_INSTALL_PATH)=" \
                | while IFS='=' read -r k v; do
                    esc=$(printf '%s' "$v" | sed "s/'/'\\\\''/g")
                    printf "export %s='%s'\n" "$k" "$esc"
                done >"$ENVF"
            WINE=$(command -v wine 2>/dev/null || echo "$DIST/bin/wine")
            OUT=$(mktemp)
            (
                set -a
                . "$ENVF"
                set +a
                echo "[hromatite] attach env: prefix=$WINEPREFIX wine=$WINE" >>"$LOG"
                "$WINE" "$EXE" $INJECT_ARGS
            ) >"$OUT" 2>&1
            cat "$OUT" >>"$LOG"
            # Old-style wow64 wine wants a 32-bit loader that only the runtime
            # container ships; outside it, fall back to proton's own wine setup.
            if grep -q "ld-linux.so.2: could not open" "$OUT"; then
                echo "[hromatite] attach mode unusable here, falling back to proton run" >>"$LOG"
                "$PROTON" run "$EXE" $INJECT_ARGS >>"$LOG" 2>&1
            fi
            rm -f "$OUT"
            rm -f "$ENVF"
        fi
        if grep -q "mapped at" "$LOG" 2>/dev/null; then
            echo "[hromatite] injected" >>"$LOG"
            break
        fi
        t=$((t + 1))
        sleep 12
    done
    echo "[hromatite] injector phase done" >>"$LOG"
) &

wait "$GAME_PID"
