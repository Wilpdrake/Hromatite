#!/usr/bin/env python3
"""Host-side memory reconnaissance for an IL2CPP process.

Why: every full-memory scan *inside* the game needs a thread of ours, and Unity's
GC fails `SuspendThread` on it ("Fatal error in GC — SuspendThread loop failed"),
freezing the game. Reading `/proc/<pid>/mem` from the host touches no game thread
at all (works because `kernel.vma.ptrace_scope = 0`).

What it does: finds a class-like structure by the *pointers* it holds — a slot
pointing at the string "<name>" with a slot pointing at "<namespace>" nearby (the
class table keeps `name` and `namespaze` next to each other).

Usage:
    uv run --no-project python tools/host_scan.py <pid> UnityEngine Camera
"""
import os
import re
import struct
import sys
import time

PAGE = 0x1000


def regions(pid):
    out = []
    with open(f"/proc/{pid}/maps") as f:
        for line in f:
            parts = line.split()
            lo, hi = (int(x, 16) for x in parts[0].split("-"))
            perms = parts[1]
            if "r" not in perms:
                continue
            if len(parts) >= 6 and parts[-1] not in ("0",) and parts[5] != "0":
                name = parts[-1]
            else:
                name = ""
            out.append((lo, hi, name))
    return out


def find_cstr_addrs(mem, pid, regions_, text, limit=4000):
    """Addresses of NUL-terminated `text` occurrences (bounded)."""
    pat = text.encode() + b"\0"
    hits = []
    for lo, hi, name in regions_:
        if len(hits) >= limit:
            break
        size = hi - lo
        if size > 4 << 30:  # skip absurd regions (mostly reserved)
            continue
        try:
            data = os.pread(mem, size, lo)
        except OSError:
            continue
        start = 0
        while len(hits) < limit:
            i = data.find(pat, start)
            if i < 0:
                break
            hits.append(lo + i)
            start = i + len(pat)
    return hits


def scan_for_words(mem, regions_, targets, ns_targets, want_off):
    """Slots whose qword equals an address in `targets`, printed with the nearest
    `ns_targets` match within `want_off` bytes."""
    tset = {t: n for n, t in enumerate(targets)}
    out = []
    for lo, hi, name in regions_:
        size = hi - lo
        if size <= 0 or size > 4 << 30:
            continue
        try:
            data = os.pread(mem, size, lo)
        except OSError:
            continue
        for t, idx in tset.items():
            pat = struct.pack("<Q", t)
            start = 0
            while True:
                i = data.find(pat, start)
                if i < 0:
                    break
                start = i + 8
                if i % 8:
                    continue
                slot = lo + i
                # look for a namespace pointer around this slot
                for d in range(-0x200, 0x208, 8):
                    if d == 0:
                        continue
                    j = i + d
                    if 0 <= j <= len(data) - 8:
                        w = struct.unpack_from("<Q", data, j)[0]
                        if w in ns_targets:
                            out.append((slot, d, w, name))
                            break
                if len(out) > 200:
                    return out
    return out


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        return
    pid = int(sys.argv[1])
    ns_text, name_text = sys.argv[2], sys.argv[3]
    mem = os.open(f"/proc/{pid}/mem", os.O_RDONLY)
    regs = regions(pid)
    print(f"pid {pid}: {len(regs)} readable regions")

    t0 = time.time()
    names = find_cstr_addrs(mem, pid, regs, name_text)
    nss = find_cstr_addrs(mem, pid, regs, ns_text)
    print(f"'{name_text}' strings: {[hex(a) for a in names]}")
    print(f"'{ns_text}' strings: {[hex(a) for a in nss]}  ({time.time()-t0:.1f}s)")
    if not names or not nss:
        return

    hits = scan_for_words(mem, regs, names, set(nss), 0x200)
    print(f"\nclass-like slots: {len(hits)}")
    for slot, d, w, reg in hits[:20]:
        print(f"  name slot {slot:#x} ({reg}) ns {d:+#x} -> {w:#x}")

    # dump the first candidate's neighbourhood
    if hits:
        slot = hits[0][0]
        base = (slot - 0x20) & ~7
        try:
            block = os.pread(mem, 0x80, base)
        except OSError:
            block = b""
        print(f"\nwindow around {base:#x}:")
        for i in range(0, len(block), 8):
            w = struct.unpack_from("<Q", block, i)[0]
            asc = ""
            if 0x10000 < w < 0x800000000000:
                try:
                    s = os.pread(mem, 32, w)
                    s = s.split(b"\0")[0]
                    if s and all(32 <= c < 127 for c in s):
                        asc = s.decode(errors="replace")
                except OSError:
                    pass
            print(f"  {base+i:#x}: {w:#018x} {asc}")


if __name__ == "__main__":
    main()
