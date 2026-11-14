#!/bin/sh
# Steam launch-options wrapper: %command% is executed *inside* the proton
# container, so this script (and the injector it starts) share the game's
# wineserver. Launched from outside (protontricks / proton run) wine keys its
# server socket on the prefix inode, which differs inside pressure-vessel, so
# the injector would see an empty process list.
#
# Steam -> VRChat -> Properties -> Launch Options:
#   /home/wilpdrake/RustroverProjects/Hromatite/tools/hromatite-launch.sh %command%
#
# HROMATITE_DISABLE=1  boot clean, no injector.
# HROMATITE_TRIES=n    injection attempts (default 5, 15s apart).

set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/.." && pwd)
PROTON="${HROMATITE_PROTON:-$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton}"
BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"
EXE="$BIN/Hromatite.exe"
DLL_WIN="Z:\\\\home\\\\wilpdrake\\\\RustroverProjects\\\\Hromatite\\\\target\\\\x86_64-pc-windows-gnu\\\\debug\\\\hromatite.dll"
LOG="$HOME/.hromatite-launch.log"
GAME_LOG="$HOME/.steam/steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser/hromatite.log"

echo "[hromatite] wrapper: $*" >>"$LOG"
"$@" &
GAME_PID=$!

if [ "${HROMATITE_DISABLE:-0}" = "1" ]; then
    wait "$GAME_PID"
    exit 0
fi

(
    # The game needs its own wineserver up before we can join it; wait for the
    # actual VRChat process instead of guessing with a fixed sleep.
    i=0
    while [ "$i" -lt 120 ]; do
        if pgrep -f "VRChat.exe" >/dev/null 2>&1; then
            echo "[hromatite] VRChat.exe seen after ${i}s" >>"$LOG"
            break
        fi
        i=$((i + 1))
        sleep 1
    done
    sleep 3

    tries="${HROMATITE_TRIES:-5}"
    t=1
    while [ "$t" -le "$tries" ]; do
        echo "[hromatite] inject attempt $t/$tries" >>"$LOG"
        if "$PROTON" run "$EXE" inject --dll "$DLL_WIN" >>"$LOG" 2>&1; then
            if grep -q "mapped at" "$GAME_LOG" 2>/dev/null; then
                echo "[hromatite] injected" >>"$LOG"
                break
            fi
        fi
        t=$((t + 1))
        sleep 15
    done
    echo "[hromatite] injector phase done" >>"$LOG"
) &

wait "$GAME_PID"
