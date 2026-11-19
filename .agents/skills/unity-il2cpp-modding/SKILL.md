---
name: unity-il2cpp-modding
description: Use when modding Unity IL2CPP games; inject, dump.
metadata:
  version: "1"
  author: hermes
  license: MIT
  hermes:
    tags: [unity, il2cpp, reverse-engineering, rust, proton, game-modding]
    related_skills: [reverse-engineer-anything, spike]
---

# Unity IL2CPP modding (native DLL against GameAssembly.dll)

The managed code of an IL2CPP Unity game lives in `GameAssembly.dll`; C# classes/methods exist only as il2cpp runtime data. A native DLL inside the game process is the practical route. Everything below is per-build: fingerprint the build BEFORE writing a line of code, because one game update re-obfuscates every export name and invalidates every RVA.

## When to Use

Any of these:
- a Unity game must be modded/cheated/instrumented from native code and its managed side is IL2CPP (`GameAssembly.dll` + `global-metadata.dat` present);
- `GetProcAddress` on `GameAssembly.dll` returns NULL for `il2cpp_*` names (export obfuscation);
- an in-process DLL must be injected into a game running under Proton/Wine;
- an offline il2cpp dump fails or metadata looks encrypted and runtime dumping is the alternative.

## 1. Fingerprint the build first (5 minutes, decides everything)

```bash
GAME=~/.local/share/Steam/steamapps/common/VRChat   # Proton: prefix in steamapps/compatdata/<appid>/pfx
strings -n 6 $GAME/*_Data/globalgamemanagers | grep -oE '6000\.[0-9.]+f[0-9]+' | head -1   # Unity version
ls -l $GAME/GameAssembly.dll $GAME/*_Data/il2cpp_data/Metadata/global-metadata.dat
od -A x -t x1z -N 32 $GAME/*_Data/il2cpp_data/Metadata/global-metadata.dat   # metadata magic
x86_64-w64-mingw32-objdump -p $GAME/GameAssembly.dll | sed -n '/^\[Ordinal\/Name Pointer\] Table/,/^$/p' | head -20
```

Decide from that output:
- **Metadata magic `af 1b b1 fa`** (`0xFAB11BAF`) → plaintext, stock Il2CppDumper/Cpp2IL work. Anything else (random-looking bytes) → **encrypted**, offline dumpers are dead, only runtime dumping works.
- **Random 11-char export names** → Beebyte-style obfuscation; `GetProcAddress("il2cpp_domain_get")` returns NULL. A handful of names always survive unrenamed (`Flush`, `il2cpp_free`, `DllCanUnloadNow`, `CloseNLSocket`) — use them as anchors to orient in the export table.
- Cache the result as a `symbols-<sha256(GameAssembly.dll)>.json` (name → RVA) and key all offsets off that hash, never off "current version".

## 2. Resolve the obfuscated il2cpp API — Unity's own name pool

`UnityPlayer.dll` contains the obfuscated export names packed in `.rdata` (observed: 235 of 251 names in one dense string pool, 0x38–0x80 apart, zero padding between) in **Unity's own il2cpp table order**. That order is NOT the published `il2cpp-api-functions.h` order — Unity 6 inserted entries mid-table, so two surviving plain anchors land at pool indexes 71/128 versus header indexes 83/116. Run `scripts/probe_il2cpp_exports.py <game_dir>` to confirm the pool exists on this build; use the order for orientation only, never as the name oracle.

**The name→address table is NOT in that image.** Check this offline before designing any runtime
scan: read `UnityPlayer.dll`, collect every aligned 8-byte word, and test whether any equals
`ImageBase + RVA(name)`. Measured on a Unity 6 VRChat build: 0 hits over all 4.2 M aligned words,
even though all 236 name strings were present. A scan restricted to the image therefore returns
`0 slots resolved` and looks like "the resolver is broken". The pointers do exist — on the **heap**,
in a live runtime structure (a `{char* name; ...}` table with 0x18-byte entries on that build), and
nothing near them holds a GameAssembly address, so a "find the sibling pointer" layout vote fails.

Resolution, in the order that actually works:
1. **Identity via the export RVAs you already have.** `symbols.rs`-style name→RVA maps are enough to
   call *any* export without naming it. Compile-time shape is the discriminator: exactly three of the
   251 exports were `48 8B 05 <disp32> C3` (`mov rax,[rip+d]; ret`) — call them at runtime, print each
   return value plus one or two levels of the object it points at. Measured on Unity 6: the winner was
   **not** `il2cpp_domain_get` but `il2cpp_get_corlib` — the return is the corlib `Il2CppImage`, and the
   "assembly list" behind it is the contiguous `Il2CppImage[]` with stride 0x48 (see §2b). Call a getter
   "domain" only when `domain_get_assemblies` returns a count you can walk by hand; a name list is not proof.
2. in-process name-pool scan as corroboration: locate each name string in `UnityPlayer`, then search
   **all readable regions** (not just the image) for a word equal to its VA; probe ±0x80 in 8-byte
   steps for a GameAssembly-range neighbour and vote per delta. Report staged counters
   (names found / occurrences / regions / MB scanned / votes) — \"0 slots\" alone cannot tell you
   whether the names, the pointers, or the layout failed.
3. fallbacks: pattern-scan GameAssembly and version-gate signatures; community symbol tables for other builds — cross-check only, never trust as offsets.
4. **Signature-based stub table — the route shipping clients use, and the only one that is fully offline.**
   `UnityPlayer.dll` carries one lazy-init stub per il2cpp API function, all with the same shape:
   `48 8D 15 <disp32>` (`lea rdx` → the obfuscated export name), `E8 <rel32>` (GetProcAddress),
   `48 89 05 <disp32>` (cache it), then `48 85 C0 75 0E 32 DB 48 8D 0D`. Compile that as a regex with
   wildcard displacements, and for each hit read the C string at `match + 7 + disp32` — that is the
   *obfuscated name*, which your name→RVA map already contains. Measured on a Unity 6 build: the generic
   prefix matched **232** times and the published API list also has 232 entries, i.e. the resolver generator has a uniform shape. **That count does not establish API order or identity.** Use the resolver's `lea` to obtain export name→RVA pairs and accept identities only from runtime-probed anchors or independently validated per-function patterns. The displacement is relative to the stub's section RVA, not its file offset; convert PE file offset→RVA first. Earlier reports of only 46 named stubs came from forgetting the `.text` raw-to-VA delta; the corrected probe reads all 232 names. A live anchor rejects canonical ordering outright (stub 12 = `il2cpp_get_corlib`, canonical 11), and the shift is not even uniform: stub 122 = `gchandle_get_target` (canonical 121) while stub 211 = `image_get_class` (canonical 211), i.e. one entry is inserted mid-list. Use the order to generate candidates only, and accept an identity when a call or the disassembly shape proves it. Keep unknown candidates separate from the verified generated Rust table. Published
      per-function patterns (e.g. `Il2CppKnownPatterns.inl`) add build-specific tail bytes
   (`48 8D 0D 62 0B`) — wildcard everything after `test rax,rax / jnz / xor bl,bl` or nothing matches;
   keep their list of API names as the ordering oracle, not their byte tails.

Prefer the behavioural route over decoding Unity's private structs: walking the domain object's fields
is what produced the assembly list, but it is build-specific and needs a guarded reader (see pitfalls).

Locate the names in **one pass** over the image: walk the bytes once, collect ASCII runs of 4..32 chars, look each up in a hash set of the name list. Per-name scans of a 34 MB `UnityPlayer.dll` (251 × 34 MB) exceed a 60 s wait budget even when they eventually finish, so the run looks like "injection silently did nothing".

### Naming the exports inside Ghidra

When the ask is "find the exports and their real names" or "name the functions", the deliverable is
**renames in the analysis database** (IDA/Ghidra), not a mapping table in the repo — the user wants to read
that listing. Keep the JSON map as a side artifact keyed by the build hash.

- Inventory offline first (PE export dir → name→RVA, sorted by RVA; alias groups of several names on one
  address are one function), then build a per-export shape table with flat `objdump -D -b binary` slices.
- Rename by **address string**: `rename_symbols_batch` with `{"target_type":"function",
  "identifier":"0x180c2a060", "new_name":"il2cpp_type_get_type"}`; batch 10–30 per call.
- **Never `create_function` to "fix" an export address that has no function** — it creates a 1-byte
  function and there is no delete, which permanently breaks that address. Report it instead.
- `get_pseudocode` can be asynchronous (returns a task id; poll `get_task_status`), so queue several and
  collect — one-decompile-per-round-trip wastes the wait.
- He authorises ~10 parallel agents for this: one agent per contiguous RVA band, a shared brief file, and a
  hard rule that each child renames only its own addresses (never its thunk targets). Validate with one
  pilot band before fanning out. Name the shared internal helpers in a separate later pass.
- **Close the pass by subtraction, not by vibes.** Subtract the assigned names from the canonical API list:
  the leftover canonical names *are* the remaining unknowns, so the second wave runs against that list
  instead of re-deriving everything. Two merge checks catch almost every invented identity — one canonical
  name assigned to two addresses (`string_new` claimed by both a thunk and an inline `strlen` copy), and an
  `il2cpp_*` name that is not in the canonical list at all.
- Do not use `get_xrefs_to` to separate functions that only touch globals: operands that never got a
  reference return zero xrefs, so a real reader looks unreferenced. Read the globals out of the pseudocode.
- **A second, independent pass is what turns candidates into names — run it before reporting.** Re-derive
  every entry from the code with the current name treated as a hypothesis (callee, struct offsets,
  argument use, string literals, IAT slots); measured yield on one build: 207 confirm, 7 genuine
  ambiguities, 4 wrong out of 218. All four misses were "adjacent thing" errors: a neighbouring bit
  (`+0x135.1` vs `+0x136.3`), `image_get_name` (`+0x08`) vs `get_filename` (`+0x40`), and a forwarder into
  `Object::New` taken for a type getter. When a reviewer claims two layout offsets are swapped, pin them with live structs containing distinguishable values — proof of a third offset does not discriminate them. For Unity 6 `MethodInfo`, `il2cpp_method_get_param` proves only `parameters@0x30`, not the neighbouring identities. On the measured build, `return_type@0x20` resolves to a `void` or class `Il2CppType`, while `klass@0x28` is the declaring class; verify that pairing and then correct the export getters/ICF alias table consistently.
