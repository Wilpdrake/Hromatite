#!/usr/bin/env python3
from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest

import dump
import gamediff
from test_meta_parse import fixture


class DumpTests(unittest.TestCase):
    def test_dump_exports_named_catalog_and_rva_map(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            src = root / "global-metadata.dat"
            out = root / "catalog.json"
            src.write_bytes(fixture())
            (root / "methods.json").write_text('{"method_rvas": {"0": "0x1234"}}')
            dump.dump(src, out, root / "methods.json")
            doc = json.loads(out.read_text())
            self.assertEqual(doc["types"][0]["name"], "UnityEngine.Camera")
            self.assertEqual(doc["types"][0]["methods"][0]["name"], "get_main")
            self.assertEqual(doc["types"][0]["methods"][0]["rva"], 0x1234)
            self.assertEqual(doc["block_size"], 0xA50)

    def test_api_and_metadata_changes_are_visible(self):
        old = {
            "apis": [{"index": 0, "api": "a", "obfuscated": "x", "rva": 1}],
            "types": [{"name": "T", "token": 1, "fields": [{"name": "f", "token": 2}],
                       "methods": [{"name": "m", "token": 1}]}],
        }
        new = {
            "apis": [{"index": 0, "api": "a", "obfuscated": "y", "rva": 2}],
            "types": [{"name": "T", "token": 2, "fields": [{"name": "g", "token": 3}],
                       "methods": [{"name": "m", "token": 1}]}],
        }
        doc = gamediff.diff_api(old, new)
        self.assertEqual(doc["exports"]["added"], ["y"])
        self.assertEqual(doc["exports"]["removed"], ["x"])
        self.assertEqual(doc["schema_changes"]["fields"]["added"], ["g"])
        self.assertEqual(doc["schema_changes"]["fields"]["removed"], ["f"])
        self.assertEqual(gamediff.summary(doc)["types"], 1)


if __name__ == "__main__":
    unittest.main()
