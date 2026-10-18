#!/usr/bin/env python3
"""TODO item 7 / docs ch. 21: after IOS logs the console out ("Press RETURN
to get started", after `exit` or the exec-timeout), pressing RETURN must
give a new prompt.

Restores a snapshot of IOS at the prompt, logs the console out with `exit`,
waits for "Press RETURN", presses RETURN and expects `Router>`.

    tools/tests/exec_timeout_test.py SNAPSHOT [-- extra c1700emu options]

Exit status 0 if the console comes back.
"""
import os
import socket
import subprocess
import sys
import tempfile
import time

args = sys.argv[1:]
extra = args[args.index("--") + 1:] if "--" in args else []
snapshot = os.path.abspath(args[0])
port = 24000 + os.getpid() % 1000
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
work = tempfile.mkdtemp(prefix="exectimeout-")
cmd = [os.path.join(root, "emu/target/release/c1700emu"), "--nvram", f"{work}/nv", "--flash", f"{work}/fl",
       "--model", "1760", "--ram", "128", "--restore", snapshot, "--tcp", str(port),
       "--log", f"{work}/emu.log", "--log-level", "info", "--stats"] + extra
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


def drain(s, t, until=None):
    out, end = b"", time.time() + t
    while time.time() < end:
        try:
            d = s.recv(4096)
            if not d:
                break
            out += d
            if until and until.encode() in out:
                break
        except socket.timeout:
            pass
    return out.replace(b"\xff\xfb\x01\xff\xfb\x03", b"").decode(errors="replace")


ok = False
try:
    s = connect()
    drain(s, 0.5)
    s.sendall(b"\r")
    drain(s, 1.0)
    s.sendall(b"exit\r")
    logout = drain(s, 60, until="Press RETURN")
    print("logged out:", "Press RETURN" in logout, flush=True)
    pause = float(os.environ.get("PAUSE", "0"))  # seconds between logout and RETURN
    if pause:
        drain(s, pause)
    probe = os.environ.get("PROBE", "").encode() + b"\r"  # what is typed at "Press RETURN"
    for attempt in range(3):
        s.sendall(probe if attempt == 0 else b"\r")
        back = drain(s, 5, until="Router>")
        print(f"RETURN #{attempt + 1}:", repr(back[-80:]), flush=True)
        if "Router>" in back:
            ok = True
            break
finally:
    try:
        emu.stdin.write(b"\x1dq")
        emu.stdin.flush()
        emu.wait(30)
    except Exception:
        emu.kill()
print("console came back after the logout" if ok else "console did NOT come back after the logout")
sys.exit(0 if ok else 1)
