#!/bin/sh
# Hermetic publisher/watcher regression tests; never use a real game prefix.
set -eu

PROJECT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
RELOAD=$PROJECT/tools/hromatite-reload.sh
WATCH=$PROJECT/tools/hromatite-watch.sh
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/hromatite-hot-test.XXXXXX")
WATCH_PID=
cleanup() {
    if [ -n "$WATCH_PID" ]; then
        kill "$WATCH_PID" 2>/dev/null || :
        wait "$WATCH_PID" 2>/dev/null || :
    fi
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT HUP INT TERM

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
assert_eq() { [ "$1" = "$2" ] || fail "expected [$2], got [$1]"; }
assert_file() { [ -f "$1" ] || fail "missing file: $1"; }
sha256_file() { sha256sum "$1" | { read -r digest _; printf '%s\n' "$digest"; }; }
assert_no_staging_files() {
    leftovers=$(find "$MODULES" -name '.*' -type f -print)
    assert_eq "$leftovers" ""
}

new_fixture() {
    FIXTURE=$TEST_ROOT/$1
    mkdir -p "$FIXTURE"
    PREFIX_DIR=$FIXTURE/pfx
    USER_DIR=$PREFIX_DIR/drive_c/users/steamuser
    MODULES=$USER_DIR/hromatite-modules
    COMMANDS=$USER_DIR/hromatite-cmd.txt
    GAME=$FIXTURE/GameAssembly.dll
    ARTIFACT=$FIXTURE/hromatite_features.dll
    mkdir -p "$USER_DIR"
    printf 'fake game build one\n' >"$GAME"
    printf 'fake feature generation one\n' >"$ARTIFACT"
    GAME_SHA=$(sha256_file "$GAME")
}

run_reload() {
    env -i PATH="$PATH" HOME="$FIXTURE/home" \
        HROMATITE_PREFIX="$PREFIX_DIR" \
        HROMATITE_GAME_ASSEMBLY="$GAME" \
        HROMATITE_GAME_SHA256="$GAME_SHA" \
        HROMATITE_ARTIFACT="$ARTIFACT" \
        sh "$RELOAD" "$@"
}

test_atomic_publication() {
    new_fixture atomic
    output=$(run_reload --no-build)
    manifest=$MODULES/current.manifest
    assert_file "$manifest"
    artifact_sha=$(sha256_file "$ARTIFACT")
    short=$(printf '%.12s\n' "$artifact_sha")
    expected_file=$MODULES/features-1-$short.dll
    assert_file "$expected_file"
    cmp "$ARTIFACT" "$expected_file" >/dev/null || fail 'published DLL differs from the artifact'
    expected="GENERATION=1
SHA256=$artifact_sha
FILE=$expected_file
GAME_SHA256=$GAME_SHA
ABI=1"
    assert_eq "$(cat "$manifest")" "$expected"
    assert_eq "$(cat "$COMMANDS")" "reload 1"
    printf '%s\n' "$output" | grep 'GENERATION=1' >/dev/null || fail 'summary lacks generation'
    assert_no_staging_files

    printf 'fake feature generation two\n' >"$ARTIFACT"
    run_reload --no-build >/dev/null
    second_sha=$(sha256_file "$ARTIFACT")
    second_short=$(printf '%.12s\n' "$second_sha")
    second_file=$MODULES/features-2-$second_short.dll
    expected="GENERATION=2
SHA256=$second_sha
FILE=$second_file
GAME_SHA256=$GAME_SHA
ABI=1"
    assert_eq "$(cat "$manifest")" "$expected"
    assert_eq "$(cat "$COMMANDS")" "reload 1
reload 2"
    assert_eq "$(sha256_file "$expected_file")" "$artifact_sha"
    assert_no_staging_files
}

test_versioned_no_overwrite() {
    new_fixture no-overwrite
    run_reload --no-build --generation 1 >/dev/null
    manifest=$MODULES/current.manifest
    before_manifest=$(sha256_file "$manifest")
    before_dll=$(find "$MODULES" -name 'features-1-*.dll' -type f)
    before_dll_sha=$(sha256_file "$before_dll")
    if run_reload --no-build --generation 1 >"$FIXTURE/out" 2>"$FIXTURE/err"; then
        fail 'second publication of generation 1 succeeded'
    fi
    grep 'refusing to overwrite' "$FIXTURE/err" >/dev/null || fail 'missing no-overwrite diagnostic'
    assert_eq "$(sha256_file "$before_dll")" "$before_dll_sha"
    assert_eq "$(sha256_file "$manifest")" "$before_manifest"
    assert_eq "$(cat "$COMMANDS")" "reload 1"
    printf 'different bytes under an already-used generation\n' >"$ARTIFACT"
    if run_reload --no-build --generation 1 >"$FIXTURE/out-reused" 2>"$FIXTURE/err-reused"; then
        fail 'reused generation with different bytes succeeded'
    fi
    assert_eq "$(sha256_file "$before_dll")" "$before_dll_sha"
    assert_no_staging_files
}

test_concurrent_publishers_do_not_delete_the_winner() {
    new_fixture concurrent
    artifact_sha=$(sha256_file "$ARTIFACT")
    short=$(printf '%.12s\n' "$artifact_sha")
    expected_file=$MODULES/features-1-$short.dll
    mkdir -p "$FIXTURE/bin"
    real_ln=$(command -v ln)
    run_marker=$FIXTURE/ln-called
    printf '#!/bin/sh\nif [ ! -e "%s" ]; then\n  : >"%s"\n  "%s" "$@"\n  sleep 0.1\nelse\n  exit 1\nfi\n' \
        "$run_marker" "$run_marker" "$real_ln" >"$FIXTURE/bin/ln"
    # One deterministic ln wins; the other fails. Do not edit the publisher.
    chmod +x "$FIXTURE/bin/ln"
    PATH="$FIXTURE/bin:$PATH" run_reload --no-build >"$FIXTURE/p1.out" 2>&1 &
    first=$!
    PATH="$FIXTURE/bin:$PATH" run_reload --no-build >"$FIXTURE/p2.out" 2>&1 &
    second=$!
    wait "$first" || :
    wait "$second" || :
    assert_file "$expected_file"
    cmp "$ARTIFACT" "$expected_file" >/dev/null || fail 'concurrent winner artifact was replaced or deleted'
    [ ! -e "$MODULES/.publisher.lock" ] || fail 'publisher lock was not released'
}

test_watch_retries_after_transient_failure() {
    new_fixture watch-retry
    SRC=$FIXTURE/src
    mkdir -p "$SRC/crates/hromatite-features/src" "$SRC/crates/hromatite-abi/src"
    printf 'first\n' >"$SRC/crates/hromatite-features/src/lib.rs"
    printf 'abi\n' >"$SRC/crates/hromatite-abi/src/lib.rs"
    printf '#!/bin/sh\nactual="%s"\nif [ ! -e "%s" ]; then\n  printf "attempt\\n" >>"%s"\n  exit 7\nfi\nprintf "attempt\\n" >>"%s"\nexec sh "$actual" "$@"\n' \
        "$RELOAD" "$FIXTURE/failed-once" "$FIXTURE/failed-once" "$FIXTURE/failed-once" "$FIXTURE/failed-once" >"$FIXTURE/retry.sh"
    chmod +x "$FIXTURE/retry.sh"
    env -i PATH="$PATH" HOME="$FIXTURE/home" \
        HROMATITE_PREFIX="$PREFIX_DIR" \
        HROMATITE_GAME_ASSEMBLY="$GAME" \
        HROMATITE_GAME_SHA256="$GAME_SHA" \
        HROMATITE_ARTIFACT="$ARTIFACT" \
        HROMATITE_WATCH_ROOT="$SRC" \
        HROMATITE_RELOAD_BIN="$FIXTURE/retry.sh" \
        sh "$WATCH" --interval 0 --stable-polls 1 --no-build >"$FIXTURE/watch.out" 2>&1 &
    WATCH_PID=$!
    tries=0
    while ! grep 'watching' "$FIXTURE/watch.out" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -lt 100 ] || fail 'retry watcher did not capture a baseline'
        sleep 0.02
    done
    printf 'stable change\n' >>"$SRC/crates/hromatite-features/src/lib.rs"
    tries=0
    while [ ! -f "$COMMANDS" ]; do
        tries=$((tries + 1))
        [ "$tries" -lt 500 ] || {
            cat "$FIXTURE/watch.out" >&2
            fail 'watcher did not retry the same stable fingerprint'
        }
        sleep 0.02
    done
    kill "$WATCH_PID" 2>/dev/null || :
    wait "$WATCH_PID" 2>/dev/null || :
    WATCH_PID=
    assert_file "$MODULES/current.manifest"
    assert_eq "$(cat "$COMMANDS")" "reload 1"
    attempts=$(wc -l <"$FIXTURE/failed-once")
    assert_eq "$attempts" "2"
}

