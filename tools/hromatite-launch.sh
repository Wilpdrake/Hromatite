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

# Steam launches the game from the client's environment, so an A/B run cannot be
# selected by exporting variables in a shell. Read them from a file instead:
#   echo HROMATITE_DISABLE=1 > ~/.hromatite-mode
#   echo HROMATITE_LIVE=0 > ~/.hromatite-mode   (one KEY=VALUE per line)
# Absent/empty file = normal injected run.
MODE_FILE="${HROMATITE_MODE_FILE:-$HOME/.hromatite-mode}"
if [ -r "$MODE_FILE" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in
            '' | \#*) continue ;;
            *=*) export "$line" ;;
        esac
    done <"$MODE_FILE"
fi

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/.." && pwd)
PROTON="${HROMATITE_PROTON:-$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/proton}"
DIST=$(dirname "$PROTON")/files
BIN="$ROOT/target/x86_64-pc-windows-gnu/release"
EXE="$BIN/Hromatite.exe"
# The DLL to inject always comes from *this* checkout (so a worktree build can
# be tested by pointing Steam at that worktree's copy of this script).
DLL_WIN="${HROMATITE_DLL:-Z:$(printf '%s' "$ROOT/target/x86_64-pc-windows-gnu/release/hromatite.dll" | tr '/' '\\\\')}"
# The managed LateUpdate or native PlayerLoop detour and GC guard live in the
# payload image, so every kind of full seal must stay disabled.
HROMATITE_FRAME_HOOK="${HROMATITE_FRAME_HOOK:-1}"
HROMATITE_LATE_UPDATE="${HROMATITE_LATE_UPDATE:-auto}"
HROMATITE_LIVE="${HROMATITE_LIVE:-0}"
HROMATITE_SEAL="${HROMATITE_SEAL:-0}"
HROMATITE_L2B="${HROMATITE_L2B:-0}"
HROMATITE_PLAYER_LOOP="${HROMATITE_PLAYER_LOOP:-0}"
HROMATITE_PLAYER_LOOP_TARGET="${HROMATITE_PLAYER_LOOP_TARGET:-Update}"
export HROMATITE_FRAME_HOOK HROMATITE_LATE_UPDATE HROMATITE_LIVE HROMATITE_SEAL
export HROMATITE_L2B HROMATITE_PLAYER_LOOP HROMATITE_PLAYER_LOOP_TARGET
if [ "$HROMATITE_FRAME_HOOK" != "1" ] || [ "$HROMATITE_LIVE" != "0" ] ||
    [ "$HROMATITE_SEAL" != "0" ] || [ "$HROMATITE_L2B" != "0" ]; then
    echo "[hromatite] unsupported legacy runtime mode: require FRAME_HOOK=1 LATE_UPDATE=auto/manual LIVE=0 SEAL=0 L2B=0" >&2
    exit 2
fi
INJECT_ARGS="inject --dll $DLL_WIN"
# Emitted only after a managed or native hook plus the inline GC guard.
LIVE_PAT='(LateUpdate|PlayerLoop) runtime ready: hooked entry .* inline GC guard installed'
LOG="$HOME/.hromatite-launch.log"
GAME_LOG="$HOME/.steam/steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser/hromatite.log"

# --- capture what the game itself prints ------------------------------------
# Unity writes "Fatal error in GC — SuspendThread loop failed" (and everything
# Boehm prints with GC_PRINT_STATS) to its own stdout/stderr, which Steam
# normally swallows. Keep it in a file next to the other logs.
# `VRChat.gc.log` is Unity's GC log: it only gets a line when the collector dies,
# and it is *overwritten* by the next run — so every launch makes a backup.
DBG_DIR="${HROMATITE_DBG_DIR:-$HOME/.hromatite-logs}"
mkdir -p "$DBG_DIR" 2>/dev/null
TS=$(date +%Y%m%d-%H%M%S)
GAME_OUT="$DBG_DIR/game-$TS.log"
GC_LOG="${HROMATITE_GCLOG:-$HOME/.local/share/Steam/steamapps/common/VRChat/VRChat.gc.log}"
[ -f "$GC_LOG" ] && cp -f "$GC_LOG" "$DBG_DIR/gc-$TS-before.log" 2>/dev/null
# Boehm's own diagnostics: per-collection stats, and the thread id / handle /
# errcode of the record that fails to suspend. Verbose is very loud, so it is
# off unless asked for.
export GC_PRINT_STATS="${GC_PRINT_STATS:-1}"
export GC_PRINT_VERBOSE_STATS="${GC_PRINT_VERBOSE_STATS:-0}"

