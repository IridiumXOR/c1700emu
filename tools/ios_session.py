"""Drive an emulated IOS boot unattended: boot (optionally via tftpdnld),
answer "no" to the setup dialog, run IOS commands, quit cleanly so the
emulator prints --stats.

    python3 tools/ios_session.py LOG IMAGE|- [-c CMDFILE] -- c1700emu-command...

IMAGE "-" means the image boots from flash (nothing to type in ROMMON);
otherwise it is fetched with `tftpdnld -r` (the emulator needs --tftp-root).
CMDFILE has one IOS command per line (default: show version/interfaces/
diag/flash:). Directives: "wait TEXT" waits up to 60 s for TEXT,
"sleep N" lets N seconds pass, "profile-reset" sends Ctrl-] p (restart
the --profile counters).
LOG gets the console; LOG.stderr the emulator's stderr (stats).
Environment: LIMIT = overall deadline in seconds (default 1500).
"""
import os, subprocess, sys, time

args = sys.argv[1:]
log_path, image = args[0], args[1]
cmds = ["enable", "terminal length 0", "show version", "show interfaces", "show diag", "show flash:"]
rest = args[2:]
if rest and rest[0] == "-c":
    cmds = [l.rstrip("\n") for l in open(rest[1]) if l.strip() and not l.startswith("#")]
    rest = rest[2:]
if rest and rest[0] == "--":
    rest = rest[1:]

log = open(log_path, "w", buffering=1)
p = subprocess.Popen(rest, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(log_path + ".stderr", "w"))
os.set_blocking(p.stdout.fileno(), False)
t0 = time.time(); buf = b""; deadline = t0 + float(os.environ.get("LIMIT", "1500"))

def pump():
    global buf
    try:
        d = p.stdout.read()
    except BlockingIOError:
        d = None
    if d:
        buf += d
        log.write(d.decode("latin-1").replace("\r", ""))

def wait_for(*needles, limit=600):
    end = min(time.time() + limit, deadline)
    start = len(buf)
    while time.time() < end:
        pump()
        for n in needles:
            if n.encode() in buf[start:]:
                return n
        if p.poll() is not None:
            return None
        time.sleep(0.05)
    return None

def mark(msg): log.write(f"\n[driver {time.time() - t0:7.1f}s] {msg}\n")
def send(s): p.stdin.write(s.encode()); p.stdin.flush()

if image != "-":
    wait_for("rommon 1 >", limit=60); mark("ROMMON prompt")
    for l in ["IP_ADDRESS=10.0.2.15", "IP_SUBNET_MASK=255.255.255.0", "DEFAULT_GATEWAY=10.0.2.1",
              "TFTP_SERVER=10.0.2.2", f"TFTP_FILE={image}", "tftpdnld -r"]:
        send(l + "\r"); time.sleep(0.4)
r = wait_for("[yes/no]:", "*** ", limit=1400); mark(f"setup dialog: {r!r}")
if r == "[yes/no]:":
    send("no\r"); wait_for("Press RETURN", limit=300)
    for _ in range(3):
        send("\r"); r = wait_for("Router>", limit=60)
        if r:
            break
    mark(f"prompt: {r!r}")
    if r:
        for c in cmds:
            if c.startswith("wait "):
                mark(f"waiting for {c[5:]!r}: {wait_for(c[5:], limit=60)!r}")
                continue
            if c.startswith("sleep "):
                end = time.time() + float(c[6:])
                while time.time() < end:
                    pump(); time.sleep(0.2)
                mark(f"slept {c[6:]} s")
                continue
            if c == "profile-reset":
                send("\x1dp"); mark("profile counters reset (Ctrl-] p)")
                continue
            send(c + "\r")
            wait_for("#", ">", limit=180)
            time.sleep(0.2); pump()
            mark(f"done: {c}")
mark("quitting")
send("\x1dq")
try:
    p.wait(60)
except Exception:
    p.kill(); mark("killed")
mark(f"exit code {p.returncode}")
