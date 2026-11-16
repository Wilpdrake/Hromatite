#!/bin/sh
# Build and atomically publish one immutable feature generation.
set -eu
umask 077

PROJECT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
PREFIX=${HROMATITE_PREFIX:-${PREFIX:-"$HOME/.local/share/Steam/steamapps/compatdata/438100/pfx"}}
USER_DIR=$PREFIX/drive_c/users/steamuser
MODULE_DIR=${HROMATITE_MODULE_DIR:-${MODULE_DIR:-$USER_DIR/hromatite-modules}}
CMD_FILE=${HROMATITE_CMD_FILE:-${CMD_FILE:-$USER_DIR/hromatite-cmd.txt}}
MANIFEST_FILE=$MODULE_DIR/current.manifest
ARTIFACT=${HROMATITE_ARTIFACT:-$PROJECT/target/x86_64-pc-windows-gnu/release/hromatite_features.dll}
GAME_ASSEMBLY=${HROMATITE_GAME_ASSEMBLY:-${GAME_ASSEMBLY:-"$HOME/.local/share/Steam/steamapps/common/VRChat/GameAssembly.dll"}}
EXPECTED_GAME_SHA=${HROMATITE_GAME_SHA256:-}
NO_BUILD=0
DRY_RUN=0
GENERATION_OVERRIDE=

fail() {
    printf 'hromatite-reload: %s\n' "$*" >&2
    exit 1
}

sha256_file() {
    sha256sum "$1" | { read -r digest _; printf '%s\n' "$digest"; }
}

valid_sha() {
    [ "${#1}" -eq 64 ] || return 1
    case $1 in *[!0-9a-f]*) return 1 ;; esac
}