- **ICF folding makes "unlocated canonical names" normal, not a gap to chase.** Identical one-line getters
  are merged, so one address owns several APIs (`0x3540` `mov (%rcx)` serves `object_get_class`,
  `class_get_image`, `image_get_assembly`, `assembly_get_image`, `property_get_parent`). Decide the alias
  set from the offset each API must read, record it beside the address, and stop looking for separate
  bodies — a `mov 0x10(%rcx)` getter is `class_get_parent` *and* `field_get_name` at the same time.
- Two more measured facts that bite when reading these APIs: `Il2CppImage+0x14` is the
  custom-attribute **range count**, not `typeStart` (that lives in `imageDef+0x00`, reached through the
  `+0x20` metadata handle), and `CreateNLSocket`/`ReadEvents`/`CloseNLSocket` are **throw stubs** —
  they load `__FILE__` and jump into an exception constructor, so calling them kills the caller.
- Budget the decompiler, not just the agents: >10 concurrent `get_pseudocode` tasks trip the MCP
  breaker and stall every caller for about a minute; 2–4 in flight is the reliable rate even when
  several agents share one open database.

Depth: `references/ghidra-export-naming.md` (export-table parsing traps, shape table, exact rename recipe,
async pseudocode, band split, canonical-list closure, measured struct table, measured stub→canonical offsets).

## 2b. Enumerate classes with zero il2cpp calls (Unity 6, measured)

The runtime keeps a plain `Il2CppClass*[]` in `GameAssembly.dll` `.data`
(`s_TypeInfoDefinitionTable`, indexed by `typeDefinitionIndex`, filled lazily). Find its address from
the code that indexes it, never from its contents (it starts zeroed) — the shape is
`mov rax,[rip+disp]` (the global) → `lea rsi,[rax+rdi*8]` → load/store of the class pointer. On the
measured build that global sits at `.data` RVA `0xe66c870`.

Reading that array is a **call-free, read-only** enumeration: ~38 k classes in ~0.3 s from the host
(`/proc/<pid>/mem`) and ~2 s from inside the game, with no GC risk and no cross-thread call.

Measured layout on Unity 6000.0.67f1 — the 2020 headers are wrong for it, so re-measure after an
update, but expect this shape:

```
Il2CppImage  +0x00 const Il2CppAssembly*   +0x08 const char* short name ("UnityEngine.CoreModule")
             +0x30 const Il2CppCodeGenModule*  +0x40 const char* "<Assembly>.dll"
             images are contiguous with stride 0x48; the getter returns the CORLIB image, and other
             assemblies (including the game's own Assembly-CSharp) sit a few entries BEFORE it —
             walk ±768 strides and match exact short names before prefix matches
Il2CppClass  +0x00 image*   +0x10 parent class*  +0x18 namespaze*   +0x50 MethodInfo**
             +0x80 FieldInfo[]  +0x98 name*     +0x124 uint16 fieldsCount
FieldInfo    stride 0x28: +0x00 type*  +0x08 offset (int32)  +0x10 name*  +0x18 token  +0x20 parent class*
MethodInfo   +0x00 methodPointer  +0x18 name*    +0x20 return_type*  +0x28 klass*
             +0x48 token          +0x4c flags (u16)   +0x52 parameters_count (u8)
; FieldInfo is the pair that gets swapped in older write-ups: offset is the int32 at +0x08, token at +0x18
; class+0x50 holds POINTERS and the array ends at a null slot
```

`il2cpp_image_get_class(image, index)` is recognised by shape, not by stub order:

```
mov rax,[rcx+0x20]   ; per-image metadata handle
mov r8d,[rax]        ; typeStart
add r8d,edx          ; + index
imul rcx,rax,0x58    ; sizeof(Il2CppTypeDefinition) = 0x58
jmp  MetadataCache::GetTypeInfoFromTypeDefinitionIndex
```

Do not guess it from tiny getters: `mov rax,[rcx]` / `[rcx+8]` return a name pointer or a packed counter
indistinguishably, and a `memcpy`-shaped function next door (`call alloc; copy index*2 bytes from arg1`)
is `il2cpp_string_new_utf16`, not class enumeration. Verified end-to-end: enumerate from the class table,
find `UnityEngine.Camera`, read its 407 `MethodInfo`s, call `get_main`'s `methodPointer`, and check the
returned object's `klass` equals the class you started from.

Three rules that make the difference between "a pointer" and "the value":

- **The table is sparse because classes are born on demand.** An untouched type is a NULL slot, so a
  cold SDK/game class is invisible until something touches it; `il2cpp_image_get_class(image, i)`
  creates it as a side effect, so sweep the assembly once (37 778 types of `Assembly-CSharp` in ~1 s,
  0 faults) and then work from the table.
- **Inherited members are not in the derived class.** `get_transform` lives on `Component`, not on
  `Camera`, so look methods up along the parent chain (`class+0x10`) before concluding a method is missing.
- **Struct-returning methods use a hidden out-pointer**: `rcx` = destination, `rdx` = `this`
  (`Vector3 Transform.get_position`). Calling them with `this` in `rcx` faults with `0xC0000005` at a
  random-looking address — that is an argument-order bug, not a bad method pointer. The same ABI carries
  a 64-byte `Matrix4x4`: read `Camera.projectionMatrix` and `worldToCameraMatrix`, multiply `P·V`
  column-major yourself and divide by `w`, instead of calling `WorldToScreenPoint`, whose struct
  **argument** ABI is still unproven. Sanity-check the matrices by projecting the camera's own position —
  it must land in the middle of the viewport. Plain instance
  methods take `this` in `rcx`; statics take nothing beyond their declared arguments. Measured chain that
  proves it: `Camera.main` (static) → `Component.get_transform(this)` → `Transform.get_position(out, this)`.

## 3. Metadata: dump in-process, not on disk

When metadata is encrypted, the decrypted form exists in RAM after `il2cpp_init`. So the "dumper" is a feature of the mod DLL, not an offline tool: `il2cpp_domain_get` → `il2cpp_domain_get_assemblies` → `il2cpp_assembly_get_image` → `il2cpp_image_get_class` / `il2cpp_class_get_methods` (each `MethodInfo` carries `methodPointer`). Build that as one crate and everything else (features, safe calls) sits on it. Do not burn time trying to decrypt `global-metadata.dat` on disk first.

The cheaper variant of the same idea — the one shipping dumpers use — is to take **only** the metadata out of the process and let `Il2CppDumper` do everything else offline: it writes `script.json` with a VA per managed method (`RVA = Address − PE ImageBase`) plus `dump.cs` with field offsets, so no `il2cpp_*` identity is needed at all. Two facts decide whether this works before you build it: the tool recognises metadata **only** by the `AF 1B B1 FA` first four bytes (patch them into a copy if the dump lacks them), and a build can keep its metadata in memory *without* a usable `Il2CppGlobalMetadataHeader` — verify a header really exists (consecutive u32 `(off, size)` pairs at `index*4`, indices 2..62) before promising a dump. Measured on VRChat Unity 6: decrypted strings are present but no validating header was found, so "dump the blob" alone was not enough.

Depth: `references/runtime-metadata-dump.md` (dumper version/runtime facts, `RequireAnyKey` trap, script.json → RVA recipe, header-base oracles, host `/proc/<pid>/mem` scanning without freezing the game).

## 4. Injection vectors (Proton/Wine host)

| Vector | Mechanics | Trade-off |
|---|---|---|
| DLL proxy (`version.dll`, `dinput8.dll`) | drop next to the exe, Windows loader picks the local copy first | fastest iteration; files in game dir (EAC/file-scan visible) |
| Separate `.exe` loader in the same prefix | `OpenProcess` → `VirtualAllocEx` + `WriteProcessMemory` (dll path) → `CreateRemoteThread(LoadLibraryW)` | disk stays clean; needs the loader launched inside `compatdata/<appid>/pfx` (protontricks-launch, or `proton run` with `STEAM_COMPAT_DATA_PATH`) |
| External Linux process | read/write `/proc/<pid>/mem` of the Proton wine process | no module inside; only read/plain writes, calling il2cpp functions needs shellcode |

Cross-process injection works across wine processes of one prefix (shared wineserver). Under Proton the game renders D3D11 → DXVK, so a `dxgi.dll`/`d3d11.dll` `Present` hook is a valid overlay route.

## 5. Rust toolchain and runtime plumbing

- `rustup target add x86_64-pc-windows-gnu` + mingw-w64; build the DLL as `crate-type = ["cdylib"]`, the `.exe` loader as a normal bin. No MSVC needed on Linux.
- **No console in the game process**: log to a file under `%USERPROFILE%` (i.e. `compatdata/<appid>/pfx/drive_c/users/steamuser/`) and drive features with `GetAsyncKeyState` hotkeys.
- **"Remote console" = tail that file from the host, not a socket.** Open a TCP/UDP port or a named
  pipe and the game gains a kernel object a scanner can enumerate; reading a log file over the prefix
  path adds nothing (the file already exists). Ship a small host script that resolves the prefix,
  `tail -F`s the log, colours tags, filters (`diag`/`dll`/`err`/substring) and has a `--status` mode
  showing the game pid + our regions from `/proc/<pid>/maps`. `awk` buffers when stdout is a pipe —
  call `fflush()` per line or the live stream looks empty. `tail -F` (capital) survives the game
  restarting and truncating the file.
