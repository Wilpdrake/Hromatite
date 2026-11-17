#!/usr/bin/env python3
"""Tests for `tools/buildgen.py`.

Two layers:

* the scan rules are exercised on synthetic buffers (no game files needed), so a
  broken ModRM/RIP-relative parse fails in milliseconds;
* the whole pipeline is replayed against the two reference builds and compared
  with the oracle table below -- the numbers that were derived by hand in Ghidra
  and confirmed in the live game.  This is the acceptance test: if a rule stops
  reproducing them, the rule is wrong.

Run:
    python3 tools/test_buildgen.py -v
Set `HROMATITE_OLD_BINARY` / `HROMATITE_GAME_DIR` to point at other copies.
"""
from __future__ import annotations

import json
import os
import struct
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import buildgen as bg  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OLD_BINARY = os.environ.get(
    "HROMATITE_OLD_BINARY",
    os.path.expanduser("~/EAC-RE/artifacts/GameAssembly_old_648f61b1.dll"),
)
GAME_DIR = os.environ.get("HROMATITE_GAME_DIR", bg.DEFAULT_GAME)

# The acceptance oracle, copied from the task brief.  Both builds were measured
# by hand (Ghidra + live VRChat), so the generator has to reproduce them.
ORACLE = {
    "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b": {
        "gc": {
            "remove_thread": 0xC417B0,
            "skip_close": 0xC417F0,
            "unlinked_next": 0xC417F6,
            "after_unlink": 0xC41818,
            "suspend_entry": 0xC41A20,
            "context_call": 0xC41AD2,
            "process_record": 0xC41A90,
            "resume_abort": 0xC41AFB,
            "suspend_abort": 0xC41B65,
            "advance_bucket": 0xC41C6A,
            "threads": 0xE6BB180,
            "thread_list_cs": 0xE6BB100,
        },
        "il2cpp": {"get_corlib": 0xC27F00, "image_get_class": 0xC2A330, "class_table": 0xE66C870},
        "layout": {
            "class_parent": 0x10,
            "class_namespace": 0x18,
            "class_methods": 0x50,
            "class_fields": 0x80,
            "class_name": 0x98,
            "class_fields_count": 0x124,
            "field_stride": 0x28,
            "field_name": 0x10,
            "field_type": 0x00,
            "field_offset": 0x08,
            "field_parent": 0x20,
            "method_name": 0x18,
            "method_return_type": 0x20,
            "method_parameters": 0x30,
            "method_flags": 0x4C,
            "method_param_count": 0x52,
        },
    },
    "245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb": {
        "gc": {
            "remove_thread": 0xC43520,
            "skip_close": 0xC43560,
            "unlinked_next": 0xC43566,
            "after_unlink": 0xC43588,
            "suspend_entry": 0xC43790,
            "context_call": 0xC43842,
            "process_record": 0xC43800,
            "resume_abort": 0xC4386B,
            "suspend_abort": 0xC438D5,
            "advance_bucket": 0xC439DA,
            "threads": 0xE6BD8A0,
            "thread_list_cs": 0xE6BD800,
        },
        "il2cpp": {"get_corlib": 0xC29C70, "image_get_class": 0xC2C0A0, "class_table": 0xE66F0D0},
        "layout": {
            "class_parent": 0xA0,
            "class_namespace": 0x18,
            "class_methods": 0x58,
            "class_fields": 0x88,
            "class_name": 0x98,
            "class_fields_count": 0x124,
            "field_stride": 0x20,
            "field_name": 0x00,
            "field_type": 0x10,
            "field_offset": 0x08,
            "field_parent": 0x00,
            "method_name": 0x20,
            "method_return_type": 0x28,
            "method_parameters": 0x30,
            "method_flags": 0x4C,
            "method_param_count": 0x52,
        },
    },
}


class FakePE:
    """Just enough of `buildgen.PE` to exercise the pure scanners."""

    def __init__(self, blobs: dict[int, bytes]) -> None:
        self.blobs = blobs

    def read_rva(self, rva: int, size: int):
        for start, blob in self.blobs.items():
            if start <= rva < start + len(blob):
                offset = rva - start
                return blob[offset : offset + size]
        return None

    def must(self, rva: int, size: int) -> bytes:
        blob = self.read_rva(rva, size)
        if blob is None or len(blob) != size:
            raise bg.BuildgenError("outside the fake image")
        return blob


def u32(value: int) -> bytes:
    return struct.pack("<I", value)


