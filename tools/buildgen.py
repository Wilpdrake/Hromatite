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
import bisect
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
SYMBOLS_RS = os.path.join(REPO, "crates", "hromatite-il2cpp", "src", "symbols.rs")

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
BUCKETS_BYTES = 0x800

# Non-export entry points: each rule is a byte shape plus one independent
# cross-check, because these addresses are patched, not merely called.
#
# `GetCurrentPlayerLoop` is the compiled managed getter: it zeroes the 0x28-byte
# `PlayerLoopSystem` it was handed in RCX and then resolves the icall
# `...PlayerLoop::GetCurrentPlayerLoopInternal()` once, caching the pointer.
PLAYER_LOOP_PROLOGUE = bytes.fromhex(
    "40534883ec5033c0c7442460000000000f57c0488bd90f11010f11411048894120"
)
PLAYER_LOOP_ICALL = b"UnityEngine.LowLevel.PlayerLoop::GetCurrentPlayerLoopInternal()"
# il2cpp's thread dispatcher: `RCX = {thread*, fn, arg}`; it bumps the thread's
# flags word, then calls the trampoline.  Same bytes on both measured builds.
WORKER_DISPATCH = bytes.fromhex(
    "48895c240848896c2410488974241848897c242041564883ec20488bd9488b31"
    "488b46088b90f800000083e2fb83fa02"
)


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
        self.exc_rva, self.exc_size = struct.unpack_from("<II", data, dirs + 3 * 8)
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
        self.pdata = self._pdata()

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

    # -- function boundaries ------------------------------------------------

    def _pdata(self):
        """`RUNTIME_FUNCTION` table: `begin -> (end, unwind_info_rva)`.

        Byte walking back from an instruction is not a function finder: rel32
        displacements are full of `0xCC` bytes.  The exception directory is
        exact, but Windows emits one entry per C++ EH scope, so the entry that
        contains an address is often a *fragment*; `function_at` follows
        `UNW_FLAG_CHAININFO` up to the primary entry.
        """
        out: dict[int, tuple[int, int]] = {}
        if not self.exc_rva or not self.exc_size:
            return out
        blob = self.read_rva(self.exc_rva, self.exc_size) or b""
        for index in range(len(blob) // 12):
            begin, end, unwind = struct.unpack_from("<III", blob, index * 12)
            if begin == 0 and end == 0:
                break
            out[begin] = (end, unwind)
        return out

    def function_at(self, rva: int):
        """Primary entry of the function that contains `rva`, or None."""
        if not self.pdata:
            return None
        starts = getattr(self, "_pdata_starts", None)
        if starts is None:
            starts = sorted(self.pdata)
            self._pdata_starts = starts
        index = bisect.bisect_right(starts, rva) - 1
        if index < 0:
            return None
        begin = starts[index]
        end, unwind = self.pdata[begin]
        if not begin <= rva < end:
            return None
        for _ in range(8):
            info = self.read_rva(unwind, 4) or b""
            # UNW_FLAG_CHAININFO (0x4) means "this is a scope inside another
            # function"; the primary entry is the end of the chain.
            if len(info) < 4 or ((info[0] >> 3) & 0x4) == 0:
                return begin
            count = info[2]
            offset = 4 + 2 * count + (2 if count & 1 else 0)
            chained = self.read_rva(unwind + offset, 12)
            if not chained:
                return begin
            parent = struct.unpack_from("<I", chained, 0)[0]
            entry = self.pdata.get(parent)
            if entry is None:
                return begin
            begin, unwind = parent, entry[1]
        return begin

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


def iat_slot_of(body: bytes, offset: int, body_rva: int = 0):
    """`FF 15 disp32` -> the IAT slot RVA it calls through."""
    if body[offset : offset + 2] != b"\xff\x15" or offset + 6 > len(body):
        return None
    return body_rva + offset + 6 + struct.unpack_from("<i", body, offset + 2)[0]


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

    context_slot = iat_slot_of(text, at(SUSPEND_CONTEXT_CALL) - text_rva, text_rva)
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
    remove_thread = pe.function_at(text_rva + lea_off) or function_start(text, text_rva, lea_off)
    remove_off = remove_thread - text_rva

    if text[remove_off + REMOVE_SKIP_CLOSE : remove_off + REMOVE_SKIP_CLOSE + 2] != b"\xff\x15":
        raise BuildgenError("remove_thread does not call through an IAT slot at +0x40")
    close_slot = iat_slot_of(text, remove_off + REMOVE_SKIP_CLOSE, text_rva)
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
        "thread_list_cs": collector_cs(pe, entry, text, text_rva),
    }
    return anchors, evidence