- **Two-way = a command file, not a socket — and build it BEFORE the deep struct work.** The payload
  polls `%USERPROFILE%\<name>-cmd.txt` inside its own `DllMain` loop (same single stub thread, no extra
  thread) while an env var keeps the entry point alive (`LIVE=<secs>`), and answers into the normal log.
  Commands that pay for themselves: `q <addr> [n]` (qwords + ascii + module tag), `b`, `s`, `ws`,
  `find <hex64>`, `findb`/`findw` (ascii/utf-16 search), `asm` (assemblies + record addresses),
  `entry <name>`, `diag`, `seal`/`quit`. One typed command replaces a rebuild plus a game restart — that
  is the difference between hours and minutes when mapping private structs. Host side grows
  `--send '<cmd>'` and `--shell` (stdin → command file, answers stream back into the same view).
- **Auto-open that console when the game starts**, don't ask the user to run it: the wrapper writes a
  marker file after injection, a host watcher sees the marker and spawns a terminal. From a headless
  context (agent shell, systemd) copy `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and
  `DBUS_SESSION_BUS_ADDRESS` out of an already-running terminal's `/proc/<pid>/environ` — without that
  the window never appears. Give the watcher a `~/.config/autostart/*.desktop` entry so it survives
  logout, and keep one window per launch by matching the interactive argv (e.g. `--shell`), not the
  script name (which also matches `--send` one-shots).
- Do not resolve modules or scan memory inside `DllMain` under the loader lock. Perform only bounded initialization there; use the project's established execution context for deferred work (an existing game-thread tick or the same stub thread that delivered the injection). Do not introduce an extra worker thread when the design goal is zero additional threads: publish a tick on the game's existing function and return from `DllMain` without an install loop that can deadlock.
- Treat `VirtualQuery` + VEH as **fault containment, not proof of Rust memory safety**: a query does not establish pointer provenance or freeze another thread's unmap/protection change (TOCTOU), and a longjmp skips destructors. `Result`/`Option` classify checked failures; they do not make an integer-backed pointer safe to dereference or transmute.
Isolate raw pointer access, generated code, FFI and arbitrary-address calls in one low-level module; use `deny(unsafe_code)` in higher-level modules. Leave genuinely arbitrary foreign-memory calls explicitly `unsafe` and preserve each caller's proof or owned-snapshot contract. Deny `unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks` and `missing_safety_doc`, and document each remaining synchronization/aliasing race instead of hiding it behind a safe wrapper. `read_unaligned` addresses alignment only, not lifetime, overlap or pointer validity.

Procedure for lint-enforced isolation and boundary review: `references/resident-hot-reload.md` (generation router, local mapper, bounded retirement),
`references/rust-unsafe-boundary.md`.
- `catch_unwind` does not catch access violations. Every raw call boundary still needs the VEH guard and its no-live-`Drop`-resources rule.
- **`catch_unwind` only works if the whole graph unwinds.** Set `panic = "unwind"` in the *workspace*
  `[profile.release]`, not as a `[profile.release.package.<dll>]` override: cargo honours `panic`,
  `lto` and `incremental` in a package override only when that package is the direct build target,
  and a workspace build silently keeps `abort`. Verify the strategy actually landed instead of trusting
  the profile: `x86_64-w64-mingw32-nm <dll>.dll | grep -c _Unwind_Resume` must be non-zero. Then wrap
  every step (entry body, recon, each command dispatch) in a `guard(label, || ...)` helper that logs the
  panic and returns `None` — an unwinding panic must never cross an `extern "system"` boundary.
- **In a manual-mapped image `catch_unwind` still kills the process**: the PE headers are erased, so
  `RtlLookupFunctionEntry` finds no `.pdata` and the unwind aborts mid-panic (symptom: a `PANIC` line in
  the log, then the injector reports `0 pages sealed` because the game is gone). Contain both panics and
  access violations with a vectored handler instead: `AddVectoredExceptionHandler` + `RtlCaptureContext` /
  `RtlRestoreContext` (a longjmp), recognising Rust's panic SEH code `0x20474343` as "panic" and
  `0xC0000005` as "bad dereference". Keep a stack of saved contexts so guards nest (`dispatch` → guarded
  call → panic) and make the guard's lock re-entrant for the owning thread; a full unwinding runtime in
  the image (`nm | grep _Unwind_Resume`) is necessary but not sufficient.

## 6. Feature ladder (validate before escalating)

1. hello-world DLL: log module base of `GameAssembly.dll`/`UnityPlayer.dll` + Unity version → proves injection and that the anti-cheat tolerates it.
2. read-only telemetry: enumerate classes/methods, read the local player → proves resolution. Read
   player/scene data through the game's **public SDK surface** rather than its own class names: Beebyte
   renames the game's classes (`Player`, `PlayerManager` may not exist by name at all) but SDK types keep
   theirs (measured: `VRC.SDKBase.VRCPlayerApi` → `get_AllPlayers` / `GetPlayerId` / `GetPosition`).
   Recipe and the `List<T>` / `Il2CppArray` / `Il2CppString` layouts: `references/il2cpp-runtime-layout.md`
   §4b, §7a, §7b.
3. first visible write (`Camera.fieldOfView`, `Time.timeScale`) → proves field offsets.
4. gameplay write (fly/noclip/locomotion hook), then overlay (ESP via Present hook).

Ship each rung with an explicit "done" check; a broken rung makes every later one ambiguous.

## Overlay/UI: keep it out of the process by default

When this user asks for a "hacker-like" operational console, make it restrained and useful: compact status, green/cyan accents, no Matrix animation or ASCII art. Put link freshness, tick health, GC, player count and camera ahead of the log; relegate long paths and exhaustive shortcuts to an on-demand help view because operational facts are more useful than decoration.

Dear ImGui is only evidence **if the module lives in the game process** — the anti-cheat reads that
process's memory, where the 20-byte `ImDrawVert` stride, the font atlas, vtables and `ImGui` strings are
all patterns. A separate host window therefore carries no signature risk and needs no `Present` hook;
reach for an in-process renderer only when the UI must be drawn inside the game surface (headset/exclusive
fullscreen/3D-anchored HUD).

Measured working (Linux host, game under Proton in Xwayland): a raw-Xlib override-redirect window with
`_NET_WM_STATE_ABOVE`, `_NET_WM_WINDOW_OPACITY`, an **empty `ShapeInput` region** (clicks pass through to
the game) and core-font text, fed by tailing the payload's log and answering through the existing
`<name>-cmd.txt` channel — zero new threads/sockets/handles inside wine. It stayed above the game window
and the payload did not notice it.

- **Two windows: a panel and a separate ESP layer.** The panel is small, draggable and only sometimes
  interactive; the ESP layer is a second override-redirect window sized to the game's *client area*,
  permanently click-through, redrawn when telemetry moves. One window cannot do both: making the panel
  interactive must not make the boxes eat clicks, and repositioning the panel must not move the boxes.
- **Transparency by shape, not by alpha.** Set `ShapeBounding` to the list of rectangles you actually
  drew (box edges, label plates, snapline segments) and leave the rest of the window out of the shape: the
  game shows through exactly, no ARGB visual and no compositor dependency. A diagonal line can only be
  shaped as rectangles, so emit it as 2×2 chunks. Keep the shape empty while no game window is found and
  re-set it on every geometry change, or a stale mask keeps drawing at the old position.
- **Core-font text is 8-bit: draw ASCII only.** `XDrawString` sends bytes, so `·`, `—`, `…` (UTF-8 two/three
  bytes) render as two or three garbage glyphs — the label shifts right and reads like a layout bug
  (`F8 · F9` came out as `F8 A- F9`). Use `|`, `-`, `..` in every string that reaches Xlib.
- **Project player positions from matrices, not `WorldToScreenPoint`.** `Camera.get_projectionMatrix` and
  `get_worldToCameraMatrix` are struct *returns*, i.e. the same hidden-out-pointer ABI already proven for
  `Transform.get_position` (`rcx` = 64-byte buffer, `rdx` = `this`), while `WorldToScreenPoint` has an
  unknown struct *argument* ABI. Multiply `P · V` column-major in the payload, do the w-divide yourself and
  emit top-left-origin pixels (what X11 draws in). Self-check in the same line: the camera's own position
  must project to the screen centre, so log the pixel error and treat a large value as a bad read.
- **Verify the overlay without launching the game.** A `--demo` flag that feeds synthetic telemetry through
  the real parser, plus any window titled like the game (`xmessage -title VRChat -geometry 1000x700+400+148`),
  exercises attach, ESP, radar and panel end to end in seconds. Capture with `import -window <id>` (find ids
  with `xwininfo -root -tree`; override-redirect windows are **not** in `_NET_CLIENT_LIST`) and check
  `xwininfo -id <id> -shape` — the reported shape extents must equal the drawn area, proving the mask.

- Build such a crate as its **own workspace root** (`[workspace]` in its `Cargo.toml`): the parent's
  `[build] target = x86_64-pc-windows-gnu` otherwise makes `cargo build --workspace` link X11 into a mingw
  binary. Build the host target explicitly and derive `LD_LIBRARY_PATH` from `nix-shell`'s `NIX_LDFLAGS`,
  or the binary dies with `libX11.so.6: cannot open shared object file`.
- If the UI must be in-process, prefer `microui`/`Nuklear`/`ktnagui`-style libraries with their own vertex
  layout over Dear ImGui; save/restore the full D3D11 state around the draw either way.
- **For an interactive TUI, keep both terminal state and command parsing on the host.** Reuse the same
  incremental log tail and append-only command file, make the renderer testable without a TTY, and verify
  headless snapshots plus a real PTY session. Recipe: `references/host-overlay-and-ui.md` §3b.

Depth: `references/host-overlay-and-ui.md` (X11 window recipe field by field, NixOS build/run, Wayland
`DISPLAY`/`XAUTHORITY` discovery, host TUI checks, UI-library candidates, in-process checklist).

## Manual mapping under wine (verified end-to-end against a stand-in target)

Prove the mapper against a throw-away loop exe first (`crates/test-target`), never against the game:
`proton run Hromatite.exe inject --proc test-target.exe --dll 'Z:\path\x.dll'` in a scratch
`STEAM_COMPAT_DATA_PATH`. Things that bite, in the order they bite:

- `OpenProcess` needs `PROCESS_CREATE_THREAD` (0x0002) or `CreateRemoteThread` fails silently.
- **wine does not special-case DLL entry points**: a bare `CreateRemoteThread(base+EP, param=base)`
  delivers only one argument, so `DllMain` sees garbage in `reason` and skips everything while still
  returning TRUE. Call the entry through a hand-encoded x64 stub that sets rcx=base, edx=1, r8=0.
- A Rust cdylib **does** carry TLS callbacks (two, for `__dyn_tls_init`/`_CRT_INIT`). Run them with the
  same kind of stub before the entry point, or std init is half-done.
- wine's `kernel32` exports are mostly **forwarders into kernelbase**: an export RVA can point inside
  the export directory. Follow forwarders ("mod.Func") instead of calling the string address.
- Apiset imports (`api-ms-win-core-synch-l1-2-0.dll`, used by std's `WaitOnAddress`) are **not in the
  module list** and `LoadLibraryW` returns a fake HMODULE (`0xffbf0000`). Resolve those thunks by asking
  every module in the target for the export name (kernelbase has them) and use that module as the base.
- Import thunks hold the RVA of `IMAGE_IMPORT_BY_NAME` — read the thunk value, then `+2` for the name.
- `CreateRemoteThread(LoadLibraryW)` returns `STILL_ACTIVE` (0x103) as the thread exit code when the
  payload is still running, so the value is only the HMODULE when the thread has actually finished. Give
  the payload an env-configurable wait budget (`HROMATITE_WAIT`) so tests return in seconds instead of
  blocking on a module that will never appear.

## Same wineserver or nothing (Proton + SteamLinuxRuntime)

`wine` keys its server socket on `/tmp/.wine-<uid>/server-<dev>-<ino>` where `<ino>` is the inode of
`$WINEPREFIX`. Inside a pressure-vessel container that inode differs from the host's, so
`proton run` / `protontricks-launch` started from the host get a **different wineserver** and see none of
VRChat's processes (`Toolhelp32` shows only themselves). Verify with `pgrep -a wineserver` and by reading
`/proc/<wineserver>/environ` for `WINEPREFIX` + `cwd` = the server dir.

Consequences: cross-process injection must run *inside* the game's container. Working routes, cheapest
first: (a) Steam launch options wrapper — `%command%` is exec'd inside the container, so a script can
start the game and then the injector in the same session; (b) `version.dll`/`dinput8.dll` proxy in the game
folder (container-agnostic); (c) `steam-runtime-launch-client --alongside-steam` if you can run the
container's own copy (host copy needs the container's glib).

Wrapper recipe that is verified end-to-end (launch options entry:
`/path/to/tools/hromatite-launch.sh %command%`, `HROMATITE_DISABLE=1` for a clean boot):

```sh
"$@" &                     # %command% = proton waitforexitandrun <game>; runs inside the container
# poll instead of sleeping: a fixed delay races with the game's wineserver coming up
while ! pgrep -f "VRChat.exe"; do sleep 1; done
sleep 3
for t in 1 2 3 4 5; do
    "$PROTON" run "$EXE" inject --dll "Z:\\path\\x.dll" && break
    sleep 15               # retry: the injector may land in a session the game abandoned
fi
wait $GAME_PID
```

A fixed `sleep 20` before injecting produced a session with *no* game process in it; poll for the real
exe name, then keep retrying the injector. Validating the wrapper needs no game — copy a loop exe to
`VRChat.exe` in a scratch prefix and run the wrapper over it.

**Env vars do not reach the wrapper from a shell.** The launch-options string is fixed inside Steam and
the game inherits the *Steam client's* environment, so `HROMATITE_DISABLE=1 steam steam://rungameid <appid>`
changes nothing when the client is already running. Give the wrapper a mode file and read it first
(`echo KEY=VALUE` lines, `export "$line"` with no `eval`, so semicolons in `WINEDLLOVERRIDES` survive):

```sh
if [ -r ~/.hromatite-mode ]; then while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in ''|\#*) continue ;; *=*) export "$line" ;; esac
done <~/.hromatite-mode; fi
```

That makes an A/B stealth run (`clean boot` / `inject + seal + LIVE=0` / `inject + console`) a one-line
file edit plus a game restart, instead of relaunching Steam under a different environment.

## Loader-backed delivery + PEB unlink

`LoadLibrary` from inside the target is the cheapest way to get **file-backed** pages (the `/proc/<pid>/maps`
entry points at a real DLL, not an anonymous region), but it leaves an entry in the PEB module list. Remove
it from the payload, not from the injector:

```rust
let peb: usize; asm!("mov {}, gs:[0x60]", out(reg) peb);   // TEB+0x60 → PEB
let ldr = *(peb + 0x18) as usize;                          // PEB->Ldr
let head = ldr + 0x20;                                     // InMemoryOrderModuleList
```

**The list node is not the struct base.** `InMemoryOrderModuleList.Flink` points at the
`InMemoryOrderLinks` field, which sits `0x10` bytes into `LDR_DATA_TABLE_ENTRY`, so on x64:
`DllBase = cur + 0x20`, `SizeOfImage = cur + 0x28`, `BaseDllName = cur + 0x48` (a `UNICODE_STRING`).
Reading `DllBase` at `cur + 0x10` (the naive offset) never matches your own base and the unlink silently
returns false — log the walk count and the expected base when the entry is not found, or you will debug it
blind. Splice out with `blink->Flink = flink; flink->Blink = blink;` and keep the entry's own links
self-referential so a later traversal cannot follow a dangling node.

## Stealth: what the anti-cheat actually sees

An injected module is not the only evidence. The order of tells: unbacked *executable private* memory
(our `VirtualAllocEx` region) → `PAGE_EXECUTE_READWRITE` on its own → PE-shaped bytes even with headers
erased → a thread whose start address is outside every loaded module → an extra process/handle in the
session → a second Proton setup writing into `system32` while the game runs. Design against that list,
not against "is the DLL in the PEB".

- Allocate `RW`, fill, then `VirtualProtect` to `RX`; never leave an RWX page behind. Run the entry point
  and TLS callbacks from a region that is attributed to a legit module, not from a fresh stub page.
- **Never invoke `proton run` while the game is running** — it re-runs Proton setup and copies files into
  `system32`, which is a plausible installation-integrity kick (VRChat error 16). Reuse the live game's
  environment from `/proc/<pid>/environ` and call `wine` directly, or work from inside the
  launch-options wrapper where the env is already correct.
- **Stomp has no tail slack to live in.** Measured on a shipping Unity 6 build: the gap between the last
  section and `SizeOfImage` is a few KB in every module (`baselib` 0x96c, `UnityPlayer` 0x7c8,
  `GameAssembly` 0xbcc), while a Rust cdylib is ~1.2 MB even in release. Tail-slack stomping therefore
  either falls back to a fresh allocation or must overwrite a section that is dead after load (`.reloc` is
  already applied). Decide this before promising "mapped into a signed module".
- Keep the payload **inline in `DllMain`** when the goal is zero extra threads: the stub thread that called
  the entry is already ours, so waiting for `GameAssembly` there costs nothing observable in the game's
  thread list. Never spawn a worker thread inside the game "just in case".
- **A long-lived payload thread is a debug scaffold, not a design.** Keeping `DllMain` alive to answer
  console commands (`LIVE=<secs>`) is the only way to get interactivity before you have a game-thread
  hook; it blocks the injector for the whole session, prevents encrypt-at-rest (a runnable module cannot
  stay XORed), and adds the thread a scanner looks for. A hook on a per-frame game method (`PlayerLoop`,
  `Present`) replaces it: entry returns at once, ticks arrive on the game's own thread, and L2 becomes
  decrypt-on-call (decrypt page → `RX` → run → XOR → `RW`, stub outside the encrypted image).
- **A GC message does not, by itself, identify an injected-thread failure.** Read the
  parameterised `<game>.gc.log` line first and compare its `thread id` with the
  payload's logged tid. Only if that id is yours should you chase the last command
  and an unguarded read; a game worker's id points instead to Boehm thread lifetime
  (see below), even when the visible text is the same `SuspendThread loop failed`.
- **Before theorising about that message, parse the minidumps** (`scripts/minidump_threads.py <crash.dmp>`
  prints the exception, the per-thread suspend histogram and the module map) — Unity leaves them at
  `<prefix>/drive_c/users/steamuser/AppData/Local/Temp/<Game>/<Game>/Crashes/Crash_*/crash.dmp`, and they
  answer what the log cannot. A minidump parser (header → stream directory; stream 3 = thread list with
  full `CONTEXT`, 4 = modules, 6 = exception, 5 = the stack pages) gives you, per thread: id, suspend
  count, RIP mapped to `module+RVA`. Two facts settle the diagnosis: (1) the exception address names the
  abort site (`0x80000003` int3 inside `GameAssembly.dll` + an RVA that sits next to the
  `SuspendThread loop failed` / `ResumeThread failed` strings in `.rdata`); (2) the shape of the suspend
  counts. Unity's bdwgc fork loops `SuspendThread → GetThreadContext → ResumeThread` up to
  `MAX_SUSPEND_THREAD_RETRIES` (1e6, and that constant shows up as `0xf4240` on the aborting thread's
  stack). If **every** thread in the dump has `SuspendCount ≥ 1` except the aborting one, the thread the
  GC kept failing on is not in the process list any more: it is a **stale `GC_threads[]` entry** for a
  terminated thread whose handle the GC still holds (wine's `GetThreadContext` on a TERMINATED thread
  always fails, and `SuspendThread` on it can still "succeed"). That is a thread-lifetime bug, not a scan
  and not an unguarded read — an aborted payload thread (longjmp out of a guard, `TerminateThread`,
  `DllMain` returning while something still expects detach) or a game thread killed by our own VEH
  leaves the entry behind, and the game dies minutes later at the next GC, long after the inject.
- `GC_PRINT_STATS`, `GC_PRINT_VERBOSE_STATS` and `GC_LOOP_ON_ABORT` are compiled into the shipping
  `GameAssembly.dll` (grep its strings); `GC_LOOP_ON_ABORT=1` turns the abort into a spin, so a gdb inside
  wine can read `t->id` out of the `GC_suspend` frame instead of guessing.
- **Read Boehm's own thread table from inside the process — do not guess whose thread is stale.** It is
  `STATIC GC_thread GC_threads[256]`, an array of pointers in `GameAssembly.dll`'s `.data` (so its RVA is
  fixed per build). Find it offline by the code that indexes it, not by its contents (the array is
  zero-initialised): `GC_lookup_thread_inner` / the detach path compiles to
  `mov (%r14,%rsi,8),%rbx; cmp %ecx,0x8(%rbx); mov (%rbx),%rbx; mov 0x10(%rbx),%rcx; call CloseHandle`
  after a `lea r14,[rip+disp]` a few bytes above — that target is the table (VRChat Unity 6 build:
  RVA `0xe6bb180`), and the same code proves the record layout `next@+0x00, id@+0x08 (DWORD),
  handle@+0x10, stack_base@+0x18`. Walk the 256 buckets, read each record's `id`, and compare with
  `Thread32First/Next` of the process: a record whose id is not a live thread is the one that will
  kill the game. Log it every few seconds (`N records, L live, D DEAD [ids]`, plus `thread 0xN started at
  <start address>` so a later `thread 0xN exited` still has a name) — measured: the dead record appeared
  13 s before the abort, and it was a **game** thread (`0x220`), while the injected stub (`0x2e0`) had a
  healthy record. When reporting, that distinction is the whole finding: "our thread is unsuspendable"
  and "a game thread exited without unregistering" look identical in the log message but need opposite
  fixes.
- **Removing a stale record is only safe once the thread object is signalled.** Unlinking on weaker evidence (`Toolhelp32` no longer lists the id, or `GetExitCodeThread` says done) races the dying thread's own `GC_unregister_my_thread`: that path walks the bucket **by id with no NULL check** (compiled as `mov (%r14,%rsi,8),%rbx; cmp %ecx,0x8(%rbx); je …; mov (%rbx),%rbx; …`), so an emptied bucket turns it into an AV at the first `cmp`. Measured: unlinking 4 s "after death" produced `0xC0000005` at `GameAssembly+0xc417dc` inside `GC_lookup_thread_inner`. Gate the unlink on `WaitForSingleObject(record->handle, 0) == WAIT_OBJECT_0` — the thread object is signalled only after the thread has fully finished — and treat `WAIT_TIMEOUT` (including merely-suspended threads) as alive.
- **The leak is a normal exit, not a kill.** Measured on the dying thread's own handle: `GetExitCodeThread == 0` — it returned from its body yet left its record behind, and the VEH saw no exception at all. So poll the table on a sub-second cadence (every ~0.8 s from whatever thread you already own) and unlink as soon as `WaitForSingleObject(handle,0)` says terminated; the window between the death and the next collection is only a few seconds, and an 8 s watcher loses the race.
- Threads are also the only place a start address exists: `NtQueryInformationThread(handle, class 9,
  &pvStartAddress, ...)` (ThreadQuerySetWin32StartAddress) names a thread by module+RVA while it is
  alive. Dump it at thread creation, never after.
- **Assume a dead *game* thread before blaming your own.** Measured: the stale record belonged to a
  worker thread (`GameAssembly+0xbf8d10`, one of ~20 pool threads that start and exit in batches) with
  id `0x220`, while the injected stub had a healthy record. Say which one it is out loud: "our thread is
  unsuspendable" and "a game thread exited without unregistering" produce the same log line but need
  opposite fixes (guard the read vs fix thread lifetime / heal the table).
- **Those `start` addresses are a shared trampoline, not the thread body** — it copies `{thread*, fn,
  out_tid*}` out of its parameter and ends with `call *fn; ret`, so `GC_unregister_my_thread` runs
  *inside* `fn`. A body that exits early therefore leaks the record with no exception anywhere: eight
  minutes with a first-chance VEH logging every passing tid produced **zero** exception events while four
  such workers came and went. So instrument rather than blame an uncaught SEH — remember each thread's
  start address at birth, and at cleanup time log that address plus `GetExitCodeThread` (0 = clean exit
  with detach skipped, anything else = it was killed by someone). Depth: `references/gc-thread-table.md`
  §7–§8 (huge-DLL slicing, call/xref scanning, trampoline + `GC_lookup_thread_inner` vs
  `GC_unregister_my_thread` incl. its `int3` assert path).
- **A linear pass beats a table hunt when the layout is unknown.** Measured on a Unity 6 build: a
  full heap sweep performed ~19 M structural tests in ~5 s and surfaced ~300 k class-shaped
  records, but no contiguous `Il2CppClass**` array existed to walk (sparse CALLOC table, entries
  scattered). Filtering for "`+0x10` == the exact name string" found 77 candidates for `Camera` in
  one pass. Cost is visible in the game (`dt=5751ms` on one frame) — acceptable for a one-shot
  lookup, not for a per-frame path. Note the finder must dedupe and must not assume the first hit
  is an `Il2CppClass`: on this build the top hit was a Unity engine type-registry record
  (`{parent, code, name, handle, "Core"}`, 0x50 stride), not the il2cpp class.
- **`procs::regions()` is a snapshot and pages disappear while you scan.** A single stale page
  turns into an AV that, with one guard per region, silently drops the whole region (measured: most
  regions "faulted", zero candidates). Scan in chunks (~8 k words) with one `veh::call_with` per
  chunk and keep the partial results; also exclude `PAGE_GUARD` regions everywhere, they raise
  `0xC00000FD` on touch.
- **A frozen game is the same bug's second face.** No dump, no dialog, ~0.1 core of CPU, thread and
  record counts not moving, yet your payload thread still answers console commands = the world is left
  suspended (a collection wedged inside `GetThreadContext`), not your scan running long. Classify with
  those four numbers before theorising about memory.
- **Healing the table: unlink, never just flag, and do it sub-second.** `suspended = 1` on a dead record
  turns "SuspendThread loop failed" into `ABORT("ResumeThread failed")` (bdwgc aborts when
  `ResumeThread == -1`), and zeroing the handle still leaves the retry loop running. Splice the node out
  of `GC_threads[bucket]` instead; keep a console command for it and auto-apply when a dead record
  appears (with an env kill switch). Cadence matters more than the repair itself: an 8 s watcher that
  diffs the live thread list noticed a dead record, unlinked it, and the process still aborted 6 s later
  because the 1e6-iteration suspend loop had already started (~6 s long) — heal from a sub-second pass
  that asks the record's own handle instead of taking a `Toolhelp32` snapshot.
  **Decide "dead" with `WaitForSingleObject(handle, 0) == WAIT_OBJECT_0`, nothing cheaper.**
  `GetExitCodeThread` and the `Thread32` list both report a thread gone while it is still inside
  `GC_unregister_my_thread`, and splicing then is fatal: `GC_lookup_thread_inner` in this build has no
  NULL check on the bucket head (`mov (%r14,%rsi,8),%rbx; cmp %ecx,0x8(%rbx)`), so the dying thread's own
  lookup walks into the emptied bucket and takes an AV at `GameAssembly+0xc417dc` — the collector abort
  becomes a "random" access violation in the same thread you just reaped. `WAIT_TIMEOUT` means alive.
  Read the exception's *module* in the
  dump: `int3` in `GameAssembly` is this abort, `int3` in `UnityPlayer` with every thread suspended is
  Unity's own `DebugBreak` (the uniform suspension is the crash reporter, not a collection). Status:
  measured — two records reaped at ~0.8 s and the following collections completed with no suspend error.
**A periodic reaper is only hygiene; it cannot guarantee the race window.** When
  normal worker exits churn sub-second, an 0.8 s and even 250 ms cadence can be
  too late: the collector starts before the next scan and the game thread holding
  the tick is suspended too. Prefer build-gated hooks on Boehm's **exact call-sites**,
  not broad Win32 API hooks: an unrelated caller for an old handle must not mutate
  the table without the collector's lock. Cover both `SuspendThread` and
  `GetThreadContext`, because a handle already closed by Boehm can fail at the
  earlier suspend call and never reach the latter guard.
  On VRChat 6000.0.67f1 (`648f61b1…`) hooking `GameAssembly+0xc41ad2` covers
  `call [GetThreadContext]; test eax,eax; jne` (14 bytes; verify loaded bytes
  before patching and relocate the whole sequence). The synthetic path enters
  the relocated `test` at trampoline+6 with `EAX=1`, returning success even when
  the record was already removed. Before deleting anything require
  `WaitForSingleObject(handle,0) == WAIT_OBJECT_0`. Write the synthetic context
  before best-effort unlink, using the target's exact Win64 ABI (`CONTEXT` is
  `0x4d0` bytes; preserve its `ContextFlags`). Removal is
  required (not a synthetic flag): the subsequent `GC_resume_all` would otherwise
  abort on the same handle. Keep the hot detour free of VirtualQuery/RPM/WPM,
  module lookup, Toolhelp, mutexes, allocation, formatting and file I/O; publish
  repairs through atomics and log them from the normal tick. A naked call-site
  wrapper must save and restore **both RCX and RDX** around its router call
  before tail-jumping to the copied OS call — the router itself may clear EDX.
  Keep shadow space separate from argument-save slots and restore RSP. Add a
  release-artifact test for argument preservation. Full ordering and ABI checks
  are in `references/gc-thread-table.md` §9. Paired inline guards can close the
  race without adding a thread or weakening the safe-unlink invariant. **Install the guard in every execution mode that needs it:**
  a stub-thread console must arm it directly, not rely on an installer called only
  from QPC/frame setup, because that tick setup never runs there. Do not install an
  in-image guard under full sealing; that requires an external shim. Classify
  `WAIT_OBJECT_0` and `WAIT_FAILED + ERROR_INVALID_HANDLE` separately from
  `WAIT_TIMEOUT` and all unknown failures; only the terminated/closed cases may
  repair a record. A final-resort VEH may resume only the exact proven Boehm
  abort instructions after pointer-matching and unlinking the dead record — never
  swallow arbitrary `int3`, access violations or timeouts. Version-gate every
  patched address and prove `GC #N` advances after the repair line, and disassemble
  the removal path to see whether `CloseHandle` precedes unlinking; that ordering
  explains how an invalid handle can remain visible to the collector.
  Mode matrix, invalid-handle semantics and static ABI checks:
  `references/gc-thread-table.md` §9.
