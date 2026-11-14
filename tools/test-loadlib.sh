#!/bin/sh
# Self-contained rig for the loadlib + PEB-unlink path.
#
# Both `proton run` invocations must happen in ONE shell: separate invocations
# end up on different wine servers and the injector sees an empty process list.
#
#   sh tools/test-loadlib.sh [prefix-name]
#
set -u

NAME="${1:-pmtestX}"
PREFIX="/home/wilpdrake/.hermes/cache/scratch/$NAME"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PROTON="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton"
DLL_WIN='Z:\home\wilpdrake\RustroverProjects\Hromatite\target\x86_64-pc-windows-gnu\release\hromatite.dll'
EXE="$ROOT/target/x86_64-pc-windows-gnu/release/Hromatite.exe"
LOG="$PREFIX/pfx/drive_c/users/steamuser/hromatite.log"

mkdir -p "$PREFIX"
[ -f "$PREFIX/VRChat.exe" ] || cp "$ROOT/target/x86_64-pc-windows-gnu/debug/test-target.exe" "$PREFIX/VRChat.exe"

export STEAM_COMPAT_DATA_PATH="$PREFIX"
export STEAM_COMPAT_CLIENT_INSTALL_PATH="$HOME/.local/share/Steam"
export HROMATITE_WAIT="${HROMATITE_WAIT:-3}"

echo "[rig] starting stand-in game in $PREFIX"
"$PROTON" run "$PREFIX/VRChat.exe" >/dev/null 2>&1 &
GAME=$!

sleep 35
# proton holds a prefix lock while its own `run` is alive; the wine server and
# the game keep running after the python wrapper dies, so drop the wrapper.
kill "$GAME" 2>/dev/null
# and the python wrapper that carries proton's prefix lock
pkill -f "proton run $PREFIX/VRChat.exe" 2>/dev/null
sleep 3
rm -f "$LOG"
echo "[rig] injecting (loadlib)"
timeout 90 "$PROTON" run "$EXE" inject --mode loadlib --dll "$DLL_WIN" 2>&1 \
    | grep -viE "freetype|_r_debug|equal to|^$|wine cannot|gameoverlay|nvngx|ld.so" | tail -8

sleep 5
echo "=== payload log ==="
grep -E "DllMain|peb|loadlib|mapped|diag:" "$LOG" 2>/dev/null | tail -12

echo "[rig] stopping game"
kill "$GAME" 2>/dev/null
pkill -f "$NAME/VRChat.exe" 2>/dev/null
