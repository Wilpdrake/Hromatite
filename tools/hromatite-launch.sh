#!/bin/sh
# shellcheck disable=SC1007,SC1003  # `CDPATH= cd` idiom; backslashes are wine path separators
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
# Timing matters: the game registers its EOSSDK callbacks (AddNotifyMessageToServer)
# around T+20 s and caches the pointers, so the payload must be mapped *before*
# that. This wrapper therefore polls /proc for the game's wine process every
# 100 ms and injects on first sight; the "runtime ready" LIVE line is no longer
# on the critical path (it is only watched in the background for the console).
#
#   HROMATITE_DISABLE=1    boot clean
#   HROMATITE_MODE=proton  fall back to `proton run`
#   HROMATITE_TRIES=n      injection attempts (default 3)
#   HROMATITE_POLL_MS=n    /proc poll period (default 100)
#   HROMATITE_FIND_MS=n    how long to look for the game process (default 120000)
#   HROMATITE_VERIFY_MS=n  how long to wait for a *single* inject to show up
#                          in hromatite.log (default 3000)
#   HROMATITE_MODE_RESTORE=0  do not put ~/.hromatite-mode back on exit

set -u
# --- millisecond clock -------------------------------------------------------
# coreutils >= 8.31 supports %3N; fall back to whole seconds if it does not.
now_ms() {
    v=$(date +%s%3N 2>/dev/null || true)
    case "${v:-}" in
        '' | *[!0-9]*) echo $(( $(date +%s) * 1000 )) ;;
        *) echo "$v" ;;
    esac
}
T0_MS=$(now_ms)
# Milliseconds since this wrapper started — every timestamp we print uses it.
rel_ms() { echo $(( $(now_ms) - T0_MS )); }

# Steam launches the game from the client's environment, so an A/B run cannot be
# selected by exporting variables in a shell. Read them from a file instead:
#   echo HROMATITE_DISABLE=1 > ~/.hromatite-mode
#   echo HROMATITE_LIVE=0 > ~/.hromatite-mode   (one KEY=VALUE per line)
# Absent/empty file = normal injected run.
MODE_FILE="${HROMATITE_MODE_FILE:-$HOME/.hromatite-mode}"
if [ -r "$MODE_FILE" ]; then
    while IFS= read -r line || [ -n "$line" ]; do
        # shellcheck disable=SC2163  # KEY=VALUE from the mode file is exported as-is
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
# Initial generation 0 comes from this checkout. Subsequent generations are
# immutable published artifacts under the user profile, loaded by the resident.
FEATURE_DLL_WIN="${HROMATITE_FEATURE_DLL:-Z:$(printf '%s' "$ROOT/target/x86_64-pc-windows-gnu/release/hromatite_features.dll" | tr '/' '\\\\')}"
export HROMATITE_FEATURE_DLL="$FEATURE_DLL_WIN"
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
# Emitted only after a managed or native hook plus the inline GC guard. Not on
# the critical path any more: the console marker does not wait for it.
LIVE_PAT='(LateUpdate|PlayerLoop) runtime ready: hooked entry .* inline GC guard installed'
# Appeared in hromatite.log the moment the payload image is mapped — this is the
# cheap per-attempt confirmation; it arrives seconds before the LIVE line.
INJECTED_PAT='mapped at 0x|payload thread started|DllMain\(PROCESS_ATTACH\)'
LOG="$HOME/.hromatite-launch.log"
PFX="${WINEPREFIX:-$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx}"
GAME_LOG="$PFX/drive_c/users/steamuser/hromatite.log"

# --- ~/.hromatite-mode is shared state --------------------------------------
# A live run (tools/live_run.sh) rewrites this file before launching; put back
# what we found so a killed run cannot leave the next boot in test mode.
MODE_BAK=""
MODE_SHA0=""
if [ -e "$MODE_FILE" ]; then
    MODE_SHA0=$(sha256sum "$MODE_FILE" 2>/dev/null | cut -d' ' -f1)
    MODE_BAK="$MODE_FILE.launch-bak.$$"
    cp -a "$MODE_FILE" "$MODE_BAK" 2>/dev/null || MODE_BAK=""
fi
restore_mode() {
    [ "${HROMATITE_MODE_RESTORE:-1}" = "1" ] || return 0
    [ -n "$MODE_SHA0" ] || return 0
    [ -n "$MODE_BAK" ] && [ -f "$MODE_BAK" ] || return 0
    cur=$([ -e "$MODE_FILE" ] && sha256sum "$MODE_FILE" 2>/dev/null | cut -d' ' -f1 || echo "")
    if [ "$cur" = "$MODE_SHA0" ]; then
        rm -f "$MODE_BAK"
        return 0
    fi
    if cp -a "$MODE_BAK" "$MODE_FILE" 2>/dev/null; then
        now=$(sha256sum "$MODE_FILE" 2>/dev/null | cut -d' ' -f1)
        echo "[hromatite] mode restored on exit: ${cur:-ABSENT} -> $now" >>"$LOG"
    else
        echo "[hromatite] mode restore FAILED; backup kept: $MODE_BAK" >>"$LOG"
    fi
    rm -f "$MODE_BAK" 2>/dev/null
}
trap restore_mode EXIT

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