- **Disassemble the exact hook target before assigning it a per-record ABI.** A function that starts by
  walking `GC_threads[]` is the suspend-all walker, not `GC_suspend(GC_thread*)`; hooking it as though
  `rcx` were a record skips the whole collection and interprets unrelated data as a handle. If a direct
  suspend guard needs a new anchor, derive its argument shape from the prologue/call sites and reject it
  on mismatch. Do not authenticate an RVA hook by comparing a generated hash constant with a literal:
  that proves only the payload's generator inputs, not the runtime module. Verify the loaded module's
  identity/bytes, and keep the previous guard until the replacement has the required live proof.
- **The host is an in-process debugger for a frozen game.** `/proc/<pid>/mem` of the wine process opens
  `r+b` even with `ptrace_scope=1`: read `GC_threads[]`, verify a record against what the payload
  logged, write one byte or unlink a node, then watch `/proc/<pid>/stat` (`utime+stime`) and the task
  count for a reaction — no rebuild, no reinjection.
- **Capture what the game prints, or you will debug blind.** Under Steam a Proton game's stdout/stderr
  is swallowed: redirect `"$@" >> ~/.hromatite-logs/game-<ts>.log 2>&1` in the launch wrapper (that is
  where Unity/Boehm print the fatal line), export `GC_PRINT_STATS=1` for per-collection stats, and copy
  `<game_dir>/VRChat.gc.log` before **and** after the run — Unity overwrites it at every fatal, so an
  un-backed-up copy is one launch away from being lost.
