#!/usr/bin/env python3
"""Locate Boehm's `GC_threads[]` in GameAssembly.dll (RVA), offline.

`il2cpp` ships bdwgc's win32_threads.c, and the GC keeps its own thread table
(`STATIC GC_thread GC_threads[THREAD_TABLE_SZ]`, 256 buckets of pointer). Reading
that table from inside the game is how we find out *which* thread makes
`GC_suspend()` spin 1 000 000 times ("SuspendThread loop failed") — the record
whose OS thread is already gone.

The table is zero-initialised, so it cannot be found by content on disk. Find it
by the code instead: `GC_lookup_thread_inner` / the detach path is

    movzbl %al,%esi              ; index = ((id >> 8) ^ id) & 0xFF
    mov    (%r14,%rsi,8),%rbx    ; 49 8b 1c f6
    cmp    %ecx,0x8(%rbx)        ; 39 4b 08     <- GC_thread.id at +0x08
    ...
    mov    (%rbx),%rbx           ; 48 8b 1b     <- GC_thread.next at +0x00
    mov    0x10(%rbx),%rcx       ; 48 8b 4b 10  <- GC_thread.handle at +0x10

with `r14` loaded by `lea <disp32>(%rip),%r14` a few instructions earlier. That
lea target is `GC_threads`.

Usage: uv run --no-project python tools/gct_table.py [path-to-GameAssembly.dll]

Prints every hit as `rva` (relative to the image base — the payload adds the
module base). The candidate list in `crates/hromatite-core/src/gcthreads.rs`
must be refreshed from this output after a game update.
"""

import struct
import sys
from pathlib import Path

DEFAULT = Path.home() / ".local/share/Steam/steamapps/common/VRChat/GameAssembly.dll"

# 49 8b 1c f6   mov (%r14,%rsi,8),%rbx
LOAD_BUCKET = b"\x49\x8b\x1c\xf6"
# 39 4b 08      cmp %ecx,0x8(%rbx)
CMP_ID = b"\x39\x4b\x08"
# 4c 8d 35 xx xx xx xx   lea disp32(%rip),%r14
LEA_R14 = b"\x4c\x8d\x35"


def sections(data: bytes):
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        raise SystemExit("not a PE file")
    coff = e_lfanew + 4
    nsec = struct.unpack_from("<H", data, coff + 2)[0]
    optsz = struct.unpack_from("<H", data, coff + 16)[0]
    out = []
    for i in range(nsec):
        o = coff + 20 + optsz + 40 * i
        name = data[o : o + 8].rstrip(b"\0").decode(errors="replace")
        vsize, vaddr, rawsz, rawptr = struct.unpack_from("<IIII", data, o + 8)
        out.append((name, vaddr, vsize, rawptr, rawsz))
    return out


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT
    data = path.read_bytes()
    text = next(s for s in sections(data) if s[0] == ".text")
    _, tva, _tvs, trp, trs = text
    tb = data[trp : trp + trs]

    hits = 0
    pos = 0
    while True:
        i = tb.find(LOAD_BUCKET, pos)
        if i < 0:
            break
        pos = i + 1
        if tb.find(CMP_ID, i, i + 12) < 0:
            continue
        # the lea that loads the table base lives just above
        for back in range(0x10, 0x50):
            if i - back < 0:
                break
            if tb[i - back : i - back + 3] == LEA_R14:
                disp = struct.unpack_from("<i", tb, i - back + 3)[0]
                next_ip = tva + (i - back) + 7
                target = next_ip + disp
                print(f"GC_threads candidate rva = {target:#x}  (code at {tva + i:#x})")
                hits += 1
                break
        else:
            print(f"pattern at {tva + i:#x}: no lea found within 0x50 bytes")
    if not hits:
        print("no hits — the stub shape changed, see the header comment")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
