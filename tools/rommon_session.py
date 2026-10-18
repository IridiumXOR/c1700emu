#!/usr/bin/env python3
"""Drive ROMMON unattended (docs ch. 25): boot the emulator to the ROMMON
prompt, type a list of commands, record what each one prints.

    python3 tools/rommon_session.py LOG [-n] [-c CMDFILE | -e CMD ...] -- c1700emu-command...

The emulator command must stay in ROMMON (e.g. --confreg 0x2100). CMDFILE
has one command per line (# comments); `-e CMD` adds commands on the
command line. `-n`: don't wait for a prompt at power-on (to act during
an automatic boot, e.g. `sleep 3` then `break`). After each command the driver waits for the next prompt
(`rommon N >`, anything ending in "> ", or an IOS `Router#`, so it also
drives IOS restored from a snapshot) or for a question ("Password:",
"[y/n]", "?"), so an answer can follow as the next line. Directives:

    sleep N      let N seconds pass
    break        send a BREAK (Ctrl-] b)
    enter        press RETURN alone (accept a question's default)
    raw TEXT     type TEXT without a carriage return
    timeout N    allow N seconds per command from now on (default 10)

LOG gets the whole console, each command framed by `>>> CMD` lines.
"""
import os, re, subprocess, sys, time

args = sys.argv[1:]
log_path = args.pop(0)
cmds = []
wait_boot = True
while args and args[0] != "--":
    a = args.pop(0)
    if a == "-n":
        wait_boot = False
    elif a == "-c":
        cmds += [l.rstrip("\n") for l in open(args.pop(0)) if l.strip() and not l.startswith("#")]
    elif a == "-e":
        cmds.append(args.pop(0))
    else:
        sys.exit(f"unknown argument {a!r}")
emu = args[1:]

log = open(log_path, "w", buffering=1)
p = subprocess.Popen(emu, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(log_path + ".stderr", "w"))
os.set_blocking(p.stdout.fileno(), False)
buf = b""
PROMPT = re.compile(rb"(rommon \d+ > ?|> |\w# ?)$")
QUESTION = re.compile(rb"(Password: |[:?\]]\s*)$")


def pump():
    global buf
    try:
        d = p.stdout.read()
    except BlockingIOError:
        d = None
    if d:
        buf += d
        log.write(d.decode("latin-1").replace("\r", ""))


def wait(limit, since):
    """Until the output since `since` ends with a prompt or a question."""
    end = time.time() + limit
    while time.time() < end:
        pump()
        tail = buf[since:].rstrip(b"\x00")
        if PROMPT.search(tail) or (len(tail) > 0 and QUESTION.search(tail)):
            time.sleep(0.15)  # anything more coming?
            pump()
            tail = buf[since:]
            if PROMPT.search(tail) or QUESTION.search(tail):
                return True
        if p.poll() is not None:
            return False
        time.sleep(0.05)
    return False


def send(s):
    p.stdin.write(s.encode("latin-1"))
    p.stdin.flush()


limit = 10.0
if wait_boot:
    ok = wait(120, 0)
    log.write(f"\n>>> [boot] {'prompt' if ok else 'NO PROMPT'}\n")
for c in cmds:
    if c.startswith("sleep "):
        end = time.time() + float(c[6:])
        while time.time() < end:
            pump()
            time.sleep(0.1)
        continue
    if c.startswith("timeout "):
        limit = float(c[8:])
        continue
    log.write(f"\n>>> {c}\n")
    since = len(buf)
    if c == "break":
        send("\x1db")
    elif c == "enter":
        send("\r")
    elif c.startswith("raw "):
        send(c[4:])
    else:
        send(c + "\r")
    if not wait(limit, since):
        log.write(f"\n>>> [no prompt after {limit:.0f} s]\n")
log.write("\n>>> [quit]\n")
send("\x1dq")
try:
    p.wait(20)
except subprocess.TimeoutExpired:
    p.kill()
