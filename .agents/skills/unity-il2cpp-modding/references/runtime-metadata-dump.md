# Runtime metadata dump → offline Il2CppDumper

Goal: get names, field offsets and **method RVAs** for every managed method without
decoding any `il2cpp_*` export identity. Pattern borrowed from `Zygisk-Il2CppDumper`
(dump at runtime, parse offline), adapted to Proton/Wine: only `global-metadata.dat`
must come from the live process, `GameAssembly.dll` is read from disk unchanged.

## Tool facts (measured, host side)

- `Il2CppDumper` v6.7.46 accepts metadata versions **16–31** (`Metadata.cs`:
  `version < 16 || version > 31` → `NotSupportedException`), i.e. Unity 6 (v31) is in
  range in principle.
- The published `net7` build runs on a .NET 8 host: `DOTNET_ROLL_FORWARD=LatestMajor`.
  NixOS: resolve the runtime with `nix build --no-link --print-out-paths nixpkgs#dotnet-sdk_8`
  and use `$store/bin/dotnet` with `DOTNET_ROOT=$store/share/dotnet` (the SDK store path
  contains `share/dotnet/shared/Microsoft.NETCore.App`; the `-wrapped` path only has `bin`).
- Ship `config.json` with `"RequireAnyKey": false`. With stdin redirected the default
  `true` makes the tool throw `InvalidOperationException: Cannot read keys…` *after* the
  real error, which hides the actual failure and looks like a crash.
- The tool decides which argv path is the metadata by reading the **first 4 bytes**:
  `0xFAB11BAF` → metadata, anything else → il2cpp binary. A memory dump without the magic
  is therefore treated as a second binary. Patch the copy before running (LE bytes
  `AF 1B B1 FA`), e.g. `printf '\257\033\261\372' | dd of=copy bs=1 seek=0 conv=notrunc`.
- CLI: `Il2CppDumper.dll <GameAssembly.dll> <metadata> <outdir>`; outputs `dump.cs`,
  `script.json`, `il2cpp.h`, `DummyDll/`, `ida.py`, `ghidra.py`.

## script.json → RVA map

`ScriptMethod` records are `{Address: ulong, Name: "Namespace.Type$$Method",
Signature, TypeSignature}`; `Address` is a **VA**, so `RVA = Address − ImageBase` with
ImageBase from the PE optional header (PE32+ at `opt+24`, PE32 at `opt+28`). Dedupe on
`(type, method, signature)` — overloads share `Name`. Emit one JSON artifact per
GameAssembly sha256, never one global table: every RVA is build-frozen.

## Measured on a VRChat Unity 6 build (what a dump attempt actually meets)

- The on-disk `global-metadata.dat` (40 764 240 B) is **partially** plaintext: a bytewise
  scramble over the head and the string-literal tables, but `mscorlib` at 38 130 016 and
  `Camera` at 24 978 351 read as plain ASCII. Do not conclude "fully encrypted" from the
  first 16 bytes alone — `strings`/`find` on the file first.
- In live memory the magic occurs **twice, both in `r-xp` code pages** of `UnityPlayer`.
  The file is mapped `r--p` (path visible in `/proc/<pid>/maps`) and still holds ciphertext.
- Decrypted metadata **strings** do exist in an anonymous slab (a 38 MB `rw-p` region with
  `mscorlib`, `mscorlib.dll`, `get_fieldOfView`, `__StaticArrayInitTypeSize=…`), next to
  `Il2CppType`-shaped arrays — but no 256-byte header whose table pairs validate was found
  (`Il2CppGlobalMetadataHeader` scan: full-pair, loose early-pair, with and without a
  version filter, whole 4.9 GB process + that slab → 0 candidates). Status: **open** — a
  magic-less, header-less in-memory form breaks both `Il2CppDumper` and a "copy the blob"
  command; the header must be recovered another way (see oracles below).

## Oracles when the header is not where the docs say

- **Header layout reminder**: the header is a run of consecutive 4-byte values; section
  `pair(index)` is `(off = i32(header + index*4), size = i32(header + 4 + index*4))` for
  indices 2..62. A scan that treats each pair as an 8-byte struct (`index*8`) finds zero
  headers and looks like "metadata really is gone".
- **Exact-base oracle**: metadata is header-at-offset-0, so for the true blob base `B`,
  `u32(B + 24) == strings_table_va − B`. Locate the strings table first (dense run of
  printable+NUL pages, e.g. via a printable-ratio profile per 4 KiB page), then test this
  equality instead of guessing pairs.
- **Runtime oracle**: the engine keeps `s_GlobalMetadata` / `s_GlobalMetadataHeader` in
  GameAssembly `.data`; find the code that reads them (`mov rax,[rip+X]` followed by a
  `+ header->stringOffset` style use) and read the global from the payload with `q`.
- Rule out sections-before-strings by looking for the **image table** instead: 32-byte
  records (nameIndex, assemblyIndex, typeStart, typeCount, …) whose nameIndex resolves to
  a string ending in `.dll`. Finding it gives a section address without the header.

## Host-side memory scanning recipe (no game freeze)

Reading `/proc/<pid>/mem` of the wine process works with `ptrace_scope=1`; it does not
suspend threads, so it is the cheap way to answer "is X in memory at all" before touching
the payload. Iterate `/proc/<pid>/maps`, skip `x` regions (or keep only `rw`/`r--`), read
1 MiB chunks with `mem.seek(lo)` + `read(n)`, and carry the last ~264 bytes into the next
chunk so patterns straddling a boundary are still found. Prefer a regex for the byte
pattern (`rb"[\x18-\x28]\x00\x00\x00"` for a small version dword) over per-offset struct
unpacking in Python loops.

Put such probes in a file and run them: a heredoc whose body contains `&` (bitwise and in
Python) is read by the host tooling as shell backgrounding, and the command is rejected
before it runs.
