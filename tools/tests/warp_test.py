#!/usr/bin/env python3
"""TODO item 7 / docs ch. 21: does IOS keep answering when idle time is
skipped without a wall-clock limit (--idle warp)?

Restores a snapshot of IOS at the prompt, connects to the console over
telnet, reads the router's clock, stays quiet for IDLE seconds while
virtual time races ahead, then presses RETURN (the console was logged out
by the exec-timeout meanwhile) and checks that it answers and how far the
clock moved.

    tools/tests/warp_test.py SNAPSHOT [IDLE_SECONDS] [-- extra c1700emu options]

Exit status 0 if IOS answers after the quiet period.
"""
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

args = sys.argv[1:]
extra = []
if "--" in args:
    extra = args[args.index("--") + 1:]
    args = args[:args.index("--")]
snapshot = os.path.abspath(args[0])
idle = float(args[1]) if len(args) > 1 else 60
port = 23000 + os.getpid() % 1000
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
work = tempfile.mkdtemp(prefix="warp-")
cmd = [os.path.join(root, "emu/target/release/c1700emu"), "--nvram", f"{work}/nv", "--flash", f"{work}/fl",
       "--model", "1760", "--ram", "128", "--restore", snapshot, "--idle", os.environ.get("IDLE_MODE", "warp"),
       "--tcp", str(port), "--log", f"{work}/emu.log", "--log-level", "info", "--stats"] + extra
# stdin stays a pipe: Ctrl-] q quits cleanly at the end, so --stats is printed
emu = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=open(f"{work}/stderr", "w"))


def connect():
    for _ in range(100):
        try:
            s = socket.create_connection(("127.0.0.1", port))
            s.settimeout(0.2)
            return s
        except OSError:
            time.sleep(0.1)
    raise SystemExit("console port never opened")


def drain(s, t):
    out, end = b"", time.time() + t
    while time.time() < end:
        try:
            d = s.recv(4096)
            if not d:
                break
            out += d
        except socket.timeout:
            pass
    return out.replace(b"\xff\xfb\x01\xff\xfb\x03", b"").decode(errors="replace")


raw = []  # everything the console printed after the quiet period


def clock(s):
    s.sendall(b"show clock\r")
    out = drain(s, 3)
    raw.append(out)
    found = re.findall(r"(\d\d):(\d\d):(\d\d)\.\d+ UTC \w+ (\w+) +(\d+)", out)
    if not found:
        return None
    h, m, sec, _, day = found[-1]
    return int(day) * 86400 + int(h) * 3600 + int(m) * 60 + int(sec)


try:
    s = connect()
    drain(s, 0.5)
    before = clock(s)
    print(f"before: {before} s virtual", flush=True)
    quiet = drain(s, idle)
    # After 10 virtual minutes IOS logs the console out (exec-timeout) and
    # waits for a RETURN on its own before anything else (docs ch. 21).
    s.sendall(b"\r")
    drain(s, 2)
    after = [clock(s) for _ in range(3)]
    if os.environ.get("SHOW_CONSOLE"):
        print("console during the quiet period:", repr(quiet[-400:]))
        for r in raw:
            print("console after a probe:", repr(r[-300:]))
    print(f"after {idle:.0f} s quiet: {after}", flush=True)
finally:
    try:
        emu.stdin.write(b"\x1dq")
        emu.stdin.flush()
        emu.wait(30)
    except Exception:
        emu.kill()
stats = open(f"{work}/stderr", errors="replace").read()
for line in stats.splitlines():
    if line.startswith("idle:") or "JIT:" in line:
        print(line[:160])
answered = before is not None and all(a is not None for a in after)
if answered:
    print(f"virtual time moved {after[0] - before} s in {idle:.0f} s: x{(after[0] - before) / idle:.0f}; console OK")
else:
    print("console did NOT answer after the quiet period")
sys.exit(0 if answered else 1)