- **Encrypt-at-rest (L2) is easy once the payload is done — and must happen from outside the image.**
  XOR every page of the mapping with a per-page keystream and `VirtualProtect` it to `PAGE_READWRITE`
  *after* the entry point has returned into the stub thread; the injector (or a thunk that never returns)
  does it, because the code performing the XOR cannot live on the page it is rewriting. At rest the
  region then has no `x` permission, no `MZ`, and no parseable code — the strongest cheap win against a
  host-side `/proc/<pid>/mem` reader. Wait for the real thread exit (a fixed timeout can expire mid-scan
  and you encrypt live code); skip the seal if the exit code is `STILL_ACTIVE` (0x103).
- **An installed frame hook and a seal are mutually exclusive.** The seal fires when the entry point
  returns, but a hook's detour lives in the image's `.text`, so the very next frame executes ciphertext;
  a trampoline page allocated outside the image does not save it (the patch site still points into the
  sealed page). Either keep the payload inline for sealed runs or pause the seal while a hook is armed —
  never ship both without deciding which one owns the image.
- **Your own ciphertext is a fingerprint you can use for verification**: with a fixed key and a page-index
  keystream, page 0 always produces the same first bytes, so the host can locate your region in
  `/proc/<pid>/maps` without knowing the base — scan anonymous `rw-p` regions and match the 8-byte
  signature. Hardcode that signature only for the current key.