test_dry_run_neither_builds_nor_writes() {
    new_fixture dry-run
    mkdir -p "$FIXTURE/bin"
    printf '#!/bin/sh\nprintf called >"%s"\n' "$FIXTURE/cargo-called" >"$FIXTURE/bin/cargo"
    chmod +x "$FIXTURE/bin/cargo"
    output=$(PATH="$FIXTURE/bin:$PATH" run_reload --dry-run)
    printf '%s\n' "$output" | grep 'dry-run: GENERATION=1' >/dev/null || fail 'missing dry-run summary'
    [ ! -e "$FIXTURE/cargo-called" ] || fail 'dry-run invoked cargo'
    [ ! -e "$MODULES/current.manifest" ] || fail 'dry-run published a manifest'
    [ ! -e "$COMMANDS" ] || fail 'dry-run appended a command'
}

test_wrong_game_hash_is_rejected_before_build() {
    new_fixture wrong-hash
    mkdir -p "$FIXTURE/bin"
    printf '#!/bin/sh\nprintf called >"%s"\n' "$FIXTURE/cargo-called" >"$FIXTURE/bin/cargo"
    chmod +x "$FIXTURE/bin/cargo"
    if env -i PATH="$FIXTURE/bin:$PATH" HOME="$FIXTURE/home" \
        HROMATITE_PREFIX="$PREFIX_DIR" \
        HROMATITE_GAME_ASSEMBLY="$GAME" \
        HROMATITE_ARTIFACT="$ARTIFACT" \
        sh "$RELOAD" >"$FIXTURE/out" 2>"$FIXTURE/err"; then
        fail 'unknown GameAssembly was accepted'
    fi
    grep 'GameAssembly hash mismatch' "$FIXTURE/err" >/dev/null || fail 'missing hash mismatch diagnostic'
    expected=$(sed -n 's/^pub const GAMEASSEMBLY_SHA256: &str = "\([0-9a-f]\{64\}\)";$/\1/p' \
        "$PROJECT/crates/hromatite-core/src/symbols.rs")
    grep "$expected" "$FIXTURE/err" >/dev/null || fail 'did not use generated symbols.rs hash'
    [ ! -e "$FIXTURE/cargo-called" ] || fail 'cargo ran before game validation'
    [ ! -e "$MODULES/current.manifest" ] || fail 'wrong hash published a manifest'
    [ ! -e "$COMMANDS" ] || fail 'wrong hash appended a command'
}

