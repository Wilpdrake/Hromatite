#!/usr/bin/env python3
"""Offline shape table for every export of a PE (GameAssembly.dll-style).

Prints one compact line per exported function so an agent can decide what a
function is in one or two decompiler calls instead of opening Ghidra blind:
RVA, size to the first top-level `ret`, export names (aliases joined by comma),
first instructions, call targets, RIP-referenced globals, and the first
instructions of a thunk target.

    python3 export_shape_dump.py /path/GameAssembly.dll
    python3 export_shape_dump.py /path/GameAssembly.dll --lo 0xc27990 --hi 0xc2b000
    python3 export_shape_dump.py /path/GameAssembly.dll --json shapes.json

Parsing traps encoded here (each one silently empties a naive version):
  * PE32+ data directories start at opt+112, PE32 at opt+96.
  * AddressOfNameOrdinals entries are u16; reading u32 there explodes later.
  * objdump rows look like `  c2a330:\t<bytes>\t<asm>` — strip the colon.
  * Ranges are file RVAs, not ImageBase-relative addresses (subtract 0x180000000).
"""
from __future__ import annotations

import argparse
import json
import re
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


def u16(d, o):
    return struct.unpack_from("<H", d, o)[0]


def u32(d, o):
    return struct.unpack_from("<I", d, o)[0]


def u64(d, o):
    return struct.unpack_from("<Q", d, o)[0]


class Pe:
    def __init__(self, data: bytes):
        self.data = data
        if data[:2] != b"MZ":
            raise SystemExit("not an MZ image")
        pe = u32(data, 0x3C)
        if data[pe:pe + 4] != b"PE\0\0":
            raise SystemExit("no PE signature")
        nsec, optsz = u16(data, pe + 6), u16(data, pe + 20)
        opt = pe + 24
        magic = u16(data, opt)
        self.image_base = u64(data, opt + 24) if magic == 0x20B else u32(data, opt + 28)
        ddir = opt + (112 if magic == 0x20B else 96)
        self.sections = []
        sec = opt + optsz
        for i in range(nsec):
            o = sec + i * 40
            self.sections.append((
                data[o:o + 8].rstrip(b"\0").decode("latin1"),
                u32(data, o + 12),   # VirtualAddress
                u32(data, o + 8),    # VirtualSize
                u32(data, o + 20),   # PointerToRawData
                u32(data, o + 16),   # SizeOfRawData
            ))
        self.export_rva = u32(data, ddir)

    def off(self, rva: int) -> int | None:
        for _name, va, vsize, rawptr, rawsize in self.sections:
            if va <= rva < va + max(vsize, rawsize):
                o = rawptr + (rva - va)
                return o if o < len(self.data) else None
        return None

    def exports(self) -> list[dict]:
        d = self.data
        eo = self.off(self.export_rva)
        if eo is None:
            raise SystemExit("export directory not mapped")
        n = u32(d, eo + 24)
        afn = self.off(u32(d, eo + 28))
        anames = self.off(u32(d, eo + 32))
        aords = self.off(u32(d, eo + 36))
        out = []
        for i in range(n):
            noff = self.off(u32(d, anames + i * 4))
            end = d.index(b"\0", noff)
            out.append({
                "name": d[noff:end].decode("latin1"),
                "rva": u32(d, afn + u16(d, aords + i * 2) * 4),
            })
        return out


def disasm(data: bytes, off_of, rva: int, nbytes: int) -> list[tuple[int, str]]:
    off = off_of(rva)
    if off is None:
        return []
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as f:
        f.write(data[off:off + nbytes])
        path = f.name
    try:
        out = subprocess.run(
            ["objdump", "-D", "-b", "binary", "-m", "i386:x86-64",
             "--adjust-vma=0x%x" % rva, path],
            capture_output=True, text=True, check=False).stdout
    finally:
        Path(path).unlink(missing_ok=True)
    ins = []
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) < 3:
            continue
        try:
            addr = int(parts[0].strip().rstrip(":"), 16)
        except ValueError:
            continue
        if parts[2].strip():
            ins.append((addr, parts[2].strip()))
    return ins


MEM_RE = re.compile(r"\[(rcx|rdx|r8|r9)(?:\+(0x[0-9a-f]+))?\]")
CALL_RE = re.compile(r"\bcall\s+([0-9a-f]+)")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--lo", default=None, help="lowest RVA (hex)")
    ap.add_argument("--hi", default=None, help="highest RVA (hex), exclusive")
    ap.add_argument("--bytes", type=int, default=400)
    ap.add_argument("--json", default=None)
    a = ap.parse_args()

    data = Path(a.binary).read_bytes()
    pe = Pe(data)
    lo = int(a.lo, 16) if a.lo else 0
    hi = int(a.hi, 16) if a.hi else 1 << 62

    by_addr: dict[int, list[str]] = {}
    for e in pe.exports():
        by_addr.setdefault(e["rva"], []).append(e["name"])

    rows = []
    for rva in sorted(by_addr):
        if not (lo <= rva < hi):
            continue
        ins = disasm(data, pe.off, rva, a.bytes)
        if not ins:
            print(f"{rva:>7x} {','.join(by_addr[rva])} <no disasm>")
            continue
        size, thunk, globs, calls = 0, None, [], []
        mem = []
        for addr, asm in ins:
            size = addr - rva
            for g in re.findall(r"# (0x[0-9a-f]+)", asm):
                if g not in globs:
                    globs.append(g)
            m = CALL_RE.search(asm)
            if m:
                calls.append(int(m.group(1), 16))
            for base, disp in MEM_RE.findall(asm):
                pair = (base, int(disp, 16) if disp else 0)
                if pair not in mem:
                    mem.append(pair)
            if asm.startswith("ret"):
                size += 1
                break
        m = re.match(r"jmp\s+([0-9a-f]+)$", ins[0][1])
        if m:
            thunk = int(m.group(1), 16)
        rows.append({
            "rva": rva, "names": by_addr[rva], "size": size, "thunk": thunk,
            "mem": mem[:10], "calls": calls[:10], "globs": globs[:8],
            "head": [x for _, x in ins[:6]],
        })
        mems = " ".join(f"{b}+{x:x}" if x else b for b, x in mem[:6])
        print(f"{rva:>7x} {size:>4} {','.join(by_addr[rva])[:28]:<28} "
              f"{' | '.join(x for _, x in ins[:4])}" + (f"  mem[{mems}]" if mems else ""))
        if globs:
            print(f"        glob[{' '.join(globs)}]")
        if thunk:
            tins = disasm(data, pe.off, thunk, 160)
            print(f"        THUNK->{thunk:x} " + " | ".join(x for _, x in tins[:5]))

    if a.json:
        Path(a.json).write_text(json.dumps({"image_base": pe.image_base, "exports": rows}, indent=1))
        print(f"wrote {a.json} ({len(rows)} addresses)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