- Get user consent before the "maximally stealthy" redesign: it changes the whole injection design
  (module stomping into a signed image, dual/shadow pages with encrypt-at-rest, no own threads), so it is
  a phase of its own, not a refactor squeezed into a feature step.

Depth: `references/eac-detection-and-stealth.md` (vectors, L1/L2/L3 ladder, env-copy recipe, sources).

## Working inside the game's own thread

- Unity's GC is stop-the-world via `SuspendThread`, and a thread of yours that sits in a long scan or a
  blocking read cannot be suspended — the game dies with `Fatal error in GC — SuspendThread loop failed`,
  or simply stops answering commands. Measured on a 15 GB machine: an in-process scan of 1.0–1.5 GB
  freezes the game, ~512 MB with a 5 ms pause between regions passes. Order the scan game modules first,
  heap last, and never poll the command file from the tick while a console thread is alive (a passive
  tick in `LIVE` mode is the fix).
- Hook a per-frame function (QPC, `Present`, PlayerLoop) and do the work there; return from `DllMain` at
  once. That is what makes a closed injector and `LIVE=0` survivable, and it removes the thread scanners
  look for. A hook and an encrypt-at-rest seal fight over the same `.text`, so pick one per run.
- **For a managed `LateUpdate` anchor, prove both the signature and that the instance is live.** Match an
  own method named exactly `LateUpdate`, then accept only `void` + zero managed parameters + non-static
  before patching: on the Unity 6 layout above read `return_type@+0x20`, `flags@+0x4c`
  (`METHOD_ATTRIBUTE_STATIC = 0x10`), `parameters_count@+0x52`, and the type tag from
  `((u32[return_type + 0x08] >> 16) & 0xff) == IL2CPP_TYPE_VOID (1)`. Name-only selection corrupts the
  detour ABI on overloads/static/parameterized methods. Materialise the type table first, install a hit
  counter, and require real hits plus a stable registered tid before calling the anchor per-frame; a
  plausible class name can have zero invocations in the current scene. Sample the status again after
  the first hit to confirm a sustained rate: installation proves only that the patch exists, and one
  hit proves only one invocation.
- **Run pre-original work early without claiming global script priority.** Invoke the guarded payload
  before the selected method's trampoline, then invoke the original even when the guarded payload fails.
  This only puts your work before that method's own body; it does not move the method ahead of other
  scripts. For automatic selection, verify a unique fully-qualified class and its method signature, use a
  type index only as a build-gated materialisation hint, and never substitute an arbitrary candidate or
  silently fall back to QPC. A root `PlayerLoop` hook is a separate mechanism: first prove the
  `GetDefaultPlayerLoop`/`SetPlayerLoop` ABI and a concrete non-null native `updateFunction` in the target
  build instead of promising it from the managed API's existence.
- **Defer automatic installation until lazy metadata is available.** From the injection stub (never a
  QPC detour), call `il2cpp_image_get_class` for the selected index to materialise it, then retry with a
  short period and a hard **wall-clock deadline measured across the whole attempt**, not an attempt
  count: one metadata/class scan can take longer than the nominal retry sleep, so `N × sleep_s` can
  turn a 30-second budget into many minutes. Report actual elapsed time in the wait log. Do not return
  from `DllMain` permanently without a tick merely because module presence preceded class creation,
  and do not silently switch an explicitly requested managed anchor to QPC. Generate pure host tests
  for selector/signature/retry/lifecycle rules; cover the real hook wiring in the target build and live
  run rather than forcing Win32-only calls into host tests.
- **For a native root PlayerLoop tick, descend past group nodes.** `GetCurrentPlayerLoop` returns a 0x28-byte managed struct via a hidden output pointer (`rcx`): type@+0x00, children@+0x08, delegate@+0x10, updateFunction@+0x18, condition@+0x20. Every child array has count@+0x18 and data@+0x20 with the same 0x28-byte stride. Root groups can all have `updateFunction = 0`: walk children with explicit depth/node bounds and select the first proven callback in the requested branch instead of starting an arbitrary fallback.
- **Dereference the updateFunction slot, not just the managed pointer.** On the measured Unity 6000.0.67f1 build the managed `updateFunction` addresses sit in RW memory and contain the executable stub address. Patching the field itself fails instruction decoding; read its first qword and patch that loaded callback. Publish its trampoline before atomically patching it, preserve the original on every call, and never call `SetPlayerLoop` merely to create a tick. For install/unhook lifecycle, forbid target changes after detach so an in-flight callback cannot jump from one subsystem's trampoline to another's.
- **Track attachment separately from a retained trampoline.** A shared unhook must clear the attached
  flag so status cannot report a live hook after its bytes were restored, while the trampoline remains
  alive for already in-flight callers. Repeating installation for that same target must repatch it; only
  a duplicate that is still attached is an idempotent success. After every restore, derive attachment
  from all currently non-detached hooks: historical detached records can mask a failed detach of a later
  matching hook. Do not change target live without a per-target original/trampoline design.
- Hand-emitted code: an instruction must occupy exactly the bytes you decoded. `mov qword [rdi+0x38],
  imm32` is 8 bytes, not 5 — emitting the shorter form desynchronises every following instruction and the
  fault lands dozens of bytes later, which reads like an unrelated bug. Disassemble your own shim and
  diff its length before blaming the hook.
- Decoder invariants: `0x66` narrows immediates to 16 bits unless REX.W wins; reject legacy prefixes
  after REX instead of retaining REX.W. Relocated displacements use `original_disp - delta`, with
  `delta = trampoline - target`. Refuse a hook when a relative branch targets its overwritten prologue.
  Atomic aligned-window patching must use `window_base - aligned_base` as the source index, not
  `target - window_base`: a test where `E9` is expected immediately after two bytes catches an
  accidental shift into a privileged opcode at the target. Cover every accepted `wmode` alignment
  through the real `write_window`, including restore, not just the naked detour ABI.
- **Treat global diagnostic queues and registries as shared mutable test state.** Isolate each
  test's fixture or serialize the tests that own the same registry with a test-only lock; otherwise
  focused tests stay green while the default parallel suite drains another test's events. After
  touching those registries, repeat the full suite with normal parallelism and compare an unexpected
  failure with the baseline before calling it a regression.
