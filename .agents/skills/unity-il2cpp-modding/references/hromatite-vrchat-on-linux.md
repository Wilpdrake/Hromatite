# Hromatite — Rust IL2CPP cheat for VRChat on NixOS/Proton

Project lives at `~/RustroverProjects/Hromatite` (cargo workspace: `hromatite-core`, `hromatite-dll`,
`test-target`; target `x86_64-pc-windows-gnu`, mingw linker). Check whether the repo already exists
before scaffolding again.

## Project memory convention (user-mandated)

`AGENTS.md` in the repo is the single cross-session memory. Every finished piece of work — stage,
feature, fixed bug, failed or successful experiment — must be **appended** to §16 «Журнал работ»
(date, commit, what was done, how it was verified, what remains). Append, never rewrite the file.
The file is treated as protected: ask before overwriting, but normal append is expected.

## Verified environment facts

- VRChat appid **438100**; game at `~/.local/share/Steam/steamapps/common/VRChat`;
  prefix `~/.local/share/Steam/steamapps/compatdata/438100/pfx`.
- `GameAssembly.dll` ~253 MB (251 Beebyte-obfuscated exports), `UnityPlayer.dll` ~34 MB
  (235 plain-text export-name strings in a tight block), Unity 6000.0.67f1, encrypted
  `global-metadata.dat` → Cpp2IL/Il2CppDumper need the obfuscation workarounds.
- Export names change every game build → resolve empirically per build sha256
  (`tools/symmap.py` generates `symbols.rs` + `symbols-<sha>.json`), never hardcode RVAs.

## Delivery that worked

Steam launch options pointing at `tools/hromatite-launch.sh %command%` → the wrapper runs **inside**
pressure-vessel, so the injector lands in the same wineserver as the game. Injector then attaches
(`--pid`) using env read from `/proc/<pid>/environ` (`WINEPREFIX`, `WINEDLLPATH`, `LD_LIBRARY_PATH`,
`PATH`). A second `proton run` from outside frequently hangs on the prefix lock and can trip
EAC error 16.

## Wine quirks that cost hours

- `CreateRemoteThread` does not deliver DLL-entry arguments → use a hand-written x64 stub
  (`sub rsp,0x28; mov rcx,base; mov edx,1; xor r8d,r8d; mov rax,cb; call rax`).
- Rust cdylib TLS callbacks are not invoked → call them manually from the stub thread.
- `kernel32` apiset forwarders need `is_pe()` + `find_export_anywhere()` (e.g. `WaitOnAddress` in
  `kernelbase.dll`); a wine-supplied fake HMODULE must not be trusted.
- PEB unlink: `cur` points at `InMemoryOrderLinks`, so `DllBase = cur + 0x20` (not `+0x10`).
- `CreateToolhelp32Snapshot` can block ~390 s when two wine sessions fight over the same prefix.
- A `STILL_ACTIVE` (0x103) return from the loadlib path means "exit code", not a module base.

## Stealth notes

Debug DLL (~6 MB) does not fit module slack (2–4 KB), so module stomp needs a release build first.
EAC on Linux v1 has no kernel driver: it looks at PEB modules, threads, VAD anomalies, RWX,
PE-in-private-memory and thread start addresses outside modules. EAC v2 (systemd + eBPF + LSM BPF,
announced Oct 2026) requires a signed well-known distro kernel — NixOS is not on the list, so the
window is realistically 6–18 months, and VRChat itself chooses whether to enable it.