echo "[hromatite] wrapper: $* (t0=$T0_MS)" >>"$LOG"
echo "[hromatite] game output -> $GAME_OUT, gc log backup -> $DBG_DIR/gc-$TS-before.log" >>"$LOG"
"$@" >>"$GAME_OUT" 2>&1 &
GAME_PID=$!

if [ "${HROMATITE_DISABLE:-0}" = "1" ]; then
    echo "[hromatite] disabled: no injection (t0=$T0_MS)" >>"$LOG"
    wait "$GAME_PID"
    exit 0
fi

# --- find the game's wine process -------------------------------------------
# One grep over /proc/*/cmdline (no fork per pid), then a handful of reads for
# the few matches. A process counts only if *its own* argv[0] is the game
# binary: the wrapper shell's command line also mentions VRChat.exe.
# Output: "<prio> <pid> <name>" sorted, VRChat.exe (prio 0) first.
scan_game_pids() {
    # shellcheck disable=SC2013  # paths of /proc/*/cmdline: no spaces, no newlines
    for f in $(grep -laE 'VRChat\.exe|launch\.exe|start_protected_game\.exe' \
        /proc/[0-9]*/cmdline 2>/dev/null); do
        p=${f#/proc/}
        p=${p%/cmdline}
        [ "$p" = "$$" ] && continue
        hitname=""
        # 1) the process *is* the game binary (wine sets /proc/pid/exe to the PE)
        exe=$(readlink "/proc/$p/exe" 2>/dev/null || true)
        ebase=${exe##*\\}
        ebase=${ebase##*/}
        elow=$(printf '%s' "$ebase" | tr '[:upper:]' '[:lower:]')
        case "$elow" in
            vrchat.exe | launch.exe | start_protected_game.exe) hitname=$elow ;;
        esac
        # 2) otherwise look at argv: the game path, or `wine <game.exe>`. A shell
        #    that merely mentions the exe on its command line (our own wrapper,
        #    proton's bash script) is not the game.
        if [ -z "$hitname" ]; then
            argv=$([ -r "$f" ] && tr '\0' '\n' <"$f" 2>/dev/null)
            argv0=$(printf '%s\n' "$argv" | head -1)
            a0=${argv0##*\\}
            a0=${a0##*/}
            a0low=$(printf '%s' "$a0" | tr '[:upper:]' '[:lower:]')
            case "$a0low" in
                vrchat.exe | launch.exe | start_protected_game.exe) hitname=$a0low ;;
                wine | wine64 | wine-preloader | wine64-preloader)
                    for a in $(printf '%s\n' "$argv" | tail -n +2); do
                        b=${a##*\\}
                        b=${b##*/}
                        bl=$(printf '%s' "$b" | tr '[:upper:]' '[:lower:]')
                        case "$bl" in
                            vrchat.exe | launch.exe | start_protected_game.exe)
                                hitname=$bl
                                break
                                ;;
                        esac
                    done
                    ;;
            esac
        fi
        [ -n "$hitname" ] || continue
        if [ "$hitname" = "vrchat.exe" ]; then prio=0; else prio=1; fi
        # Same prefix only: another Proton app has its own WINEPREFIX.
        wp=$(tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | sed -n 's/^WINEPREFIX=//p' | head -1)
        wp=${wp%/}
        if [ -n "$wp" ]; then
            [ "$wp" = "${PFX%/}" ] || continue
        else
            # No environ to read (very early / other user): require wine-ish exe.
            exe=$(readlink "/proc/$p/exe" 2>/dev/null || true)
            case "$exe" in
                *wine* | *Wine* | *.exe | *.EXE) ;;
                *\\*) ;;
                *) continue ;;
            esac
        fi
        printf '%s %s %s\n' "$prio" "$p" "$hitname"
    done | sort -n -k1,1 -k2,2
}

