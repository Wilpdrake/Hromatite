#!/bin/sh
# Rig for the L2 "seal" path, run the way Steam runs the game: inside the
# SteamLinuxRuntime (pressure-vessel) container, then injecting through the
# *game's own* wine environment (/proc/<pid>/environ).
#
# Outside that container `proton run <injector>` hangs while a wineserver for
# the prefix is already live, and plain `wine` dies on the missing 32-bit
# loader (/lib/ld-linux.so.2), so the container is not optional.
#
#   sh tools/test-seal.sh [prefix-name]
#
set -u

NAME="${1:-pmsealX}"
PREFIX="/home/wilpdrake/.hermes/cache/scratch/$NAME"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PROTON="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton"
SLR="$HOME/.local/share/Steam/steamapps/common/SteamLinuxRuntime_4/_v2-entry-point"
WINE="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/files/bin/wine"
DLL_WIN='Z:\home\wilpdrake\RustroverProjects\Hromatite\target\x86_64-pc-windows-gnu\release\hromatite.dll'
EXE="$ROOT/target/x86_64-pc-windows-gnu/release/Hromatite.exe"
LOG="$PREFIX/pfx/drive_c/users/steamuser/hromatite.log"

mkdir -p "$PREFIX"
[ -f "$PREFIX/VRChat.exe" ] || cp "$ROOT/target/x86_64-pc-windows-gnu/debug/test-target.exe" "$PREFIX/VRChat.exe"

# kill by pattern without hitting our own shell (the pattern sits in the
# caller's command line too)
kill_pat() {
    pgrep -f "$1" 2>/dev/null | grep -vx -e "$$" -e "$PPID" | xargs -r kill 2>/dev/null
}

export STEAM_COMPAT_DATA_PATH="$PREFIX"
export STEAM_COMPAT_CLIENT_INSTALL_PATH="$HOME/.local/share/Steam"
export HROMATITE_WAIT="${HROMATITE_WAIT:-2}"

echo "[rig] booting stand-in game inside SteamLinuxRuntime ($PREFIX)"
"$SLR" --verb=waitforexitandrun -- "$PROTON" waitforexitandrun "$PREFIX/VRChat.exe" >/tmp/rig-game.log 2>&1 &

i=0
GPID=""
while [ "$i" -lt 90 ]; do
    for p in $(pgrep -f "$NAME/VRChat.exe" 2>/dev/null); do
        case "$(readlink -f "/proc/$p/exe" 2>/dev/null)" in
            *wine*) GPID="$p"; break ;;
        esac
    done
    [ -n "$GPID" ] && break
    i=$((i + 1))
    sleep 1
done
echo "[rig] game wine pid ${GPID:-?} after ${i}s"
[ -z "$GPID" ] && { tail -5 /tmp/rig-game.log; exit 1; }
sleep 3
rm -f "$LOG"

echo "[rig] inject (attach env, manual map + --seal)"
ENVF=$(mktemp)
tr '\0' '\n' <"/proc/$GPID/environ" \
    | grep -E "^(WINEPREFIX|WINEDLLPATH|WINEDLLOVERRIDES|WINEDEBUG|LD_LIBRARY_PATH|LD_PRELOAD|PATH|HOME|USER|TMPDIR|XDG_RUNTIME_DIR|DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|STEAM_COMPAT_DATA_PATH|STEAM_COMPAT_CLIENT_INSTALL_PATH)=" \
    | while IFS='=' read -r k v; do
        esc=$(printf '%s' "$v" | sed "s/'/'\\\\''/g")
        printf "export %s='%s'\n" "$k" "$esc"
      done >"$ENVF"
(
    set -a
    . "$ENVF"
    set +a
    timeout 180 "$WINE" "$EXE" inject --seal --dll "$DLL_WIN"
) 2>&1 | grep -viE "freetype|_r_debug|equal to|^$|wine cannot|http|gameoverlay|ld.so" | tail -10
rm -f "$ENVF"

sleep 2
echo "=== injector log ==="
grep -E "mapped at|seal:|DllMain|diag" "$LOG" 2>/dev/null | tail -12

ADDR=$(grep -oE "mapped at 0x[0-9a-f]+" "$LOG" 2>/dev/null | tail -1 | sed 's/.*0x//')
echo "=== host view of ${ADDR:-?} (from /proc) ==="
if [ -n "${ADDR:-}" ]; then
    PID=$(grep -l "^$ADDR" /proc/*/maps 2>/dev/null | head -1 | cut -d/ -f3)
    echo "host pid: ${PID:-?}"
    if [ -n "${PID:-}" ]; then
        grep "^$ADDR" /proc/"$PID"/maps | head -3
        echo -n "first 16 bytes: "
        dd if=/proc/"$PID"/mem bs=1 skip=$((0x$ADDR)) count=16 2>/dev/null | od -A n -t x1 | head -1
    fi
fi

echo "[rig] stopping game"
kill_pat "$NAME/VRChat.exe"
kill_pat "_v2-entry-point"