def collector_cs(pe: PE, entry: int, text: bytes, text_rva: int) -> int:
    """The critical section `GC_suspend_all` enters.

    The old heuristic ("most referenced lea target just before the table")
    pointed at the string `"CP1252"` on the next build and hung the game on an
    `EnterCriticalSection` that never returns.  The collector's own lock is what
    the entry guard must hold, and it is named by one instruction: the `lea rcx`
    that feeds `call [EnterCriticalSection]` inside the stop-the-world pass.
    """
    offset = entry - text_rva
    body = text[offset : offset + 0x260]
    votes: dict[int, int] = {}
    for site in find_all(body, b"\xff\x15"):
        slot = iat_slot_of(body, site, text_rva + offset)
        if pe.imports.get(slot, (None, None))[1] != "EnterCriticalSection":
            continue
        for back in range(site - 3, max(site - 18, 0), -1):
            if body[back : back + 3] != b"\x48\x8d\x0d":
                continue
            target = rip_target(text_rva + offset + back, struct.unpack_from("<i", body, back + 3)[0], 7)
            votes[target] = votes.get(target, 0) + 1
            break
    if len(votes) != 1:
        raise BuildgenError(
            f"GC_suspend_all enters {len(votes)} critical section(s); refusing to guess"
        )
    target = next(iter(votes))
    if target % 8:
        raise BuildgenError(f"GC_suspend_all lock {target:#x} is not pointer aligned")
    return target


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
            # Both come from the transfered export map; a build whose map lost
            # them gets 0 and the name-based callers refuse instead of calling
            # the module base.
            "class_from_name": names.get("il2cpp_class_from_name", 0),
            "class_from_type": names.get("il2cpp_class_from_il2cpp_type", 0),
            # Object/array/string factories the overlay needs to build managed
            # buffers: `il2cpp_object_new` + `il2cpp_array_new` allocate through
            # the game's own heap so Unity's GC owns the memory, and
            # `il2cpp_string_new` copies a literal out of this image.
            "object_new": names.get("il2cpp_object_new", 0),
            "array_new": names.get("il2cpp_array_new", 0),
            "string_new": names.get("il2cpp_string_new", 0),
            "class_init": names.get("il2cpp_runtime_class_init", 0),
            # A strong GC handle: the only way to keep a managed object alive
            # between calls, because a pointer stored in our own image is not a
            # GC root and the collector frees the object under us.
            "gchandle_new": names.get("il2cpp_gchandle_new", 0),
            "class_table": class_table,
        },
        {"getters": [(hex(rva), hex(global_rva), hex(value)) for rva, global_rva, value in getters]},
    )


# ---------------------------------------------------------------------------
# non-export entry points (PlayerLoop getter, worker dispatcher, GC unregister)
# ---------------------------------------------------------------------------


def code_sections(pe: PE):
    """`(name, rva, blob)` for the sections that hold executable code.

    IL2CPP emits the managed methods into its own `il2cpp` section (178 MB on
    the current build), *not* into `.text` — a `.text`-only scan silently finds
    nothing, which is exactly how the PlayerLoop getter used to look "missing".
    """
    out = []
    for name in (".text", "il2cpp"):
        entry = pe.section(name)
        if entry is None:
            continue
        out.append((name, entry[1], pe.crange(name)))
    return out


def find_in_code(pe: PE, needle: bytes, limit: int = 8):
    """Every `(rva, blob, offset)` where `needle` starts inside a code section."""
    out = []
    for name, rva, blob in code_sections(pe):
        for hit in find_all(blob, needle):
            out.append((name, rva + hit, blob, hit))
            if len(out) >= limit:
                return out
    return out


