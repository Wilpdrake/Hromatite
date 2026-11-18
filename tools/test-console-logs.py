#!/usr/bin/env python3
"""Drive hromatite-console in a real PTY and check switching between logs.

Builds a fake Proton prefix whose files sit exactly where the real ones do
(payload log, Unity player log, EAC launcher log, EAC hooks log, GC log, wrapper
capture, launcher log), then drives F9/F10 and the local `log` command.
"""
import fcntl
import os
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else (
    "crates/hromatite-console/target/x86_64-unknown-linux-gnu/release/hromatite-console"
)
COLS, ROWS = 120, 24

F9 = b"\x1b[20~"
F10 = b"\x1b[21~"

root = tempfile.mkdtemp(prefix="logs-pty-")
home = os.path.join(root, "home")
prefix = os.path.join(root, "steamapps/compatdata/438100/pfx")
user = os.path.join(prefix, "drive_c/users/steamuser")


def write(path, lines):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        for line in lines:
            handle.write(line + "\n")


write(os.path.join(user, "hromatite.log"), [
    "[1] [dll] DllMain(PROCESS_ATTACH), inline",
    "[2] [late] hits=42 hz=90.0 tid=0x1c4",
    "[3] [con] pos: get_position = (1.000, 2.000, 3.000)  [raw 0x1 0x2]",
])
write(os.path.join(user, "AppData/LocalLow/VRChat/VRChat/output_log_2026-11-18.txt"), [
    "2026-11-18 07:08:11  Log - [Behaviour] OnPlayerJoined Nyx",
    "2026-11-18 07:08:12  Log - [Behaviour] Entering Room: Pty Apartment",
])
write(os.path.join(user, "AppData/Roaming/EasyAntiCheat/aaaa/bbbb/anticheatlauncher.log"), [
    "[EasyAntiCheat] System name: 'linux64'",
    "[AntiCheatClient] Session Begin: Success",
])
write(os.path.join(user, "hromatite_eac_hooks.log"), [
    "[eac] out frame len=512 dt=2007",
])
write(os.path.join(root, "steamapps/common/VRChat/VRChat.gc.log"), [
    "GC #58 World-stopped marking took 5 msecs",
])
write(os.path.join(home, ".hromatite-logs/game-20261118-070803.log"), [
    "wine: wrapper captured stdout",
])
write(os.path.join(home, ".hromatite-launch.log"), [
    "[hromatite] game wine pid 903380 after 8s",
])

env = {
    "HOME": home,
    "PATH": "/run/current-system/sw/bin:/usr/bin:/bin",
    "HROMATITE_PREFIX": prefix,
    "TERM": "xterm-256color",
}

pid, fd = pty.fork()
if pid == 0:
    os.execve(BIN, [BIN, "--prefix", prefix], env)


def resize(rows, cols=COLS):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.kill(pid, 28)  # SIGWINCH


def drain(seconds=0.8):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
    return out


def last_frame(raw):
    text = raw.decode("utf-8", "replace")
    frames = text.split("\x1b[H\x1b[0m")
    frame = frames[-1] if len(frames) > 1 else text
    return re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", frame)


failures = []


def check(label, condition, detail=""):
    print(f"[{'ok' if condition else 'FAIL'}] {label}"
          + (f" — {detail}" if detail and not condition else ""))
    if not condition:
        failures.append(label)


resize(ROWS)
try:
    frame = last_frame(drain(1.4))
    check("tab strip lists every discovered log",
          "logs 1/7" in frame and "payload" in frame and "game" in frame
          and "eac" in frame and "hooks" in frame and "gc" in frame
          and "stdout" in frame and "launch" in frame, frame[:400])
    check("payload is the active view", "[payload]" in frame and "DllMain" in frame,
          frame[:600])

    os.write(fd, F9)
    frame = last_frame(drain(0.8))
    check("F9 switches to the Unity player log",
          "[game]" in frame and "Entering Room: Pty Apartment" in frame, frame[:800])
    check("the strip renumbers the active view", "logs 2/7" in frame, frame[:200])

    os.write(fd, F9)
    frame = last_frame(drain(0.8))
    check("F9 again reaches the EAC log",
          "[eac]" in frame and "Session Begin: Success" in frame, frame[:800])

    os.write(fd, b"log gc\r")
    frame = last_frame(drain(0.8))
    check("`log gc` jumps by name",
          "[gc]" in frame and "World-stopped marking" in frame, frame[:800])
    check("`log` is host-side and never leaves a tx echo",
          "> log gc" not in frame, frame[:800])

    os.write(fd, F10)
    frame = last_frame(drain(0.8))
    check("F10 steps back to the EAC log", "[eac]" in frame, frame[:400])

    os.write(fd, b"logs\r")
    frame = last_frame(drain(0.8))
    check("`logs` prints the numbered list", "logs: 1)payload" in frame, frame[-500:])

    # While another log is on screen, the command echo lands in the payload view.
    os.write(fd, b"pos\r")
    frame = last_frame(drain(1.0))
    check("a sent command marks the payload view as unread",
          "payload+" in frame, frame[:400])
    os.write(fd, b"log payload\r")
    frame = last_frame(drain(0.8))
    check("switching back shows the echo and clears the counter",
          "> pos" in frame and "payload+" not in frame, frame[:800])
finally:
    os.write(fd, b"\x03")
    time.sleep(0.3)
    try:
        drain(0.4)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    os.close(fd)

# Headless proof that --view picks the starting log without a terminal.
snapshot = subprocess.run(
    [BIN, "--prefix", prefix, "--view", "eac", "--snapshot", "--size", "120x18"],
    env=env, capture_output=True, text=True, check=False)
text = snapshot.stdout
check("--view eac starts on the EAC log",
      snapshot.returncode == 0 and "[eac]" in text and "Session Begin" in text,
      snapshot.stderr or text[:400])
check("the snapshot is escape-free and exactly 18 rows",
      "\x1b" not in text and len(text.splitlines()) == 18,
      f"{len(text.splitlines())} rows")
unknown = subprocess.run(
    [BIN, "--prefix", prefix, "--view", "nope", "--snapshot", "--size", "120x18"],
    env=env, capture_output=True, text=True, check=False)
check("an unknown --view is reported instead of silently ignored",
      "no such log view: nope" in unknown.stdout, unknown.stdout[-400:])

shutil.rmtree(root, ignore_errors=True)

print()
if failures:
    print(f"FAILED: {len(failures)} check(s): {failures}")
    sys.exit(1)
print("all log-switching checks passed")
