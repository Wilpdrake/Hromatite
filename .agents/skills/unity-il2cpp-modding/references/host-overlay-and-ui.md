# Host-side overlay and UI without in-process ImGui

## 1. Threat model: where the signature evidence lives

Anti-cheat on Linux (EAC and friends) reads the **game process** — `/proc/<pid>/maps`, `/proc/<pid>/mem`,
`process_vm_readv`. A UI module inside that process is evidence:

- `ImDrawVert` is a **20-byte** vertex struct; scanners pattern-match that stride and the default
  20-byte layout;
- the default font atlas (a 512×512 texture plus its glyph tables) is itself a signature;
- strings and symbols (`ImGui`, `imgui_impl_dx11`, `ImFontAtlas`, vtables) survive stripping more often
  than people expect;
- vendored public frameworks (EAC/BattlEye/Ricochet/ACE/Vanguard) all scan for these.

Consequence: **elementally, ImGui in a separate process is invisible to the game's anti-cheat.** Prefer a
host window whenever the UI does not have to be drawn inside the game surface (headset/VR, exclusive
fullscreen, or a 3D-anchored HUD). Only then accept an in-process renderer, and then do not ship Dear
ImGui itself.

## 2. Verified X11 overlay (Linux host, game under Proton/Xwayland)

Measured working: an override-redirect X11 window from a plain host binary, sitting above a
VRChat window running under Proton in a pressure-vessel container, drawing live camera/player numbers it
read from the payload's log file. No thread, socket, pipe or allocation was added to the game.

Build it with raw Xlib FFI (no crates; matches a dependency-free Rust workspace):

```rust
#[link(name = "X11")] unsafe extern "C" { /* XOpenDisplay, XCreateWindow, XMapWindow, ... */ }
#[link(name = "Xext")] unsafe extern "C" { fn XShapeCombineRectangles(...) -> c_int; }
```

Window recipe, each piece load-bearing:

- `XCreateWindow(root, x, y, w, h, 0, depth=CopyFromParent, class=InputOutput, visual=NULL, mask, attrs)`
  with `valuemask = CWBackPixel | CWEventMask | CWOverrideRedirect` (bits `1<<1`, `1<<11`, `1<<9`).
  `override_redirect = 1` keeps the WM from decorating/moving it; without it the compositor re-parents
  the window and your coordinates mean nothing.
- `_NET_WM_STATE = _NET_WM_STATE_ABOVE` (type `ATOM`, format 32) — this is what actually keeps it above
  a maximised game window on EWMH compositors (kwin, mutter). `XRaiseWindow` alone is not enough.
- `_NET_WM_WINDOW_OPACITY` (type `CARDINAL`, 32-bit, value `0.85 * u32::MAX`) gives translucency when a
  compositor is running and is harmlessly ignored otherwise.
- **Click-through** = an *empty* input region:
  `XShapeCombineRectangles(dpy, win, ShapeInput /*2*/, 0,0, NULL, 0, ShapeSet /*0*/, Unsorted /*0*/)`
  after `XShapeQueryExtension`. Empty input region ⇒ mouse events fall through to the game; the window
  still draws. This is the default mode for a HUD. To make widgets clickable, flip to a full-region input
  shape (or drop the shape call) on a hotkey — otherwise the overlay swallows every click in its rect.
- Text: `XLoadQueryFont(dpy, "fixed")`, then `XSetFont(dpy, gc, fid)` where `fid` is the **second
  pointer-sized word** of `XFontStruct` (`*(font as *const c_ulong).add(1)`). Draw with `XDrawString`;
  there is no shaping, so a fixed font is the honest choice for aligned numbers.
- `XSetWindowAttributes` is a plain LP64 struct: four `c_ulong` fields, then `int` fields with padding
  (`bit_gravity`, `win_gravity`, `backing_store`, pad), two `c_ulong`, `int save_under` + pad, `long
  event_mask`, `long do_not_propagate_mask`, `int override_redirect` + pad, `c_ulong colormap`, `c_ulong
  cursor`. Declare it exactly like that; a missing pad shifts every later field.
- **Backing pixmap, one blit per frame.** Create `XCreatePixmap(dpy, win, w, h, XDefaultDepth(dpy,
  XDefaultScreen(dpy)))` plus its own `XCreateGC(dpy, pixmap, ...)`, draw background and text into the
  pixmap, then `XCopyArea(dpy, pixmap, win, win_gc, 0,0,w,h, 0,0)`. Without this every widget redraw
  flickers through the compositor.
