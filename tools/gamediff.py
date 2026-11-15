#!/usr/bin/env python3
"""Diff API maps (schema 2) and full metadata dumps produced by tools/dump.py."""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def members(items, name_key="name"):
    return {item[name_key]: item for item in items}


def diff_named(old_items, new_items, name_key="name", id_key="index"):
    old, new = members(old_items, name_key), members(new_items, name_key)
    added = sorted(set(new) - set(old))
    removed = sorted(set(old) - set(new))
    changed = []
    for name in sorted(set(old) & set(new)):
        a, b = old[name], new[name]
        if a != b:
            changed.append({"name": name, "old": a, "new": b})
    return {"added": added, "removed": removed, "changed": changed}


def diff_api(old, new):
    a = old["apis"]
    b = new["apis"]
    changes = diff_named(a, b, name_key="index")
    changes["schema_changes"] = {
        "metadata": diff_named(old.get("types", []), new.get("types", [])),
        "fields": diff_named([f for t in old.get("types", []) for f in t["fields"]],
                            [f for t in new.get("types", []) for f in t["fields"]]),
        "methods": diff_named([m for t in old.get("types", []) for m in t["methods"]],
                             [m for t in new.get("types", []) for m in t["methods"]]),
    }
    # API entries still use `index` as their stable row identity; for obfuscated
    # exports also report explicit renamed / moved / identity-changed buckets.
    old_named = {r["obfuscated"]: r for r in a if r.get("obfuscated")}
    new_named = {r["obfuscated"]: r for r in b if r.get("obfuscated")}
    changes["exports"] = {"added": sorted(set(new_named) - set(old_named)),
                          "removed": sorted(set(old_named) - set(new_named))}
    return changes


def summary(doc):
    return {
        "metadata": summary_part(doc, "schema_changes", "metadata"),
        "fields": summary_part(doc, "schema_changes", "fields"),
        "methods": summary_part(doc, "schema_changes", "methods"),
        "exports": summary_part(doc, None, "exports"),
        "types": len(doc["schema_changes"]["metadata"]["changed"]),
    }


def summary_part(doc, outer, inner):
    return {k: len(doc[outer][inner][k]) for k in ("added", "removed", "changed")} if outer \
        else {k: len(doc[inner][k]) for k in ("added", "removed")}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("old", type=Path)
    ap.add_argument("new", type=Path)
    ap.add_argument("--json", type=Path)
    ap.add_argument("--limit", type=int, default=12)
    args = ap.parse_args()
    old = json.loads(args.old.read_text())
    new = json.loads(args.new.read_text())
    doc = diff_api(old, new)
    doc["from"] = str(args.old)
    doc["to"] = str(args.new)
    doc["summary"] = summary(doc)
    text = json.dumps(doc, indent=2, sort_keys=True) + "\n"
    if args.json:
        args.json.write_text(text)
        print(f"wrote {args.json}")
    else:
        print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
