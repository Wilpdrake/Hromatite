#!/usr/bin/env python3
"""Offline mirror of hromatite-core::metadata (IL2CPP metadata v29–31).

No packages outside the standard library. Input must be a decrypted dump, hence
the on-disk encrypted global-metadata.dat is rejected by its missing magic.
"""
from __future__ import annotations

import argparse
from functools import lru_cache
import json
from pathlib import Path
import struct
from typing import Any

MAGIC = 0xFAB11BAF
HEADER_SIZE = 64 * 4
TYPE_SIZE = 88
IMAGE_SIZE = 32
ASSEMBLY_SIZE = 80
FIELD_SIZE = 12
PROPERTY_SIZE = 20
MAX_SIZE = 128 * 1024 * 1024
SECTION_INDICES = list(range(2, 64, 2))
PRE31_RECORD_SIZES = {6: -1, 40: TYPE_SIZE, 42: IMAGE_SIZE, 44: ASSEMBLY_SIZE}


class MetadataError(ValueError):
    pass


def i32(data: bytes, off: int) -> int:
    if off < 0 or off + 4 > len(data):
        raise MetadataError(f"i32 outside buffer at {off:#x}")
    return struct.unpack_from("<i", data, off)[0]


def u16(data: bytes, off: int) -> int:
    if off < 0 or off + 2 > len(data):
        raise MetadataError(f"u16 outside buffer at {off:#x}")
    return struct.unpack_from("<H", data, off)[0]


def pair(data: bytes, index: int) -> tuple[int, int]:
    return i32(data, index * 4) & 0xFFFFFFFF, i32(data, (index + 1) * 4)


