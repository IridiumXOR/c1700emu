"""Drive an emulated IOS boot unattended: boot (optionally via tftpdnld),
answer "no" to the setup dialog, run IOS commands, quit cleanly so the
emulator prints --stats.

    python3 tools/ios_session.py LOG IMAGE|- [-c CMDFILE] -- c1700emu-command...

IMAGE "-" means the image boots from flash (nothing to type in ROMMON);
"@" means the machine was restored from a snapshot already at the IOS
prompt (pass --restore); otherwise the image is fetched with `tftpdnld -r`
(the emulator needs --tftp-root).
CMDFILE has one IOS command per line (default: show version/interfaces/
diag/flash:). Directives: "wait TEXT" waits up to 60 s for TEXT,
"sleep N" lets N seconds pass, "profile-reset" sends Ctrl-] p (restart
the --profile counters), "snapshot" sends Ctrl-] s and waits until the
snapshot is saved (--snapshot FILE), "core" sends Ctrl-] d (ELF core dump,
--core FILE), "host CMD" runs CMD on the host and logs its output (e.g. a
ping to the router over a TAP device).
LOG gets the console; LOG.stderr the emulator's stderr (stats).
Environment: LIMIT = overall deadline in seconds (default 1500); SCALE =
factor for every individual wait (default 1; slow modes such as
--cpu jit-check need e.g. SCALE=10 LIMIT=20000).
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

last_cmd = 0  # where the output of the last command starts in buf

def wait_for(*needles, limit=600, since=None):
    end = min(time.time() + limit * float(os.environ.get("SCALE", "1")), deadline)
    start = len(buf) if since is None else since
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

def wait_stderr(*needles, limit=120):
    """Wait for a message the emulator prints on stderr (snapshot, core dump)."""
    end = time.time() + limit
    while time.time() < end:
        pump()
        text = open(log_path + ".stderr", errors="replace").read()
        for n in needles:
            if n in text[wait_stderr.seen:]:
                wait_stderr.seen = text.index(n, wait_stderr.seen) + len(n)
                return n
        time.sleep(0.1)
    return None
wait_stderr.seen = 0

if image not in ("-", "@"):
    wait_for("rommon 1 >", limit=60); mark("ROMMON prompt")
    for l in ["IP_ADDRESS=10.0.2.15", "IP_SUBNET_MASK=255.255.255.0", "DEFAULT_GATEWAY=10.0.2.1",
              "TFTP_SERVER=10.0.2.2", f"TFTP_FILE={image}", "tftpdnld -r"]:
        send(l + "\r"); time.sleep(0.4)
if image == "@":
    send("\r"); r = wait_for("Router>", "Router#", limit=60); mark(f"restored, prompt: {r!r}")
else:
    r = wait_for("[yes/no]:", "*** ", limit=1400); mark(f"setup dialog: {r!r}")
if r == "[yes/no]:":
    send("no\r"); wait_for("Press RETURN", limit=300)
    for _ in range(3):
        send("\r"); r = wait_for("Router>", limit=60)
        if r:
            break
    mark(f"prompt: {r!r}")
if r in ("Router>", "Router#"):
    for c in cmds:
        if c.startswith("wait "):
            # also look at what the previous command already printed
            mark(f"waiting for {c[5:]!r}: {wait_for(c[5:], limit=60, since=last_cmd)!r}")
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
        if c.startswith("host "):
            r = subprocess.run(c[5:], shell=True, capture_output=True, text=True, timeout=120)
            log.write(f"\n[host] $ {c[5:]}\n{r.stdout}{r.stderr}")
            mark(f"host command exit {r.returncode}")
            continue
        if c in ("snapshot", "core"):
            send("\x1ds" if c == "snapshot" else "\x1dd")
            mark(f"{c}: {wait_stderr('snapshot saved', 'core dump (', 'failed')!r}")
            continue
        last_cmd = len(buf)
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