valid_shell_path() {
    case $1 in
        /*) ;;
        *) return 1 ;;
    esac
    case $1 in *[!A-Za-z0-9_./:+,=@%-]*) return 1 ;; esac
}

read_current_manifest() {
    [ -f "$MANIFEST_FILE" ] && [ ! -L "$MANIFEST_FILE" ] ||
        fail 'malformed current.manifest: not a regular file'
    OLD_GENERATION= OLD_SHA= OLD_FILE= OLD_GAME_SHA= OLD_ABI=
    line_number=0 key= value=
    while IFS='=' read -r key value || [ -n "${key:-}${value:-}" ]; do
        line_number=$((line_number + 1))
        case "$line_number:$key" in
            1:GENERATION) OLD_GENERATION=$value ;;
            2:SHA256) OLD_SHA=$value ;;
            3:FILE) OLD_FILE=$value ;;
            4:GAME_SHA256) OLD_GAME_SHA=$value ;;
            5:ABI) OLD_ABI=$value ;;
            *) fail "malformed current.manifest: unexpected line $line_number" ;;
        esac
        key= value=
    done <"$MANIFEST_FILE"
    [ "$line_number" -eq 5 ] || fail 'malformed current.manifest: expected exactly five fields'
    case $OLD_GENERATION in
        ''|*[!0-9]*|0) fail 'malformed current.manifest: invalid GENERATION' ;;
    esac
    [ "${#OLD_GENERATION}" -le 9 ] || fail 'malformed current.manifest: GENERATION is too large'
    valid_sha "$OLD_SHA" || fail 'malformed current.manifest: invalid SHA256'
    valid_sha "$OLD_GAME_SHA" || fail 'malformed current.manifest: invalid GAME_SHA256'
    [ "$OLD_ABI" = 1 ] || fail 'malformed current.manifest: unsupported ABI'
    valid_shell_path "$OLD_FILE" || fail 'malformed current.manifest: FILE must be a shell-safe absolute path'
    short=$(printf '%.12s\n' "$OLD_SHA")
    [ "$OLD_FILE" = "$MODULE_DIR/features-$OLD_GENERATION-$short.dll" ] ||
        fail 'malformed current.manifest: FILE does not match GENERATION/SHA256'
    [ -f "$OLD_FILE" ] && [ ! -L "$OLD_FILE" ] ||
        fail 'malformed current.manifest: referenced DLL is missing or not regular'
    actual=$(sha256_file "$OLD_FILE")
    [ "$actual" = "$OLD_SHA" ] || fail 'malformed current.manifest: DLL hash does not match SHA256'
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --no-build) NO_BUILD=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --generation)
            shift
            [ "$#" -gt 0 ] || fail '--generation needs a positive integer'
            GENERATION_OVERRIDE=$1
            ;;
        -h|--help)
            printf '%s\n' 'usage: hromatite-reload.sh [--no-build] [--dry-run] [--generation N]'
            exit 0
            ;;
        *) fail "unknown option: $1" ;;
    esac
    shift
done

if [ -z "$EXPECTED_GAME_SHA" ]; then
    EXPECTED_GAME_SHA=$(sed -n 's/^pub const GAMEASSEMBLY_SHA256: &str = "\([0-9a-f]\{64\}\)";$/\1/p' \
        "$PROJECT/crates/hromatite-core/src/symbols.rs")
fi
valid_sha "$EXPECTED_GAME_SHA" ||
    fail 'expected GameAssembly SHA-256 must be 64 lowercase hex characters'
valid_shell_path "$MODULE_DIR" ||
    fail 'MODULE_DIR must be a shell-safe absolute Linux path (A-Za-z0-9_./:+,=@%=-)'
[ -f "$GAME_ASSEMBLY" ] || fail "GameAssembly not found: $GAME_ASSEMBLY"
ACTUAL_GAME_SHA=$(sha256_file "$GAME_ASSEMBLY")
[ "$ACTUAL_GAME_SHA" = "$EXPECTED_GAME_SHA" ] || fail "GameAssembly hash mismatch: expected $EXPECTED_GAME_SHA, got $ACTUAL_GAME_SHA"
GENERATION=1
if [ -e "$MANIFEST_FILE" ]; then
    read_current_manifest
    GENERATION=$((OLD_GENERATION + 1))
fi
if [ -n "$GENERATION_OVERRIDE" ]; then
    case $GENERATION_OVERRIDE in
        ''|*[!0-9]*|0) fail '--generation needs a positive integer' ;;
    esac
    [ "${#GENERATION_OVERRIDE}" -le 9 ] || fail '--generation is too large'
    GENERATION=$GENERATION_OVERRIDE
fi
if [ "$NO_BUILD" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then
    (CDPATH= cd -- "$PROJECT" && cargo build --release -p hromatite-features)
fi
[ -f "$ARTIFACT" ] || fail "feature artifact not found: $ARTIFACT"
FEATURE_SHA=$(sha256_file "$ARTIFACT")
SHORT_SHA=$(printf '%.12s\n' "$FEATURE_SHA")

if [ "$DRY_RUN" -eq 1 ]; then
    printf 'dry-run: GENERATION=%s\nSHA256=%s\nGAME_SHA256=%s\nABI=1\n' \
        "$GENERATION" "$FEATURE_SHA" "$ACTUAL_GAME_SHA"
    exit 0
fi

mkdir -p "$MODULE_DIR"
VERSIONED_FILE=$MODULE_DIR/features-$GENERATION-$SHORT_SHA.dll
[ ! -e "$VERSIONED_FILE" ] || fail "refusing to overwrite $VERSIONED_FILE"
STAGE=$MODULE_DIR/.features-$GENERATION-$$.stage
MANIFEST_TMP=$MODULE_DIR/.current-$$.manifest
BACKUP=$MODULE_DIR/.previous-$$.manifest
HAD_MANIFEST=0
MANIFEST_SWAPPED=0
COMMITTED=0
cleanup() {
    rm -f "$STAGE" "$MANIFEST_TMP"
    if [ "$COMMITTED" -eq 0 ]; then
        rm -f "$VERSIONED_FILE"
        if [ "$MANIFEST_SWAPPED" -eq 1 ]; then
            rm -f "$MANIFEST_FILE"
            if [ "$HAD_MANIFEST" -eq 1 ]; then
                mv "$BACKUP" "$MANIFEST_FILE"
            fi
        fi
    fi
    rm -f "$BACKUP"
}
trap cleanup EXIT HUP INT TERM
( set -C; : >"$STAGE" ) 2>/dev/null || fail "temporary artifact exists: $STAGE"
( set -C; : >"$MANIFEST_TMP" ) 2>/dev/null || fail "temporary manifest exists: $MANIFEST_TMP"
if [ -f "$MANIFEST_FILE" ]; then
    HAD_MANIFEST=1
    ( set -C; cp "$MANIFEST_FILE" "$BACKUP" ) 2>/dev/null || fail "cannot back up $MANIFEST_FILE"
fi
cp "$ARTIFACT" "$STAGE"
ln "$STAGE" "$VERSIONED_FILE" 2>/dev/null || fail "refusing to overwrite $VERSIONED_FILE"
rm -f "$STAGE"
printf 'GENERATION=%s\nSHA256=%s\nFILE=%s\nGAME_SHA256=%s\nABI=1\n' \
    "$GENERATION" "$FEATURE_SHA" "$VERSIONED_FILE" "$ACTUAL_GAME_SHA" >"$MANIFEST_TMP"
mv "$MANIFEST_TMP" "$MANIFEST_FILE"
MANIFEST_SWAPPED=1
printf 'reload %s\n' "$GENERATION" >>"$CMD_FILE"
COMMITTED=1
printf 'GENERATION=%s\nSHA256=%s\nFILE=%s\nGAME_SHA256=%s\nABI=1\n' \
    "$GENERATION" "$FEATURE_SHA" "$VERSIONED_FILE" "$ACTUAL_GAME_SHA"
