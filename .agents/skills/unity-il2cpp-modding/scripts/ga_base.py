#!/usr/bin/env python3
"""Find the in-memory base of GameAssembly.dll in a live wine process.

Needed every time a console command takes an *address* (a GameAssembly function,
a .data table): the numbers are RVAs from the on-disk file, and the process has
them relocated somewhere else. Do NOT trust the mapping that `/proc/<pid>/maps`
shows at file offset 0: wine often maps only the 4 KB PE header there while the
code/data live in anonymous regions elsewhere.

Two independent oracles, both measured:

  * export-name oracle (default): read an 11-char obfuscated export name from
    GameAssembly.dll, convert its file offset to an RVA through the section
    table, then look for that exact byte string in the process. Every hit
gives a candidate base = hit - rva; the true one is the value that also makes
    a known stub (e.g. `il2cpp_domain_get` = `48 8B 05 disp32 C3`) byte-exact.
  * table oracle: if the payload already printed an address for a .data table
    with a fixed RVA (Boehm `GC_threads[]`), base = printed_addr - rva.

Usage:
    uv run --no-project python ga_base.py <pid> [--name wYRnnMQuvvH]
    uv run --no-project python ga_base.py <pid> --table-addr 0x6ffff90cb180 --table-rva 0xe6bb180
"""
from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

DEFAULT_GAME = Path.home() / ".local/share/Steam/steamapps/common/VRChat"


def sections(data: bytes):
    """(name, virtual_address, virtual_size, raw_ptr, raw_size).

    Field order inside IMAGE_SECTION_HEADER matters: VirtualSize is at +8 and
    VirtualAddress at +12 — swapping them (a common slip) yields RVAs larger
    than SizeOfImage and a base that matches nothing.
    """
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    nsec = struct.unpack_from("<H", data, pe + 6)[0]
    optsz = struct.unpack_from("<H", data, pe + 20)[0]
    base = pe + 24 + optsz
    for i in range(nsec):
        off = base + i * 40
        name = data[off:off + 8].rstrip(b"\0").decode("latin1")
        vsize, va, rsize, rptr = struct.unpack_from("<IIII", data, off + 8)
        yield name, va, vsize, rptr, rsize


def file_offset_to_rva(data: bytes, fo: int) -> int | None:
    for _name, va, _vsize, rptr, rsize in sections(data):
        if rptr <= fo < rptr + rsize:
            return va + (fo - rptr)
    return None


def search(pid: int, needle: bytes, limit: int = 12) -> list[int]:
    hits: list[int] = []
    mem = open(f"/proc/{pid}/mem", "rb", buffering=0)
    for line in Path(f"/proc/{pid}/maps").read_text().splitlines():
        parts = line.split()
        if "r" not in parts[1]:
            continue
        lo, hi = (int(x, 16) for x in parts[0].split("-"))
        if hi - lo < len(needle):
            continue
        try:
            mem.seek(lo)
            buf = mem.read(hi - lo)
        except OSError:
            continue
        i = buf.find(needle)
        while i >= 0:
            hits.append(lo + i)
            if len(hits) >= limit:
                return hits
            i = buf.find(needle, i + 1)
    return hits


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("pid", type=int)
    ap.add_argument("--game-dir", type=Path, default=DEFAULT_GAME)
    ap.add_argument("--name", default="wYRnnMQuvvH",
                    help="a surviving 11-char export name from the symbol map")
    ap.add_argument("--table-addr", type=lambda v: int(v, 0), default=None)
    ap.add_argument("--table-rva", type=lambda v: int(v, 0), default=None)
    args = ap.parse_args(argv)

    if args.table_addr and args.table_rva:
        print(f"base {args.table_addr - args.table_rva:#x} (from table address)")
        return 0

    ga = args.game_dir / "GameAssembly.dll"
    data = ga.read_bytes()
    name = args.name.encode()
    fo = data.find(name)
    if fo < 0:
        print(f"export name {args.name!r} not in {ga}", file=sys.stderr)
        return 2
    rva = file_offset_to_rva(data, fo)
    print(f"export name at file {fo:#x} -> rva {rva:#x}")
    hits = search(args.pid, name)
    print(f"{len(hits)} memory hit(s)")
    for h in hits:
        print(f"  {h:#x} -> base {h - rva:#x}")
    print("\nConfirm the winner: base + rva(il2cpp_domain_get) must start with "
          "`48 8b 05 .. .. .. .. c3` (read it back with /proc/<pid>/mem).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
