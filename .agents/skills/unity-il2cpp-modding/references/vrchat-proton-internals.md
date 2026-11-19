# VRChat under Proton — internals worth remembering

Layout (paths are per Steam library):

```
steamapps/common/VRChat/
├─ VRChat.exe, UnityPlayer.dll, GameAssembly.dll, baselib.dll, libsodium.dll
├─ start_protected_game.exe        # EAC launcher; Settings.json -> "executable": "VRChat.exe"
├─ EasyAntiCheat/                  # EOS variant: EasyAntiCheat_EOS_Setup.exe, Settings.json (productid/sandboxid)
└─ VRChat_Data/
   ├─ globalgamemanagers           # Unity version string lives here as plain text
   └─ il2cpp_data/Metadata/global-metadata.dat   # ENCRYPTED in current builds
steamapps/compatdata/<appid>/pfx/drive_c/         # the wine prefix (VRChat appid 438100)
```

Observed on a Unity 6000.0.67f1 build (`GameAssembly.dll` 252 339 712 B, `UnityPlayer.dll` 33.7 MB,
`global-metadata.dat` 40.7 MB):

- 251 exports; names are random 11-char strings (`AQIKwyWT_pM`, `A_eHSn_ViIs`); only a handful survive
  (`Flush`, `DllCanUnloadNow`, `il2cpp_free`, `CloseNLSocket`, `CreateNLSocket`,
  `il2cpp_gchandle_get_target`, `il2cpp_native_stack_trace`).
- 234–235 of those names appear inside `UnityPlayer.dll` `.rdata`, inline strings, 8/16-byte aligned, no
  relocations. They are **not** in one window: a naive `find()` puts one anchor (`Flush`) ~940 KB away
  from the rest. Pick occurrences inside the densest 32 KB window per name, then sort by offset.
- The pool order is Unity's table order, and it disagrees with the Unity 2020 `il2cpp-api-functions.h`
  order: plain anchors sit at pool indexes 71 (`il2cpp_native_stack_trace`) and 128
  (`il2cpp_gchandle_get_target`) versus header indexes 83 / 116. Never map names by index.
- `global-metadata.dat` starts with non-magic bytes (`3a 08 77 f3`) i.e. encrypted; older community notes
  cite `{EM@` / `0x404d457b` for a different build — check bytes, never trust a remembered header.
- `globalgamemanagers` contains `6000.0.67f1-DWR`, so version detection is one `strings | grep`.

Anti-cheat: EAC **EOS**. The bootstrap log in the prefix
(`pfx/drive_c/users/steamuser/AppData/Roaming/EasyAntiCheat/<productid>/<deploymentid>/anticheatlauncher.log`)
reports `System name: 'linux64'` and pulls `easyanticheat_linux_x64.eac` (~12 MB) from
`modules-cdn.eac-prod.on.epicgames.com` — i.e. on Linux the protection is a native userspace module, no
kernel driver. EAC v2 (systemd service + eBPF + LSM-BPF, signed kernel from a well-known key list, TPM 2.0,
Secure Boot, measured boot, IOMMU, lockdown integrity, kexec off) is the clock on this whole approach: it
explicitly targets "other processes reading or writing protected game process memory", which kills the
external `/proc/<pid>/mem` route first. A distro whose kernel key is not on Epic's list cannot satisfy v2 at
all. Say this when planning, and note that the game developer chooses old vs v2 implementation.

Unity drift: hard-coded il2cpp struct offsets break on a Unity major bump; accessor methods exist for native
externs but trivial forwarders (`Mathf.Max`, `Component.GetComponent<T>`) are inlined away and absent from
live metadata. Resolve fields at runtime from the dumped metadata rather than caching offsets across
versions.

Runtime specifics:
- Prefix user profile: `compatdata/438100/pfx/drive_c/users/steamuser/` — write logs there; the game
  process has no console. `USERPROFILE` resolves to it inside wine.
- The game normally launches through `start_protected_game.exe`; `VRChat.exe` is its child, so inject into
  the child. The `launch.exe --no-vr --no-eac` path (Steam launch options) boots without EAC — the cheap
  session for first injections, but never assume it stays that way in a normal user launch.
- Python-free tooling note: the prefix is a normal directory on disk, so a host-side Rust/Python tool can
  read the game's files (GameAssembly, UnityPlayer, metadata) without wine at all — only *process* access
  needs the wine session.
- Session separation: inside `SteamLinuxRuntime_4` the wine server dir is something like
  `/tmp/.wine-1000/server-27-<ino-of-prefix-as-seen-in-container>`; a host-side `proton run` or
  `protontricks-launch` computes a different `<ino>` and starts a second server. Compare
  `pgrep -a wineserver` plus each server's `cwd`/`WINEPREFIX` before blaming your injector. Only a Steam
  launch-options wrapper (or a DLL proxy) puts your binary in the game's session.
- Under Proton the game renders D3D11 → DXVK, so a `dxgi.dll`/`d3d11.dll` `Present` hook is a valid overlay
  route and hooks stay on the Windows side of the boundary.