- **The text must be drawn into the pixmap with the pixmap's GC.** Calling `XDrawString(dpy, win, win_gc,
  ...)` and then blitting the pixmap over the window erases the strings — the panel shows checkbox/slider
  rectangles with no labels, which reads as a font or colour bug and is neither.
- Draw loop: `XFillRectangle` the background, `XSetForeground` + `XDrawString` per line, `XFlush`; pump
  `XPending`/`XNextEvent` with a 192-byte event buffer so Expose/Configure are consumed, and call
  `XSetGraphicsExposures(dpy, gc, 0)` on the blit GC or every `XCopyArea` queues a GraphicsExpose event.
- **Cadence:** a 33 ms loop for input responsiveness, but repaint only when something changed (telemetry
  line parsed, hover/drag/slider, shape change) or every ~250 ms for a live uptime counter. Measured:
  unconditional 60 fps full-window blit ≈24 % of a core, event-driven + 4 Hz ≈8 %.
- **Widgets** are just hit-test rectangles: keep one `widget_rects()` table shared by hit-testing and
  drawing (id, x, y, w, h), a `hover: Option<u8>`, a `dragging` flag for the header strip and a
  `slider: Option<u8>` while a button is held; persist every change to a small `key=value` config file so
  position and toggles survive restarts.
- **Click-through ↔ interactive:** with an empty `ShapeInput` the panel receives no mouse events at all,
  so bind a global hotkey to swap the input shape: `XGrabKey(dpy, XKeysymToKeycode(dpy, XK_F8), mods,
  root, owner_events=1, ...)` with `mods` in `{0, NumLockMask, LockMask}`. The grab lives on the root
  window, so it fires even while the overlay is transparent to the pointer.
- **Following the game window:** read `_NET_CLIENT_LIST` on the root (`XGetWindowProperty`, type
  `XA_WINDOW`), `XFetchName` each id until the title matches, then `XGetGeometry` +
  `XTranslateCoordinates(game_win, root, 0, 0, ...)` for the absolute origin and `XMoveWindow` the
  overlay there each frame (skip the move when the coordinates did not change). Resizing the overlay to
  the game's size (needed for pixel-aligned ESP) is a separate step from following it.

### 2b. A separate ESP layer (boxes over the game, panel stays a panel)

Do not press ESP into the panel window. Use a second window with the same attributes, and give each one
only the job it can do:

| | panel | ESP layer |
|---|---|---|
| geometry | fixed size, dragged by the user | `XMoveWindow`+`XResizeWindow` to the game's **client area** every ~700 ms, recreate the pixmap on any change |
| input shape | full rounded region when interactive, empty otherwise | always empty |
| bounding shape | rounded mask, set on resize | list of drawn rectangles, set every frame |
| events | `KeyPress` (root grab), `Button*`, `Motion`, `Expose` | `Expose` only |

- **Transparency = `ShapeBounding`, not ARGB.** Build the rect list while drawing: every piece you emit is
  also pushed into a `Vec<XRectangle>`; after `XCopyArea`, call
  `XShapeCombineRectangles(dpy, esp_win, ShapeBounding /*0*/, …, rects, n, ShapeSet, Unsorted)`. Only those
  rectangles exist, everything else is the game — no 32-bit visual, no compositor support needed. The same
  trick gives the *panel* genuinely rounded corners (bounding mask = rounded rect) instead of a dark square
  behind the rounding.
- **A diagonal cannot be shaped as a line.** Snaplines from the screen bottom-centre to each box are drawn
  as ~40 chunks of 2×2 rects, each one pushing a shape rectangle; a single `XDrawLine` inside a rectangular
  mask leaks the black pixmap background.
- Repaint the whole canvas each frame (fill black, then draw): the shape hides the rest, but stale pixels
  survive `XCopyArea`.
- Set both masks empty at startup — before the first game-window lookup the ESP window is a small opaque
  square at 0,0 that eats clicks — and re-apply `ShapeInput = empty` after every resize.
- Map camera pixels → window pixels with the camera's own `pixelWidth`/`pixelHeight`
  (`kx = win_w / pix_w`): the render target and the X window are not always the same size, and hardcoding
  1920×1080 draws boxes that are right at one resolution and progressively wrong at the next.
- After changing the panel's input shape or the game geometry, `XRaiseWindow` the panel again — an
  override-redirect window can drift below the game after a shape call.
- Draw order per frame: clear → corner brackets → label plates (fill black, then text in the distance
  colour) → snaplines → one `XCopyArea` → shape update. Brackets cost a quarter of the ink of a full
  rectangle and read as "cheat" rather than "UI".
- Keep the shape update at ~30 Hz, not 60: each is a server round trip with hundreds of rectangles.
  Telemetry arrives at 4–5 Hz, so smoothness comes from interpolating (lerp towards the newest coordinates,
  fade `alpha` in/out per id), not from asking faster.
- Radar/2-D plotting needs the *world* position, not the projected one: put it in the same telemetry line
  (`w=x,y,z`) instead of opening a second command. Rotate by `atan2(forward.x, forward.z)` so the camera
  faces up, and scale by the max-range slider.

## 3. Data channel (nothing new inside the game)

The overlay is just another *reader* of the interface the payload already exposes:

- telemetry: tail `<prefix>/drive_c/users/steamuser/<name>.log` and parse the payload's own answer
  lines (prefix them so parsing is unambiguous, e.g. `[con] pos: get_position = (x, y, z)`);
- commands: append lines to `<name>-cmd.txt` (the file channel the console already uses) on a slower
  cadence than the payload polls — 400 ms for a position read, 2–3 s for a whole-list refresh;
- keep the file handle open and read incrementally, buffering a partial last line (a split line silently
  drops the value and looks like "telemetry stopped").

**Pick a format that survives splitting.** `key=value` tokens are convenient until a value contains a
space: `cam=(1.00, 1.60, 0.00)` parsed by a generic "find `key=`, cut at the next space" helper yields
`(1.00,` and silently drops the feature built on it (a radar that reports "no camera" while the numbers
are right there). Keep parenthesised triples on their own parser (match `key=(` … `)`) and put a
free-text field such as a player name **last**, since it may contain spaces too.

This keeps the detection surface at "one more process reads a file that already exists".

### 3b. Interactive TUI over the same file channel

Build a terminal console as a separate host crate, not as another payload thread:

1. Use `[workspace]` in the host crate and explicit host-target builds, exactly as
   for the overlay. A UI library is optional: Rust `std`, raw termios and ANSI
   cover readline/history, completion, filtering, scrolling and a command palette
   without pulling a dependency tree into a previously dependency-free project.
2. Keep terminal I/O outside the model. The model parses tagged log lines and
   owns history/filter/follow state; a small incremental tail preserves partial
   lines and truncation, while a separate writer append-opens the command file
   and emits one newline-terminated command per line. Keep telemetry acquisition
   below the payload's polling cadence and batch explicit refreshes.
3. Give the CLI `--prefix`, `--log`, clearly marked `--demo`, `--snapshot` and a
   bounded `--size WxH`. Render from the real model with synthetic input in demo
   mode so the test exercises the same parser and layout as the live view; never
   present fixture telemetry as game evidence.
4. Test the headless surface first: parse CRLF/legacy lines, cover split reads,
   command append framing, history/completion/filter semantics and exact snapshot
   height with no escape bytes. Then run a PTY integration test that submits
   real keystrokes, verifies the commands appended to a temporary command file,
   follows an appended log line and confirms termios/alternate-screen restoration
   on exit. Include a dangerous command rejected on first Enter and sent on the
   second, plus auto-refresh and stale-log suppression, because model tests do
   not exercise the host writer/scheduler wiring. A snapshot alone cannot catch
   raw-input or cleanup regressions.
5. Parse terminal input incrementally with the pending escape bytes retained
   across reads. Treat `Esc` alone as quit only after a short timeout, recognize
   both CSI (`Esc [`) and SS3 (`Esc O`) arrows/Home/End, and consume unknown CSI
   through its final byte without executing each byte separately: interpreting a
   split arrow's `Esc` as quit makes the TUI exit on the next keypress.
6. Make truncation and rewrite first-class tail states. Preserve an overlap
   window (256 bytes is enough here) across a size decrease and reconcile it at
   line boundaries after regrowth; merely resetting to the old offset or line
   count duplicates answers and restores the previous prompt history instead of
   the new session's. Unit-test shrink→regrow with a rewritten same-length line.
7. Measure terminal layout in display columns, not Unicode scalar values. Use a
   display-width implementation (`unicode-width` in Rust), strip escape bytes
   from snapshot output, and test CJK/combining characters and wide glyphs:
   `chars().count()` misaligns borders and cursor positions and hand-written
   width tables grow into an unmaintainable Unicode subset.
8. Spend the main screen on operational state: a two-line header for connection
   freshness, tick/GC health, target count and camera leaves most rows for the
   command/answer stream. Drop noisy Unix timestamps in favour of short tags and
   keep paths and the full key map in F1 help; permanent file paths and verbose
   chrome otherwise crowd out useful telemetry.
9. Generate completion and the command palette from one catalogue containing
   name, syntax, description and risk class. Insert argument-taking commands
   into the prompt rather than sending them incomplete; require a second Enter
   for dangerous commands both in the palette and when typed directly, because
   guarding only one input path leaves an accidental-write bypass. Cancel that
   confirmation on edit or Esc.
10. Put auto-refresh in a host scheduler with separate fast/slow deadlines and
    an explicit stale-log threshold. Skip sends while the log is stale and reset
    deadlines on resume; otherwise a quiet or dead payload accumulates commands
    that all execute after recovery. Record a local `tx` echo only after the
    append writer succeeds, so a failed send cannot masquerade as executed.
11. Treat each `esp: head` as a new target snapshot: clear the previous id set
    before ingesting its `esp: id` rows, because unioning snapshots inflates the
    player count after targets leave. Prefer an explicit `who: list size` count
    when one is present.
12. Keep plain snapshot lines deterministic and add ANSI accents only in the
    terminal renderer, after sanitising log controls. Test both paths: snapshots
    must contain no escapes, while addresses/ids may be cyan in the live view;
    otherwise styling tests can miss terminal-injection bytes from log data.
13. Verify fixtures against the producer, not other tests: VRChat emits
    `who: list 0x..., size N`, so parsing only the simplified `who: list size N`
    leaves real telemetry unknown. Distinguish a missing ESP snapshot (unknown)
    from a received header with no ids (zero). Never clamp a tiny terminal up to
    the minimum dashboard size; render the size warning inside its real bounds.

## 4. Building and running on NixOS (host binary next to a windows-gnu workspace)

- Make the overlay its own workspace root: put `[workspace]` (empty) in its `Cargo.toml`. Otherwise the
  parent workspace's `[build] target = "x86_64-pc-windows-gnu"` applies and `cargo build --workspace`
  tries to link `-lX11` into a mingw binary.
- Build for the host target explicitly:
  `nix-shell -p xorg.libX11 xorg.libXext --run 'cargo build --release --manifest-path crates/<overlay>/Cargo.toml --target x86_64-unknown-linux-gnu'`
  The nix-shell supplies dev outputs (`libX11.so`, `libXext.so`) and `cc` with `NIX_LDFLAGS`.
- **Runtime** needs the same libs on the loader path — the nix-store libs are not in the default
  search path, so a bare binary dies with `error while loading shared libraries: libX11.so.6`. Derive it
  from the same environment:
  `LD_LIBRARY_PATH=$(echo $NIX_LDFLAGS | tr ' ' '\n' | grep '^-L' | sed 's/^-L//' | paste -sd:)`
  and ship a tiny launcher that does this (`tools/<name>-overlay.sh`).
- **`.gitignore` with `/target` does not cover a nested crate.** A new `crates/<overlay>/target` gets
  committed (24 build artefacts, binary blobs in diffs) until the pattern is `crates/<overlay>/target/`
  or `**/target/`; fix with `git rm -r --cached` so the files stay on disk.
- Session plumbing on Plasma Wayland: the game is an **Xwayland** client, so `DISPLAY` is `:0` (not
  `$WAYLAND_DISPLAY`) and `XAUTHORITY` is the kwin-generated `/run/user/<uid>/xauth_*`. Neither is in an
  agent/session environment — read them from the Xwayland cmdline (`-auth <file>`, display `:N`) or fall
  back to the first `/run/user/<uid>/xauth_*`. Screenshot verification: `spectacle -b -n -o shot.png` with
  `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` set.

### 4b. Verify without the game (demo telemetry + a stand-in window)

1. **A `--demo` flag in the binary.** Feed synthetic telemetry through the *real* parser (`esp: head
   w=1920 …`, then one `esp: id=…` line per made-up player) instead of a fake telemetry struct: that
   exercises the parser, the smoothing, the radar and the panel with zero game involvement, and lets the
   user judge the design before anything is injected.
2. **A stand-in window titled like the game.** The overlay only needs a window whose name it recognises:
   `xmessage -title VRChat -geometry 1000x700+400+148` is enough for attach, client-area sizing and the
   whole ESP layer.
3. **Capture the window itself, not the screen.** `xwininfo -root -tree | grep -i <title>` gives the id —
   override-redirect windows are **absent from `_NET_CLIENT_LIST`**, so a client-list scan never finds the
   overlay or its ESP layer. `import -window 0x… shot.png` (ImageMagick from `nix-shell -p imagemagick`)
   captures exactly that window: small enough for an image model to read, no desktop noise.
   `xwininfo -id 0x… -shape` prints `Window shape extents`; the extents must cover the drawn area and
   nothing else — that is the proof the mask works and that clicks pass through.

When the image model is unavailable or keeps timing out, **read the pixels as text**:

```bash
convert shot.png -crop 340x120+0+0 +repage txt:- > /tmp/top.txt   # then regex (x,y): (r,g,b) in Python
```

Print one char per pixel (or per 2×2) with a crude classifier — `' '` near-black, `'#'` bright, `'C'`
cyan-ish, `'.'` mid — and a 340-px panel becomes ~85 readable columns. That is enough to catch mojibake
(two junk glyphs where one was meant), overlapping right-aligned text, a clipped footer and a missing
widget, and it is the only way to tell "the shape covers the box" from "the box was drawn".

## 5. If the UI must live in-process

Candidates, and why each is off ImGui's signature set:

| Project | Shape | Fits when |
|---|---|---|
| `matwtff/ktnagui` | C++20, D3D11, written *because* ImGui is signature-scanned; 24-byte vertex stride, HLSL compiled in memory, own font atlas | you want a ready non-ImGui immediate-mode engine |
| `rxi/microui` | C, two files, ~1.5k LOC, backend-agnostic | smallest embed; you write the D3D11 backend (a few hundred lines) |
| `Immediate-Mode-UI/Nuklear` | single-header ANSI C, widgets (sliders/combos/trees) | richer widget set, still no ImGui bytes |
| `insomfaze/vgui-framework` | C++17, own renderer + input + widgets | desktop-window UIs, not an overlay |

External-overlay references: `procmod/overlay` (Rust, D3D11 click-through, immediate-mode API),
`michel-pi/GameOverlay.Net` (C#, Direct2D1), `zaafar/ClickableTransparentOverlay` (C# + ImGui.NET — fine,
because it is a different process), `nertigel/StealthOverlay` (C++ D3D11 ImGui, streamproof, global
keyboard hook), `Vasik96/imgui-overlay-linux` (Rust + X11).

In-process checklist: hook `Present`/`dxgi` only if you must; save and restore the full D3D11 state (14
points: viewport, scissor, blend, depth-stencil, samplers, shaders, input layout, render targets) or the
host game's next frame is visibly corrupted; keep shaders compiled in memory rather than `.cso` files on
disk; and remember the hook itself is a thread-start/patch artefact your invisibility story has to cover.

## 6. Small gotchas that cost build cycles

- Rust inline format args: `{x:8.2}` compiles; `{x:8.2f}` is `unknown format trait f` (the trait letter,
  not the type, follows the colon).
- `XCreateWindow` returns `0` on failure — check it, or the draw calls silently target window 0.
- Xlib text is **bytes**: any non-ASCII character (`·`, `—`, `…`) becomes two or three glyphs in a fixed
  font, shifting everything after it. Keep every drawn string ASCII (`|`, `-`, `..`).
- `pkill -f <pat>` also matches the shell running it when that literal appears in the same command line:
  a `pkill -f "overlay --demo"` appended to a build command can kill the session before the next step.
  Put the kill in its own call, or use a bracket pattern whose literal appears nowhere else.
- Reading `/proc/<pid>/mem` from a Python/agent probe can return short or `EIO`; loop the read and reopen
  the handle after an error, otherwise `struct.unpack` reports a layout bug that does not exist.
