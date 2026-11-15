#!/bin/sh
# Host half of the runtime IL2CPP dump pipeline:
#
#   in-game  `dumpmeta Z:\home\<user>\.hromatite-dumps\<sha12>\global-metadata.dat`
#   host     tools/il2cppdump.sh
#   result   dump.cs / script.json / il2cpp.h / DummyDll in the out directory
#
# The only thing that must come from the live process is global-metadata.dat,
# because VRChat ships it encrypted on disk (Il2CppDumper then says
# "Metadata file not found or encrypted"). GameAssembly.dll is read from the
# game directory unchanged. This build dumps metadata WITHOUT the FAB11BAF
# magic in live memory (measured over /proc/<pid>/mem), so the script patches
# the four magic bytes into a copy before calling the dumper.
#
# Read-only with respect to the game. Needs dotnet 8 (taken from PATH, else
# from nix) and network on the first run (the dumper release is cached).
set -eu

VERSION=6.7.46
REPO_URL=https://github.com/Perfare/Il2CppDumper/releases/download
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/hromatite/il2cppdumper
GAME=${GAME:-"$HOME/.local/share/Steam/steamapps/common/VRChat"}
DUMPS=${HROMATITE_DUMPS:-"$HOME/.hromatite-dumps"}
META=""
OUT=""
PATCH=auto          # auto = patch only when the magic is absent
TOOL_DIR=""

usage() {
    cat <<'EOF'
usage: il2cppdump.sh [options]

  --game DIR        game directory (default: $GAME or the Steam VRChat path)
  --metadata FILE   decrypted metadata dump (default: <dumps>/<sha12>/global-metadata.dat)
  --out DIR         output directory (default: <dumps>/<sha12>/dump)
  --no-patch        never add the FAB11BAF magic
  --force-patch     always rewrite the magic bytes
  --tool-dir DIR    use an already unpacked Il2CppDumper directory
  -h, --help        this text

The metadata dump is produced from inside the game process:
    dumpmeta Z:\home\<user>\.hromatite-dumps\<sha12>\global-metadata.dat
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --game) GAME=$2; shift 2 ;;
        --metadata) META=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --no-patch) PATCH=never; shift ;;
        --force-patch) PATCH=always; shift ;;
        --tool-dir) TOOL_DIR=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage; exit 2 ;;
    esac
done

GA=$GAME/GameAssembly.dll
[ -f "$GA" ] || { printf 'no GameAssembly.dll at %s\n' "$GA" >&2; exit 2; }
sha=$(sha256sum "$GA")
sha=${sha%% *}
sha12=$(printf '%s' "$sha" | cut -c1-12)
if [ -z "$META" ]; then META=$DUMPS/$sha12/global-metadata.dat; fi
if [ -z "$OUT" ]; then OUT=$DUMPS/$sha12/dump; fi

if [ ! -f "$META" ]; then
    printf 'no metadata dump at %s\n' "$META" >&2
    printf 'produce it from inside the game (console / --send):\n' >&2
    printf '  dumpmeta Z:%s\n' "$(printf '%s' "$META" | tr '/' '\\')" >&2
    exit 3
fi

# ---- the dumper itself -----------------------------------------------------
if [ -z "$TOOL_DIR" ]; then
    TOOL_DIR=$CACHE/Il2CppDumper-$VERSION
    if [ ! -f "$TOOL_DIR/Il2CppDumper.dll" ]; then
        printf 'fetching Il2CppDumper v%s into %s\n' "$VERSION" "$CACHE"
        mkdir -p "$CACHE"
        zip=$CACHE/il2cppdumper-$VERSION.zip
        if [ ! -f "$zip" ]; then
            curl -fsSL --retry 3 -o "$zip.part" \
                "$REPO_URL/v$VERSION/Il2CppDumper-net7-v$VERSION.zip"
            mv "$zip.part" "$zip"
        fi
        rm -rf "$TOOL_DIR.tmp"
        mkdir -p "$TOOL_DIR.tmp"
        unzip -q -o "$zip" -d "$TOOL_DIR.tmp"
        mv "$TOOL_DIR.tmp" "$TOOL_DIR"
    fi
fi
[ -f "$TOOL_DIR/Il2CppDumper.dll" ] || { printf 'dumper not found in %s\n' "$TOOL_DIR" >&2; exit 4; }

# Console.ReadKey on a redirected stdin throws; keep the run non-interactive.
if grep -q '"RequireAnyKey": true' "$TOOL_DIR/config.json" 2>/dev/null; then
    sed -i 's/"RequireAnyKey": true/"RequireAnyKey": false/' "$TOOL_DIR/config.json"
fi

# ---- dotnet ----------------------------------------------------------------
if command -v dotnet >/dev/null 2>&1; then
    DOTNET=$(command -v dotnet)
    DOTNET_ROOT=${DOTNET_ROOT:-$(dirname "$(dirname "$DOTNET")")}
else
    printf 'dotnet not in PATH, resolving it through nix (cached after first run)\n'
    store=$(nix build --no-link --print-out-paths nixpkgs#dotnet-sdk_8 2>/dev/null | tail -1)
    [ -n "$store" ] || { printf 'cannot resolve dotnet-sdk_8 through nix\n' >&2; exit 4; }
    DOTNET=$store/bin/dotnet
    DOTNET_ROOT=$store/share/dotnet
fi
export DOTNET_ROOT
# The published build targets net7; the host runtime is 8.
export DOTNET_ROLL_FORWARD=LatestMajor
export DOTNET_CLI_TELEMETRY_OPTOUT=1

# ---- magic ----------------------------------------------------------------
mkdir -p "$OUT"
meta_arg=$META
magic=$(od -An -tx1 -N4 "$META" | tr -d ' \n')
patch=no
if [ "$PATCH" = always ] || { [ "$PATCH" = auto ] && [ "$magic" != af1bb1fa ]; }; then
    meta_arg=$OUT/global-metadata.patched.dat
    cp "$META" "$meta_arg"
    # little-endian 0xFAB11BAF
    printf '\257\033\261\372' | dd of="$meta_arg" bs=1 seek=0 conv=notrunc status=none
    patch=yes
fi
printf 'game:     %s\n' "$GAME"
printf 'sha256:   %s\n' "$sha"
printf 'metadata: %s (magic %s, patched %s)\n' "$META" "$magic" "$patch"
printf 'out:      %s\n' "$OUT"

# ---- run -------------------------------------------------------------------
if "$DOTNET" "$TOOL_DIR/Il2CppDumper.dll" "$GA" "$meta_arg" "$OUT" >"$OUT/dumper.log" 2>&1; then
    status=0
else
    status=$?
fi
cat "$OUT/dumper.log"

printf '\n--- outputs ---\n'
ls -la "$OUT" 2>/dev/null | sed -n '2,40p'
if [ -f "$OUT/script.json" ]; then
    printf 'method map: tools/il2cpp_methods.py %s/script.json --game-dir %s --out %s/methods.json\n' \
        "$OUT" "$GAME" "$OUT"
fi
exit $status
