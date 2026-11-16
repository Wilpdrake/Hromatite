# Hot-reload host tooling

This document covers the **Linux host publisher, opt-in watcher, and independent
host TUI**. They use the existing prefix files and do not start, stop, attach to,
or validate a live game. Building, publishing, and runtime acceptance are separate
steps.

## Paths and publication

The publisher builds `hromatite-features`, copies the resulting DLL to an
immutable generation file, atomically replaces `current.manifest`, and only then
appends `reload N` to the existing command channel.

```sh
tools/hromatite-reload.sh [--no-build] [--dry-run] [--generation N]
```

Defaults, in override order:

| Setting | Environment override | Default |
| --- | --- | --- |
| Proton prefix | `HROMATITE_PREFIX`, then `PREFIX` | `~/.local/share/Steam/steamapps/compatdata/438100/pfx` |
| Module directory | `HROMATITE_MODULE_DIR`, then `MODULE_DIR` | `PREFIX/drive_c/users/steamuser/hromatite-modules` |
| Command file | `HROMATITE_CMD_FILE`, then `CMD_FILE` | `PREFIX/drive_c/users/steamuser/hromatite-cmd.txt` |
| Feature artifact | `HROMATITE_ARTIFACT` | `target/x86_64-pc-windows-gnu/release/hromatite_features.dll` |
| GameAssembly | `HROMATITE_GAME_ASSEMBLY`, then `GAME_ASSEMBLY` | `~/.local/share/Steam/steamapps/common/VRChat/GameAssembly.dll` |
| Expected game SHA-256 | `HROMATITE_GAME_SHA256` | `GAMEASSEMBLY_SHA256` in `crates/hromatite-core/src/symbols.rs` |

The expected and actual GameAssembly hashes must match. A mismatch is rejected
**before Cargo runs or any module/command file is written**. `--dry-run` validates
the game and artifact and prints the candidate metadata, but neither builds nor
writes a manifest, DLL, or command. `--generation N` is for explicit recovery, not
ordinary numbering; the normal next generation is the valid current generation
plus one, or 1 for the first publication.

Each generation is named:

```text
features-N-SHA12.dll
```

`SHA12` is the first 12 lowercase hexadecimal characters of the feature DLL's
SHA-256. Existing versioned files are never overwritten. The publisher uses a
private staging file and a same-directory hard link for the versioned DLL, writes
the manifest to another same-directory temporary file, and renames that manifest
onto `current.manifest`. If appending the command fails, it removes the new,
uncommitted version and restores the previous manifest. Older generation DLLs
are retained for selection and rollback; the publisher does not garbage-collect
them.

Module paths are restricted to absolute shell-safe paths
(`A-Za-z0-9_./:+,=@%-`). Temporary publication files and the command channel are
created under `umask 077`.

## `current.manifest` format

The actual file is exactly five LF-terminated `KEY=VALUE` lines, in this order:

```text
GENERATION=3
SHA256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
FILE=/absolute/linux/path/features-3-0123456789ab.dll
GAME_SHA256=648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b
ABI=1
```

The example hashes illustrate the field widths; the publisher always computes
them from the selected artifact and game build. `GENERATION` is a positive decimal
integer, both hashes are exactly 64 lowercase hexadecimal characters, and `ABI`
is currently `1`.

**`FILE` is the absolute Linux host path, not a Windows path.** This lets later
host invocations verify the old manifest and referenced DLL before publication.
A game-side Wine/Win32 consumer must translate it instead of passing it verbatim
to a Windows file API:

- any absolute host path `/some/path` maps to `Z:\some\path` (replace forward
  slashes with backslashes after adding the `Z:` drive);
- the default module directory also lives below the prefix's `drive_c`, so the
  same generation is available as
  `C:\users\steamuser\hromatite-modules\features-N-SHA12.dll`.

These are two mappings of the same file; the manifest does not contain an
additional `WIN_FILE` field.

## Opt-in watcher

```sh
tools/hromatite-watch.sh [--interval N] [--stable-polls N] [--once] [--no-build]
```

The watcher polls only `crates/hromatite-features`, `crates/hromatite-abi`, and
the workspace `Cargo.toml`/`Cargo.lock`. Defaults are a two-second interval and
two identical polls before publication. It then invokes `hromatite-reload.sh`.
A failure leaves the current generation in place and is reported. `--once` waits
for one stabilized source change, publishes at most once, and exits with the
publisher's status. The TUI never starts this watcher implicitly; manual reload
remains available.

Useful watcher overrides are `HROMATITE_WATCH_ROOT`, `HROMATITE_WATCH_INTERVAL`,
`HROMATITE_WATCH_STABLE_POLLS`, and `HROMATITE_RELOAD_BIN`.

## TUI commands and risk boundaries

Start the independent Linux console with:

```sh
tools/hromatite-tui.sh [--prefix DIR] [--log FILE]
```

Hot-reload operations use the existing append-only command file:

| Command | Meaning | Confirmation |
| --- | --- | --- |
| `reload` or `reload latest` | Activate the latest published manifest | Second Enter |
| `reload N` or `reload gen N` | Select an immutable generation | Second Enter |
| `reload status` | Request read-only generation state | None |
| `rollback` | Return to the previous resident generation | Second Enter |
| `config show` | Show hot configuration | None |
| `config reload` | Re-read hot configuration | None |
| `config set <key> <value>` | Change one hot configuration value | Second Enter |

**F7** puts only the bare `reload` command in the prompt and arms the same risk
confirmation; Enter confirms it. It does not build, run the watcher, enable
automatic telemetry, or bypass the second-Enter guard. Selecting `reload` in the
F4 palette follows the same two-Enter flow: optional generation arguments do not
force a template detour. `rollback` also needs the second Enter when selected
from the palette. Editing or Esc cancels a pending dangerous command.

The publisher itself appends `reload N`; F7 and typed TUI commands do not invoke
the publisher.

## Log records shown by the TUI

The model accepts the resident atomic state record in this field order:

```text
[unix-time] [hot] state=active generation=3 previous=2 pending=none sha=0123456789ab flight=0 error=none
```

An error record uses `error=<text>` instead of `error=none`. The feature record
is:

```text
[unix-time] [con] feature: generation=3 abi=1
```

The compact status row displays hot state, current/previous/pending generations,
in-flight count, SHA, feature generation and ABI. Hot errors are marked as errors
and their text is shown. Truncated or otherwise invalid records do not replace
the last valid model state; unrelated feature logs are not guessed to be hot
state records.

## Host-only verification

Run the console suite from its independent workspace and explicitly select the
Linux host target. Running the parent workspace's default test command selects
its global `x86_64-pc-windows-gnu` target and is not the console test invocation.

```sh
(
  cd crates/hromatite-console
  cargo test --workspace --target x86_64-unknown-linux-gnu
)

sh -n tools/hromatite-reload.sh tools/hromatite-watch.sh tools/test-hromatite-hot.sh
sh tools/test-hromatite-hot.sh all

tools/hromatite-tui.sh --demo --snapshot --size 140x24
```

The shell harness creates fake GameAssembly/feature files and a temporary Proton
prefix. It asserts exact manifest contents, the latest-manifest update, immutable
generations, dry-run/no-command behavior, wrong-game rejection before build,
rollback after command append failure, malformed-state rejection, and one stable
watcher publication. It never reads or writes the real game prefix. The demo
snapshot is synthetic, plain text, and does not send commands. No live-game hot
reload acceptance is implied by these host checks.
