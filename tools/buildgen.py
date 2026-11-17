#!/usr/bin/env python3
"""Derive every per-build number Hromatite needs from `GameAssembly.dll`.

The game re-obfuscates its exports and relinks the binary on every update, so
each number the runtime relies on (Boehm thread-table anchors, il2cpp entry
points, struct offsets) is a landmine for the next build.  This tool rebuilds
them from the shipped file alone:

* export map      - names transferred from the previous build (anchors, a
                    monotone body DP and call-graph isomorphism; see
                    `tools/export_transfer.py`);
* GC anchors      - byte shapes inside `GC_remove_thread` / `GC_suspend_all`,
                    cross-checked against the import table (CloseHandle,
                    GetThreadContext);
* il2cpp anchors  - the metadata-cache global, the `image_get_class` body and
                    the corlib getter;
* struct layouts  - read out of the functions that are *required* to load those
                    fields (`class_get_fields`, `class_get_method_from_name`,
                    `field_get_value`, `field_get_offset`, ...).  A field that
                    cannot be proven is written as `0` with a note, never
                    guessed, and every feature must then refuse to run.

Nothing here trusts "the order of the stubs" or any header from another Unity
version: only code shapes, validated on two consecutive builds (the oracle in
`tools/test_buildgen.py`).

Usage:
    python3 tools/buildgen.py --game DIR [--previous-map FILE] [--out-dir DIR]
                              [--update-descriptor] [--dry-run] [--check]

Read-only with respect to the game: it parses files, it never touches a live
process.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import export_transfer as xt  # noqa: E402  (same directory, used for the name transfer)

DEFAULT_GAME = os.path.expanduser("~/.local/share/Steam/steamapps/common/VRChat")
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA_DIR = os.path.join(REPO, "tools", "data")
BUILD_RS = os.path.join(REPO, "crates", "hromatite-abi", "src", "build.rs")
SYMBOLS_RS = os.path.join(REPO, "crates", "hromatite-core", "src", "symbols.rs")

# Distances inside `GC_suspend_all` / `GC_remove_thread` that stayed constant
# across both measured builds.  They are *hints*: every one of them is verified
# against the instruction shape at the computed address before being used.
SUSPEND_MSG_XREF_FIRST = 0x128
SUSPEND_MSG_XREF_SECOND = 0x139
SUSPEND_CONTEXT_CALL = 0xB2
SUSPEND_PROCESS_RECORD = 0x70
SUSPEND_RESUME_ABORT = 0xDB
SUSPEND_ABORT = 0x145
SUSPEND_ADVANCE_BUCKET = 0x24A
REMOVE_SKIP_CLOSE = 0x40
REMOVE_UNLINKED_NEXT = 0x46
REMOVE_AFTER_UNLINK = 0x68
STW_CS_DISTANCE = 0x40
BUCKETS_BYTES = 0x800


class BuildgenError(RuntimeError):
    """A rule could not be proven on this binary; the feature must fail closed."""


# ---------------------------------------------------------------------------
# PE reader
# ---------------------------------------------------------------------------


class PE:
    """Just enough PE to read sections, exports, relocations and imports."""

    def __init__(self, path: str) -> None:
        self.path = path
        with open(path, "rb") as handle:
            self.data = handle.read()
        data = self.data
        e = struct.unpack_from("<I", data, 0x3C)[0]
        if data[e : e + 4] != b"PE\0\0":
            raise BuildgenError(f"{path}: not a PE")
        sections = struct.unpack_from("<H", data, e + 6)[0]
        opt_size = struct.unpack_from("<H", data, e + 20)[0]
        optional = e + 24
        magic = struct.unpack_from("<H", data, optional)[0]
        if magic != 0x20B:
            raise BuildgenError(f"{path}: not PE32+")
        self.image_base = struct.unpack_from("<Q", data, optional + 24)[0]
        dirs = optional + 112
        self.export_rva, self.export_size = struct.unpack_from("<II", data, dirs)
        self.reloc_rva, self.reloc_size = struct.unpack_from("<II", data, dirs + 5 * 8)
        self.import_rva, self.import_size = struct.unpack_from("<II", data, dirs + 8)
        self.sections: list[tuple[str, int, int, int, int]] = []
        table = optional + opt_size
        for index in range(sections):
            base = table + index * 40
            name = data[base : base + 8].rstrip(b"\0").decode("latin1")
            vsize, rva, rawsize, rawptr = struct.unpack_from("<IIII", data, base + 8)
            self.sections.append((name, rva, vsize, rawptr, rawsize))
        self.exports = self._exports()
        self.relocs = self._relocs()
        self.imports = self._imports()

    # -- raw access ---------------------------------------------------------

    def read_rva(self, rva: int, size: int):
        for _, va, vsize, rawptr, rawsize in self.sections:
            if va <= rva < va + max(vsize, rawsize):
                offset = rawptr + (rva - va)
                if offset + size <= len(self.data):
                    return self.data[offset : offset + size]
                return None
        return None

    def must(self, rva: int, size: int) -> bytes:
        blob = self.read_rva(rva, size)
        if blob is None or len(blob) != size:
            raise BuildgenError(f"{self.path}: RVA {rva:#x} (+{size:#x}) is outside the image")
        return blob

    def u16(self, rva: int) -> int:
        return struct.unpack("<H", self.must(rva, 2))[0]

    def u32(self, rva: int) -> int:
        return struct.unpack("<I", self.must(rva, 4))[0]

    def u64(self, rva: int) -> int:
        return struct.unpack("<Q", self.must(rva, 8))[0]

    def section(self, name: str):
        for entry in self.sections:
            if entry[0] == name:
                return entry
        return None

    def crange(self, name: str) -> bytes:
        entry = self.section(name)
        if entry is None:
            raise BuildgenError(f"{self.path}: no {name} section")
        _, rva, vsize, _, _ = entry
        blob = self.read_rva(rva, vsize)
        if blob is None:
            raise BuildgenError(f"{self.path}: cannot read {name}")
        return blob

    def crva(self, name: str) -> int:
        entry = self.section(name)
        if entry is None:
            raise BuildgenError(f"{self.path}: no {name} section")
        return entry[1]

    # -- directories --------------------------------------------------------

    def _exports(self):
        base = self.export_rva
        count = struct.unpack("<I", self.must(base + 20, 4))[0]
        funcs = struct.unpack("<I", self.must(base + 28, 4))[0]
        names = struct.unpack("<I", self.must(base + 32, 4))[0]
        ords = struct.unpack("<I", self.must(base + 36, 4))[0]
        out = []
        for index in range(count):
            name_rva = struct.unpack("<I", self.must(names + 4 * index, 4))[0]
            raw = bytearray()
            cursor = name_rva
            while len(raw) < 256:
                byte = self.read_rva(cursor, 1)
                if not byte or byte == b"\0":
                    break
                raw += byte
                cursor += 1
            ordinal = struct.unpack("<H", self.must(ords + 2 * index, 2))[0]
            func = struct.unpack("<I", self.must(funcs + 4 * ordinal, 4))[0]
            out.append((raw.decode("latin1"), func))
        return out

    def _relocs(self):
        out = {}
        if not self.reloc_rva:
            return out
        ptr, end = self.reloc_rva, self.reloc_rva + self.reloc_size
        while ptr < end:
            head = self.read_rva(ptr, 8)
            if not head:
                break
            page, size = struct.unpack("<II", head)
            if size < 8:
                break
            body = self.read_rva(ptr + 8, (size - 8) // 2 * 2) or b""
            for index in range(len(body) // 2):
                entry = struct.unpack_from("<H", body, index * 2)[0]
                kind, offset = entry >> 12, entry & 0xFFF
                if kind in (1, 2):
                    out[page + offset] = 2
                elif kind == 3:
                    out[page + offset] = 4
                elif kind == 10:
                    out[page + offset] = 8
            ptr += size
        return out

    def _imports(self):
        """`IAT slot RVA -> ("dll.dll", "Function")` for named imports."""
        out = {}
        if not self.import_rva:
            return out
        cursor = self.import_rva
        while True:
            descriptor = self.read_rva(cursor, 20)
            if not descriptor or descriptor == b"\0" * 20:
                break
            original, _, _, name_rva, first_thunk = struct.unpack("<IIIII", descriptor)
            dll = bytearray()
            probe = name_rva
            while len(dll) < 128:
                byte = self.read_rva(probe, 1)
                if not byte or byte == b"\0":
                    break
                dll += byte
                probe += 1
            dll_name = dll.decode("latin1")
            thunk_rva = original or first_thunk
            index = 0
            while index < 4096:
                slot = self.read_rva(thunk_rva + index * 8, 8)
                if not slot:
                    break
                value = struct.unpack("<Q", slot)[0]
                if value == 0:
                    break
                if value >> 63:
                    name = f"#{value & 0xFFFF}"
                else:
                    hint = self.read_rva((value & 0x7FFFFFFF) + 2, 64) or b""
                    name = hint.split(b"\0")[0].decode("latin1")
                out[first_thunk + index * 8] = (dll_name, name)
                index += 1
            cursor += 20
        return out

    # -- helpers used by the name transfer ----------------------------------

    def masked_body(self, rva: int, nxt: int):
        size = max(4, min(256, nxt - rva))
        blob = self.read_rva(rva, size)
        if not blob:
            return None
        mask = bytearray(len(blob))
        for index in range(len(blob)):
            for span in range(self.relocs.get(rva + index, 0)):
                if index + span < len(blob):
                    mask[index + span] = 1
        return bytes(0xAA if masked else byte for byte, masked in zip(blob, mask)), bytes(mask)


# ---------------------------------------------------------------------------
# scan helpers (pure, unit-tested on synthetic buffers)
# ---------------------------------------------------------------------------


def find_all(hay: bytes, needle: bytes) -> list[int]:
    out = []
    if not needle or len(hay) < len(needle):
        return out
    index = 0
    while index + len(needle) <= len(hay):
        hit = hay.find(needle, index)
        if hit < 0:
            break
        out.append(hit)
        index = hit + 1
    return out


def rip_target(site_rva: int, disp: int, insn_len: int) -> int:
    return (site_rva + insn_len + disp) & 0xFFFFFFFF


def lea_targets(hay: bytes, hay_rva: int, target: int) -> list[int]:
    """Every `lea reg,[rip+disp32]` (REX.W + 8D) whose target is `target`."""
    out = []
    for site in find_all(hay, b"\x48\x8d"):
        out.extend(_lea_at(hay, hay_rva, site, target))
    for site in find_all(hay, b"\x4c\x8d"):
        out.extend(_lea_at(hay, hay_rva, site, target))
    return sorted(out)


def _lea_at(hay: bytes, hay_rva: int, site: int, target: int) -> list[int]:
    if site + 7 > len(hay):
        return []
    if hay[site + 2] & 0xC7 != 0x05:
        return []
    disp = struct.unpack_from("<i", hay, site + 3)[0]
    if rip_target(hay_rva + site, disp, 7) == target:
        return [hay_rva + site]
    return []


def u8_at(blob: bytes, offset: int):
    return blob[offset] if 0 <= offset < len(blob) else None


def load_disp_8(blob: bytes, offset: int):
    """`48 8B r/m+disp8` (mod=01) -> the displacement, else None."""
    if blob[offset : offset + 2] != b"\x48\x8b" or offset + 3 > len(blob):
        return None
    modrm = blob[offset + 2]
    if modrm & 0xC7 != 0x40 and modrm & 0xC7 != 0x44:
        return None
    return blob[offset + 3]


def naked_load(pe, rva: int):
    """`mov rax,[rcx+disp]; ret` -> disp (disp8 and disp32 forms)."""
    body = pe.read_rva(rva, 9) or b""
    if body[:3] == b"\x48\x8b\x81" and body[7:8] == b"\xc3":
        return struct.unpack_from("<I", body, 3)[0]
    if body[:3] == b"\x48\x8b\x41" and body[4:5] == b"\xc3":
        return body[3]
    if body[:2] == b"\x8b\x41" and body[3:4] == b"\xc3":
        return body[2]
    return None


def follow_thunks(pe, rva: int, hops: int = 4) -> int:
    """Follow forwarder thunks: a short prefix and/or `E9 rel32` (MSVC ICF aliases)."""
    for _ in range(hops):
        body = pe.read_rva(rva, 10) or b""
        stop = body.find(b"\xe9")
        if stop < 0 or stop > 6 or b"\xc3" in body[:stop]:
            return rva
        rva = (rva + stop + 5 + struct.unpack_from("<i", body, stop + 1)[0]) & 0xFFFFFFFF
    raise BuildgenError(f"thunk chain at {rva:#x} is too long")


def iat_slot_of(body: bytes, offset: int):
    """`FF 15 disp32` -> the IAT slot RVA it calls through."""
    if body[offset : offset + 2] != b"\xff\x15" or offset + 6 > len(body):
        return None
    return offset + 6 + struct.unpack_from("<i", body, offset + 2)[0]


def function_start(hay: bytes, hay_rva: int, site_off: int, limit: int = 0x100) -> int:
    """Walk back from `site_off` to the byte after the int3 padding."""
    index = site_off
    while index > 0 and hay[index - 1] != 0xCC:
        index -= 1
        if site_off - index > limit:
            raise BuildgenError("no int3 padding before the anchor")
    return hay_rva + index


# ---------------------------------------------------------------------------
# GC anchors
# ---------------------------------------------------------------------------


def gc_anchors(pe: PE):
    text, text_rva = pe.crange(".text"), pe.crva(".text")
    rdata, rdata_rva = pe.crange(".rdata"), pe.crva(".rdata")

    messages = [rdata_rva + hit for hit in find_all(rdata, b"SuspendThread loop failed")]
    if not messages:
        raise BuildgenError("no \"SuspendThread loop failed\" string in .rdata")
    sites = []
    for message in messages:
        sites.extend(lea_targets(text, text_rva, message))
    if not sites:
        raise BuildgenError("the abort message is never referenced by a lea")
    first = min(sites)
    entry = first - SUSPEND_MSG_XREF_FIRST

    def at(offset: int):
        return entry + offset

    evidence = {}
    if at(SUSPEND_MSG_XREF_SECOND) not in sites:
        raise BuildgenError("the second abort-message xref is not where it should be")
    if at(SUSPEND_ABORT) - text_rva >= len(text) or text[at(SUSPEND_ABORT) - text_rva] != 0xCC:
        raise BuildgenError("suspend abort site is not int3")
    if text[at(SUSPEND_RESUME_ABORT) - text_rva] != 0xCC:
        raise BuildgenError("resume abort site is not int3")
    if text[at(SUSPEND_CONTEXT_CALL) - text_rva : at(SUSPEND_CONTEXT_CALL) - text_rva + 2] != b"\xff\x15":
        raise BuildgenError("context call site is not `call [rip]`")
    if text[at(SUSPEND_ADVANCE_BUCKET) - text_rva : at(SUSPEND_ADVANCE_BUCKET) - text_rva + 4] != b"\x49\x83\xc7\x08":
        raise BuildgenError("bucket advance is not `add r15,8`")

    context_slot = iat_slot_of(text, at(SUSPEND_CONTEXT_CALL) - text_rva)
    context_name = pe.imports.get(context_slot, (None, None))[1] if context_slot is not None else None
    evidence["context_call"] = context_name or "shape only"

    walk = find_all(text, b"\x49\x8b\x1c\xf6")
    if not walk:
        raise BuildgenError("no `mov rbx,[r14+rsi*8]` bucket walk")
    walk_off = walk[0]
    lea_off = None
    start = walk_off - 0x40
    for index in range(max(start, 0), walk_off):
        if text[index : index + 3] == b"\x4c\x8d\x35":
            lea_off = index
    if lea_off is None:
        raise BuildgenError("no `lea r14,[rip]` before the bucket walk")
    disp = struct.unpack_from("<i", text, lea_off + 3)[0]
    threads = rip_target(text_rva + lea_off, disp, 7)
    remove_thread = function_start(text, text_rva, lea_off)
    remove_off = remove_thread - text_rva

    if text[remove_off + REMOVE_SKIP_CLOSE : remove_off + REMOVE_SKIP_CLOSE + 2] != b"\xff\x15":
        raise BuildgenError("remove_thread does not call through an IAT slot at +0x40")
    close_slot = iat_slot_of(text, remove_off + REMOVE_SKIP_CLOSE)
    close_name = pe.imports.get(close_slot, (None, None))[1] if close_slot is not None else None
    if close_name and close_name != "CloseHandle":
        raise BuildgenError(f"remove_thread +0x40 calls {close_name}, not CloseHandle")
    if text[remove_off + REMOVE_AFTER_UNLINK : remove_off + REMOVE_AFTER_UNLINK + 3] != b"\x48\x8d\x05":
        raise BuildgenError("after_unlink is not `lea rax,[rip]`")
    evidence["skip_close"] = close_name or "shape only"
    evidence["unlinked_next"] = f"{remove_thread:#x}+0x46"

    # Cross-check the table: `lea r15,[rip]` == threads and `lea r12,[rip]`
    # == threads + 0x800 inside GC_suspend_all.
    entry_off = entry - text_rva
    body = text[entry_off : entry_off + 0x260]
    seen = []
    for index in range(len(body) - 7):
        if body[index : index + 2] in (b"\x4c\x8d", b"\x48\x8d") and body[index + 2] & 0xC7 == 0x05:
            target = rip_target(entry + index, struct.unpack_from("<i", body, index + 3)[0], 7)
            seen.append(target)
    if threads not in seen:
        raise BuildgenError("GC_suspend_all does not load the same thread table")
    if threads + BUCKETS_BYTES not in seen:
        raise BuildgenError("GC_suspend_all does not load the table end")

    anchors = {
        "remove_thread": remove_thread,
        "skip_close": remove_thread + REMOVE_SKIP_CLOSE,
        "unlinked_next": remove_thread + REMOVE_UNLINKED_NEXT,
        "after_unlink": remove_thread + REMOVE_AFTER_UNLINK,
        "suspend_entry": entry,
        "context_call": at(SUSPEND_CONTEXT_CALL),
        "process_record": at(SUSPEND_PROCESS_RECORD),
        "resume_abort": at(SUSPEND_RESUME_ABORT),
        "suspend_abort": at(SUSPEND_ABORT),
        "advance_bucket": at(SUSPEND_ADVANCE_BUCKET),
        "threads": threads,
        "thread_list_cs": list_cs(text, text_rva, threads, threads - STW_CS_DISTANCE),
    }
    return anchors, evidence


def list_cs(text: bytes, text_rva: int, threads: int, stw: int) -> int:
    """Most-referenced lea target in the 0x200 bytes before the table."""
    counts: dict[int, int] = {}
    low = threads - 0x200
    for index in range(len(text) - 7):
        if text[index : index + 2] != b"\x48\x8d":
            continue
        if text[index + 2] & 0xC7 != 0x05:
            continue
        target = rip_target(text_rva + index, struct.unpack_from("<i", text, index + 3)[0], 7)
        if not low <= target < threads or target == stw:
            continue
        if target % 8:  # a CRITICAL_SECTION starts on a pointer boundary
            continue
        counts[target] = counts.get(target, 0) + 1
    if not counts:
        return 0
    return max(counts.items(), key=lambda item: (item[1], -item[0]))[0]


# ---------------------------------------------------------------------------
# il2cpp anchors
# ---------------------------------------------------------------------------


def il2cpp_anchors(pe: PE, names: dict[str, int], runtime_cache: dict | None):
    text, text_rva = pe.crange(".text"), pe.crva(".text")

    class_table = None
    for site in find_all(text, b"\x48\x8b\x05"):
        if text[site + 7 : site + 11] != b"\x48\x8d\x34\xf8":
            continue
        disp = struct.unpack_from("<i", text, site + 3)[0]
        class_table = rip_target(text_rva + site, disp, 7)
        break
    if class_table is None:
        raise BuildgenError("no `mov rax,[rip]; lea rsi,[rax+rdi*8]` for the class table")

    image_get_class = None
    patterns = (
        (b"\x44\x8b\x00\x44\x03\xc2\x41\x83\xf8\xff\x75\x07\x33\xc9\xe9", 4),
        (b"\x44\x8b\x00\x44\x03\xc2\x41\x83\xf8\xff\x75", 0),
    )
    for prefix_len in (4, 7):
        needle = b"\x48\x8b\x41" if prefix_len == 4 else b"\x48\x8b\x81"
        for site in find_all(text, needle):
            cursor = site + prefix_len
            for suffix, _ in patterns:
                if text[cursor : cursor + len(suffix)] == suffix:
                    image_get_class = text_rva + site
                    break
            if image_get_class:
                break
        if image_get_class:
            break
    if image_get_class is None:
        raise BuildgenError("no il2cpp_image_get_class body")
    if not any(rva == image_get_class for _, rva in pe.exports):
        raise BuildgenError("the image_get_class body is not an exported entry point")

    # `il2cpp_get_corlib` returns an image; the other naked getters return
    # small constants (pool chunk size, a channel count, ...).  The corlib slot
    # holds a metadata pointer (two packed 32-bit offsets), so it is the only
    # one above 4 GiB in the file.
    getters = []
    for _, rva in pe.exports:
        body = pe.read_rva(rva, 8) or b""
        if not body.startswith(b"\x48\x8b\x05") or body[7:8] != b"\xc3":
            continue
        global_rva = rip_target(rva, struct.unpack_from("<i", body, 3)[0], 7)
        try:
            value = pe.u64(global_rva)
        except BuildgenError:
            continue
        getters.append((rva, global_rva, value))
    big = [entry for entry in getters if entry[2] > 0xFFFF_FFFF]
    get_corlib = None
    if len(big) == 1:
        get_corlib = big[0][0]
    elif runtime_cache and runtime_cache.get("get_corlib"):
        get_corlib = int(runtime_cache["get_corlib"], 16) if isinstance(runtime_cache["get_corlib"], str) else int(runtime_cache["get_corlib"])
    if get_corlib is None:
        raise BuildgenError(
            f"cannot single out il2cpp_get_corlib among {len(getters)} naked getters "
            "(no runtime cache to confirm it)"
        )

    return (
        {
            "get_corlib": get_corlib,
            "image_get_class": image_get_class,
            "class_table": class_table,
        },
        {"getters": [(hex(rva), hex(global_rva), hex(value)) for rva, global_rva, value in getters]},
    )


# ---------------------------------------------------------------------------
# struct layouts
# ---------------------------------------------------------------------------


def _need(names: dict[str, int], name: str) -> int:
    rva = names.get(name)
    if rva is None:
        raise BuildgenError(f"no export named {name}")
    return rva


def _load_before_store(body: bytes):
    """`mov rax,[class+disp]; mov [iterator],rax` -> the pointer displacement.

    `il2cpp_class_get_fields` and `il2cpp_class_get_methods` both publish the
    array pointer to the caller's iterator with `48 89 07`, so the load right
    in front of that store is the field we are after (0x80/0x50 on the older
    build, 0x88/0x58 on the newer one).
    """
    for site in find_all(body, b"\x48\x89\x07"):
        for back in (7, 4):
            start = site - back
            if start < 0 or body[start : start + 2] != b"\x48\x8b":
                continue
            modrm = body[start + 2]
            mod, rm = modrm >> 6, modrm & 7
            if rm == 4:
                continue
            if mod == 2 and back == 7:
                return struct.unpack_from("<I", body, start + 3)[0]
            if mod == 1 and back == 4:
                return body[start + 3]
    return None


def _count_word(body: bytes):
    """`cmp WORD PTR [reg+disp32],0` -> the displacement."""
    for match in re.finditer(rb"\x66\x83[\xb8-\xbf](....)\x00", body, re.S):
        return struct.unpack("<I", match.group(1))[0]
    return None


def _stride(body: bytes):
    candidates = set()
    for match in re.finditer(rb"\x48\x83\xc0(.)", body, re.S):
        candidates.add(match.group(1)[0])
    for match in re.finditer(rb"\x48\xc1[\xe0-\xe3](.)", body, re.S):
        candidates.add(1 << match.group(1)[0])
    # `lea reg,[r9+disp8]` (`4D 8D /r` with mod=01) -- the iterator end.
    for index in range(len(body) - 3):
        if body[index : index + 2] not in (b"\x49\x8d", b"\x4d\x8d"):
            continue
        if body[index + 2] & 0xC7 != 0x41:
            continue
        candidates.add(body[index + 3])
    plausible = {value for value in candidates if 0x10 <= value <= 0x80}
    return plausible.pop() if len(plausible) == 1 else None


def _chain_parent(body: bytes):
    """`mov rbx,[rbx+disp]; test rbx,rbx` -> the parent offset (the class chain walk)."""
    for site in find_all(body, b"\x48\x8b\x5b"):
        if b"\x48\x85\xdb" in body[site + 4 : site + 12]:
            return body[site + 3]
    for site in find_all(body, b"\x48\x8b\x9b"):
        if b"\x48\x85\xdb" in body[site + 7 : site + 15]:
            return struct.unpack_from("<I", body, site + 3)[0]
    return None


def layout(pe: PE, names: dict[str, int]):
    notes = []
    values: dict[str, int] = {}

    def store(key: str, value, note_when_missing: str):
        if value is None:
            values[key] = 0
            notes.append(f"{key}: 0 ({note_when_missing})")
        else:
            values[key] = value

    class_get_fields = follow_thunks(pe, _need(names, "il2cpp_class_get_fields"))
    fields_body = pe.read_rva(class_get_fields, 0x100) or b""
    store("class_fields", _load_before_store(fields_body), "fields pointer not loaded twice")
    store("class_fields_count", _count_word(fields_body), "no `cmp word [class+disp],0`")
    store("field_stride", _stride(fields_body), "no single stride candidate")

    methods_body = pe.read_rva(follow_thunks(pe, _need(names, "il2cpp_class_get_methods")), 0x100) or b""
    store("class_methods", _load_before_store(methods_body), "methods pointer not loaded twice")

    store("class_name", naked_load(pe, follow_thunks(pe, _need(names, "il2cpp_class_get_name"))), "not a naked getter")
    store("class_namespace", naked_load(pe, follow_thunks(pe, _need(names, "il2cpp_class_get_namespace"))), "not a naked getter")

    # The parent offset is proven by the class-chain walk inside
    # `class_get_method_from_name`; the export map alone is not enough because
    # `class_get_parent` shares its body with `field_get_name` (ICF), so a
    # transfer can land on the wrong member of that alias group.
    method_from_name = follow_thunks(pe, _need(names, "il2cpp_class_get_method_from_name"))
    method_body = pe.read_rva(method_from_name, 0x140) or b""
    chain_parent = _chain_parent(method_body)
    getter_parent = None
    if "il2cpp_class_get_parent" in names:
        getter_parent = naked_load(pe, follow_thunks(pe, names["il2cpp_class_get_parent"]))
    if chain_parent is not None:
        values["class_parent"] = chain_parent
        if getter_parent is not None and getter_parent != chain_parent:
            notes.append(
                f"class_parent: {chain_parent:#x} (parent-chain walk inside class_get_method_from_name; "
                f"the transferred name points at {getter_parent:#x}, an ICF alias of another getter)"
            )
    else:
        store("class_parent", getter_parent, "neither a parent-chain walk nor a naked getter")

    field_offset_fn = follow_thunks(pe, _need(names, "il2cpp_field_get_offset"))
    offset_body = pe.read_rva(field_offset_fn, 9) or b""
    field_offset = None
    if offset_body[:3] == b"\x48\x63\x41" and offset_body[4:5] == b"\xc3":
        field_offset = offset_body[3]
    elif offset_body[:3] == b"\x48\x63\x81" and offset_body[7:8] == b"\xc3":
        field_offset = struct.unpack_from("<I", offset_body, 3)[0]
    store("field_offset", field_offset, "not `movsxd rax,[rcx+disp]`")

    field_value_fn = follow_thunks(pe, _need(names, "il2cpp_field_get_value"))
    value_body = pe.read_rva(field_value_fn, 0x80) or b""
    field_type = None
    test_at = value_body.find(b"\xf7\x46\x08")
    if test_at >= 0:
        window = value_body[max(test_at - 24, 0) : test_at]
        for match in re.finditer(rb"\x48\x8b[\x32\x72\xb2](.*?)$", window, re.S):
            chunk = window[match.start() :]
            if chunk[2] == 0x32:
                field_type = 0
            elif len(chunk) >= 4:
                field_type = chunk[3]
            break
    store("field_type", field_type, "no `test [type+8]` after the type load")

    field_from_name = follow_thunks(pe, _need(names, "il2cpp_class_get_field_from_name"))
    name_body = pe.read_rva(field_from_name, 0xC0) or b""
    strcmp_at = name_body.find(b"\x48\x8b\xc7\x4c\x2b\xc7")
    field_name = None
    if strcmp_at > 0:
        window = name_body[max(strcmp_at - 16, 0) : strcmp_at]
        if b"\x4d\x8b\x02" in window:
            field_name = 0
        else:
            match = re.search(rb"\x4d\x8b[\x42\x52]\x04", window, re.S)
            if match is None:
                match = re.search(rb"\x4d\x8b[\x42\x52](.)", window, re.S)
            if match:
                field_name = match.group(1)[0]
    store("field_name", field_name, "no strcmp against a field-name load")

    if "il2cpp_field_get_parent" in names:
        candidate = naked_load(pe, follow_thunks(pe, names["il2cpp_field_get_parent"]))
        stride = values.get("field_stride") or 0
        used = {values.get("field_name"), values.get("field_type"), values.get("field_offset")}
        if candidate is not None and (
            not stride or candidate >= stride or candidate % 8 or candidate in used
        ):
            notes.append(
                f"field_parent: 0 (the transferred il2cpp_field_get_parent points at {candidate:#x}, "
                f"which does not fit a {stride:#x}-byte FieldInfo)"
            )
            values["field_parent"] = 0
        else:
            store("field_parent", candidate, "the export is not a naked getter")
    else:
        values["field_parent"] = 0
        notes.append("field_parent: 0 (no il2cpp_field_get_parent export on this build)")

    marker = method_body.find(b"\x41\x0f\xb6\x06\x38\x01")
    method_name = None
    if marker > 0:
        window = method_body[max(marker - 12, 0) : marker]
        match = re.search(rb"\x49\x8b[\x48-\x4f](.)", window, re.S)
        if match:
            method_name = match.group(1)[0]
    store("method_name", method_name, "no method-name load before the first-char compare")

    flags = None
    match = re.search(rb"\x41\x0f\xb7\x40(.)", method_body, re.S)
    if match:
        flags = match.group(1)[0]
    store("method_flags", flags, "no `movzx eax,word [method+disp]`")

    param_count = None
    match = re.search(rb"\x41\x0f\xb6\x40(.)", method_body, re.S)
    if match:
        param_count = match.group(1)[0]
    store("method_param_count", param_count, "no `movzx eax,byte [method+disp]`")

    param_body = pe.read_rva(follow_thunks(pe, _need(names, "il2cpp_method_get_param")), 0x30) or b""
    parameters = None
    match = re.search(rb"\x0f\xb6(\x41|[\x81\x91]....)", param_body, re.S)
    if match:
        count = param_body[match.start() + 3] if match.group(1)[0] == 0x41 else struct.unpack_from("<I", param_body, match.start() + 3)[0]
        if param_count is not None and count != param_count:
            notes.append(f"method_param_count: {param_count:#x} (method_get_param compares {count:#x})")
    match = re.search(rb"\x48\x8b\x41(.)\x8b\xd2", param_body, re.S)
    if match:
        parameters = match.group(1)[0]
    store("method_parameters", parameters, "no `mov rax,[method+disp]` before the index load")

    if method_name is not None:
        values["method_return_type"] = method_name + 8
        notes.append(
            f"method_return_type: {method_name + 8:#x} (heuristic: name+8; no function reads it in this build)"
        )
    else:
        values["method_return_type"] = 0
        notes.append("method_return_type: 0 (method name unknown)")

    ordered = [
        "class_parent",
        "class_namespace",
        "class_methods",
        "class_fields",
        "class_name",
        "class_fields_count",
        "field_stride",
        "field_name",
        "field_type",
        "field_offset",
        "field_parent",
        "method_name",
        "method_return_type",
        "method_parameters",
        "method_flags",
        "method_param_count",
    ]
    return {key: values[key] for key in ordered}, notes


# ---------------------------------------------------------------------------
# extra runtime addresses (best effort, reported as unknown when not proven)
# ---------------------------------------------------------------------------


def extra_anchors(pe: PE, names: dict[str, int], gc: dict[str, int]):
    out: dict[str, object] = {}
    text, text_rva = pe.crange(".text"), pe.crva(".text")
    # GC_unregister_my_thread: the function that calls GC_remove_thread.
    callers = []
    target = gc["remove_thread"]
    for index in range(len(text) - 5):
        if text[index] != 0xE8:
            continue
        dest = text_rva + index + 5 + struct.unpack_from("<i", text, index + 1)[0]
        if dest == target:
            callers.append(function_start(text, text_rva, index))
    out["gc_unregister"] = sorted(set(callers))
    out["class_from_type"] = names.get("il2cpp_class_from_il2cpp_type")
    out["known_names"] = sorted(
        name
        for name in (
            "il2cpp_class_from_il2cpp_type",
            "il2cpp_image_get_class",
            "il2cpp_get_corlib",
        )
        if name in names
    )
    return out


# ---------------------------------------------------------------------------
# driver
# ---------------------------------------------------------------------------


def unity_version(game_dir: str) -> str:
    path = os.path.join(game_dir, "VRChat_Data", "globalgamemanagers")
    try:
        with open(path, "rb") as handle:
            blob = handle.read(4096)
    except OSError:
        return "unknown"
    match = re.search(rb"(6000\.\d+\.\d+[a-z0-9]*-[A-Za-z0-9]+|20\d\d\.\d+\.\d+[a-z]\d+)", blob)
    return match.group(1).decode() if match else "unknown"


def names_from_map(path: str) -> dict[str, int]:
    with open(path) as handle:
        doc = json.load(handle)
    out = {}
    for entry in doc.get("entries", []):
        if entry.get("real_name"):
            out.setdefault(entry["real_name"], int(entry["rva"], 16))
    return out


def cache_dir() -> str:
    path = os.path.expanduser("~/.cache/hromatite/builds")
    os.makedirs(path, exist_ok=True)
    return path


def cache_binary(path: str, sha: str, keep: bool) -> None:
    if not keep:
        return
    dest = os.path.join(cache_dir(), f"GameAssembly-{sha[:12]}.dll")
    if os.path.exists(dest) and os.path.getsize(dest) == os.path.getsize(path):
        return
    with open(path, "rb") as source, open(dest, "wb") as target:
        while True:
            chunk = source.read(1 << 22)
            if not chunk:
                break
            target.write(chunk)


def transfer_names(old_path: str, new_path: str, previous_map: str, out_path: str) -> dict:
    old, new = xt.PE(old_path), xt.PE(new_path)
    with open(previous_map) as handle:
        previous = json.load(handle)
    named = {int(entry["rva"], 16): entry for entry in previous["entries"] if entry.get("real_name")}
    old_addrs = sorted(named)
    new_addrs = sorted({rva for _, rva in new.exports})

    pairs = xt.align(old, new, old_addrs, new_addrs)
    rows = xt.callgraph_check(old, new, [(a, b) for a, b in pairs if b])
    weak = [row for row in rows if row["score"] < xt.CB_THRESHOLD]

    by_addr: dict[int, list[str]] = {}
    for name, rva in new.exports:
        by_addr.setdefault(rva, []).append(name)
    names = {}
    for old_rva, new_rva in pairs:
        if new_rva and old_rva in named:
            names.setdefault(new_rva, named[old_rva])
    entries = []
    for rva in sorted(by_addr):
        entry = {"addr": hex(new.image_base + rva), "rva": hex(rva), "exports": sorted(by_addr[rva])}
        if rva in names:
            source = names[rva]
            entry["real_name"] = source["real_name"]
            entry["confidence"] = "transfer"
            entry["evidence"] = (
                f"from {source['rva']} ({source['real_name']}): anchors + monotone body DP + call-graph isomorphism"
            )
        else:
            entry["real_name"] = None
            entry["confidence"] = "unknown"
            entry["evidence"] = "no counterpart proven; inspect by shape before naming"
        entries.append(entry)
    sha = hashlib.sha256(open(new_path, "rb").read()).hexdigest()
    doc = {
        "schema": "hromatite/il2cpp-export-names/1",
        "source": "tools/buildgen.py (export_transfer: anchors + monotone DP + call-graph isomorphism)",
        "gameassembly_sha256": sha,
        "previous_map": os.path.basename(previous_map),
        "image_base": hex(new.image_base),
        "export_count": len(new.exports),
        "distinct_addresses": len(by_addr),
        "named_addresses": sum(1 for entry in entries if entry["real_name"]),
        "validation": {"pairs": len(rows), "call_graph_disagreements": weak},
        "entries": entries,
    }
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w") as handle:
        json.dump(doc, handle, indent=1)
    return doc


def load_existing_map(out_dir: str, sha: str):
    """The per-build name map already in the tree, matched by the SHA it records."""
    if not os.path.isdir(out_dir):
        return None
    for name in sorted(os.listdir(out_dir)):
        if not (name.startswith("il2cpp-export-names") and name.endswith(".json")):
            continue
        path = os.path.join(out_dir, name)
        try:
            with open(path) as handle:
                doc = json.load(handle)
        except (OSError, ValueError):
            continue
        if str(doc.get("gameassembly_sha256", "")).lower() == sha:
            return doc
    return None


def run(
    game_dir: str,
    previous_map: str | None,
    out_dir: str,
    update_descriptor: bool,
    dry_run: bool,
    previous_binary: str | None = None,
    keep_binary: bool = True,
    unity: str | None = None,
):
    gameassembly = os.path.join(game_dir, "GameAssembly.dll")
    pe = PE(gameassembly)
    sha = hashlib.sha256(open(gameassembly, "rb").read()).hexdigest()
    sha12 = sha[:12]

    detected = unity_version(game_dir)
    report = {"sha256": sha, "unity": unity or (detected if detected != "unknown" else "unknown"), "problems": []}
    if not dry_run:
        cache_binary(gameassembly, sha, keep_binary)

    # 1. names
    maps = sorted(
        (
            os.path.join(out_dir, name)
            for name in (os.listdir(out_dir) if os.path.isdir(out_dir) else [])
            if name.startswith("il2cpp-export-names-") and name.endswith(".json")
        ),
        key=os.path.getmtime,
    )
    previous = previous_map or (maps[-1] if maps else None)
    names: dict[str, int] = {}
    existing_map = load_existing_map(out_dir, sha)
    if existing_map is not None:
        names = {
            entry["real_name"]: int(entry["rva"], 16)
            for entry in existing_map.get("entries", [])
            if entry.get("real_name")
        }
        report["names"] = {
            "map": f"existing map for {sha[:12]}",
            "named": existing_map.get("named_addresses", len(names)),
            "total": len(existing_map.get("entries", [])),
            "source": "already in the tree (no transfer needed)",
        }
    elif previous:
        target_map = os.path.join(out_dir, f"il2cpp-export-names-{sha12}.json")
        with open(previous) as handle:
            previous_doc = json.load(handle)
        previous_sha = previous_doc.get("gameassembly_sha256", "")
        if previous_binary is None:
            candidate = os.path.join(cache_dir(), f"GameAssembly-{previous_sha[:12]}.dll")
            if os.path.exists(candidate):
                previous_binary = candidate
        if dry_run:
            report["names"] = f"would transfer from {os.path.basename(previous)}"
        elif previous_binary and os.path.exists(previous_binary):
            doc = transfer_names(previous_binary, gameassembly, previous, target_map)
            names = {
                entry["real_name"]: int(entry["rva"], 16)
                for entry in doc["entries"]
                if entry.get("real_name")
            }
            report["names"] = {
                "map": target_map,
                "named": doc["named_addresses"],
                "total": len(doc["entries"]),
                "weak": len(doc["validation"]["call_graph_disagreements"]),
            }
        else:
            report["problems"].append(
                "previous GameAssembly binary is not cached; pass --previous-binary "
                f"(looked for GameAssembly-{previous_sha[:12]}.dll)"
            )
    else:
        report["problems"].append("no previous map: export names cannot be transferred")

    if not names:
        known = os.path.join(out_dir, f"il2cpp-export-names-{sha12}.json")
        if os.path.exists(known):
            names = names_from_map(known)

    runtime_cache = None
    cache_path = os.path.join(out_dir, f"build-{sha12}.json")
    if os.path.exists(cache_path):
        with open(cache_path) as handle:
            runtime_cache = json.load(handle)

    # 2. anchors
    gc, gc_evidence = gc_anchors(pe)
    try:
        il2cpp, il2cpp_evidence = il2cpp_anchors(pe, names, runtime_cache)
    except BuildgenError as error:
        report["problems"].append(f"il2cpp anchors: {error}")
        il2cpp, il2cpp_evidence = None, {"error": str(error)}

    # 3. layouts
    layout_values, layout_notes = {}, []
    if names:
        try:
            layout_values, layout_notes = layout(pe, names)
        except BuildgenError as error:
            report["problems"].append(f"layout: {error}")
    else:
        report["problems"].append("layout: needs transferred export names")

    extra = extra_anchors(pe, names, gc) if names else {"gc_unregister": []}
    gc_unregister = extra.get("gc_unregister") or []
    if not isinstance(gc_unregister, list):
        gc_unregister = []
    class_from_type = extra.get("class_from_type")

    descriptor = {
        "schema": "hromatite/build/1",
        "source": "tools/buildgen.py",
        "sha256": sha,
        "unity": report["unity"],
        "gc": {key: f"{value:#x}" for key, value in gc.items()},
        "il2cpp": {key: f"{value:#x}" for key, value in (il2cpp or {}).items()} if il2cpp else {},
        "layout": {key: f"{value:#x}" for key, value in layout_values.items()},
        "extra": {
            "gc_unregister": [f"{value:#x}" for value in gc_unregister],
            "class_from_type": f"{class_from_type:#x}" if class_from_type else None,
        },
        "evidence": {"gc": gc_evidence, "il2cpp": il2cpp_evidence, "layout": layout_notes},
        "note": "; ".join(
            layout_notes
            + ["gc.thread_list_cs: most-referenced 8-byte-aligned lea target before the table (heuristic)"]
        ),
        "verified": bool(il2cpp)
        and not [note for note in layout_notes if ": 0 (" in note and not note.startswith("field_parent")],
    }
    report["gc"] = descriptor["gc"]
    report["il2cpp"] = descriptor["il2cpp"]
    report["layout"] = descriptor["layout"]
    report["notes"] = layout_notes
    report["extra"] = descriptor["extra"]

    if not dry_run:
        with open(cache_path, "w") as handle:
            json.dump(descriptor, handle, indent=1)
        report["descriptor"] = cache_path
        if update_descriptor:
            report["descriptor_update"] = update_build_rs(descriptor, out_dir)
    return report


def render_build(descriptor: dict) -> str:
    sha = descriptor["sha256"]
    tag = sha[:8].upper()
    gc = descriptor["gc"]
    il2cpp = descriptor["il2cpp"]
    layout = descriptor["layout"]

    def word(value: str) -> str:
        raw = int(value, 16)
        return f"0x{raw >> 16:04x}_{raw & 0xFFFF:04x}"

    def small(value: str) -> str:
        return f"0x{int(value, 16):02x}"

    lines = [
        f"/// Build `{sha[:12]}` (Unity {descriptor.get('unity', 'unknown')}), generated by `tools/buildgen.py`.",
        f"pub const BUILD_{tag}: Build = Build {{",
        f'    sha256: "{sha}",',
        f'    unity: "{descriptor.get("unity", "unknown")}",',
        "    gc: GcAnchors {",
    ]
    for key in (
        "remove_thread",
        "skip_close",
        "unlinked_next",
        "after_unlink",
        "suspend_entry",
        "context_call",
        "process_record",
        "resume_abort",
        "suspend_abort",
        "advance_bucket",
        "threads",
        "thread_list_cs",
    ):
        lines.append(f"        {key}: {word(gc[key])},")
    lines.append("    },")
    lines.append("    il2cpp: Il2cppAnchors {")
    for key in ("get_corlib", "image_get_class", "class_table"):
        value = il2cpp.get(key)
        lines.append(f"        {key}: {word(value) if value else '0x0000_0000'},")
    lines.append("    },")
    lines.append("    layout: Layout {")
    for key in (
        "class_parent",
        "class_namespace",
        "class_methods",
        "class_fields",
        "class_name",
        "class_fields_count",
        "field_stride",
        "field_name",
        "field_type",
        "field_offset",
        "field_parent",
        "method_name",
        "method_return_type",
        "method_parameters",
        "method_flags",
        "method_param_count",
    ):
        lines.append(f"        {key}: {small(layout.get(key, '0x0'))},")
    lines.append("    },")
    lines.append(f"    verified: {'true' if descriptor.get('verified') else 'false'},")
    note = descriptor.get("note", "") or "; ".join(descriptor.get("evidence", {}).get("layout", []))
    escaped = note.replace("\\", "\\\\").replace('"', '\\"')
    lines.append(f'    note: "{escaped}",')
    lines.append("};")
    return "\n".join(lines)


BEGIN = "// --- buildgen:begin (kept in sync by tools/buildgen.py) ---"
END = "// --- buildgen:end ---"


def update_build_rs(descriptor: dict, out_dir: str) -> str:
    with open(BUILD_RS) as handle:
        source = handle.read()
    if BEGIN not in source or END not in source:
        raise BuildgenError(f"{BUILD_RS}: buildgen markers are missing")
    # Keep every descriptor already emitted, newest last, and replace this SHA.
    blocks = []
    for name in sorted(os.listdir(out_dir)):
        if not (name.startswith("build-") and name.endswith(".json")):
            continue
        with open(os.path.join(out_dir, name)) as handle:
            doc = json.load(handle)
        if doc.get("sha256") == descriptor["sha256"]:
            doc = descriptor
        blocks.append(doc)
    blocks.sort(key=lambda doc: doc["unity"])
    body = "\n\n".join(render_build(block) for block in blocks)
    rendered = f"{BEGIN}\n{body}\n\npub static KNOWN: &[Build] = &[\n"
    rendered += "".join(f"    BUILD_{block['sha256'][:8].upper()},\n" for block in blocks)
    rendered += "];\n" + END
    start = source.index(BEGIN)
    stop = source.index(END) + len(END)
    with open(BUILD_RS, "w") as handle:
        handle.write(source[:start] + rendered + source[stop:])
    return f"{BUILD_RS}: {len(blocks)} build(s)"


def main() -> int:
    parser = argparse.ArgumentParser(description="derive per-build anchors from GameAssembly.dll")
    parser.add_argument("--game", default=DEFAULT_GAME)
    parser.add_argument("--unity", help="override the Unity version when the game data is elsewhere")
    parser.add_argument("--previous-map")
    parser.add_argument("--previous-binary", help="old GameAssembly.dll for the name transfer")
    parser.add_argument("--no-cache", action="store_true", help="do not cache this build's binary")
    parser.add_argument("--out-dir", default=DATA_DIR)
    parser.add_argument("--update-descriptor", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--json", action="store_true", help="print the descriptor only")
    args = parser.parse_args()

    game_dir = os.path.expanduser(args.game)
    out_dir = os.path.abspath(args.out_dir)
    try:
        report = run(
            game_dir,
            args.previous_map,
            out_dir,
            args.update_descriptor,
            args.dry_run,
            previous_binary=args.previous_binary,
            keep_binary=not args.no_cache,
            unity=args.unity,
        )
    except BuildgenError as error:
        print(f"buildgen: {error}", file=sys.stderr)
        return 2
    print(json.dumps(report, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