class ScanRules(unittest.TestCase):
    def test_find_all_returns_overlapping_starts(self):
        self.assertEqual(bg.find_all(b"aaXaaXaa", b"aa"), [0, 3, 6])
        self.assertEqual(bg.find_all(b"abc", b""), [])
        self.assertEqual(bg.find_all(b"ab", b"abcd"), [])

    def test_lea_targets_handles_both_rex_prefixes(self):
        text_rva = 0x1000
        target = 0x5000
        first = b"\x48\x8d\x0d" + struct.pack("<i", target - (text_rva + 7))
        second = b"\x4c\x8d\x35" + struct.pack("<i", target - (text_rva + 16 + 7))
        text = first + b"\x90" * 9 + second + b"\xcc" * 8
        self.assertEqual(bg.lea_targets(text, text_rva, target), [text_rva, text_rva + 16])
        self.assertEqual(bg.lea_targets(text, text_rva, target + 1), [])

    def test_naked_load_reads_disp8_and_disp32(self):
        pe = FakePE({0x1000: b"\x48\x8b\x41\x18\xc3", 0x2000: b"\x48\x8b\x81\x98\x00\x00\x00\xc3"})
        self.assertEqual(bg.naked_load(pe, 0x1000), 0x18)
        self.assertEqual(bg.naked_load(pe, 0x2000), 0x98)
        self.assertIsNone(bg.naked_load(pe, 0x3000))

    def test_load_before_store_finds_the_iterator_publish(self):
        disp32 = b"\x48\x8b\x83" + u32(0x88) + b"\x48\x89\x07"
        disp8 = b"\x48\x8b\x43\x58\x48\x89\x07"
        self.assertEqual(bg._load_before_store(disp32), 0x88)
        self.assertEqual(bg._load_before_store(disp8), 0x58)
        self.assertIsNone(bg._load_before_store(b"\x48\x8b\x54\x24\x30\x48\x89\x07"))

    def test_count_word_reads_the_cmp_displacement(self):
        body = b"\x66\x83\xbb" + u32(0x124) + b"\x00"
        self.assertEqual(bg._count_word(body), 0x124)
        self.assertIsNone(bg._count_word(b"\x90" * 8))

    def test_stride_accepts_shift_add_and_lea_forms(self):
        self.assertEqual(bg._stride(b"\x48\x83\xc0\x28"), 0x28)
        self.assertEqual(bg._stride(b"\x48\xc1\xe1\x05"), 0x20)
        self.assertEqual(bg._stride(b"\x49\x8d\x49\x20"), 0x20)
        # two disagreeing candidates must not be guessed
        self.assertIsNone(bg._stride(b"\x48\x83\xc0\x28\x48\xc1\xe1\x05"))

    def test_chain_parent_recognises_both_load_forms(self):
        self.assertEqual(bg._chain_parent(b"\x48\x8b\x5b\x10\x48\x85\xdb"), 0x10)
        self.assertEqual(bg._chain_parent(b"\x48\x8b\x9b" + u32(0xA0) + b"\x48\x85\xdb"), 0xA0)
        self.assertIsNone(bg._chain_parent(b"\x48\x8b\x5b\x10\x90\x90"))

    def test_follow_thunks_walks_a_prefixed_stub(self):
        stub = b"\x45\x33\xc9\xe9" + struct.pack("<i", 0x1C)
        body = b"\x48\x89\x5c\x24\x08"
        pe = FakePE({0x1000: stub + b"\xcc" * 16, 0x1024: body})
        self.assertEqual(bg.follow_thunks(pe, 0x1000), 0x1024)
        self.assertEqual(bg.follow_thunks(pe, 0x1024), 0x1024)


class ReferenceBuilds(unittest.TestCase):
    """Replay the generator on the two shipped files and compare with the oracle."""

    def _names(self, sha: str) -> dict[str, int]:
        for name in sorted(os.listdir(os.path.join(REPO, "tools", "data"))):
            if not name.startswith("il2cpp-export-names"):
                continue
            path = os.path.join(REPO, "tools", "data", name)
            with open(path) as handle:
                doc = json.load(handle)
            if doc.get("gameassembly_sha256") != sha:
                continue
            return {
                entry["real_name"]: int(entry["rva"], 16)
                for entry in doc["entries"]
                if entry.get("real_name")
            }
        self.skipTest(f"no name map for {sha[:12]}")

    def _check(self, binary: str):
        pe = bg.PE(binary)
        sha = bg.hashlib.sha256(open(binary, "rb").read()).hexdigest()
        expected = ORACLE[sha]
        gc, _ = bg.gc_anchors(pe)
        self.assertEqual(gc, expected["gc"])
        names = self._names(sha)
        il2cpp, _ = bg.il2cpp_anchors(pe, names, None)
        self.assertEqual(il2cpp, expected["il2cpp"])
        layout, _ = bg.layout(pe, names)
        self.assertEqual(layout, expected["layout"])
        return sha

    def test_previous_build_reproduces_the_oracle(self):
        if not os.path.exists(OLD_BINARY):
            self.skipTest(f"{OLD_BINARY} is not available")
        self.assertEqual(
            self._check(OLD_BINARY),
            "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b",
        )

    def test_current_build_reproduces_the_oracle(self):
        gameassembly = os.path.join(GAME_DIR, "GameAssembly.dll")
        if not os.path.exists(gameassembly):
            self.skipTest(f"{gameassembly} is not available")
        self.assertEqual(
            self._check(gameassembly),
            "245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb",
        )

    def test_descriptor_in_the_tree_matches_its_map(self):
        data = os.path.join(REPO, "tools", "data")
        for name in sorted(os.listdir(data)):
            if not (name.startswith("build-") and name.endswith(".json")):
                continue
            with open(os.path.join(data, name)) as handle:
                descriptor = json.load(handle)
            expected = ORACLE.get(descriptor["sha256"])
            if expected is None:
                continue
            for section in ("gc", "il2cpp", "layout"):
                for key, value in expected[section].items():
                    self.assertEqual(
                        int(descriptor[section][key], 16),
                        value,
                        f"{name}: {section}.{key}",
                    )


if __name__ == "__main__":
    unittest.main()
