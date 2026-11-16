#!/bin/sh
# Opt-in polling watcher. It never runs as a side effect of the TUI.
set -u
umask 077

PROJECT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
RELOAD_BIN=${HROMATITE_RELOAD_BIN:-$PROJECT/tools/hromatite-reload.sh}
WATCH_ROOT=${HROMATITE_WATCH_ROOT:-$PROJECT}
INTERVAL=${HROMATITE_WATCH_INTERVAL:-2}
STABLE_POLLS=${HROMATITE_WATCH_STABLE_POLLS:-2}
ONCE=0
NO_BUILD=0

fail() {
    printf 'hromatite-watch: %s\n' "$*" >&2
    exit 2
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --once) ONCE=1 ;;
        --no-build) NO_BUILD=1 ;;
        --interval)
            shift
            [ "$#" -gt 0 ] || fail '--interval needs a non-negative integer'
            INTERVAL=$1
            ;;
        --stable-polls)
            shift
            [ "$#" -gt 0 ] || fail '--stable-polls needs a positive integer'
            STABLE_POLLS=$1
            ;;
        -h|--help)
            printf '%s\n' 'usage: hromatite-watch.sh [--interval N] [--stable-polls N] [--once] [--no-build]'
            exit 0
            ;;
        *) fail "unknown option: $1" ;;
    esac
    shift
done
case $INTERVAL in ''|*[!0-9]*) fail '--interval needs a non-negative integer' ;; esac
case $STABLE_POLLS in ''|*[!0-9]*|0) fail '--stable-polls needs a positive integer' ;; esac
[ -f "$RELOAD_BIN" ] || fail "reload tool not found: $RELOAD_BIN"

source_fingerprint() {
    {
        find "$WATCH_ROOT/crates/hromatite-features" \
             "$WATCH_ROOT/crates/hromatite-abi" \
             -type f -exec sha256sum {} + || return 1
        for file in "$WATCH_ROOT/Cargo.toml" "$WATCH_ROOT/Cargo.lock"; do
            if [ -f "$file" ]; then
                sha256sum "$file" || return 1
            fi
        done
    } | LC_ALL=C sort | sha256sum | { read -r digest _; printf '%s\n' "$digest"; }
}

printf 'watching %s (interval=%ss stable=%s); manual reload remains available\n' \
    "$WATCH_ROOT" "$INTERVAL" "$STABLE_POLLS"
baseline=$(source_fingerprint) || fail 'cannot fingerprint source tree'
pending=
pending_count=0
while :; do
    current=$(source_fingerprint) || {
        printf 'hromatite-watch: source fingerprint failed; keeping current generation\n' >&2
        sleep "$INTERVAL"
        continue
    }
    if [ "$current" != "$baseline" ]; then
        if [ "$current" = "$pending" ]; then
            pending_count=$((pending_count + 1))
        else
            pending=$current
            pending_count=1
        fi
        if [ "$pending_count" -ge "$STABLE_POLLS" ]; then
            printf 'source change stabilized; running hromatite-reload\n'
            status=0
            if [ "$NO_BUILD" -eq 1 ]; then
                sh "$RELOAD_BIN" --no-build || status=$?
            else
                sh "$RELOAD_BIN" || status=$?
            fi
            if [ "$status" -eq 0 ]; then
                printf 'reload publication succeeded\n'
                # Only a published generation retires the source fingerprint.
                # Otherwise the same stable change remains eligible for retry.
                baseline=$pending
            else
                printf 'hromatite-watch: reload failed (status=%s); current generation preserved\n' \
                    "$status" >&2
            fi
            pending=
            pending_count=0
            [ "$ONCE" -eq 0 ] || exit "$status"
        fi
    else
        pending=
        pending_count=0
    fi
    sleep "$INTERVAL"
done
