#!/usr/bin/env python3
"""Resolve il2cpp API functions offline, by signature, without the game.

The obfuscated GameAssembly exports are called through lazy-init stubs inside
UnityPlayer.dll: `lea rdx, [rip+disp]` (the *name* string) followed by a call to
GetProcAddress and a store. `Il2CppKnownPatterns.inl` (from another client)
describes the first 22+ bytes of each stub with `??` wildcards, which is enough
to find it — and the `lea` gives us the export name, which `symbols.rs` maps to an
RVA. No game process, no brute force, no OOM.

Usage:
    uv run --no-project python tools/resolve_api.py il2cpp_class_from_name ...
"""
import re
import struct
import sys

GAME = "/home/wilpdrake/.local/share/Steam/steamapps/common/VRChat"
SYMBOLS = "/home/wilpdrake/RustroverProjects/Hromatite/crates/hromatite-core/src/symbols.rs"
PATTERNS = "/home/wilpdrake/Downloads/Il2CppKnownPatterns.inl"


def load_patterns(path):
    out = {}
    with open(path) as f:
        for line in f:
            m = re.match(r'\{"([^"]+)",\s*"([0-9A-Fa-f? ]+)"\}', line.strip())
            if not m:
                continue
            name, pat = m.group(1), m.group(2).split()
            out[name] = [None if p in ("??", "?") else int(p, 16) for p in pat]
    return out


def load_symbols(path):
    src = open(path, encoding="utf-8").read()
    return {n: int(r, 16) for n, r in re.findall(r'\("([^"]+)",\s*(0x[0-9a-fA-F]+)\)', src)}


class Pe:
    def __init__(self, path):
        self.data = open(path, "rb").read()
        d = self.data
        pe = struct.unpack_from("<I", d, 0x3C)[0]
        self.image_base = struct.unpack_from("<Q", d, pe + 24 + 24)[0]
        nsec = struct.unpack_from("<H", d, pe + 6)[0]
        opt_size = struct.unpack_from("<H", d, pe + 20)[0]
        st = pe + 24 + opt_size
        self.secs = []
        for i in range(nsec):
            b = st + i * 40
            vsize, va, rsize, raw = struct.unpack_from("<IIII", d, b + 8)
            self.secs.append((va, max(vsize, rsize), raw))

    def off(self, rva):
        for va, size, raw in self.secs:
            if va <= rva < va + size:
                return raw + (rva - va)
        return None

    def cstr_at_va(self, va, limit=128):
        o = self.off(va - self.image_base)
        if o is None:
            return None
        end = self.data.find(b"\0", o, o + limit)
        if end < 0:
            return None
        s = self.data[o:end]
        if not s or not all(33 <= c < 127 for c in s):
            return None
        return s.decode()


def find_pattern(data, pat):
    n = len(pat)
    first = pat[0]
    i = 0
    while True:
        i = data.find(bytes([first]), i)
        if i < 0 or i + n > len(data):
            return None
        ok = True
        for k in range(n):
            p = pat[k]
            if p is not None and data[i + k] != p:
                ok = False
                break
        if ok:
            return i
        i += 1


def main():
    names = sys.argv[1:]
    pats = load_patterns(PATTERNS)
    syms = load_symbols(SYMBOLS)
    up = Pe(f"{GAME}/UnityPlayer.dll")
    ga = Pe(f"{GAME}/GameAssembly.dll")

    for name in names:
        pat = pats.get(name)
        if not pat:
            print(f"{name:34s} pattern: MISSING")
            continue
        off = find_pattern(up.data, pat)
        if off is None:
            print(f"{name:34s} pattern not found")
            continue
        # `48 8D 15 disp32` → lea rdx, [rip+disp]
        disp = struct.unpack_from("<i", up.data, off + 3)[0]
        va = up.image_base + off + 7 + disp
        s = up.cstr_at_va(va)
        rva = syms.get(s) if s else None
        extra = ""
        if rva is not None:
            fo = ga.off(rva)
            b = ga.data[fo:fo + 8]
            thunk = ""
            if b[0] == 0xE9:
                rel = struct.unpack_from("<i", ga.data, fo + 1)[0]
                tgt = rva + 5 + rel
                to = ga.off(tgt)
                thunk = f" thunk->{tgt:#x} {ga.data[to:to+16].hex(' ')}"
            extra = f" rva={rva:#x}{thunk}"
        print(f"{name:34s} stub@{off:#x} name={s!r}{extra}")


if __name__ == "__main__":
    main()
