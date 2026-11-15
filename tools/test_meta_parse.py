#!/usr/bin/env python3
"""Tests for the offline parser mirror; an integration dump is opt-in."""
from __future__ import annotations

import os
from pathlib import Path
import struct
import unittest

import meta_parse


def put(d: bytearray, off: int, value: int, short: bool = False):
    struct.pack_into("<H" if short else "<i", d, off, value)


def section(d: bytearray, index: int, off: int, size: int):
    put(d, index * 4, off)
    put(d, index * 4 + 4, size)


def fixture(version: int = 31) -> bytes:
    d = bytearray(0xC00)
    put(d, 0, meta_parse.MAGIC - (1 << 32))
    put(d, 4, version)
    strings = b"UnityEngine\0Camera\0get_main\0fieldOfView\0Other\0TestAssembly\0"
    d[0x400:0x400 + len(strings)] = strings
    section(d, 6, 0x400, len(strings))
    section(d, 12, 0x500, 36 if version >= 31 else 32)
    section(d, 24, 0x600, 12)
    section(d, 10, 0x700, 20)
    section(d, 40, 0x800, 88)
    section(d, 42, 0x900, 32)
    section(d, 44, 0xA00, 80)
    put(d, 0x800, 12)       # name Camera
    put(d, 0x804, 0)        # namespace UnityEngine
    put(d, 0x808, 123)
    put(d, 0x80C, -1)
    put(d, 0x810, -1)
    put(d, 0x814, -1)
    put(d, 0x818, -1)
    put(d, 0x844, 1, True)  # field_count
    put(d, 0x840, 1, True)  # method_count
    put(d, 0x842, 1, True)  # property_count
    put(d, 0x850, 0x02000001)
    put(d, 0x500, 19)       # get_main
    put(d, 0x508, 12)
    put(d, 0x500 + (24 if version >= 31 else 20), 0x06000001)
    put(d, 0x600, 28)       # fieldOfView
    put(d, 0x604, 50)
    put(d, 0x700, 28)
    # Reserved property tail stays zero, as in native metadata records.
    put(d, 0x900, 46)       # TestAssembly
    put(d, 0x90C, 1)
    put(d, 0x918, -1)
    put(d, 0xA10, 46)
    put(d, 0xA28, 6000)
    return bytes(d)


class MetaParseTests(unittest.TestCase):
    def test_catalog_v31(self):
        m = meta_parse.Meta(fixture())
        self.assertEqual(m.summary(), {
            "schema": 1, "version": 31, "block_size": 0xA50, "types": 1,
            "methods": 1, "fields": 1, "properties": 1, "images": 1, "assemblies": 1,
        })
        t = m.type_dump("UnityEngine.Camera")
        self.assertEqual(t["methods"][0]["name"], "get_main")
        self.assertEqual(t["methods"][0]["image_index"], 0)
        self.assertEqual(t["fields"][0]["name"], "fieldOfView")
        self.assertEqual(t["fields"][0]["type"], "type#50")
        self.assertEqual(t["image"], "TestAssembly")
        self.assertEqual(m.image_method_range(m.images()[0]), range(1))

    def test_string_literals(self):
        d = bytearray(fixture())
        section(d, 2, 0xB00, 8)
        section(d, 4, 0xB40, 5)
        put(d, 0xB00, 5)
        d[0xB40:0xB45] = b"hello"
        m = meta_parse.Meta(bytes(d))
        self.assertEqual(m.literal(m.string_literals()[0]), "hello")

    def test_method_width_v29_v30(self):
        for v in (29, 30):
            m = meta_parse.Meta(fixture(v))
            self.assertEqual(m.methods_of(m.types()[0])[0]["name_index"], 19)

    def test_rejects_invalid_input(self):
        with self.assertRaises(meta_parse.MetadataError):
            meta_parse.Meta(b"")
        d = bytearray(fixture())
        put(d, 4, 32)
        with self.assertRaisesRegex(meta_parse.MetadataError, "version"):
            meta_parse.Meta(bytes(d))
        d = bytearray(fixture())
        section(d, 40, 0x800, 89)
        with self.assertRaises(meta_parse.MetadataError):
            meta_parse.Meta(bytes(d))

    def test_member_range_validation(self):
        d = bytearray(fixture())
        put(d, 0x824, 1_000)
        with self.assertRaises(meta_parse.MetadataError):
            meta_parse.Meta(bytes(d)).methods_of(meta_parse.Meta(bytes(d)).types()[0])

    def test_encrypted_game_file_is_not_accepted(self):
        game = Path.home() / ".local/share/Steam/steamapps/common/VRChat/VRChat_Data/il2cpp_data/Metadata/global-metadata.dat"
        if game.exists():
            with self.assertRaisesRegex(meta_parse.MetadataError, "magic"):
                meta_parse.Meta(game.read_bytes()[:4096])

    @unittest.skipUnless(os.environ.get("HROMATITE_META_DUMP"), "no integration dump supplied")
    def test_real_runtime_dump(self):
        m = meta_parse.Meta.read(os.environ["HROMATITE_META_DUMP"])
        self.assertGreater(m.summary()["types"], 1)
        self.assertIsNotNone(m.type("UnityEngine", "Camera"))


if __name__ == "__main__":
    unittest.main()
