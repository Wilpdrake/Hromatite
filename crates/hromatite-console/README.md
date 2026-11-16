# hromatite-console

Interactive Linux host TUI for Hromatite's **existing** Proton file channel.
It tails `hromatite.log` incrementally and opens `hromatite-cmd.txt` only with
`create + append`; it does not add any thread, socket or pipe to the game.
The compact green/cyan dashboard shows link freshness, managed-tick rate, GC
health, player count, camera and the most recent command/answer stream. F1 keeps
file paths and the complete key reference out of the main view.

## Interactive operations

- **F4 or `:`** opens a searchable command palette. Every entry has syntax,
  a description and a `SAFE`, `ADV` or `RISK` label. Commands that need
  arguments are inserted into the prompt instead of being sent incomplete.
- **`RISK` commands require two Enter presses**, both when selected from the
  palette and when typed directly. Editing or Esc cancels confirmation. This is
  only a host-side guard around existing payload commands.
- **F5** sends one `pos`, `who`, `late`, `gct` batch. **F6** toggles automatic
  telemetry: `pos` every second and the other three every five seconds. No
  commands are sent while the log is older than ten seconds; refresh resumes
  after a fresh write. Successful sends appear as `tx | > command` locally.
- The status fields are parsed from existing `late`, `gct`, `who`, `pos` and
  `esp` lines. No payload support was added for them.

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
| Enter | Send a safe command, or confirm a `RISK` command on the second press |
| Up / Down | Command history (with draft preservation), or palette selection |
| Tab | Complete a payload `help` command; show ambiguous matches |
| Ctrl-R, or `/` on empty input | Incrementally filter the in-memory log; Enter accepts |
| PgUp / PgDn | Scroll; pausing/resuming follow is explicit in the status row |
| F1 | Open/close the field manual and file paths |
| F2 | Pause/resume following new lines |
| F3 | Toggle errors + GC filter |
| F4, or `:` on empty input | Search the described command palette |
| F5 | Request `pos`, `who`, `late`, `gct` telemetry in one append |
| F6 | Toggle rate-limited automatic telemetry |
| F8 | Clear the view only, never truncate or modify the log |
| Esc | Cancel confirmation or close the current view; in the command view, leave |
| Ctrl-C | Leave and restore the original termios and screen |

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
`DEMO // SYNTHETIC`, paths identify the fixture in F1, and command/telemetry
actions never touch live files. `--snapshot` emits one plain frame without
entering termios or the alternate screen. The only third-party dependency is
`unicode-width`; the small binary uses manual Linux termios/ioctl FFI.
