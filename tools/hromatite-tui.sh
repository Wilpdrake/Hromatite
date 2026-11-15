#!/bin/sh
# Host-only interactive console. Reads the existing log and appends commands;
# it creates no files, sockets or threads inside the game process.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MANIFEST="$ROOT/crates/hromatite-console/Cargo.toml"
BIN="$ROOT/crates/hromatite-console/target/x86_64-unknown-linux-gnu/release/hromatite-console"

if ! command -v cargo >/dev/null 2>&1; then
    echo "hromatite-tui: cargo is required (or run the prebuilt $BIN)" >&2
    exit 1
fi

if [ ! -x "$BIN" ]; then
    echo "hromatite-tui: building host binary…" >&2
    cargo build --release --manifest-path "$MANIFEST" --target x86_64-unknown-linux-gnu
fi

exec "$BIN" "$@"
