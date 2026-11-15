# hromatite-console

Interactive Linux host TUI for Hromatite's **existing** Proton file channel.
It tails `hromatite.log` incrementally and opens `hromatite-cmd.txt` only with
`create + append`; it does not add any thread, socket or pipe to the game.

```sh
tools/hromatite-tui.sh [--prefix DIR] [--log FILE]
```

The default prefix is `$HROMATITE_PREFIX`, or the VRChat app 438100 Proton
prefix under `~/.local/share/Steam`. The usual paths are:

- log: `PREFIX/drive_c/users/steamuser/hromatite.log`
- commands: `PREFIX/drive_c/users/steamuser/hromatite-cmd.txt`

## Keys

| Key | Action |
| --- | --- |
| Enter | Send the command through the append channel |
| Up / Down | Command history (with draft preservation) |
| Tab | Complete a payload `help` command; show ambiguous matches |
| Ctrl-R, or `/` on empty input | Incrementally filter the in-memory log; Enter accepts |
| PgUp / PgDn | Scroll; pausing/resuming follow is explicit in the status row |
| F2 | Pause/resume following new lines |
| F3 | Toggle errors + GC filter |
| F4 | Palette: `pos`, `who`, `esp`, `late`, `gct` |
| F5 | Request `pos`, `who`, `late`, `gct` telemetry in one append |
| F8 | Clear the view only, never truncate or modify the log |
| Esc / Ctrl-C | Leave and restore the original termios and screen |

`Left`, `Right`, `Home`, `End`, `Backspace` and `Delete` edit the input. The
model keeps at most 20,000 parsed lines and the tail keeps at most 1 MiB of an
unterminated line, avoiding unbounded growth. Old unframed lines and the
`[unix timestamp] [tag] message` format are both accepted.

## Headless verification

```sh
tools/hromatite-tui.sh --demo --snapshot --size 100x24
cargo test --manifest-path crates/hromatite-console/Cargo.toml \
  --target x86_64-unknown-linux-gnu
```

`--demo` uses only a built-in synthetic fixture. Its header says
`DEMO — SYNTHETIC (no game)`, paths identify the fixture, and command/telemetry
actions never touch live files. `--snapshot` emits one plain frame without
entering termios or the alternate screen. The crate has no third-party
dependencies; only its small binary uses manual Linux termios/ioctl FFI.
