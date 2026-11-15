#!/usr/bin/env python3
"""Export a decrypted IL2CPP metadata dump as stable build-comparison JSON.

Optional method RVA map (produced in-process) has the form:
  {"method_rvas": {"<global metadata method index>": <int or hex string>}}
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys

from meta_parse import Meta, MetadataError


def int_value(v):
    return int(v, 0) if isinstance(v, str) else int(v)


def dump(meta_path: Path, output: Path, rva_map_path: Path | None):
    m = Meta.read(meta_path)
    rvas = {}
    if rva_map_path:
        raw = json.loads(rva_map_path.read_text())
        rvas = {int(k): int_value(v) for k, v in raw.get("method_rvas", raw).items() if int_value(k) >= 0}
    types = []
    for t in m.types():
        image = m.image_for(t)
        methods = []
        for method in m.methods_of(t):
            methods.append({
                "index": method["index"], "name": m.string(method["name_index"]),
                "token": method["token"], "flags": method["flags"],
                "parameter_count": method["parameter_count"],
                **({"rva": rvas[method["index"]]} if method["index"] in rvas else {}),
        })
        types.append({
            "index": t["index"], "name": m.type_name(t), "token": t["token"],
            "byval_type_index": t["byval_type_index"], "image": m.string(image["name_index"]),
            "flags": t["flags"], "bitfield": t["bitfield"],
            "methods": methods,
            "fields": [{**f, "name": m.string(f["name_index"]), "type": f"type#{f['type_index']}"}
                       for f in m.fields_of(t)],
            "properties": [{**p, "name": m.string(p["name_index"])} for p in m.properties_of(t)],
        })
    doc = {
        **m.summary(), "metadata_sha256": hashlib.sha256(m.data).hexdigest(),
        "rva_map": str(rva_map_path) if rva_map_path else None,
        "types": types,
    }
    with output.open("w", encoding="utf-8") as f:
        json.dump(doc, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"{len(types)} types, {len(m.methods())} methods, {len(rvas)} RVAs -> {output}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("metadata", type=Path)
    ap.add_argument("output", type=Path)
    ap.add_argument("--rva-map", type=Path)
    args = ap.parse_args()
    try:
        dump(args.metadata, args.output, args.rva_map)
    except (OSError, ValueError, MetadataError) as exc:
        print(f"dump: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