- **Land large removals one subsystem at a time.** Delete a module and its call sites, compile and
  test that slice, then proceed: chained position-based edits against stale offsets can duplicate
  entire blocks, and a formatter can accept duplicate methods or unresolved symbols because it does
  not resolve names or types.
- Live-memory scanners need OS-backed copies or owned snapshots, not public `usize` dereferences.
  Reuse bounded chunk buffers (64 KiB typical), exclude chunk-lookahead from candidate starts, and
  reset pointer-run state immediately after every failed chunk.
- A hand-written crypt/`rep` loop that uses `rdi` (or `rsi`) as its counter destroys the pointer you
  needed afterwards — reload it after the loop.
- `VirtualProtect(RX)` only over executable sections. Making `.data` RX breaks any detour that writes its
  own counters or locks a Rust `static` mutex living there.
- Keep every shim's state page inside the image range you pass in; a Rust `static` can be linked *outside*
  the mapped image, and a probe comparing "is this address inside the image" then treats plaintext as
  ciphertext.
- `VirtualQuery` answers with a section-by-section split, not `SizeOfImage`. Pass `(base, size)` and the
  executable range from the injector through the stub's spare register instead of recomputing them
  in-process.
- Debugging a shim: have it write stage / fault-address / fault-code counters into a data page and read
  that page from the host (`/proc/<pid>/mem`, needs `ptrace_scope=0`). With `gdb` on wine, ignore its
  signals (`handle SIGUSR1 SIGUSR2 nostop noprint`) or it stops constantly on normal traffic.

Depth: `references/inline-hook-shim-debugging.md` (decrypt-on-entry shim, stage markers, measured faults).

## Pitfalls

- **When an anti-cheat layer is present, the game's log is the only ground truth for what is active.**
  VRChat's EOS anti-cheat opens and closes a session on every world join (`[AntiCheatClient] BeginSession`
  / `EndSession` around each `Entering Room` / `OnLeftRoom`), so "protection is on for this run" is the
  wrong model — read the pairs. Depth: the `reverse-engineer-anything` skill's
  anti-cheat artifact reference (§ Where the heartbeat lives).
- **An inline hook's trampoline must jump back to `entry + copied_len`; resuming at `entry` is an infinite
  self-call** that looks like a hung game with a growing log. Cap every payload log at open time and
  rotate at tens of MB — one such recursion wrote 8.7 GB in ~35 s from inside the game, on the host's disk.
- `UnityPlayer.dll` has **no static import** of `GameAssembly.dll` — do not look for ordinal mapping in its import table; resolution is dynamic by name at runtime.
- The name block in `.rdata` is **inline strings, not pointers** (8/16-byte aligned, no base relocations). Searching for an array of pointers to them finds nothing; search for the strings themselves and their file order.
- Beebyte renames exports per build, so any hardcoded RVA is valid for exactly one GameAssembly hash. Re-fingerprint after every game update.
- Old Unity `il2cpp-api-functions.h` lists (2017–2020) are a good ordering proxy but shorter than a Unity 6 table; align by prefix + count, and confirm at runtime (`il2cpp_domain_get` must return a stable non-NULL pointer, `il2cpp_domain_get_assemblies` > 1 assembly), never by assumption.
- Public dumps from another build (community repos, older Unity) give class/method *names* only; offsets and export names differ.
- **Unity 6 moved things that older write-ups still put where they were** (checked against
  `il2cpp-class-internals.h` 6000.0.x and a live dump): `MethodInfo` is
  `methodPointer@0x00, virtualMethodPointer@0x08, invoker_method@0x10, name@0x18` (the name moved
  off `+0x10`); `Il2CppCodeGenModule` ends at `staticConstructorTypeIndices@0x70,
  metadataRegistration@0x78, codeRegistaration@0x80` and has **no `typeInfoTable`**, so a
  "walk the module's type table" plan yields zero classes; `s_TypeInfoTable`
  (`GlobalMetadata.cpp`) is `IL2CPP_CALLOC(typesCount*8)` — a sparse array of NULLs with lazily
  filled class pointers, so density-window detectors must tolerate holes (accept "zero or
  class-like" runs, not 75 %-dense windows). Confirm any layout against the current build before
  writing a scanner: fetch the matching `il2cpp-class-internals.h` for the Unity version and dump
  one live struct of the type you think you have. Measured 6000.0.67f1 differs again:
  `Il2CppClass` keeps `namespaze@0x18`, `methods@0x50`, `fields@0x80`, `name@0x98`
  (`fieldsCount` u16 `@0x124`), `FieldInfo` has stride 0x28 with `name@0x10`, and the class table is
  `s_TypeInfoDefinitionTable` in `.data` (found from the indexing code, see §2b).
- **Do not make foreign il2cpp calls from a QPC detour.**
  `QueryPerformanceCounter` is called by wine/DXVK/audio threads too, so the detour
  can land on a thread Boehm never registered; the first allocation inside the
  callee starts a collection from it and Unity pops `Fatal error in GC —
  Collecting from unknown thread`. Materialise classes and perform exploratory
  calls on the injection stub before it returns. After a managed frame anchor has
  produced real hits on a stable, GC-registered Unity thread, move only
  already-identified calls with a proven ABI onto that thread and re-check the
  sequence live; caller identity and ABI, not the word "tick", decide whether a
  call is admissible. Keep unbounded scans out of every per-frame path.
- **The decrypted metadata may not exist as one blob at all.** Measured on a
  Beebyte-protected Unity 6 build: the file is `mmap`ed `r--p` and still ciphertext,
  the magic appears twice in the whole 4.9 GB process and both inside `r-xp` code,
  a 38 MB anonymous slab holds decrypted *strings* (`mscorlib.dll`, field names)
  plus `Il2CppType`-like arrays but **no 256-byte header**, and four structural
  header oracles (full pair check, early-pairs-only, with/without a version match)
  return zero candidates. So "dump the metadata and run Il2CppDumper" can be
  impossible — check for the header before promising that pipeline.
- **Identify the module base twice, from content.** `gct_addr - rva` and
  `strstr("<obfuscated export name>") - its_rva` must agree; the PE header of a
  wine-mapped image is often a 4 KB stub while the body sits in anonymous regions,
  so "the region that starts with MZ" is the wrong base.
- **Never brute-force call every export to "identify" the API.** A phase that called all 163
  callable exports with `(domain, &count)` killed the game *in stub-thread mode* — the log shows a
  few `caught 0xc0000005` and then the process is gone with no dump, because the set contains
  shutdown/allocator-shaped entries. Call only functions already identified (by disassembly shape, a
  surviving name, or a stub whose target you verified), and put an RSS watchdog plus a small budget
  around anything exploratory. The same phase under a QPC detour also pops
  `Fatal error in GC — Collecting from unknown thread`, so exploratory calls belong to stub-thread
  mode (`LIVE>0`) and the frame-hook mode stays read-only.
- **Cache resolved method entries with `Box::leak`, never a `Mutex`.** A guard is a longjmp: the panic path
  abandons every frame below it, so a `MutexGuard` alive at that moment is leaked *locked* and every later
  command blocks forever. Leak a `Box<struct>` once (the ABI entries are stable for the process lifetime)
  and read it through a raw pointer.
- **Guard every dereference when walking live heap** (`VirtualQuery` → committed + readable, then read).
  One unguarded word read past the end of an array faults inside the stub thread and kills the game;
  the log then shows the mapping succeeding and a later step failing, which reads like an unrelated bug.
  Keep exactly **one** helper module (`safe::{readable_at, read_u64, read_bytes, ascii_at}`) and route
  every scan through it: a per-file copy of the `readable` check drifts, and each file that kept its own
  quickly grew an unguarded window (`-0x80..+0x80` probes, nested `q+8*i` dumps, heap-list walks).
- **Snapshot heap structures during the scan, not after it.** The game keeps allocating while you walk,
  so re-reading the same address a second later shows recycled bytes (your own log text, in the worst
  case). Copy the neighbourhood into your own buffer at the moment of the hit. The same holds for a
  search command (`find`/`findb`): the hits are true for that instant only — follow up at once or re-run.
- A post-injection step reporting "0 pages written / N failed" usually means the **target process is
  already gone**, not that the write API broke: verify the process is alive before blaming
  `ReadProcessMemory`/`WriteProcessMemory`.
- **A payload that stays alive in `DllMain` makes the injector block** for as long as the session lives,
  so the injector's own "mapped at" line arrives minutes late or never. Treat an early line from the
  payload (e.g. "console ready") as the success signal, and poll only the **new tail** of the log
  (record the file size before launching, then `tail -c +<size+1>`) or the first poll matches the
  previous run's text.
- `pgrep -f <pattern>` matches your own shell whenever the pattern appears in the caller's command line;
  piping it into `kill` therefore kills the session. The bracket trick (`VRChat/launch[.]exe`) only helps
  when the pattern string itself is the thing quoted in your command line — it still matches if the same
  literal appears elsewhere in the same command (a log path, an earlier `echo`, a `grep "VRChat.exe"`),
  and `pkill -f <script>.py` matches the wrapper shell that launched that script. The most expensive shape
  is `kill -9 $(pgrep -f '<pattern>')` inside one command: the shell running it carries `<pattern>` in
  its own argv, so the substitution hands back the caller's pid and the command dies with SIGKILL itself
  (a session-ending "exit code -9"). Collect the pids in a *separate* call, kill explicit numbers, and
  verify with `ps -eo pid,args | grep` instead of assuming the kill landed.
- In `/proc/<pid>/maps` an anonymous region ends with `0` + a trailing space, so the inode is field `$5`
  and `$6` is empty — filtering on `$6=="0"` silently matches nothing. Reading `/proc/<pid>/mem` can
  block waiting for ptrace, so always wrap the read in `timeout`.
- **A `/proc/<pid>/mem` read can return short or `EIO` while the game is mapping/reclaiming, and one
  failed read poisons the open file object**: every later `read()` then returns short, which surfaces as
  a `struct.error` from `unpack` rather than a read error — you will chase a layout bug that is not there.
  Read in a loop until `n` bytes, treat a short/`EIO` read as "skip this chunk", and reopen the handle
  after any `EIO` before continuing.
- **Respect who owns the game launch.** If the user says they will launch it, do not kill/start/restart;
  wait for their run and use read-only console commands. If the user withdraws restart permission
  mid-cycle, stop all further kill/start/inject actions immediately and finish only host-side checks
  and commits; re-check the process and GC log afterwards instead of assuming it remained alive. If
  the user explicitly asks for another log capture driven by the agent, kill leftovers by explicit
  pid, start `steam steam://rungameid <appid>` with `background=true`, and treat "new run is live" as
  the payload's fresh readiness line, not merely a new native process.
- **Kill stale stand-in targets before an A/B run.** Leftover scratch-prefix copies of a loop exe named
  `VRChat.exe` satisfy the wrapper's `pgrep -f VRChat.exe` poll and any lookup by process name, so the
  injector can land in the wrong prefix while the real game boots untouched (the log then shows a clean
  run and you conclude "injection did nothing"). `pgrep -a VRChat.exe`, check each pid's exe and
  `WINEPREFIX`, kill the leftovers, then launch.
- Deep disassembly of a 250 MB `GameAssembly.dll`: `objdump -d --start-address/--stop-address` silently
  prints nothing for high RVAs. Slice the bytes out yourself (`file_off = rva - section_va + raw_ptr`)
  and disassemble as flat code (`objdump -D -b binary -m i386:x86-64 --adjust-vma=0x<rva>`); RIP
  displacements are then relative to the slice, so add the slice's real RVA when computing a target —
  otherwise a correct pattern looks wrong.
- A second **loadlib** delivery into a process that already has the DLL loaded returns the same base and
  does **not** run `DllMain` again. A rig run with injector lines but no payload lines is that, not a
  broken entry point — restart the game between such tests.
- `xxd` is not guaranteed on NixOS — use `od -A x -t x1z`.
- NixOS mingw link fails with `cannot find -l:libpthread.a`: add
  `rustflags = ["-C", "link-arg=-L/run/current-system/sw/lib"]` to `.cargo/config.toml` (the lib comes
  from `pkgsCross.mingwW64.windows.pthreads`).
- The export-name pool order in `UnityPlayer.dll` is **not** the Unity 2020 `il2cpp-api-functions.h` order:
  two surviving plain anchors (`il2cpp_native_stack_trace`, `il2cpp_gchandle_get_target`) land at pool
  indexes 71/128 versus header indexes 83/116, so Unity 6 inserted entries in the middle. Do not map
  obfuscated names by order — resolve empirically at runtime.
- Flush stdout before `std::process::exit` in a wine-hosted CLI: `println!` is block-buffered when
  stdout is a pipe, so an exiting wrapper loses every line it printed (the log file keeps them, which
  makes the loss look like "the tool never ran").
- A second `proton run` can hang for minutes (at `CreateToolhelp32Snapshot` or before the exe even starts)
  while another `proton run` on the same prefix is still alive — Proton appears to hold a prefix lock.
  Start the game and inject from **one** shell invocation, or kill the Proton python wrapper (leaving the
  wineserver and game running) before injecting. Ship a `--pid` flag that skips process enumeration for
  when Toolhelp blocks anyway.
- Run host-side Python probes with `uv run --no-project python script.py` when the host has no system python3.

## For this user

- Plan first, implement after approval: state verified facts (with the commands that produced them), then phase the work M0..M4 with a "done" criterion per phase, then close with numbered choices — `1 — do (A/B/C)`, `2 — change plan`, `3 — alternatives`, `4 — skip`. Russian.
- He writes Rust on purpose ("future safe calls" even when going raw memory), so propose a workspace of crates (bindings / symmap / injector / cheat) rather than one DLL blob.
- Testing on a throwaway account / invite-only world is the default assumption; say which rung carries ban risk.
- He wants **maximum stealth**, not just "it works": expect to be asked for dual/shadow memory and
  "clean under a scan", so keep injection design separable from feature code.
- Commit every change in the project repo as a real commit (small, message = the verified fact), not one
  giant commit at the end. Append every finished step to the repo's work-log section (AGENTS.md §17 in
  Hromatite) with the evidence that proved it.
