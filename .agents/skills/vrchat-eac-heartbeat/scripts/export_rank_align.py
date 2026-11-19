#!/usr/bin/env python3
"""Map obfuscated il2cpp-API export RVAs from an old GameAssembly build to a new one.

Beebyte re-randomises export *names* on every game patch, but the functions keep
their order and size inside the il2cpp-api block (0xc20000..0xc30000), so the
address-ordered sequences of export groups align one-to-one. The export names that
survive unrenamed (il2cpp_gchandle_get_target, il2cpp_native_stack_trace, ...) are
free anchors: they must map onto themselves or the alignment is a guess.

Usage:
  export_rank_align.py <old symbols.json> <new symbols.json> [name=0xRVA ...]
  export_rank_align.py old.json new.json wYRnnMQuvvH=0xc27f00 HToJgvthiQt=0xc28050

Input files are the `symbols-<sha12>.json` side outputs of tools/symmap.py. Keep the
pre-update json around, otherwise there is nothing to align against.
"""
import json
import sys

BLOCK = (0xC20000, 0xC30000)


def load(path):
    with open(path) as handle:
        return json.load(handle)["exports"]


def groups(exports):
    """Export groups by address inside the il2cpp-api block, in address order."""
    items = sorted((rva, name) for name, rva in exports.items() if BLOCK[0] <= rva < BLOCK[1])
    out = []
    for rva, name in items:
        if out and out[-1][0] == rva:
            out[-1][1].append(name)
        else:
            out.append([rva, [name]])
    return out


def similarity(old_groups, new_groups):
    old_gaps = [b[0] - a[0] for a, b in zip(old_groups, old_groups[1:])]
    new_gaps = [b[0] - a[0] for a, b in zip(new_groups, new_groups[1:])]
    same = sum(1 for a, b in zip(old_gaps, new_gaps) if a == b)
    return same, len(old_gaps)


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2
    old, new = load(argv[1]), load(argv[2])
    old_groups, new_groups = groups(old), groups(new)
    if len(old_groups) != len(new_groups):
        print(f"! group count differs: old {len(old_groups)} new {len(new_groups)}")
        return 1
    same, total = similarity(old_groups, new_groups)
    span_old = old_groups[-1][0] - old_groups[0][0]
    span_new = new_groups[-1][0] - new_groups[0][0]
    print(
        f"groups: {len(old_groups)}/{len(new_groups)}  identical gaps {same}/{total}  "
        f"span {span_old:#x}/{span_new:#x}"
    )

    mapping = {}
    for (old_rva, old_names), (new_rva, new_names) in zip(old_groups, new_groups):
        for name in old_names:
            mapping[name] = (new_rva, new_names)

    anchors = [
        n
        for n in (
            "il2cpp_gchandle_get_target",
            "il2cpp_native_stack_trace",
            "il2cpp_free",
            "Flush",
            "CloseZStream",
            "CreateZStream",
        )
        if n in mapping
    ]
    ok = 0
    for name in anchors:
        new_rva, new_names = mapping[name]
        good = name in new_names
        ok += good
        print(
            f"anchor {name}: old {old[name]:#x} -> {new_rva:#x} {new_names} "
            f"{'OK' if good else 'MISMATCH'}"
        )

    for spec in argv[3:]:
        name, _, raw = spec.partition("=")
        believed = int(raw, 16) if raw else old.get(name)
        if name not in old:
            print(f"? {name} is not an export of the old build")
            continue
        if believed is not None and believed != old[name]:
            print(f"! {name}: you said {believed:#x}, old build has {old[name]:#x}")
        new_rva, new_names = mapping[name]
        moved = "moved" if new_rva != old[name] else "unchanged"
        print(f"{name:28s} old {old[name]:#x} -> new {new_rva:#x} [{','.join(new_names)}] {moved}")

    print(f"anchors {ok}/{len(anchors)} self-consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