test_command_failure_restores_previous_manifest() {
    new_fixture command-rollback
    run_reload --no-build >/dev/null
    manifest=$MODULES/current.manifest
    cp "$manifest" "$FIXTURE/gen1.manifest"
    printf 'second feature bytes\n' >"$ARTIFACT"
    rm "$COMMANDS"
    mkdir "$COMMANDS"
    if run_reload --no-build >"$FIXTURE/out" 2>"$FIXTURE/err"; then
        fail 'publication succeeded with an unwritable command channel'
    fi
    assert_eq "$(cat "$manifest")" "$(cat "$FIXTURE/gen1.manifest")"
    assert_eq "$(find "$MODULES" -name 'features-2-*.dll' -type f -print)" ""
    assert_no_staging_files
}

test_malformed_manifest_and_path_abort_without_side_effects() {
    new_fixture malformed
    mkdir -p "$MODULES" "$FIXTURE/bin"
    printf 'GENERATION=broken\nFILE=/tmp/not-loaded\n' >"$MODULES/current.manifest"
    cp "$MODULES/current.manifest" "$FIXTURE/before.manifest"
    printf '#!/bin/sh\nprintf called >"%s"\n' "$FIXTURE/cargo-called" >"$FIXTURE/bin/cargo"
    chmod +x "$FIXTURE/bin/cargo"
    if PATH="$FIXTURE/bin:$PATH" run_reload >"$FIXTURE/out" 2>"$FIXTURE/err"; then
        fail 'malformed current.manifest was accepted'
    fi
    grep 'malformed current.manifest' "$FIXTURE/err" >/dev/null || fail 'missing malformed manifest diagnostic'
    assert_eq "$(cat "$MODULES/current.manifest")" "$(cat "$FIXTURE/before.manifest")"
    [ ! -e "$FIXTURE/cargo-called" ] || fail 'cargo ran before manifest validation'
    [ ! -e "$COMMANDS" ] || fail 'malformed manifest appended a command'
    assert_no_staging_files

    rm "$MODULES/current.manifest"
    if env -i PATH="$PATH" HOME="$FIXTURE/home" \
        HROMATITE_PREFIX="$PREFIX_DIR" \
        HROMATITE_MODULE_DIR="$FIXTURE/not shell safe" \
        HROMATITE_GAME_ASSEMBLY="$GAME" \
        HROMATITE_GAME_SHA256="$GAME_SHA" \
        HROMATITE_ARTIFACT="$ARTIFACT" \
        sh "$RELOAD" --no-build >"$FIXTURE/out2" 2>"$FIXTURE/err2"; then
        fail 'shell-unsafe module path was accepted'
    fi
    grep 'shell-safe' "$FIXTURE/err2" >/dev/null || fail 'missing shell-safe path diagnostic'
    [ ! -e "$FIXTURE/not shell safe" ] || fail 'unsafe module path was created'
}

