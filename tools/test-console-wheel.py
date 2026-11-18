#!/usr/bin/env python3
"""Drive hromatite-console in a real PTY and check mouse-wheel scrolling.

Feeds SGR mouse reports (wheel up/down) and asserts what the rendered footer
says, so the check covers the escape decoder, the model clamp and the renderer.
"""
import fcntl
import os
import pty
import re
import select
import shutil
import struct
import sys
import tempfile
import termios
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else (
    "crates/hromatite-console/target/x86_64-unknown-linux-gnu/release/hromatite-console"
)
COLS, ROWS = 110, 24

WHEEL_UP = b"\x1b[<64;10;10M"
WHEEL_DOWN = b"\x1b[<65;10;10M"

workdir = tempfile.mkdtemp(prefix="wheel-pty-")
log_path = os.path.join(workdir, "hromatite.log")
prefix = os.path.join(workdir, "pfx")
os.makedirs(prefix, exist_ok=True)
with open(log_path, "w", encoding="utf-8") as handle:
    for index in range(200):
        handle.write(f"[{index}] [late] hits={index} hz=90.0 tid=0x1c4\n")

pid, fd = pty.fork()
if pid == 0:
    os.environ["HROMATITE_PREFIX"] = prefix
    os.execv(BIN, [BIN, "--log", log_path])


def resize(rows, cols=COLS):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    os.kill(pid, 28)  # SIGWINCH


resize(ROWS)


def drain(seconds=0.8):
    out = b""
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
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
    status = "ok" if condition else "FAIL"
    print(f"[{status}] {label}{(' — ' + detail) if detail and not condition else ''}")
    if not condition:
        failures.append(label)


try:
    boot = drain(1.2)
    check("mouse reporting enabled", b"\x1b[?1000h" in boot and b"\x1b[?1006h" in boot)
    check("alternate screen enabled", b"\x1b[?1049h" in boot)
    frame = last_frame(boot)
    check("starts following the tail", "FOLLOW" in frame and "back 0" in frame, frame[-300:])

    os.write(fd, WHEEL_UP * 2)
    frame = last_frame(drain(0.8))
    check("wheel up pauses and scrolls 6 rows", "PAUSED" in frame and "back 6" in frame, frame[-300:])

    os.write(fd, WHEEL_UP * 100)
    frame = last_frame(drain(1.0))
    match = re.search(r"back (\d+)", frame)
    top = int(match.group(1)) if match else -1
    check("wheel up clamps at the top of the scrollback", top == 200 - 17, f"back {top}")

    os.write(fd, WHEEL_DOWN * (top // 3 + 5))
    frame = last_frame(drain(1.0))
    check("wheel down returns to the tail", "FOLLOW" in frame and "back 0" in frame, frame[-300:])

    # The field manual must scroll too (needs a body shorter than 12 rows).
    os.write(fd, b"help\r")
    frame = last_frame(drain(0.8))
    check("field manual opens locally", "FIELD MANUAL" in frame, frame[-300:])
    resize(12)
    frame = last_frame(drain(0.8))
    check("manual is clipped on a short terminal", "cmd " not in frame, frame[-300:])
    os.write(fd, WHEEL_UP * 10)
    frame = last_frame(drain(0.8))
    check("wheel scrolls the manual", "cmd " in frame and "log " in frame, frame[-300:])
    os.write(fd, b"\x1b")
    frame = last_frame(drain(0.8))
    check("Esc closes the manual", "FIELD MANUAL" not in frame, frame[-300:])
    resize(ROWS)
    drain(0.4)

    # Palette: the wheel walks the generated command list.
    os.write(fd, b":")
    frame = last_frame(drain(0.8))
    check("palette opens with the generated table", "pos" in frame and "SAFE" in frame, frame[-400:])
    os.write(fd, b"eac")
    frame = last_frame(drain(0.8))
    check("palette filters to feature-only commands", "eac.status" in frame, frame[-500:])
    os.write(fd, b"\x1b")
    drain(0.4)
finally:
    os.write(fd, b"\x03")
    time.sleep(0.3)
    try:
        drained = drain(0.4)
        check("mouse reporting released", b"\x1b[?1000l" in drained and b"\x1b[?1006l" in drained)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    os.close(fd)
    shutil.rmtree(workdir, ignore_errors=True)

print()
if failures:
    print(f"FAILED: {len(failures)} check(s): {failures}")
    sys.exit(1)
print("all wheel checks passed")