- He authorises several agents on one repo, but demands they not overwrite each other. Freeze the shared
  surface first in one parent commit (workspace `Cargo.toml` profile, `ffi.rs` declarations, `lib.rs`
  `mod` list, empty stub modules), then give each child a disjoint file list and say explicitly which
  files it must not touch. Give each child its own `git worktree` + branch off that frozen commit instead
  of a shared tree: in one tree each child's `cargo build` compiles a sibling's half-written file and
  reports phantom errors, and two `git commit`s race on `index.lock`. Children commit on their own branch,
  the parent merges. Only one game process exists, so children verify by `cargo build` and static
  reasoning; the parent serialises every in-game run (A/B modes, live commands). Subagents cannot write
  `AGENTS.md` (protected agent-instruction file → "write denied"), so the parent appends the journal entry.
- When he asks for "more logs", what he wants is **instrumentation shipped, not analysis written**: add
  the counter/command/capture, rebuild, commit, and tell him the exact grep that will answer the question
  on the next run (e.g. "the fix worked if `reaped …` appears before `SuspendThread loop failed`").
  Always fold in the game's own output (`<game>.gc.log` before/after, captured stdout) — he counts {"the
  game's log"} as part of our logging.
- **If the log must explain event-scoped behaviour (anti-cheat session per room, heartbeat cadence
  around world loads), give it a schema, not prose.** One JSONL line per event with a process-wide
  `epoch` that increments on every `BeginSession`, a `seq`, and `dt_ms` since the previous heartbeat;
  emit a separate `gap` record when `dt_ms > 2 × nominal interval`, measured **across** epochs (the first
  heartbeat of the next world then reports the whole world-load stall). Mirror the game's own lines
  (`Entering Room:`, `OnLeftRoom`, `OnDisconnected`, `[AntiCheatClient]`) into the same file, same clock,
  `[game]`-prefixed. Use identical field names (`epoch`, `seq`, `dt_ms`, `gap`) in the host-side tool and
  in the in-payload logger so both logs merge, and ship a reader that prints the timeline plus per-epoch
  counts — a generator whose session state you cannot see is not testable.
- A terse live report ("the game hung", "it crashed again") means **triage now, not theory**: is the
  process still alive, what does the payload log end with (timestamps!), what is in `<game>.gc.log`,
  is there a new `crash.dmp` (`scripts/minidump_threads.py`), what do `/proc/<pid>/task` count and
  `utime+stime` say. Answer with those facts, the fix already built/committed, and one next step — a
  report first, questions after.
- This work runs in long autonomous stretches (rebuild → relaunch → read log, repeated). Break the
  silence with a one-line status after every cycle; he asks "what are you doing?" the moment the trail
  goes quiet for several minutes. "Do both" style answers (a numbered choice like `1 + 2`) mean do both
  in one pass, not one and then wait.
- When he says "read the articles", he wants the distilled technique plus sources, and accepts mirrors /
  writeups when the forum itself is behind a challenge page.
- "Давай проименуем функции" / "найдём экспорты и их реальные названия" means **rename them in the open
  Ghidra/IDA database so he can read the listing** — not an analysis document. If a bare ambiguity would
  derail the naming, resolve it with the decompiler rather than shipping `cand_` names everywhere.
- "Перепроверь все имена, разрешаю глубокий анализ" is a request for an **independent blind re-derivation**
  (fresh agents, old name = hypothesis, proof from callees/offsets/arguments), never a re-read of the
  evidence you already wrote. He accepts 10 parallel reviewers with disjoint address bands; report
  `confirm / uncertain / wrong` counts and every correction as `addr: was → is, why`.
  He also saves the Ghidra project himself when the tool refuses, so don't stall on that — but do say
  plainly that unsaved renames live only in the open session.
- If `AGENTS.md` opens with a "next session prompt" block, that block is a **consumed one-shot**: read it,
  delete it from the file (heading + fenced block), commit the deletion, then follow its instructions. He
  says so explicitly ("прочитай промпт для себя и удали его"), and will be annoyed to see it still there
  next session.

Depth: `references/ghidra-export-naming.md` (naming the obfuscated exports in Ghidra: export-table parsing,
shape table, rename recipe, parallel bands),
`references/gc-thread-table.md` (minidump parsing, `GC_threads[]` discovery + record layout,
repair semantics, freeze-vs-crash, host `/proc/<pid>/mem` poking, huge-DLL disassembly),
`references/inline-hook-shim-debugging.md` (decrypt-on-entry shim, stage markers, measured faults),
`references/vrchat-proton-internals.md` (VRChat layout, EAC, observed numbers),
`references/il2cpp-runtime-layout.md` (getter → corlib image → `Il2CppImage[]` → class table, measured `Il2CppClass`/`FieldInfo`/`MethodInfo` offsets, guarded walker, host `/proc/<pid>/mem` reads),
`references/runtime-metadata-dump.md` (runtime metadata dump → Il2CppDumper: versions, dotnet/`RequireAnyKey` traps, script.json→RVA, header oracles),
`references/host-overlay-and-ui.md` (host-side X11 overlay recipe, NixOS build/run, ImGui-free UI library candidates).
Scripts: `scripts/probe_il2cpp_exports.py` (name-pool probe),
`scripts/export_shape_dump.py` (per-export shape table: RVA, size, arg-register memory operands, call/global
references, thunk targets — run it before naming anything in the decompiler),
`scripts/minidump_threads.py` (exception + per-thread suspend histogram from a Unity `crash.dmp`).