test_watch_publishes_one_stable_change() {
    new_fixture watch
    SRC=$FIXTURE/src
    mkdir -p "$SRC/crates/hromatite-features/src" "$SRC/crates/hromatite-abi/src"
    printf 'first\n' >"$SRC/crates/hromatite-features/src/lib.rs"
    printf 'abi\n' >"$SRC/crates/hromatite-abi/src/lib.rs"
    env -i PATH="$PATH" HOME="$FIXTURE/home" \
        HROMATITE_PREFIX="$PREFIX_DIR" \
        HROMATITE_GAME_ASSEMBLY="$GAME" \
        HROMATITE_GAME_SHA256="$GAME_SHA" \
        HROMATITE_ARTIFACT="$ARTIFACT" \
        HROMATITE_WATCH_ROOT="$SRC" \
        sh "$WATCH" --once --interval 0 --stable-polls 1 --no-build \
        >"$FIXTURE/watch.out" 2>"$FIXTURE/watch.err" &
    WATCH_PID=$!
    tries=0
    while ! grep 'watching' "$FIXTURE/watch.out" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -lt 100 ] || { cat "$FIXTURE/watch.err" >&2; fail 'watcher did not start'; }
        sleep 0.02
    done
    printf 'second, stable\n' >>"$SRC/crates/hromatite-features/src/lib.rs"
    tries=0
    while kill -0 "$WATCH_PID" 2>/dev/null; do
        tries=$((tries + 1))
        [ "$tries" -lt 200 ] || { cat "$FIXTURE/watch.err" >&2; fail 'watcher did not publish'; }
        sleep 0.02
    done
    wait "$WATCH_PID"
    WATCH_PID=
    assert_file "$COMMANDS"
    assert_eq "$(cat "$COMMANDS")" "reload 1"
    assert_file "$MODULES/current.manifest"
}

TESTS='atomic no-overwrite concurrent dry-run wrong-hash command-rollback malformed watch watch-retry'
run_test() {
    printf 'TEST %s\n' "$1"
    case $1 in
        atomic) test_atomic_publication ;;
        no-overwrite) test_versioned_no_overwrite ;;
        concurrent) test_concurrent_publishers_do_not_delete_the_winner ;;
        dry-run) test_dry_run_neither_builds_nor_writes ;;
        wrong-hash) test_wrong_game_hash_is_rejected_before_build ;;
        command-rollback) test_command_failure_restores_previous_manifest ;;
        malformed) test_malformed_manifest_and_path_abort_without_side_effects ;;
        watch) test_watch_publishes_one_stable_change ;;
        watch-retry) test_watch_retries_after_transient_failure ;;
        *) fail "unknown test $1" ;;
    esac
    printf 'PASS %s\n' "$1"
}

requested=${1:-all}
if [ "$requested" = all ]; then
    for name in $TESTS; do run_test "$name"; done
else
    case " $TESTS " in
        *" $requested "*) run_test "$requested" ;;
        *) fail "unknown test $requested (choose all or:$TESTS)" ;;
    esac
fi
