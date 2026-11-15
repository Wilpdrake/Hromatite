#!/usr/bin/env python3
"""Offline tests for api_map; no third-party Python packages required."""
from __future__ import annotations

import json
import os
from pathlib import Path
import struct
import tempfile
import unittest

import api_map


def synthetic_up() -> api_map.Pe:
    """One PE32+ section: file +0x200 maps to RVA +0x1000 (delta -0xe00)."""
    data = bytearray(0x400)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<H", data, 0x84 + 2, 1)
    struct.pack_into("<H", data, 0x84 + 16, 0xF0)
    opt = 0x98
    struct.pack_into("<H", data, opt, 0x20B)
    struct.pack_into("<Q", data, opt + 24, 0x180000000)
    sec = opt + 0xF0
    data[sec:sec + 8] = b".text\0\0\0"
    struct.pack_into("<IIII", data, sec + 8, 0x1000, 0x1000, 0x200, 0x200)
    stub_off = 0x210
    stub_rva = 0x1010
    name_rva = 0x1080
    stub = bytearray(29)
    stub[:] = bytes.fromhex(
        "48 8d 15 00 00 00 00 e8 00 00 00 00 48 89 05 00 00 00 00 "
        "48 85 c0 75 0e 32 db 48 8d 0d"
    )
    struct.pack_into("<i", stub, 3, name_rva - (stub_rva + 7))
    data[stub_off:stub_off + 29] = stub
    data[0x280:0x280 + 12] = b"domainObf_X\0"
    return api_map.Pe(bytes(data))


class FakeGa(api_map.Pe):
    def __init__(self):
        self.data = b"game"

    def export_names(self):
        return {"domainObf_X": 0x12340}


class ApiMapTests(unittest.TestCase):
    def test_pe_file_offset_is_not_rva(self):
        pe = synthetic_up()
        self.assertEqual(pe.off_to_rva(0x210), 0x1010)
        self.assertEqual(pe.rva_to_off(0x1080), 0x280)
        self.assertIsNone(pe.rva_to_off(0x1F00))  # beyond raw data

    def test_rip_relative_name_uses_rva(self):
        stubs = api_map.find_stubs(synthetic_up())
        self.assertEqual(len(stubs), 1)
        self.assertEqual(stubs[0].name_rva, 0x1080)
        self.assertEqual(stubs[0].obfuscated_name, "domainObf_X")

    def test_identity_comes_from_anchor_not_list_order(self):
        report = api_map.report_for(
            synthetic_up(), FakeGa(), ["il2cpp_domain_get", "il2cpp_init"], {}, "sha",
            {"domainObf_X": "il2cpp_domain_get"},
        )
        self.assertEqual(report["apis"][0]["index"], 0)
        self.assertEqual(report["apis"][0]["api"], "il2cpp_domain_get")
        self.assertEqual(report["apis"][0]["rva"], 0x12340)
        self.assertFalse(report["stub_order_is_canonical"])
        rust = api_map.rust_source(report, "6000.test")
        self.assertIn('("il2cpp_domain_get", "domainObf_X", 0x00012340)', rust)

    def test_short_and_truncated_inputs_are_errors(self):
        with self.assertRaises(api_map.PeError):
            api_map.Pe(b"MZ")
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "names.txt"
            p.write_text("init\ninit\n")
            with self.assertRaises(ValueError):
                api_map.canonical_names(p)

    @unittest.skipUnless(os.environ.get("HROMATITE_TEST_GAME") == "1", "live build not requested")
    def test_current_build(self):
        up = api_map.Pe.read(api_map.DEFAULT_GAME / "UnityPlayer.dll")
        stubs = api_map.find_stubs(up)
        self.assertEqual(len(stubs), 232)
        self.assertTrue(all(s.obfuscated_name for s in stubs))
        self.assertFalse(api_map.report_for(
            up, api_map.Pe.read(api_map.DEFAULT_GAME / "GameAssembly.dll"), [], {}, "sha", {}
        )["stub_order_is_canonical"])


if __name__ == "__main__":
    unittest.main()