def lea_refs_to(body: bytes, body_rva: int, target: int):
    """Offsets of `lea reg,[rip+d]` inside `body` that point at `target`."""
    out = []
    for site in range(len(body) - 7):
        if body[site : site + 2] not in (b"\x48\x8d", b"\x4c\x8d"):
            continue
        if body[site + 2] & 0xC7 != 0x05:
            continue
        if rip_target(body_rva + site, struct.unpack_from("<i", body, site + 3)[0], 7) == target:
            out.append(body_rva + site)
    return out


def player_loop_getter(pe: PE):
    """The compiled `PlayerLoop.GetCurrentPlayerLoop`, by shape + icall name."""
    rdata_rva, rdata = pe.crva(".rdata"), pe.crange(".rdata")
    at = rdata.find(PLAYER_LOOP_ICALL)
    if at < 0:
        raise BuildgenError("the PlayerLoop icall name is not in .rdata")
    icall = rdata_rva + at
    candidates = find_in_code(pe, PLAYER_LOOP_PROLOGUE)
    proven = []
    for _, rva, _, _ in candidates:
        body = pe.read_rva(rva, 0x80) or b""
        refs = lea_refs_to(body, rva, icall)
        if refs:
            proven.append((rva, refs))
    if len(proven) != 1:
        raise BuildgenError(
            f"{len(candidates)} PlayerLoop-shaped prologue(s), {len(proven)} of them resolve the icall; "
            "refusing to guess"
        )
    return proven[0][0], proven[0][1]


def worker_dispatch(pe: PE, text: bytes, text_rva: int):
    """il2cpp's thread dispatcher, cross-checked by the creator that leas it."""
    at = find_all(text, WORKER_DISPATCH)
    if len(at) != 1:
        raise BuildgenError(f"{len(at)} worker-dispatch bodies found, need exactly one")
    dispatch = text_rva + at[0]
    if not lea_targets(text, text_rva, dispatch):
        raise BuildgenError("nothing refers to the worker dispatcher; shape matched by accident")
    return dispatch


def gc_unregister(pe: PE, remove_thread: int, text: bytes, text_rva: int):
    """The one function that calls `GC_remove_thread` (`GC_unregister_my_thread`)."""
    callers = set()
    for index in find_all(text, b"\xe8"):
        dest = rip_target(text_rva + index, struct.unpack_from("<i", text, index + 1)[0], 5)
        if dest != remove_thread:
            continue
        callers.add(pe.function_at(text_rva + index) or function_start(text, text_rva, index))
    if len(callers) != 1:
        raise BuildgenError(f"{len(callers)} functions call GC_remove_thread, need exactly one")
    return callers.pop()


def runtime_anchors(pe: PE, gc):
    text, text_rva = pe.crange(".text"), pe.crva(".text")
    getter, refs = player_loop_getter(pe)
    dispatch = worker_dispatch(pe, text, text_rva)
    unregister = gc_unregister(pe, gc["remove_thread"], text, text_rva)
    return (
        {
            "get_current_player_loop": getter,
            "worker_dispatch": dispatch,
            "gc_unregister": unregister,
        },
        {
            "get_current_player_loop": f"{getter:#x} (0x28-byte out struct zeroed, icall referenced at "
            + ", ".join(f"{r:#x}" for r in refs)
            + ")",
            "worker_dispatch": f"{dispatch:#x} (unique body, referenced by its creator)",
            "gc_unregister": f"{unregister:#x} (only caller of GC_remove_thread {gc['remove_thread']:#x}; .pdata entry)",
        },
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
        # Not derivable from code shapes: the image table is walked with a
        # constant while the records are contiguous, so only a live process can
        # measure it.  A descriptor that already carries a value for this build
        # keeps it (see `merge_layout`); a brand-new build gets 0 and m3 refuses.
        "image_stride",
        "image_name",
        "image_filename",
    ]
    for key in ("image_stride", "image_name", "image_filename"):
        values.setdefault(key, 0)
    return {key: values[key] for key in ordered}, notes