echo "[hromatite] wrapper: $*" >>"$LOG"
echo "[hromatite] game output -> $GAME_OUT, gc log backup -> $DBG_DIR/gc-$TS-before.log" >>"$LOG"
"$@" >>"$GAME_OUT" 2>&1 &
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

    PFX="${WINEPREFIX:-$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx}"

    # Маркер для хостового сторожа автоконсоли (tools/hromatite-autoconsole.sh).
    mark_live() {
        DONE=1
        # HROMATITE_CONSOLE=0 — не поднимать окно консоли автоматически
        [ "${HROMATITE_CONSOLE:-1}" = "1" ] || return 0
        printf '%s %s\n' "$(date +%s)" "$PFX" >"$HOME/.hromatite-live" 2>/dev/null &&
            echo "[hromatite] live marker written" >>"$LOG"
    }

    # Сигнал готовности — успешный managed hook с уже установленным GC guard.
    # Смотрим только НОВЫЙ хвост файла, иначе совпадёт с прошлым запуском.
    SZ0=$(wc -c <"$GAME_LOG" 2>/dev/null || echo 0)
    new_tail() { tail -c +$((SZ0 + 1)) "$GAME_LOG" 2>/dev/null; }

    wait_live() { # $1 = сколько секунд
        n=0
        while [ "$n" -lt "$1" ]; do
            new_tail | grep -qE "$LIVE_PAT" && return 0
            n=$((n + 1))
            sleep 1
        done
        return 1
    }

    mode="${HROMATITE_MODE:-attach}"
    tries="${HROMATITE_TRIES:-3}"
    t=1
    while [ "$t" -le "$tries" ]; do
        echo "[hromatite] inject attempt $t/$tries (mode $mode)" >>"$LOG"
        if [ "$mode" = "proton" ] || [ -z "$GPID" ]; then
            "$PROTON" run "$EXE" $INJECT_ARGS >>"$LOG" 2>&1 &
            wait_live 180 && mark_live
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
                exec "$WINE" "$EXE" $INJECT_ARGS
            ) >"$OUT" 2>&1 &
            INJ=$!
            n=0
            while [ "$n" -lt 90 ]; do
                # успех определяем по строке payload'а в логе игры
                if new_tail | grep -qE "$LIVE_PAT"; then
                    mark_live
                    break
                fi
                # ...или по падению на отсутствующем 32-битном лоадере
                grep -q "ld-linux.so.2: could not open" "$OUT" 2>/dev/null && break
                kill -0 "$INJ" 2>/dev/null || break
                n=$((n + 1))
                sleep 1
            done
            if [ "${DONE:-0}" != "1" ]; then
                wait "$INJ" 2>/dev/null
                cat "$OUT" >>"$LOG"
                if grep -q "ld-linux.so.2: could not open" "$OUT"; then
                    echo "[hromatite] attach mode unusable here, falling back to proton run" >>"$LOG"
                    "$PROTON" run "$EXE" $INJECT_ARGS >>"$LOG" 2>&1 &
                    wait_live 180 && mark_live
                fi
            fi
            rm -f "$OUT"
            rm -f "$ENVF"
        fi
        if [ "${DONE:-0}" = "1" ]; then
            echo "[hromatite] injected (payload live)" >>"$LOG"
            break
        fi
        t=$((t + 1))
        sleep 12
    done
    echo "[hromatite] injector phase done" >>"$LOG"
) &

wait "$GAME_PID"

# The GC log only ever gets content when the collector dies, so keep this run's
# copy with its own timestamp, next to the game stdout capture.
if [ -f "$GC_LOG" ]; then
    cp -f "$GC_LOG" "$DBG_DIR/gc-$TS-after.log" 2>/dev/null &&
        echo "[hromatite] gc log kept: $DBG_DIR/gc-$TS-after.log ($(wc -c <"$GC_LOG") bytes)" >>"$LOG"
fi
echo "[hromatite] game exited; output: $GAME_OUT" >>"$LOG"
