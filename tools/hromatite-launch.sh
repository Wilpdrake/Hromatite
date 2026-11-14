#!/bin/sh
# Steam launch-options wrapper: %command% is executed *inside* the proton
# container, so this script (and the injector it starts) share the game's
# wineserver. Started from outside (protontricks/proton run) wine keys its
# server socket on the prefix inode, which differs inside pressure-vessel.
#
# Steam -> VRChat -> Properties -> Launch Options:
#   /home/wilpdrake/RustroverProjects/Hromatite/tools/hromatite-launch.sh %command%
#
# Set HROMATITE_DISABLE=1 to boot clean (no injector).

set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/.." && pwd)
PROTON="${HROMATITE_PROTON:-$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton}"
EXE="$ROOT/target/x86_64-pc-windows-gnu/debug/Hromatite.exe"
DLL_WIN="Z:\\home\\wilpdrake\\RustroverProjects\\Hromatite\\target\\x86_64-pc-windows-gnu\\debug\\hromatite.dll"
LOG="$HOME/.hromatite-launch.log"

echo "[hromatite] wrapper: $*" >>"$LOG"
"$@" &
GAME_PID=$!

if [ "${HROMATITE_DISABLE:-0}" != "1" ]; then
    (
        sleep "${HROMATITE_DELAY:-20}"
        echo "[hromatite] injecting after ${HROMATITE_DELAY:-20}s" >>"$LOG"
        "$PROTON" run "$EXE" inject --dll "$DLL_WIN" >>"$LOG" 2>&1
        echo "[hromatite] injector exit $?" >>"$LOG"
    ) &
fi

wait "$GAME_PID"
