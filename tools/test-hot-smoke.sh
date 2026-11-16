#!/bin/sh
# Real local-map/init/canary/retire under an isolated Proton prefix, never VRChat.
# The first run performs A/B swaps; the second proves that malformed input is a
# contained error rather than a process-killing AV. Logs are retained on purpose.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PREFIX=${1:-${PM_HOT_PREFIX:-"$HOME/.hermes/cache/scratch/pmhotsmoke3"}}
PROTON="$HOME/.local/share/Steam/steamapps/common/Proton 10.0/proton"
EXE="$ROOT/target/x86_64-pc-windows-gnu/release/Hromatite.exe"
LOG="$PREFIX/pfx/drive_c/users/steamuser/hromatite.log"
FEATURE_WIN='Z:\\home\\wilpdrake\\RustroverProjects\\Hromatite\\target\\x86_64-pc-windows-gnu\\release\\hromatite_features.dll'
[ "$ROOT" = "/home/wilpdrake/RustroverProjects/Hromatite" ] || {
    FEATURE_WIN="Z:$(printf '%s' "$ROOT/target/x86_64-pc-windows-gnu/release/hromatite_features.dll" | tr '/' '\\\\')"
}
export STEAM_COMPAT_DATA_PATH="$PREFIX"
export STEAM_COMPAT_CLIENT_INSTALL_PATH="$HOME/.local/share/Steam"
mkdir -p "$PREFIX"

printf '%s\n' "[smoke] four real feature generations in $PREFIX"
if ! timeout 180 "$PROTON" run "$EXE" hot-smoke 4 "$FEATURE_WIN" "$FEATURE_WIN" >"$PREFIX/stdout.log" 2>&1; then
    printf '%s\n' "[smoke] load/canary failed; see $PREFIX/stdout.log and $LOG" >&2
    exit 1
fi
[ -f "$LOG" ] || {
    printf '%s\n' "[smoke] payload log missing: $LOG" >&2
    exit 1
}
for n in 0 1 2 3; do
    grep -Fq "feature: generation=$n abi=1" "$LOG" || {
        printf '%s\n' "[smoke] missing real canary generation $n" >&2
        exit 1
    }
done
grep -Fq "smoke: final shutdown/retire" "$LOG" || {
    printf '%s\n' "[smoke] final retirement missing" >&2
    exit 1
}
if grep -Fq "exception 0xc0000005" "$LOG"; then
    printf '%s\n' "[smoke] access violation found in $LOG" >&2
    exit 1
fi

printf '%s' 'not-a-pe' >"$PREFIX/not-pe.dll"
if timeout 180 "$PROTON" run "$EXE" hot-smoke 1 "Z:\\not\\pe.dll" "$FEATURE_WIN" >"$PREFIX/invalid.log" 2>&1; then
    printf '%s\n' '[smoke] malformed image unexpectedly loaded' >&2
    exit 1
fi
printf '%s\n' "[smoke] A/B generations and retirement OK; malformed input rejected; log: $LOG"
