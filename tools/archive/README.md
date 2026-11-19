# tools/archive — retired host scripts (reference only)

Nothing here is wired into a live workflow. Kept because `AGENTS.md` §17 and
`docs/archive/IL2CPP-DUMP.md` narrate the sessions that used them.

| File | What it was | What replaced it |
|---|---|---|
| `test-loadlib.sh` | Manual stand: isolated Proton prefix, `inject --mode loadlib` + PEB unlink check, run by hand from `AGENTS.md` history. | Nothing automated — the loadlib path is exercised ad hoc against the `test-target` process; the script is kept for a manual re-check. |

Also retired in the same cleanup (deleted outright, not archived — superseded and
not worth restoring):

| Deleted | Reason |
|---|---|
| `tools/gct_table.py` | Replaced by `tools/buildgen.py` (`GcAnchors::threads`). |
| `tools/resolve_api.py` | Replaced by `tools/api_map.py` + `buildgen.py` + `export_transfer.py`. |
| `tools/hromatite-console.sh` | Replaced by the Rust `hromatite-console` TUI (`tools/hromatite-tui.sh`). |
| `tools/test-seal.sh` | The L2 `--seal` mode was removed from the runtime. |
| `tools/meta_parse.py`, `tools/dump.py`, `tools/il2cpp_methods.py`, `tools/il2cppdump.sh`, `tools/test_dumps.py`, `tools/test_il2cpp_methods.py`, `tools/test_meta_parse.py`, `tools/gamediff.py` | The archived IL2CPP dump pipeline (`docs/archive/IL2CPP-DUMP.md`); `dumpmeta` no longer exists in the payload, `gamediff.py` only diffed `dump.py` output. |
