#!/usr/bin/env python3
"""Offline tests for il2cpp_methods; no third-party Python packages required."""
from __future__ import annotations

import json
import struct
import tempfile
import unittest
from pathlib import Path

import il2cpp_methods as m

BASE = 0x180000000


def synthetic_pe(image_base: int = BASE, pe32plus: bool = True) -> bytes:
    data = bytearray(0x400)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    opt = 0x80 + 24
    if pe32plus:
        struct.pack_into("<H", data, opt, 0x20B)
        struct.pack_into("<Q", data, opt + 24, image_base)
    else:
        struct.pack_into("<H", data, opt, 0x10B)
        struct.pack_into("<I", data, opt + 28, image_base & 0xFFFFFFFF)
    return bytes(data)


def script(*methods) -> dict:
    return {"ScriptMethod": [
        {"Address": BASE + rva, "Name": name, "Signature": sig, "TypeSignature": ""}
        for name, sig, rva in methods
    ]}


class ConvertTests(unittest.TestCase):
    def test_rva_is_va_minus_image_base(self):
        entries, dupes = m.convert(script(
            ("UnityEngine.Camera$$get_main", "Camera Camera::get_main()", 0xC27F00),
            ("UnityEngine.Transform$$get_position", "Vector3 Transform::get_position()", 0x1234),
        ), BASE)
        self.assertEqual([e["rva"] for e in entries], [0x1234, 0xC27F00])
        self.assertEqual(dupes, 0)

    def test_name_is_split_on_double_dollar(self):
        entries, _ = m.convert(script(("Namespace.Type$$Method", "void", 0x10)), BASE)
        self.assertEqual(entries[0]["type"], "Namespace.Type")
        self.assertEqual(entries[0]["method"], "Method")

    def test_duplicates_keep_the_first_record(self):
        entries, dupes = m.convert(script(
            ("A$$B", "sig", 0x10),
            ("A$$B", "sig", 0x20),
            ("A$$B", "other sig", 0x30),
        ), BASE)
        self.assertEqual(dupes, 1)
        self.assertEqual(sorted(e["rva"] for e in entries), [0x10, 0x30])

    def test_addresses_below_image_base_are_dropped(self):
        payload = {"ScriptMethod": [
            {"Address": 0x1000, "Name": "Bad$$Pointer", "Signature": "void"},
            {"Address": None, "Name": "No$$Address", "Signature": "void"},
            {"Address": BASE + 0x40, "Name": "Ok$$One", "Signature": "void"},
        ]}
        entries, _ = m.convert(payload, BASE)
        self.assertEqual([e["name"] for e in entries], ["Ok$$One"])

    def test_unsorted_input_is_ordered_by_rva(self):
        entries, _ = m.convert(script(("T$$b", "", 0x300), ("T$$a", "", 0x100)), BASE)
        self.assertEqual([e["rva"] for e in entries], [0x100, 0x300])


class PeTests(unittest.TestCase):
    def test_pe32plus_image_base(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "GameAssembly.dll"
            path.write_bytes(synthetic_pe(0x140000000))
            self.assertEqual(m.pe_image_base(path), 0x140000000)

    def test_pe32_image_base(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.dll"
            path.write_bytes(synthetic_pe(0x400000, pe32plus=False))
            self.assertEqual(m.pe_image_base(path), 0x400000)

    def test_missing_mz_exits(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.dll"
            path.write_bytes(b"\0" * 0x100)
            with self.assertRaises(SystemExit):
                m.pe_image_base(path)


class RustTests(unittest.TestCase):
    def test_rust_table_is_syntactically_plain(self):
        entries, _ = m.convert(script(("A$$B\"quoted", "", 0x10)), BASE)
        text = m.rust_table(entries, "deadbeef", "6000.0.67f1-DWR")
        self.assertIn("pub static METHODS: &[(&str, u32)] = &[", text)
        self.assertIn('("A$$B\\"quoted", 0x10),', text)
        self.assertIn("deadbeef", text)
        self.assertTrue(text.endswith("];\n"))


class CliTests(unittest.TestCase):
    def test_cli_writes_json_and_rust(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "GameAssembly.dll").write_bytes(synthetic_pe())
            (tmp / "script.json").write_text(json.dumps(script(
                ("UnityEngine.Camera$$get_main", "Camera", 0xC27F00),
                ("VRC.SDKBase.VRCPlayerApi$$GetPlayerByGameObject", "x", 0x1FF0),
            )))
            out = tmp / "methods.json"
            rust = tmp / "game_methods.rs"
            code = m.main([str(tmp / "script.json"), "--gameassembly", str(tmp / "GameAssembly.dll"),
                           "--out", str(out), "--rust", str(rust), "--limit", "0"])
            self.assertEqual(code, 0)
            payload = json.loads(out.read_text())
            self.assertEqual(payload["count"], 2)
            self.assertEqual(payload["image_base"], BASE)
            self.assertEqual(len({e["name"] for e in payload["methods"]}), 2)
            self.assertIn("METHODS", rust.read_text())

    def test_filter_selects_camera_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "GameAssembly.dll").write_bytes(synthetic_pe())
            (tmp / "script.json").write_text(json.dumps(script(
                ("UnityEngine.Camera$$get_main", "Camera", 0x100),
                ("VRC.SDKBase.VRCPlayerApi$$GetPlayerByGameObject", "x", 0x200),
            )))
            out = tmp / "methods.json"
            code = m.main([str(tmp / "script.json"), "--gameassembly", str(tmp / "GameAssembly.dll"),
                           "--out", str(out), "--filter", "camera", "--limit", "0"])
            self.assertEqual(code, 0)
            self.assertEqual(json.loads(out.read_text())["count"], 1)


if __name__ == "__main__":
    unittest.main()
