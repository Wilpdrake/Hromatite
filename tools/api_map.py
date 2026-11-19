#!/usr/bin/env python3
"""Offline Beebyte export map from UnityPlayer's lazy il2cpp resolver stubs.

GameAssembly's export names change each build. Every stub contributes an
obfuscated-name → RVA pair, but API identity comes only from unrenamed exports
and explicit runtime anchors: stub order and third-party patterns are checked,
never trusted implicitly. The report keeps candidates separate from proof.

Read-only with respect to the game. Run through uv on the NixOS host:
    uv run --no-project python tools/api_map.py
    uv run --no-project python tools/api_map.py --check
    uv run --no-project python tools/api_map.py --patterns ~/Downloads/Il2CppKnownPatterns.inl
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
from dataclasses import dataclass

# Prefix shared by the regular Unity 6 lazy resolver stubs. Displacements are
# deliberately wildcarded: bytes 8E/04/... after this prefix belong to an
# individual wrapper in Il2CppKnownPatterns.inl, not to the resolver itself.
STUB_RE = re.compile(
    rb"\x48\x8d\x15...."      # lea rdx, [rip + name_disp32]
    rb"\xe8...."             # call rel32 → GetProcAddress helper
    rb"\x48\x89\x05...."     # mov [rip + cache_disp32], rax
    rb"\x48\x85\xc0"         # test rax, rax
    rb"\x75\x0e"             # jnz +0xe
    rb"\x32\xdb"             # xor bl, bl
    rb"\x48\x8d\x0d", re.DOTALL,
)

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_GAME = Path.home() / ".local/share/Steam/steamapps/common/VRChat"
DEFAULT_NAMES = ROOT / "tools/data/il2cpp-api-names.txt"
DEFAULT_ANCHORS = ROOT / "tools/data/il2cpp-api-anchors.json"
DEFAULT_OUT = ROOT / "crates/hromatite-il2cpp/src/api_symbols.rs"
DEFAULT_REPORT = ROOT / "api_map.json"


def u16(data: bytes, off: int) -> int:
    return struct.unpack_from("<H", data, off)[0]


def u32(data: bytes, off: int) -> int:
    return struct.unpack_from("<I", data, off)[0]


class PeError(ValueError):
    pass


@dataclass(frozen=True)
class Section:
    name: str
    va: int
    vsize: int
    raw: int
    raw_size: int

    @property
    def map_size(self) -> int:
        return max(self.vsize, self.raw_size)


class Pe:
    """Small, strict PE32+/PE32 reader; file offsets and RVAs stay distinct."""

    def __init__(self, data: bytes):
        self.data = data
        if len(data) < 0x40 or data[:2] != b"MZ":
            raise PeError("not an MZ image")
        pe = u32(data, 0x3C)
        if pe + 24 > len(data) or data[pe:pe + 4] != b"PE\0\0":
            raise PeError("bad PE header")
        nsec = u16(data, pe + 6)
        opt_size = u16(data, pe + 20)
        opt = pe + 24
        if opt + opt_size > len(data):
            raise PeError("truncated optional header")
        magic = u16(data, opt)
        if magic == 0x20B:
            self.image_base = struct.unpack_from("<Q", data, opt + 24)[0]
            dirs = opt + 112
        elif magic == 0x10B:
            self.image_base = u32(data, opt + 28)
            dirs = opt + 96
        else:
            raise PeError(f"unsupported optional-header magic {magic:#x}")
        self.dirs = dirs
        st = opt + opt_size
        if st + nsec * 40 > len(data):
            raise PeError("truncated section table")
        self.sections = []
        for i in range(nsec):
            b = st + i * 40
            vsize, va, raw_size, raw = struct.unpack_from("<IIII", data, b + 8)
            name = data[b:b + 8].rstrip(b"\0").decode("ascii", errors="replace")
            self.sections.append(Section(name, va, vsize, raw, raw_size))

    @classmethod
    def read(cls, path: os.PathLike[str] | str) -> "Pe":
        return cls(Path(path).read_bytes())

    def rva_to_off(self, rva: int) -> int | None:
        # Crawling off_t == raw + min(rva-va, rsize_sane): using the mapped span
        # prevents an RVA in virtual zero-fill from becoming a file read.
        for sec in self.sections:
            if sec.va <= rva < sec.va + sec.map_size:
                delta = rva - sec.va
                if delta >= sec.raw_size:
                    return None
                off = sec.raw + delta
                return off if 0 <= off < len(self.data) else None
        return None

    def off_to_rva(self, off: int) -> int:
        for sec in self.sections:
            if sec.raw <= off < sec.raw + sec.raw_size:
                return sec.va + off - sec.raw
        raise PeError(f"file offset {off:#x} is not inside a section")

    def cstring_rva(self, rva: int, limit: int = 128) -> str | None:
        off = self.rva_to_off(rva)
        if off is None:
            return None
        end = self.data.find(b"\0", off, min(len(self.data), off + limit))
        if end < 0 or end == off:
            return None
        raw = self.data[off:end]
        if not all(0x21 <= c <= 0x7E for c in raw):
            return None
        return raw.decode("ascii")

    def export_dir(self) -> int:
        # IMAGE_DIRECTORY_ENTRY_EXPORT == 0; bounds checked by export_names().
        return u32(self.data, self.dirs)

    def export_names(self) -> dict[str, int]:
        exp = self.export_dir()
        d = self.rva_to_off(exp)
        if d is None:
            return {}
        n_names = u32(self.data, d + 24)
        funcs = u32(self.data, d + 28)
        names = u32(self.data, d + 32)
        ords = u32(self.data, d + 36)
        n_off = self.rva_to_off(names)
        f_off = self.rva_to_off(funcs)
        o_off = self.rva_to_off(ords)
        if None in (n_off, f_off, o_off) or n_names > 1_000_000:
            raise PeError("invalid export arrays")
        assert n_off is not None and f_off is not None and o_off is not None
        out: dict[str, int] = {}
        for i in range(n_names):
            nrva = u32(self.data, n_off + i * 4)
            name = self.cstring_rva(nrva)
            if name is None:
                raise PeError(f"invalid export name {i}")
            ordinal = u16(self.data, o_off + i * 2)
            out[name] = u32(self.data, f_off + ordinal * 4)
        return out


@dataclass(frozen=True)
class Stub:
    index: int
    file_offset: int
    rva: int
    name_rva: int
    obfuscated_name: str | None


def find_stubs(up: Pe) -> list[Stub]:
    out = []
    for i, match in enumerate(STUB_RE.finditer(up.data)):
        off = match.start()
        rva = up.off_to_rva(off)
        disp = struct.unpack_from("<i", up.data, off + 3)[0]
        name_rva = rva + 7 + disp
        if name_rva < 0 or name_rva > 0xFFFF_FFFF:
            raise PeError(f"stub {off:#x}: invalid name RVA {name_rva:#x}")
        out.append(Stub(i, off, rva, name_rva, up.cstring_rva(name_rva)))
    return out


def canonical_names(path: os.PathLike[str] | str) -> list[str]:
    names = []
    text = Path(path).read_text(encoding="utf-8")
    if text.lstrip().startswith("static"):
        names = ["il2cpp_" + n for n in re.findall(r'"([a-z][a-z0-9_]+)"', text)]
    else:
        for line in text.splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                names.append(line if line.startswith("il2cpp_") else "il2cpp_" + line)
    if len(names) != len(set(names)):
        raise ValueError("duplicate canonical API names")
    return names


def load_anchors(path: os.PathLike[str] | str) -> dict[str, str]:
    """Only identities proved independently of stub order or KnownPatterns."""
    raw = json.loads(Path(path).read_text(encoding="utf-8"))
    out = {}
    for api, record in raw.items():
        if not api.startswith("il2cpp_") or not record.get("obfuscated"):
            raise ValueError(f"invalid anchor {api!r}")
        out[record["obfuscated"]] = api
    return out


def load_patterns(path: os.PathLike[str] | str | None) -> dict[str, list[int | None]]:
    if path is None:
        return {}
    out = {}
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        m = re.match(r'\{"([^"]+)",\s*"([0-9a-fA-F? ]+)"\}', line.strip())
        if not m:
            continue
        out[m.group(1)] = [None if b in ("?", "??") else int(b, 16)
                          for b in m.group(2).split()]
    return out


def matches_mask_at(haystack: bytes, off: int, mask: list[int | None]) -> bool:
    return off >= 0 and off + len(mask) <= len(haystack) and all(
        b is None or haystack[off + i] == b for i, b in enumerate(mask)
    )


def masked_hits(haystack: bytes, mask: list[int | None],
                candidates: list[int] | None = None) -> list[int]:
    if not mask or mask[0] is None:
        return []
    hits = []
    # Checking the few hundred known stub addresses avoids 229 × 34 MB scans.
    pos = 0
    needle = bytes([mask[0]])
    while candidates is None:
        pos = haystack.find(needle, pos)
        if pos < 0:
            return hits
        if matches_mask_at(haystack, pos, mask):
            hits.append(pos)
        pos += 1
    return [off for off in candidates or [] if matches_mask_at(haystack, off, mask)]


def report_for(up: Pe, ga: Pe, names: list[str],
               patterns: dict[str, list[int | None]], ga_sha: str,
               anchors: dict[str, str]) -> dict:
    stubs = find_stubs(up)
    exports = ga.export_names()
    pattern_at: dict[int, list[str]] = {}
    stub_offsets = [s.file_offset for s in stubs]
    for api, mask in patterns.items():
        for off in masked_hits(up.data, mask, stub_offsets):
            pattern_at.setdefault(off, []).append(api)

    # Surviving il2cpp_* export names and explicit runtime anchors are the only
    # identities strong enough for the generated Rust table. A KnownPatterns hit
    # remains a *candidate*: in this build an exact domain_get pattern lands on a
    # different export, demonstrating that the pattern file is from another build.
    identities = dict(anchors)
    for obf in {s.obfuscated_name for s in stubs if s.obfuscated_name}:
        if obf in names:
            identities[obf] = obf
    rows = []
    pattern_conflicts = []
    for stub in stubs:
        api = identities.get(stub.obfuscated_name or "")
        marked = sorted(set(pattern_at.get(stub.file_offset, [])))
        if api and marked and api not in marked:
            pattern_conflicts.append({
                "anchor": api, "obfuscated": stub.obfuscated_name, "patterns": marked
            })
        rva = exports.get(stub.obfuscated_name) if stub.obfuscated_name else None
        if api and rva is None:
            raise PeError(f"anchor {api} points to non-exported name {stub.obfuscated_name}")
        rows.append({
            "index": stub.index,
            "api": api,
            "identity": "runtime-anchor" if stub.obfuscated_name in anchors else (
                "unobfuscated-export" if api else None
            ),
            "obfuscated": stub.obfuscated_name,
            "rva": rva,
            "stub_file_offset": stub.file_offset,
            "stub_rva": stub.rva,
            "name_rva": stub.name_rva,
            "pattern_candidates": marked,
            "resolution": "resolved" if rva is not None else (
                "null-name-slot" if stub.obfuscated_name is None else "name-not-in-exports"
            ),
        })
    # Offsets can repeat (aliases), so validate anchors against both names above.
    for obf, api in anchors.items():
        if obf not in exports:
            raise PeError(f"missing anchor export {api} -> {obf}")
    return {
        "schema": 2,
        "gameassembly_sha256": ga_sha,
        "unityplayer_sha256": hashlib.sha256(up.data).hexdigest(),
        "canonical_api_count": len(names),
        "stub_count": len(stubs),
        "named_stub_count": sum(r["obfuscated"] is not None for r in rows),
        "resolved_export_count": sum(r["rva"] is not None for r in rows),
        "identified_api_count": sum(r["api"] is not None for r in rows),
        "unidentified_api_count": len(names) - sum(r["api"] is not None for r in rows),
        "export_count": len(exports),
        "pattern_count": len(patterns),
        "pattern_hit_count": len({p for row in rows for p in row["pattern_candidates"]}),
        "stub_order_is_canonical": False,
        "stub_order_note": (
            "Rejected by live anchor: domain_get is wYRnnMQuvvH at stub 12, not canonical API 75"
        ),
        "pattern_conflicts_against_anchors": pattern_conflicts,
        "apis": rows,
    }


def rust_source(report: dict, unity: str) -> str:
    lines = [
        "// @generated by tools/api_map.py - do not edit by hand.",
        f"pub const UNITY_VERSION: &str = {json.dumps(unity)};",
        f"pub const GAMEASSEMBLY_SHA256: &str = {json.dumps(report['gameassembly_sha256'])};",
        f"pub const API_STUB_COUNT: usize = {report['stub_count']};",
        f"pub const API_IDENTIFIED_COUNT: usize = {report['identified_api_count']};",
        "",
        "/// (canonical il2cpp API, Beebyte export, GameAssembly RVA).",
        "/// Only runtime anchors and surviving names: unknown identities are omitted.",
        "pub const API: &[(&str, &str, u32)] = &[",
    ]
    for row in report["apis"]:
        if row["rva"] is None or row["api"] is None:
            continue
        lines.append(
            f"    ({json.dumps(row['api'])}, {json.dumps(row['obfuscated'])}, 0x{row['rva']:08x}),"
        )
    lines += [
        "];",
        "",
        "/// GameAssembly RVA of a canonical API (accepts il2cpp_x and x).",
        "pub fn rva(name: &str) -> Option<u32> {",
        "    API.iter().find_map(|(api, _, rva)| {",
        "        (name == *api || name.strip_prefix(\"il2cpp_\") == api.strip_prefix(\"il2cpp_\"))",
        "            .then_some(*rva)",
        "    })",
        "}",
        "",
    ]
    return "\n".join(lines)


def unity_version(game: Path) -> str:
    path = game / "VRChat_Data/globalgamemanagers"
    if not path.exists():
        return "unknown"
    data = path.read_bytes()[:4096]
    m = re.search(rb"(\d{4}\.\d+\.\d+[a-z]\d+)", data)
    return m.group(1).decode() if m else "unknown"


def write_if_changed(path: Path, content: str, check: bool) -> bool:
    old = path.read_text(encoding="utf-8") if path.exists() else None
    same = old == content
    if check and not same:
        print(f"stale: {path}", file=sys.stderr)
    elif not same:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        print(f"wrote {path}")
    return same


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--game-dir", type=Path, default=DEFAULT_GAME)
    ap.add_argument("--names", type=Path, default=DEFAULT_NAMES)
    ap.add_argument("--anchors", type=Path, default=DEFAULT_ANCHORS)
    ap.add_argument("--patterns", type=Path)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--report", type=Path, default=DEFAULT_REPORT)
    ap.add_argument("--check", action="store_true", help="fail if generated files differ; do not write")
    ap.add_argument("--require-complete", action="store_true", help="fail if any API has no export RVA")
    args = ap.parse_args(argv)
    try:
        up = Pe.read(args.game_dir / "UnityPlayer.dll")
        ga = Pe.read(args.game_dir / "GameAssembly.dll")
        names = canonical_names(args.names)
        patterns = load_patterns(args.patterns)
        anchors = load_anchors(args.anchors)
        anchor_data = json.loads(args.anchors.read_text(encoding="utf-8"))
        exports = ga.export_names()
        for obf, api in anchors.items():
            expected = int(anchor_data[api]["rva"], 16)
            if exports.get(obf) != expected:
                raise PeError(f"stale anchor {api}: {obf} has {exports.get(obf)!r}, expected {expected:#x}")
        sha = hashlib.sha256(ga.data).hexdigest()
        report = report_for(up, ga, names, patterns, sha, anchors)
    except (OSError, ValueError) as exc:
        print(f"api_map: {exc}", file=sys.stderr)
        return 1
    same_out = write_if_changed(args.out, rust_source(report, unity_version(args.game_dir)), args.check)
    same_report = write_if_changed(
        args.report, json.dumps(report, indent=2, sort_keys=True) + "\n", args.check
    )
    print(f"stubs {report['stub_count']}; named/RVA {report['named_stub_count']}/{report['resolved_export_count']}; "
          f"identified {report['identified_api_count']}/{report['canonical_api_count']}; "
          f"pattern candidates {report['pattern_hit_count']}/{report['pattern_count']}")
    if args.check and not (same_out and same_report):
        return 1
    if args.require_complete and report["identified_api_count"] != report["canonical_api_count"]:
        print("ERROR: API identities unresolved; inspect api_map.json. "
              "Stub order is NOT canonical on this build.", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