(
    poll_ms="${HROMATITE_POLL_MS:-100}"
    find_ms="${HROMATITE_FIND_MS:-120000}"
    verify_ms="${HROMATITE_VERIFY_MS:-3000}"
    grace_ms="${HROMATITE_GRACE_MS:-30000}"
    tries="${HROMATITE_TRIES:-3}"
    mode="${HROMATITE_MODE:-attach}"

    # Маркер для хостового сторожа автоконсоли (tools/hromatite-autoconsole.sh).
    mark_live() {
        # HROMATITE_CONSOLE=0 — не поднимать окно консоли автоматически
        [ "${HROMATITE_CONSOLE:-1}" = "1" ] || return 0
        printf '%s %s\n' "$(date +%s)" "$PFX" >"$HOME/.hromatite-live" 2>/dev/null &&
            echo "[hromatite] live marker written at +$(rel_ms) ms" >>"$LOG"
    }

    # Новый хвост hromatite.log: только то, что дописал этот запуск.
    SZ0=$([ -f "$GAME_LOG" ] && wc -c <"$GAME_LOG" 2>/dev/null || echo 0)
    case "$SZ0" in '' | *[!0-9]*) SZ0=0 ;; esac
    new_tail() { tail -c +$((SZ0 + 1)) "$GAME_LOG" 2>/dev/null; }

    wait_pat() { # $1 regex, $2 seconds
        n=0
        while [ "$n" -lt "$2" ]; do
            # Игра закрылась — ждать больше нечего (иначе сторож живёт впустую).
            kill -0 "$GAME_PID" 2>/dev/null || return 1
            new_tail | grep -aqE "$1" && return 0
            n=$((n + 1))
            sleep 1
        done
        return 1
    }

    # Ожидание сигнала об инжекте с шагом 100 мс — это не «готовность хуков»,
    # а факт маппинга образа, который появляется почти сразу.
    wait_injected() { # $1 milliseconds
        left=$1
        while [ "$left" -gt 0 ]; do
            new_tail | grep -aqE "$INJECTED_PAT" && return 0
            sleep 0.1 2>/dev/null || sleep 1
            left=$((left - poll_ms))
        done
        return 1
    }

    # $1 pid, $2 mode (attach|proton) — runs the injector, waits for the marker.
    do_inject() {
        ipid=$1
        imode=$2
        if [ "$imode" = "proton" ]; then
            echo "[hromatite] inject (proton run) for pid $ipid at +$(rel_ms) ms" >>"$LOG"
            # shellcheck disable=SC2086  # INJECT_ARGS is a word list on purpose
            "$PROTON" run "$EXE" $INJECT_ARGS >>"$LOG" 2>&1 &
        else
            ENVF=$(mktemp)
            # Values contain ';', spaces and '=' (WINEDLLOVERRIDES), so every
            # assignment is single-quoted with inner quotes escaped.
            tr '\0' '\n' <"/proc/$ipid/environ" 2>/dev/null \
                | grep -E "^(WINEPREFIX|WINEDLLPATH|WINEDLLOVERRIDES|WINEDEBUG|LD_LIBRARY_PATH|LD_PRELOAD|PATH|HOME|USER|TMPDIR|XDG_RUNTIME_DIR|DISPLAY|WAYLAND_DISPLAY|XAUTHORITY|STEAM_COMPAT_DATA_PATH|STEAM_COMPAT_CLIENT_INSTALL_PATH)=" \
                | while IFS='=' read -r k v; do
                    esc=$(printf '%s' "$v" | sed "s/'/'\\\\''/g")
                    printf "export %s='%s'\n" "$k" "$esc"
                done >"$ENVF"
            WINE=$(command -v wine 2>/dev/null || echo "$DIST/bin/wine")
            OUT=$(mktemp)
            echo "[hromatite] inject (attach) into pid $ipid at +$(rel_ms) ms (wine=$WINE)" >>"$LOG"
            (
                set -a
                # shellcheck disable=SC1090  # generated: KEY='value' lines
                . "$ENVF"
                set +a
                echo "[hromatite] attach env: prefix=$WINEPREFIX wine=$WINE" >>"$LOG"
                # shellcheck disable=SC2086  # INJECT_ARGS is a word list on purpose
                exec "$WINE" "$EXE" $INJECT_ARGS
            ) >"$OUT" 2>&1 &
        fi
        ok=0
        wait_injected "$verify_ms" && ok=1
        if [ "$ok" = "1" ]; then
            echo "[hromatite] inject ok for pid $ipid at +$(rel_ms) ms" >>"$LOG"
        else
            # Не блокируемся до конца: инжектор может ещё работать.
            grep -aq "ld-linux.so.2: could not open" "$OUT" 2>/dev/null && ok=2
            echo "[hromatite] inject not confirmed for pid $ipid after ${verify_ms} ms" >>"$LOG"
        fi
        [ -n "${ENVF:-}" ] && rm -f "$ENVF"
        [ -n "${OUT:-}" ] && {
            cat "$OUT" >>"$LOG"
            rm -f "$OUT"
        }
        ENVF=""
        OUT=""
        [ "$ok" = "1" ] && return 0
        [ "$ok" = "2" ] && return 2
        return 1
    }

    t_proc=""
    deadline=$(( $(now_ms) + find_ms ))
    idle_deadline=$deadline
    tied=" "
    attempt=0
    inject_ok=0

    while [ "$(now_ms)" -lt "$idle_deadline" ]; do
        # Игра уже вышла — искать некого.
        kill -0 "$GAME_PID" 2>/dev/null || break
        # First candidate we have not tried yet; awk keeps whole lines intact
        # (a `for x in $(...)` would split "prio pid name" into words).
        hit=$(scan_game_pids | awk -v tied="$tied" 'index(tied, " " $2 " ") == 0 { print; exit }')
        if [ -z "$hit" ]; then
            sleep 0.1 2>/dev/null || sleep 1
            continue
        fi
        c_pid=$(printf '%s' "$hit" | cut -d' ' -f2)
        c_name=$(printf '%s' "$hit" | cut -d' ' -f3)
        tied="$tied$c_pid "
        if [ -z "$t_proc" ]; then
            t_proc=$(rel_ms)
            echo "[hromatite] game process $c_name pid $c_pid at +${t_proc} ms" >>"$LOG"
        fi
        attempt=$((attempt + 1))
        do_inject "$c_pid" "$mode"
        rc=$?
        if [ "$rc" = "2" ] && [ "$mode" = "attach" ]; then
            echo "[hromatite] attach mode unusable here, falling back to proton run" >>"$LOG"
            mode="proton"
            if do_inject "$c_pid" "proton"; then rc=0; fi
        fi
        if [ "$rc" = "0" ]; then
            inject_ok=1
            t_inject=$(rel_ms)
            echo "[hromatite] timing: t0=$T0_MS t_proc=+${t_proc}ms t_inject=+${t_inject}ms target=$c_name pid=$c_pid attempt=$attempt" >>"$LOG"
            printf '[hromatite] t0=%s t_proc=+%sms t_inject=+%sms target=%s pid=%s attempt=%s\n' \
                "$T0_MS" "$t_proc" "$t_inject" "$c_name" "$c_pid" "$attempt" >&2
            mark_live
            # VRChat.exe is the process EAC hooks into; a bootstrapper only
            # carries the payload until it execs the real game.
            [ "$c_name" = "vrchat.exe" ] && break
            # Keep looking for the real game for a while, but do not reset the
            # attempt budget: bootstrappers are cheap, VRChat.exe is what counts.
            idle_deadline=$(( $(now_ms) + grace_ms ))
        fi
        [ "$attempt" -ge "$tries" ] && break
    done

    if [ "$inject_ok" = "0" ]; then
        echo "[hromatite] no inject succeeded (tries=$tries, mode=$mode) after +$(rel_ms) ms" >>"$LOG"
    fi
    # Наблюдение после инжекта — не на критическом пути:
    #   1) `hooks installed N/M`: инжект мог приехать раньше, чем игра успела
    #      загрузить EOSSDK/GameAssembly, и тогда хуки встанут позже (или не
    #      встанут — это видно здесь и в tools/eac_probe.sh);
    #   2) LIVE-строка — только для протокола и маркера консоли.
    (
        if wait_pat 'hooks installed [0-9]+/[0-9]+' 60; then
            echo "[hromatite] hooks at +$(rel_ms) ms: $(new_tail | grep -aE 'hooks installed' | tail -1)" >>"$LOG"
        elif kill -0 "$GAME_PID" 2>/dev/null; then
            echo "[hromatite] no 'hooks installed' line within 60 s (injected before module load?)" >>"$LOG"
        else
            echo "[hromatite] game exited before any 'hooks installed' line" >>"$LOG"
        fi
        wait_pat "$LIVE_PAT" 180 &&
            echo "[hromatite] runtime ready (LIVE) at +$(rel_ms) ms" >>"$LOG" &&
            mark_live
    ) &
    echo "[hromatite] injector phase done at +$(rel_ms) ms" >>"$LOG"
) </dev/null >>"$LOG" 2>&1 &

wait "$GAME_PID"

# The GC log only ever gets content when the collector dies, so keep this run's
# copy with its own timestamp, next to the game stdout capture.
if [ -f "$GC_LOG" ]; then
    cp -f "$GC_LOG" "$DBG_DIR/gc-$TS-after.log" 2>/dev/null &&
        echo "[hromatite] gc log kept: $DBG_DIR/gc-$TS-after.log ($(wc -c <"$GC_LOG") bytes)" >>"$LOG"
fi
echo "[hromatite] game exited at +$(rel_ms) ms; output: $GAME_OUT" >>"$LOG"