def merge_layout(derived: dict[str, int], existing: dict[str, int], notes: list[str]):
    """Keep what a live session measured when the generator disagrees.

    The export-name transfer can land on an ICF alias (it does for
    `il2cpp_class_get_name` on 245768f2: the transferred getter reads `+0x98`,
    while the live class record has its name at `+0x10`).  A descriptor that
    already exists for this exact SHA was read out of a running game, so it wins
    and the disagreement is reported instead of silently applied.
    """
    merged = dict(derived)
    for key in ("image_stride", "image_name", "image_filename"):
        if existing.get(key):
            merged[key] = existing[key]
        elif not merged.get(key):
            notes.append(f"{key}: 0 (never derived offline; measure it in a live process)")
    for key, value in existing.items():
        if not value or key not in merged:
            continue
        if merged[key] != value:
            notes.append(
                f"{key}: {value:#x} (kept from the live-verified descriptor; "
                f"the generator derived {merged[key]:#x})"
            )
            merged[key] = value
    return merged, notes


# ---------------------------------------------------------------------------
# extra runtime addresses (best effort, reported as unknown when not proven)
# ---------------------------------------------------------------------------


def extra_anchors(pe: PE, names: dict[str, int]):
    """Diagnostic extras that are not part of the runtime contract."""
    return {
        "class_from_type": names.get("il2cpp_class_from_il2cpp_type"),
        "known_names": sorted(
            name
            for name in (
                "il2cpp_class_from_il2cpp_type",
                "il2cpp_image_get_class",
                "il2cpp_get_corlib",
            )
            if name in names
        ),
    }


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

    # 3. layouts (merged with what a live session already measured)
    layout_values, layout_notes = {}, []
    existing_layout = {}
    if runtime_cache:
        existing_layout = {
            key: int(value, 16) for key, value in (runtime_cache.get("layout") or {}).items()
        }
    if names:
        try:
            layout_values, layout_notes = layout(pe, names)
            layout_values, layout_notes = merge_layout(layout_values, existing_layout, layout_notes)
        except BuildgenError as error:
            report["problems"].append(f"layout: {error}")
    else:
        report["problems"].append("layout: needs transferred export names")

    extra = extra_anchors(pe, names) if names else {}
    runtime, runtime_evidence = {}, {"error": "not derived"}
    try:
        runtime, runtime_evidence = runtime_anchors(pe, gc)
    except BuildgenError as error:
        report["problems"].append(f"runtime anchors: {error}")

    descriptor = {
        "schema": "hromatite/build/1",
        "source": "tools/buildgen.py",
        "sha256": sha,
        "unity": report["unity"],
        "gc": {key: f"{value:#x}" for key, value in gc.items()},
        "il2cpp": {key: f"{value:#x}" for key, value in (il2cpp or {}).items()} if il2cpp else {},
        "runtime": {key: f"{value:#x}" for key, value in runtime.items()},
        "layout": {key: f"{value:#x}" for key, value in layout_values.items()},
        "extra": {
            "class_from_type": (
                f"{extra['class_from_type']:#x}" if extra.get("class_from_type") else None
            ),
            "known_names": extra.get("known_names", []),
        },
        "evidence": {
            "gc": gc_evidence,
            "il2cpp": il2cpp_evidence,
            "runtime": runtime_evidence,
            "layout": layout_notes,
        },
        "note": "; ".join(
            [f"runtime.{key}: {value}" for key, value in runtime_evidence.items()] + layout_notes
            if runtime
            else layout_notes + [str(runtime_evidence)]
        ).strip("; "),
        "verified": bool(il2cpp)
        and bool(runtime)
        and not [note for note in layout_notes if ": 0 (" in note and not note.startswith("field_parent")],
    }
    report["gc"] = descriptor["gc"]
    report["il2cpp"] = descriptor["il2cpp"]
    report["runtime"] = descriptor["runtime"]
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
    for key in (
        "get_corlib",
        "image_get_class",
        "class_from_name",
        "class_from_type",
        "class_table",
        "object_new",
        "array_new",
        "string_new",
        "class_init",
        "gchandle_new",
    ):
        value = il2cpp.get(key)
        lines.append(f"        {key}: {word(value) if value else '0x0000_0000'},")
    lines.append("    },")
    lines.append("    runtime: RuntimeAnchors {")
    runtime = descriptor.get("runtime", {})
    for key in ("get_current_player_loop", "worker_dispatch", "gc_unregister"):
        value = runtime.get(key)
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
        "image_stride",
        "image_name",
        "image_filename",
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