class Meta:
    def __init__(self, data: bytes):
        self.data = data
        self.version = i32(data, 4) if len(data) >= HEADER_SIZE else -1
        if len(data) < HEADER_SIZE or i32(data, 0) & 0xFFFFFFFF != MAGIC:
            raise MetadataError("missing decrypted metadata magic AF 1B B1 FA")
        if self.version not in (29, 30, 31):
            raise MetadataError(f"unsupported metadata version {self.version}")
        for index in SECTION_INDICES:
            off, size = pair(data, index)
            if size < 0 or (off == 0 and size != 0) or (size and off % 4):
                raise MetadataError(f"invalid section {index}: {off:#x}/{size:#x}")
        headers = {index: pair(data, index) for index in SECTION_INDICES}
        self.sections = headers
        self.strings = headers[6]
        self.method_section = headers[12]
        self.field_section = headers[24]
        self.property_section = headers[10]
        self.type_section = headers[40]
        self.image_section = headers[42]
        self.assembly_section = headers[44]
        self.block_size = max([HEADER_SIZE] + [off + size for off, size in headers.values()])
        if self.block_size > MAX_SIZE or self.block_size > len(data):
            raise MetadataError(f"invalid metadata span {self.block_size:#x}")
        if not self.strings[1] or not self.type_section[1] or not self.image_section[1] or not self.assembly_section[1]:
            raise MetadataError("missing required metadata tables")
        for section, stride in [
            (self.type_section, TYPE_SIZE), (self.image_section, IMAGE_SIZE),
            (self.assembly_section, ASSEMBLY_SIZE), (self.field_section, FIELD_SIZE),
            (self.property_section, PROPERTY_SIZE), (self.method_section, self.method_size),
        ]:
            self._table(section, stride, "header")
        self.data = data[:self.block_size]

    @classmethod
    def read(cls, path: str | Path) -> "Meta":
        return cls(Path(path).read_bytes())

    @property
    def method_size(self) -> int:
        return 36 if self.version >= 31 else 32

    def _table(self, section: tuple[int, int], stride: int, what: str) -> tuple[int, int]:
        off, size = section
        if size == 0:
            return 0, 0
        if size < 0 or size % stride or off < HEADER_SIZE or off + size > self.block_size:
            raise MetadataError(f"{what}: invalid table {off:#x}+{size:#x}")
        return off, size // stride

    def string(self, index: int) -> str:
        start, size = self.strings
        if index < 0 or index >= size:
            raise MetadataError(f"string {index:#x} outside blob")
        off = start + index
        end = self.data.find(b"\0", off, start + size)
        if end < 0:
            raise MetadataError(f"unterminated string {index:#x}")
        try:
            return self.data[off:end].decode("utf-8")
        except UnicodeDecodeError as exc:
            raise MetadataError(f"invalid UTF-8 at string {index:#x}") from exc

    @lru_cache(maxsize=None)
    def types(self) -> list[dict[str, Any]]:
        start, count = self._table(self.type_section, TYPE_SIZE, "types")
        out = []
        for index in range(count):
            o = start + index * TYPE_SIZE
            out.append({
                "index": index, "name_index": i32(self.data, o) & 0xFFFFFFFF,
                "namespace_index": i32(self.data, o + 4) & 0xFFFFFFFF,
                "byval_type_index": i32(self.data, o + 8),
                "declaring_type_index": i32(self.data, o + 12),
                "parent_index": i32(self.data, o + 16),
                "element_type_index": i32(self.data, o + 20),
                "generic_container_index": i32(self.data, o + 24),
                "flags": i32(self.data, o + 28) & 0xFFFFFFFF,
                "field_start": i32(self.data, o + 32), "method_start": i32(self.data, o + 36),
                "property_start": i32(self.data, o + 44),
                "method_count": u16(self.data, o + 64), "property_count": u16(self.data, o + 66),
                "field_count": u16(self.data, o + 68), "bitfield": i32(self.data, o + 80) & 0xFFFFFFFF,
                "token": i32(self.data, o + 84) & 0xFFFFFFFF,
            })
        return out

    @lru_cache(maxsize=None)
    def methods(self) -> list[dict[str, Any]]:
        start, count = self._table(self.method_section, self.method_size, "methods")
        v31 = self.version >= 31
        out = []
        for index in range(count):
            o = start + index * self.method_size
            out.append({
                "index": index, "name_index": i32(self.data, o) & 0xFFFFFFFF,
                "declaring_type": i32(self.data, o + 4), "return_type": i32(self.data, o + 8),
                "return_parameter_token": (i32(self.data, o + 12) & 0xFFFFFFFF) if v31 else None,
                "parameter_start": i32(self.data, o + (16 if v31 else 12)),
                "generic_container_index": i32(self.data, o + (20 if v31 else 16)),
                "token": i32(self.data, o + (24 if v31 else 20)) & 0xFFFFFFFF,
                "flags": u16(self.data, o + (28 if v31 else 24)),
                "iflags": u16(self.data, o + (30 if v31 else 26)),
                "slot": u16(self.data, o + (32 if v31 else 28)),
                "parameter_count": u16(self.data, o + (34 if v31 else 30)),
            })
        return out

    @lru_cache(maxsize=None)
    def fields(self) -> list[dict[str, Any]]:
        start, count = self._table(self.field_section, FIELD_SIZE, "fields")
        return [{
            "index": index, "name_index": i32(self.data, start + index * FIELD_SIZE) & 0xFFFFFFFF,
            "type_index": i32(self.data, start + index * FIELD_SIZE + 4),
            "token": i32(self.data, start + index * FIELD_SIZE + 8) & 0xFFFFFFFF,
        } for index in range(count)]

    @lru_cache(maxsize=None)
    def properties(self) -> list[dict[str, Any]]:
        start, count = self._table(self.property_section, PROPERTY_SIZE, "properties")
        return [{
            "index": index, "name_index": i32(self.data, start + index * PROPERTY_SIZE) & 0xFFFFFFFF,
            "get": i32(self.data, start + index * PROPERTY_SIZE + 4),
            "set": i32(self.data, start + index * PROPERTY_SIZE + 8),
            "attrs": i32(self.data, start + index * PROPERTY_SIZE + 12) & 0xFFFFFFFF,
            "token": i32(self.data, start + index * PROPERTY_SIZE + 16) & 0xFFFFFFFF,
        } for index in range(count)]

    @lru_cache(maxsize=None)
    def images(self) -> list[dict[str, Any]]:
        start, count = self._table(self.image_section, IMAGE_SIZE, "images")
        return [{
            "index": index, "name_index": i32(self.data, start + index * IMAGE_SIZE) & 0xFFFFFFFF,
            "assembly_index": i32(self.data, start + index * IMAGE_SIZE + 4),
            "type_start": i32(self.data, start + index * IMAGE_SIZE + 8),
            "type_count": i32(self.data, start + index * IMAGE_SIZE + 12) & 0xFFFFFFFF,
            "entry_point_index": i32(self.data, start + index * IMAGE_SIZE + 24),
            "token": i32(self.data, start + index * IMAGE_SIZE + 28) & 0xFFFFFFFF,
        } for index in range(count)]

    @lru_cache(maxsize=None)
    def assemblies(self) -> list[dict[str, Any]]:
        start, count = self._table(self.assembly_section, ASSEMBLY_SIZE, "assemblies")
        return [{
            "index": index, "image_index": i32(self.data, start + index * ASSEMBLY_SIZE),
            "token": i32(self.data, start + index * ASSEMBLY_SIZE + 4) & 0xFFFFFFFF,
            "referenced_start": i32(self.data, start + index * ASSEMBLY_SIZE + 8),
            "referenced_count": i32(self.data, start + index * ASSEMBLY_SIZE + 12),
            "name_index": i32(self.data, start + index * ASSEMBLY_SIZE + 16) & 0xFFFFFFFF,
            "major": i32(self.data, start + index * ASSEMBLY_SIZE + 40),
            "minor": i32(self.data, start + index * ASSEMBLY_SIZE + 44),
            "build": i32(self.data, start + index * ASSEMBLY_SIZE + 48),
            "revision": i32(self.data, start + index * ASSEMBLY_SIZE + 52),
        } for index in range(count)]

    def string_literals(self) -> list[dict[str, int]]:
        start, count = self._table(self.sections[2], 8, "string literals")
        return [{"length": i32(self.data, start + i * 8) & 0xFFFFFFFF,
                 "data_index": i32(self.data, start + i * 8 + 4)} for i in range(count)]

    def literal(self, literal: dict[str, int]) -> str:
        base, size = self.sections[4]
        start = base + literal["data_index"] if literal["data_index"] >= 0 else -1
        end = start + literal["length"]
        if start < base or end > base + size:
            raise MetadataError("literal range outside blob")
        try:
            return self.data[start:end].decode("utf-8")
        except UnicodeDecodeError as exc:
            raise MetadataError("literal is not UTF-8") from exc

    def type_name(self, t: dict[str, Any]) -> str:
        ns = self.string(t["namespace_index"])
        return f"{ns}.{self.string(t['name_index'])}" if ns else self.string(t["name_index"])

    def type(self, namespace: str, name: str) -> dict[str, Any] | None:
        return next((t for t in self.types() if self.type_name(t) == f"{namespace}.{name}".strip(".")), None)

    def image_for(self, t: dict[str, Any]) -> dict[str, Any]:
        image = next((i for i in self.images()
                      if 0 <= i["type_start"] <= t["index"] < i["type_start"] + i["type_count"]), None)
        if image is None:
            raise MetadataError(f"no image owns type {t['index']}")
        return image

    def _slice(self, records: list[dict[str, Any]], start: int, count: int, what: str) -> list[dict[str, Any]]:
        if count == 0:
            return []
        if start < 0 or start + count > len(records):
            raise MetadataError(f"{what}: invalid range {start}+{count}")
        return records[start:start + count]

    def methods_of(self, t: dict[str, Any]) -> list[dict[str, Any]]:
        return self._slice(self.methods(), t["method_start"], t["method_count"], "methods")

    def fields_of(self, t: dict[str, Any]) -> list[dict[str, Any]]:
        return self._slice(self.fields(), t["field_start"], t["field_count"], "fields")

    def properties_of(self, t: dict[str, Any]) -> list[dict[str, Any]]:
        return self._slice(self.properties(), t["property_start"], t["property_count"], "properties")

    def image_method_range(self, image: dict[str, Any]) -> range:
        ranges = [(t["method_start"], t["method_start"] + t["method_count"])
                  for t in self.types()
                  if image["type_start"] <= t["index"] < image["type_start"] + image["type_count"]
                  and t["method_count"]]
        if not ranges:
            return range(0)
        lo, hi = min(x[0] for x in ranges), max(x[1] for x in ranges)
        if sum(b - a for a, b in ranges) != hi - lo or lo < 0 or hi > len(self.methods()):
            raise MetadataError(f"image {image['index']}: non-contiguous method range")
        return range(lo, hi)

    def summary(self) -> dict[str, Any]:
        return {
            "schema": 1, "version": self.version, "block_size": self.block_size,
            "types": len(self.types()), "methods": len(self.methods()),
            "fields": len(self.fields()), "properties": len(self.properties()),
            "images": len(self.images()), "assemblies": len(self.assemblies()),
        }

    def type_dump(self, full_name: str) -> dict[str, Any]:
        ns, _, name = full_name.rpartition(".")
        t = self.type(ns, name)
        if t is None:
            raise MetadataError(f"type not found: {full_name}")
        image = self.image_for(t)
        method_range = self.image_method_range(image)
        return {
            **t, "full_name": self.type_name(t), "image": self.string(image["name_index"]),
            "methods": [{**m, "name": self.string(m["name_index"]),
                         "image_index": m["index"] - method_range.start} for m in self.methods_of(t)],
            "fields": [{**f, "name": self.string(f["name_index"]),
                        "type": f"type#{f['type_index']}"} for f in self.fields_of(t)],
            "properties": [{**p, "name": self.string(p["name_index"])} for p in self.properties_of(t)],
        }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("dump", type=Path)
    ap.add_argument("--type")
    ap.add_argument("--json", type=Path)
    args = ap.parse_args()
    try:
        m = Meta.read(args.dump)
        out = m.type_dump(args.type) if args.type else m.summary()
    except (OSError, MetadataError) as exc:
        ap.error(str(exc))
    text = json.dumps(out, indent=2, sort_keys=True) + "\n"
    if args.json:
        args.json.write_text(text)
        print(f"wrote {args.json}")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
