#!/usr/bin/env python3
"""Summarise a Windows minidump: exception, per-thread suspend counts, module map.

Use it on Unity crash dumps
(`<prefix>/drive_c/users/steamuser/AppData/Local/Temp/<Game>/<Game>/Crashes/*/crash.dmp`)
whenever a game dies with a bare fatal message. The histogram it prints is what
tells a stale GC-thread record (`SuspendThread loop failed`) apart from a real
fault in the injected code: every thread shows SuspendCount >= 1 except the one
that aborted, and the aborting RIP is an int3 inside GameAssembly.

    uv run --no-project python scripts/minidump_threads.py crash.dmp [--stack N]

Stdlib only, on purpose (hosts with no pip/system python are the norm here).
"""

import bisect
import struct
import sys

MDMP_MAGIC = 0x504D444D
ST_THREAD = 3
ST_MODULE = 4
ST_MEMORY = 5
ST_EXCEPTION = 6

# CONTEXT (x64) offsets
CTX_RCX, CTX_RDX, CTX_RSP, CTX_RIP = 0x80, 0x88, 0x98, 0xF8
CTX_R8, CTX_R9 = 0xB8, 0xC0


def parse(path):
    d = open(path, "rb").read()
    magic, _ver, nstreams, dir_rva = struct.unpack_from("<IIII", d, 0)
    if magic != MDMP_MAGIC:
        raise SystemExit(f"{path}: not a minidump")
    streams = {}
    for i in range(nstreams):
        stype, size, rva = struct.unpack_from("<III", d, dir_rva + 12 * i)
        streams[stype] = (size, rva)
    return d, streams


def modules(d, streams):
    out = []
    size, rva = streams[ST_MODULE]
    n = struct.unpack_from("<I", d, rva)[0]
    for i in range(n):
        o = rva + 4 + 108 * i
        base, sz = struct.unpack_from("<QI", d, o)
        name_rva = struct.unpack_from("<I", d, o + 20)[0]
        ln = struct.unpack_from("<I", d, name_rva)[0]
        name = d[name_rva + 4 : name_rva + 4 + ln].decode("utf-16le", "replace")
        out.append((base, sz, name.split("\\")[-1]))
    out.sort()
    return out


def modof(mods, addr):
    bases = [m[0] for m in mods]
    i = bisect.bisect_right(bases, addr) - 1
    if i >= 0:
        b, s, n = mods[i]
        if b <= addr < b + s:
            return f"{n}+{addr - b:#x}"
    return None


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    path = sys.argv[1]
    stack_bytes = 0
    if "--stack" in sys.argv:
        stack_bytes = int(sys.argv[sys.argv.index("--stack") + 1])
    d, streams = parse(path)
    mods = modules(d, streams)

    if ST_EXCEPTION in streams:
        _, rva = streams[ST_EXCEPTION]
        tid = struct.unpack_from("<I", d, rva)[0]
        code, _flags, _rec, addr = struct.unpack_from("<IIQQ", d, rva + 8)
        ctx_size, ctx_rva = struct.unpack_from("<II", d, rva + 8 + 152)
        rip = struct.unpack_from("<Q", d, ctx_rva + CTX_RIP)[0]
        print(f"exception: tid={tid} code={code:#010x} at {addr:#x} ({modof(mods, addr)})")
        print(f"           context rip={rip:#x} ({modof(mods, rip)})")
    else:
        tid = None
        print("exception: no exception stream (dump taken without a fault)")

    size, rva = streams[ST_THREAD]
    n = struct.unpack_from("<I", d, rva)[0]
    hist = {}
    rows = []
    for i in range(n):
        o = rva + 4 + 48 * i
        t_id, susp, _pc, _pri, _teb, stk, stksz, stk_rva, _cs, ctx_rva = struct.unpack_from(
            "<IIIIQQIIII", d, o
        )
        rip = struct.unpack_from("<Q", d, ctx_rva + CTX_RIP)[0]
        rsp = struct.unpack_from("<Q", d, ctx_rva + CTX_RSP)[0]
        hist[susp] = hist.get(susp, 0) + 1
        rows.append((t_id, susp, rip, rsp, stk, stksz, stk_rva))
    print(f"threads: {n}, suspend histogram: {sorted(hist.items())}")
    zero = [t[0] for t in rows if t[1] == 0]
    print(f"threads with SuspendCount == 0: {[hex(t) for t in zero] or 'none'}")
    if zero == [tid] or (tid is not None and zero == [tid]):
        print("  -> only the faulting thread is running: the thread the collector")
        print("     kept failing on is GONE from the process (stale GC_threads[] entry)")
    for t_id, susp, rip, rsp, stk, stksz, stk_rva in sorted(rows):
        mark = "  <== FAULT" if t_id == tid else ""
        print(f"  tid={t_id:>6} susp={susp} rip={rip:#x} ({modof(mods, rip)}){mark}")
        if stack_bytes:
            lo = max(rsp, stk)
            hi = min(stk + stksz, rsp + stack_bytes)
            for a in range(lo, hi, 8):
                v = struct.unpack_from("<Q", d, stk_rva + (a - stk))[0]
                m = modof(mods, v)
                if m:
                    print(f"      {a:#x}: {v:#x} {m}")


if __name__ == "__main__":
    main()
