#!/usr/bin/env python3
"""Release-artifact regression test for the naked Boehm detour's Win64 ABI."""

from __future__ import annotations

import re
import shutil
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DLL = ROOT / "target/x86_64-pc-windows-gnu/release/hromatite.dll"


def llvm_tool(name: str) -> str:
    path = shutil.which(f"x86_64-w64-mingw32-{name}")
    if path is None:
        raise unittest.SkipTest(f"x86_64-w64-mingw32-{name} is unavailable")
    return path


def run(args: list[str]) -> str:
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE).stdout


class GcDetourAbiTest(unittest.TestCase):
    def test_live_jump_restores_original_rcx_and_rdx(self) -> None:
        nm = run([llvm_tool("nm"), str(DLL)])
        match = re.search(r"^([0-9a-f]+) T .*gc_context_detour$", nm, re.MULTILINE)
        self.assertIsNotNone(match, "gc_context_detour symbol missing from release DLL")
        address = int(match.group(1), 16)
        disassembly = run(
            [
                llvm_tool("objdump"),
                "-d",
                f"--start-address=0x{address:x}",
                f"--stop-address=0x{address + 0x30:x}",
                str(DLL),
            ]
        )
        self.assertIn("sub    $0x30,%rsp", disassembly)
        self.assertIn("mov    %rcx,0x20(%rsp)", disassembly)
        self.assertIn("mov    %rdx,0x28(%rsp)", disassembly)
        route_call = disassembly.index("call")
        first_save = disassembly.index("mov    %rcx,0x20(%rsp)")
        second_save = disassembly.index("mov    %rdx,0x28(%rsp)")
        self.assertLess(first_save, route_call)
        self.assertLess(second_save, route_call)
        first_restore = disassembly.index("mov    0x20(%rsp),%rcx", route_call)
        second_restore = disassembly.index("mov    0x28(%rsp),%rdx", route_call)
        final_jump = disassembly.index("jmp    *%r11", route_call)
        self.assertLess(route_call, first_restore)
        self.assertLess(first_restore, second_restore)
        self.assertLess(second_restore, final_jump)
        self.assertIn("add    $0x30,%rsp", disassembly)


if __name__ == "__main__":
    unittest.main()
