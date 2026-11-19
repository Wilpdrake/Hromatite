# EAC detection vectors and the stealth ladder

What an internal cheat leaves behind, and what to do about each tell. Applies to EAC (EOS) on
Windows and, mostly unchanged, to its Linux module reading a wine process from the host.

## Detection vectors that actually fire

1. **Unbacked executable private memory.** An injector's `VirtualAllocEx` region has no file backing,
   so a VAD/maps walk flags it: `private + executable + no file section` is treated as shellcode or a
   manual map, then scanned. This is the first thing to remove from your footprint.
2. **`PAGE_EXECUTE_READWRITE` alone.** Legit code is RX, legit data is RW; RWX is reported even when no
   PE signature is found. Allocate RW, fill, then flip to RX.
3. **PE-shaped bytes in private memory.** Erasing `MZ`/`PE` is not enough: page-aligned section
   boundaries, IAT arrays of resolved pointers and `.text` entropy are all pattern-matched.
4. **Thread start outside any known module.** A thread-notify callback checks whether the start address
   falls inside a loaded image; a remote thread created from an anonymous stub is a direct give-away.
5. **Process tree and handles.** An extra wine process in the same session with an open handle to the
   game (and a second wineserver invocation) is visible in telemetry.
6. **Installation integrity.** Anything that writes into the game tree or `system32` while the game runs
   can surface as "Anti-Cheat failed to verify your installation's integrity" (VRChat error 16).

## Stealth ladder (cheapest first)

- **L1 — backed, non-RWX, headerless.** Map into an existing signed image's slack space ("module
  stomping"; a small always-loaded DLL such as `baselib.dll` works well) so `maps`/VAD attributes the
  region to a legit module path; keep pages RX; zero the headers after mapping.
- **L2 — dual/shadow pages ("double memory").** Keep a pristine copy of every code page (what a scanner
  reads) and a working copy encrypted at rest, `PAGE_READWRITE`, not executable. Execute through a tiny
  fixed stub: decrypt page → RX → run → XOR back → RW. Outside those microseconds a scan sees either
  clean original bytes or ciphertext. Optional `PAGE_GUARD` + VEH on the pristine pages to learn when you
  are being read and force the clean view at that moment.
- **L3 — no own threads, no own process.** Drive logic from a hook that already runs every frame
  (`Present`, Unity player loop) instead of a worker thread; if a thread is unavoidable, create it via
  `NtCreateThreadEx` with `THREAD_CREATE_FLAGS_HIDE_FROM_DEBUGGER` and exit it immediately.

Caveat specific to Proton: the Linux anti-cheat reads via `/proc/<pid>/mem` / `process_vm_readv`, which
ignore page protections. Execute-only pages therefore buy nothing there; backing, absence of RWX,
encryption at rest and a clean thread/process tree are what matter.

## Injecting without a second Proton setup (the integrity trap)

Calling `proton run` while the game is already running re-runs Proton's own setup: it copies DLLs into
`system32` (symptoms: `Proton: Error while copying to .../system32/nvngx_update.exe`), which is both slow
and a plausible trigger for the installation-integrity kick. Instead reuse the live session's environment
and call wine directly:

```sh
G=$(pgrep -a -f "<game>.exe" | grep -v bash | grep -E "Z:" | awk '{print $1}' | head -1)
tr '\0' '\n' < /proc/$G/environ \
  | grep -E "^(WINEPREFIX|WINEDLLPATH|WINEDLLOVERRIDES|LD_LIBRARY_PATH|PATH|HOME|USER|TMPDIR|XDG_RUNTIME_DIR|STEAM_COMPAT_DATA_PATH|STEAM_COMPAT_CLIENT_INSTALL_PATH)=" \
  | sed 's/^/export /' > /tmp/gameenv.sh
set -a; . /tmp/gameenv.sh; set +a
"$PROTON_DIR/files/bin/wine" "$EXE" inject --dll "Z:\\path\\x.dll"
```

Copying the env is what puts you in the *same* wineserver; `proton run` does not. Drop `WINEDEBUG` from
the copy (a value like `-all` gets parsed as an `env` option). Inside the launch-options wrapper the env
is already inherited, so the wrapper only needs the env copy when it spawns a fresh shell.

Isolate blame before redesigning: boot once with injection disabled, once with injection but no second
`proton run`. Only then attribute a kick to memory detection rather than to the launcher.

## EAC Linux v2 — the clock on this whole approach

Epic announced (October 2026) a Linux rebuild of EAC that runs **without a kernel module**, using a
systemd service, eBPF and LSM-BPF hooks. The sentence that matters for any external reader:

> "This enables anti-cheat to restrict other processes from reading or writing protected game process
> memory without needing a kernel module, as well as monitor for cheats loaded in different ways."

Practical consequences:

- A host-side `/proc/<pid>/mem` / `process_vm_readv` reader (the "variant C" that looks safest today) is
  exactly what LSM-BPF is meant to block. In-process, file-backed, encrypted-at-rest designs survive the
  transition better than external ones.
- Requirements: signed kernel and modules from Epic's well-known key list (Canonical, Fedora, Universal
  Blue, Debian, openSUSE, SUSE, Rocky, MS UEFI CA, Alma), TPM 2.0, Secure Boot, measured boot, IOMMU,
  kernel lockdown integrity, kexec disabled, LSM BPF active. Distros outside that list (NixOS) and the
  Steam Deck are simply not supported yet, so the realistic failure mode is "the game no longer launches",
  not "the cheat got caught".
- Game developers choose per title whether to keep the old implementation or move to v2, and Epic pilots
  in Fortnite first — so this is a horizon of months, not days. Say so when planning, and keep the
  injection design swappable.

A separate Windows-side vector worth remembering: EAC's own module is mapped into the game process
(`aftermathlabs`), which is how a "protect my own memory" style check can reach user-mode structures.

## Observed numbers (one Unity 6 VRChat build)

Order-of-magnitude anchors, never offsets:

- `GameAssembly.dll` ~252 MB, 251 exports; `UnityPlayer.dll` ~34 MB; `global-metadata.dat` ~40 MB and
  encrypted — plaintext metadata exists only in RAM after `il2cpp_init`.
- 234 of the export names sit in a dense `.rdata` block of `UnityPlayer.dll`; that pool order is not the
  published Unity 2020 header order, so name→address must be resolved empirically.
- Image slack after the last section (what tail-slack stomping could use): 0x7c8–0xfcc across
  `baselib.dll`, `UnityPlayer.dll`, `GameAssembly.dll`, the game exe — kilobytes against a ~1.2 MB
  release payload.
- Manual-map bookkeeping for a Rust cdylib: ~1900 DIR64 relocations, 2 TLS callbacks, 1 executable
  section; `CreateRemoteThread(LoadLibraryW)` returns `STILL_ACTIVE` (0x103) while the payload still runs.

## Where this comes from

UnknownCheats threads are behind a Cloudflare interstitial for plain HTTP clients, so prefer the
writeups they cite: the `EAC-REVERSED-WRITEUP` repo (`detection_methods.md`,
`internal_cheats_and_injectors.md`, `usermode_techniques.md`) for VAD/thread/integrity mechanics, the
`aftermathlabs` EAC-mapper analysis for how EAC gets its own module into the game, and general module
stomping writeups for the L1 technique.
