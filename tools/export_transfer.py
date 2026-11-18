#!/usr/bin/env python3
"""Transfer il2cpp export names from an old GameAssembly build to a new one.

The game re-obfuscates every export name and shifts every RVA, so the previous
map cannot be reused verbatim.  This tool re-derives the correspondence from the
binaries alone:

1. anchors    - tiny exports (<=24 B) are matched by their exact body bytes,
                masked by the PE relocation table (so RIP-relative operands and
                absolute addresses do not defeat the comparison);
2. fill       - the remaining exports are aligned order-preservingly (the linker
                keeps function order) inside the windows between anchors, scored
                by the fraction of equal unmasked body bytes;
3. acceptance - for every pair the internal call/jump targets of the old
                function, mapped through the alignment, must equal the target set
                of the new one (call-graph isomorphism).  Anything below the
                threshold is reported, never silently accepted.

Usage:
    python3 tools/export_transfer.py --old OLD.dll --new NEW.dll \
        --map tools/data/il2cpp-export-names.json \
        --out tools/data/il2cpp-export-names-<sha12>.json

Requires only the stdlib plus objdump (binutils) for the call-graph check.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import struct
import subprocess
import sys
import tempfile

TINY = 24
SLICE = 64
TEXT_LO = 0x1000          # internal-call filter: .text start (below image base offset)
CB_THRESHOLD = 0.75


class PE:
    def __init__(self, path: str) -> None:
        self.path = path
        self.data = open(path, "rb").read()
        d = self.data
        e = struct.unpack_from("<I", d, 0x3C)[0]
        if d[e:e + 4] != b"PE\0\0":
            raise SystemExit(f"{path}: not a PE")
        nsec, = struct.unpack_from("<H", d, e + 6)
        opt_size, = struct.unpack_from("<H", d, e + 20)
        opt = e + 24
        if struct.unpack_from("<H", d, opt)[0] != 0x20B:
            raise SystemExit(f"{path}: not PE32+")
        self.image_base, = struct.unpack_from("<Q", d, opt + 24)
        dirs = opt + 112
        self.export_rva, self.export_size = struct.unpack_from("<II", d, dirs)
        self.reloc_rva, self.reloc_size = struct.unpack_from("<II", d, dirs + 5 * 8)
        self.sections = []
        so = opt + opt_size
        for i in range(nsec):
            name = d[so + i * 40: so + i * 40 + 8].rstrip(b"\0").decode("latin1")
            vsize, va, rawsize, rawptr = struct.unpack_from("<IIII", d, so + i * 40 + 8)
            self.sections.append((name, va, vsize, rawptr, rawsize))
        self.exports = self._exports()
        self.relocs = self._relocs()

    def must(self, rva: int, n: int) -> bytes:
        b = self.read_rva(rva, n)
        if b is None or len(b) != n:
            raise SystemExit(f"{self.path}: RVA {rva:#x} (+{n}) is outside the image")
        return b

    def read_rva(self, rva: int, n: int):
        for _, va, vsize, rawptr, rawsize in self.sections:
            if va <= rva < va + max(vsize, rawsize):
                off = rawptr + (rva - va)
                return self.data[off:off + n] if off + n <= len(self.data) else None
        return None

    def _exports(self):
        base = self.export_rva
        nnames, = struct.unpack("<I", self.must(base + 20, 4))
        funcs_rva, = struct.unpack("<I", self.must(base + 28, 4))
        names_rva, = struct.unpack("<I", self.must(base + 32, 4))
        ords_rva, = struct.unpack("<I", self.must(base + 36, 4))
        out = []
        for i in range(nnames):
            nrva, = struct.unpack("<I", self.must(names_rva + 4 * i, 4))
            name = b""
            p = nrva
            while True:
                c = self.read_rva(p, 1)
                if not c or c == b"\0":
                    break
                name += c
                p += 1
            ordi, = struct.unpack("<H", self.must(ords_rva + 2 * i, 2))
            frva, = struct.unpack("<I", self.must(funcs_rva + 4 * ordi, 4))
            out.append((name.decode("latin1"), frva))
        return out

    def _relocs(self):
        out = {}
        if not self.reloc_rva:
            return out
        ptr, end = self.reloc_rva, self.reloc_rva + self.reloc_size
        while ptr < end:
            hdr = self.read_rva(ptr, 8)
            if not hdr:
                break
            page, size = struct.unpack("<II", hdr)
            if size == 0:
                break
            body = self.read_rva(ptr + 8, (size - 8) // 2 * 2) or b""
            for k in range(len(body) // 2):
                ent, = struct.unpack_from("<H", body, k * 2)
                typ, off = ent >> 12, ent & 0xFFF
                if typ in (1, 2):
                    out[page + off] = 2
                elif typ == 3:
                    out[page + off] = 4
                elif typ == 10:
                    out[page + off] = 8
            ptr += size
        return out

    def masked_body(self, rva: int, nxt: int):
        n = max(4, min(256, nxt - rva))
        b = self.read_rva(rva, n)
        if not b:
            return None
        mask = bytearray(len(b))
        for i in range(len(b)):
            span = self.relocs.get(rva + i, 0)
            for j in range(span):
                if i + j < len(b):
                    mask[i + j] = 1
        return bytes(0xAA if m else c for c, m in zip(b, mask)), bytes(mask)


def sim(a, b) -> float:
    (av, am), (bv, bm) = a, b
    n = min(len(av), len(bv))
    kept = eq = 0
    for i in range(n):
        if am[i] or bm[i]:
            continue
        kept += 1
        if av[i] == bv[i]:
            eq += 1
    return eq / kept if kept else 0.0


def bounds(addrs):
    return {a: (addrs[i + 1] if i + 1 < len(addrs) else a + 32) for i, a in enumerate(addrs)}


def align(old: PE, new: PE, old_addrs, new_addrs):
    ob, nb = bounds(old_addrs), bounds(new_addrs)
    obody = {a: old.masked_body(a, ob[a]) for a in old_addrs}
    nbody = {a: new.masked_body(a, nb[a]) for a in new_addrs}

    keys = {}
    for a in new_addrs:
        v = nbody[a][0] if nbody[a] else b""
        if len(v) <= TINY:
            keys.setdefault(v, []).append(a)
    anchors = []
    for a in old_addrs:
        v = obody[a][0] if obody[a] else b""
        if len(v) <= TINY and len(keys.get(v, [])) == 1:
            anchors.append((a, keys[v][0]))
    chain, last = [], -1
    for a, b in anchors:
        if b > last:
            chain.append((a, b))
            last = b

    oidx = {a: i for i, a in enumerate(old_addrs)}
    nidx = {a: i for i, a in enumerate(new_addrs)}
    segs = [(-1, -1)] + [(oidx[a], nidx[b]) for a, b in chain] + [(len(old_addrs), len(new_addrs))]
    pairs = []
    for k in range(len(segs) - 1):
        o0, n0 = segs[k]
        o1, n1 = segs[k + 1]
        ol, nl = old_addrs[o0 + 1:o1], new_addrs[n0 + 1:n1]
        if not ol:
            continue
        if not nl:
            pairs.extend((a, None) for a in ol)
            continue
        N, M = len(ol), len(nl)
        S = [[sim(obody[a], nbody[b]) for b in nl] for a in ol]
        dp = [[0.0] * (M + 1) for _ in range(N + 1)]
        for i in range(1, N + 1):
            for j in range(1, M + 1):
                dp[i][j] = max(dp[i - 1][j], dp[i][j - 1], dp[i - 1][j - 1] + S[i - 1][j - 1])
        i, j = N, M
        loc = []
        while i > 0 and j > 0:
            if dp[i][j] == dp[i - 1][j]:
                i -= 1
            elif dp[i][j] == dp[i][j - 1]:
                j -= 1
            else:
                loc.append((ol[i - 1], nl[j - 1]))
                i -= 1
                j -= 1
        pairs.extend(reversed(loc))
    pairs.extend(chain)
    pairs.sort()
    return pairs


def disasm(blob: bytes, tag: str):
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as f:
        f.write(blob)
        path = f.name
    out = subprocess.run(["objdump", "-D", "-b", "binary", "-m", "i386:x86-64",
                          "--adjust-vma=0", path], capture_output=True, text=True).stdout
    ins = []
    for line in out.splitlines():
        m = re.match(r"\s*([0-9a-f]+):\t([0-9a-f ]+)\t(.*)", line)
        if m:
            ins.append((int(m.group(1), 16), bytes.fromhex(m.group(2).replace(" ", ""))))
    return ins


def call_targets(ins, base_local: int, rva: int, hi: int):
    res = set()
    for addr, by in ins:
        if not by:
            continue
        real = rva + (addr - base_local)
        t = None
        if by[0] in (0xE8, 0xE9) and len(by) == 5:
            t = real + 5 + struct.unpack_from("<i", by, 1)[0]
        elif len(by) >= 6 and by[0] == 0x0F and 0x80 <= by[1] <= 0x8F:
            t = real + 6 + struct.unpack_from("<i", by, 2)[0]
        elif len(by) == 2 and (by[0] == 0xEB or 0x70 <= by[0] <= 0x7F):
            t = real + 2 + struct.unpack_from("<b", by, 1)[0]
        if t is not None and TEXT_LO <= t < hi:
            res.add(t)
    return res


def callgraph_check(old: PE, new: PE, pairs):
    old_addrs = [a for a, b in pairs if b]
    new_addrs = [b for a, b in pairs if b]
    def blob(pe, addrs):
        parts = []
        for i, a in enumerate(addrs):
            nxt = addrs[i + 1] if i + 1 < len(addrs) else a + 16
            b = pe.read_rva(a, max(8, min(SLICE, nxt - a))) or b""
            parts.append(b + b"\xcc" * (SLICE - len(b)))
        return b"".join(parts)
    oi = disasm(blob(old, old_addrs), "old")
    ni = disasm(blob(new, new_addrs), "new")
    hi = max(va + vs for _, va, vs, _, _ in old.sections)
    omap = dict(zip(old_addrs, new_addrs))
    rmap = {v: k for k, v in omap.items()}
    rows = []
    for k, (a, b) in enumerate(pairs):
        if not b:
            continue
        ot = call_targets([x for x in oi if k * SLICE <= x[0] < (k + 1) * SLICE], k * SLICE, a, hi)
        nt = call_targets([x for x in ni if k * SLICE <= x[0] < (k + 1) * SLICE], k * SLICE, b, hi)
        mapped = {omap[t] for t in ot if t in omap}
        back = {rmap[t] for t in nt if t in rmap}
        union = len(mapped | back)
        score = len(mapped & back) / union if union else 1.0
        rows.append({"old_rva": hex(a), "new_rva": hex(b), "score": round(score, 3),
                     "old_targets": len(ot), "new_targets": len(nt)})
    return rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--old", required=True)
    ap.add_argument("--new", required=True)
    ap.add_argument("--map", required=True, help="map valid for --old")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    old, new = PE(args.old), PE(args.new)
    prev = json.load(open(args.map))
    named = {int(e["rva"], 16): e for e in prev["entries"] if e.get("real_name")}
    old_addrs = sorted(named)
    new_addrs = sorted({r for _, r in new.exports})

    pairs = align(old, new, old_addrs, new_addrs)
    rows = callgraph_check(old, new, [(a, b) for a, b in pairs if b])
    weak = [r for r in rows if r["score"] < CB_THRESHOLD]
    print(f"aligned {len(pairs)} old addresses; call-graph disagreements: {len(weak)}")
    for r in weak:
        print("  WEAK", r)

    by_addr = {}
    for n, r in new.exports:
        by_addr.setdefault(r, []).append(n)
    names = {}
    for a, b in pairs:
        if b and a in named:
            names.setdefault(b, named[a])
    entries = []
    for r in sorted(by_addr):
        e = {"addr": hex(new.image_base + r), "rva": hex(r), "exports": sorted(by_addr[r])}
        if r in names:
            src = names[r]
            e["real_name"] = src["real_name"]
            e["confidence"] = "transfer"
            e["evidence"] = f"from {src['rva']} ({src['real_name']}): anchors + monotone body DP + call-graph isomorphism"
        else:
            e["real_name"] = None
            e["confidence"] = "unknown"
            e["evidence"] = "no counterpart proven; inspect by shape before naming"
        entries.append(e)
    doc = {
        "schema": "hromatite/il2cpp-export-names/1",
        "source": "tools/export_transfer.py (anchors + monotone DP + call-graph isomorphism)",
        "gameassembly_sha256": hashlib.sha256(open(args.new, "rb").read()).hexdigest(),
        "previous_map": args.map,
        "image_base": hex(new.image_base),
        "export_count": len(new.exports),
        "distinct_addresses": len(by_addr),
        "named_addresses": sum(1 for e in entries if e["real_name"]),
        "validation": {"pairs": len(rows), "call_graph_disagreements": weak},
        "entries": entries,
    }
    json.dump(doc, open(args.out, "w"), indent=1)
    print("wrote", args.out, "named", doc["named_addresses"], "of", len(entries))
    return 0


if __name__ == "__main__":
    sys.exit(main())
