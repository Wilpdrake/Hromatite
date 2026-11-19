#!/usr/bin/env python3
"""Fingerprint an IL2CPP Unity build and test the obfuscated-export name-pool hypothesis.

Usage: uv run --no-project python probe_il2cpp_exports.py /path/to/GameDir

Prints: export count, how many export names also appear in UnityPlayer.dll,
whether they form a contiguous .rdata block (the Unity il2cpp API name table),
name pitch/alignment, and the file order of the pool (to align against
il2cpp-api-functions.h order). Read-only: opens files, writes nothing.
"""
import re
import struct
import subprocess
import sys
from collections import Counter

OBJDUMP = "x86_64-w64-mingw32-objdump"


def exports_of(dll: str):
    out = subprocess.run([OBJDUMP, "-p", dll], capture_output=True, text=True).stdout
    names, in_tbl = [], False
    for line in out.splitlines():
        if line.startswith("[Ordinal/Name Pointer] Table"):
            in_tbl = True
            continue
        if in_tbl:
            if not line.strip():
                break
            m = re.search(r"^\s*\[\s*\d+\]\s*\+base\[\s*\d+\]\s+[0-9a-f]{4}\s+(\S+)\s*$", line)
            if m:
                names.append(m.group(1))
    return names


def parse_pe(d: bytes):
    e_lfanew = struct.unpack_from("<I", d, 0x3C)[0]
    coff = e_lfanew + 4
    nsec = struct.unpack_from("<H", d, coff + 2)[0]
    opt_size = struct.unpack_from("<H", d, coff + 16)[0]
    opt = coff + 20
    magic = struct.unpack_from("<H", d, opt)[0]
    img_base = struct.unpack_from("<Q" if magic == 0x20B else "<I", d, opt + 24)[0]
    secs = []
    for i in range(nsec):
        b = opt + opt_size + i * 40
        name = d[b:b + 8].rstrip(b"\0").decode(errors="replace")
        vsize, vaddr, rsize, raw = struct.unpack_from("<IIII", d, b + 8)
        secs.append((name, vaddr, max(vsize, rsize), raw, rsize))
    return img_base, secs


def main(game_dir: str):
    names = exports_of(f"{game_dir}/GameAssembly.dll")
    print(f"exports: {len(names)}")
    obf = [n for n in names if len(n) == 11 and re.fullmatch(r"[A-Za-z0-9_]+", n)]
    print(f"obfuscated-looking (11 chars): {len(obf)}")
    print(f"surviving plain names: {[n for n in names if n not in obf][:20]}")

    up = open(f"{game_dir}/UnityPlayer.dll", "rb").read()
    offs = {}
    for n in names:
        i = up.find(n.encode() + b"\0")
        if i != -1:
            offs[n] = i
    print(f"names also present in UnityPlayer.dll: {len(offs)}")
    if not offs:
        print("name-pool hypothesis NOT confirmed on this build")
        return

    ordered = sorted(offs.items(), key=lambda kv: kv[1])
    print(f"pool span: {hex(ordered[0][1])}..{hex(ordered[-1][1])}")
    diffs = [b[1] - a[1] for a, b in zip(ordered, ordered[1:])]
    print(f"pitch histogram (top): {Counter(diffs).most_common(5)}")
    big = [d for d in diffs if d > 0x1000]
    print(f"gaps >0x1000 (pool splits): {len(big)} -> {[hex(x) for x in big[:5]]}")
    print("file order (first 20, align against il2cpp-api-functions.h):")
    for n, o in ordered[:20]:
        print(f"  {hex(o)}  {n}")

    img_base, secs = parse_pe(up)
    print(f"image base: {hex(img_base)}; sections: {[(s[0], hex(s[1])) for s in secs]}")
    print("note: names are inline strings, not pointers -> expect ~0 pointer refs; "
          "confirm by dumping 32 bytes around one offset with od.")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1].rstrip("/"))
