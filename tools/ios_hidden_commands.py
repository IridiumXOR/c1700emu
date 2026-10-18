#!/usr/bin/env python3
"""Find IOS commands that only `service internal` reveals (docs ch. 25).

Restores a snapshot of IOS at the `Router#` prompt (with `terminal length
0`), types `?` in several modes (user and privileged exec, `show`,
`debug`, `test`, `clear`, global, interface and line configuration),
switches `service internal` on, types the same again, and reports the
keywords that only appear the second time, with the help text IOS gives
for them. For every hidden `show` keyword it also lists `show KEYWORD ?`.

    tools/ios_hidden_commands.py SNAPSHOT OUTDIR [-- extra c1700emu options]

OUTDIR gets the raw listings (`<context>.<plain|internal>.txt`) and
`hidden.md`, a Markdown table per context. The emulator is started with
`--restore SNAPSHOT --model 1760 --ram 128 --cpu jit` (add more after --).
"""
import os, re, subprocess, sys, time

args = sys.argv[1:]
snapshot, outdir = args[0], args[1]
extra = args[args.index("--") + 1:] if "--" in args else []
root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
emu = os.environ.get("C1700EMU", os.path.join(root, "emu/target/release/c1700emu"))
os.makedirs(outdir, exist_ok=True)
work = os.path.join(outdir, "run")
os.makedirs(work, exist_ok=True)
cmd = [emu, "--nvram", f"{work}/nv", "--flash", f"{work}/fl", "--model", "1760", "--ram", "128", "--cpu", "jit",
       "--restore", os.path.abspath(snapshot), "--log", f"{work}/emu.log"] + extra
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(f"{work}/stderr", "w"))
os.set_blocking(p.stdout.fileno(), False)
console = open(f"{work}/console.log", "w", buffering=1)
buf = b""
PROMPT = re.compile(rb"Router[\w\-()]*[#>][^\r\n]*$")


def pump():
    global buf
    try:
        d = p.stdout.read()
    except BlockingIOError:
        d = None
    if d:
        buf += d
        console.write(d.decode("latin-1").replace("\r", ""))
    return bool(d)


def settle(since, limit=30.0, quiet=0.6):
    """Wait until output since `since` ends with a prompt and stays quiet."""
    end, last = time.time() + limit, time.time()
    while time.time() < end:
        if pump():
            last = time.time()
        elif time.time() - last > quiet and PROMPT.search(buf[since:].rstrip(b" ") + b" "):
            return buf[since:].decode("latin-1").replace("\r", "")
        time.sleep(0.05)
    return buf[since:].decode("latin-1").replace("\r", "")


def send(s):
    p.stdin.write(s.encode("latin-1"))
    p.stdin.flush()


def line(text):
    since = len(buf)
    send(text + "\r")
    return settle(since)


def help_listing(prefix):
    """Type `prefix ?`, return IOS's help lines, then erase the input line."""
    since = len(buf)
    send((prefix + " " if prefix else "") + "?")
    out = settle(since)
    send("\x15")  # Ctrl-U: delete the partial line IOS put back
    settle(len(buf), limit=2, quiet=0.3)
    return out


ENTRY = re.compile(r"^  ([A-Za-z0-9][\w\-.:/<>|]*)\s+(.*\S)?\s*$")


def keywords(listing):
    found = {}
    for l in listing.splitlines():
        m = ENTRY.match(l)
        if m:
            found[m.group(1)] = (m.group(2) or "").strip()
    return found


# context name -> (commands to enter the mode, prefix typed before "?", commands to leave)
CONTEXTS = [
    ("user-exec", ["disable"], "", ["enable"]),
    ("exec", [], "", []),
    ("show", [], "show", []),
    ("debug", [], "debug", []),
    ("test", [], "test", []),
    ("clear", [], "clear", []),
    ("config", ["configure terminal"], "", ["end"]),
    ("interface", ["configure terminal", "interface FastEthernet0/0"], "", ["end"]),
    ("line", ["configure terminal", "line console 0"], "", ["end"]),
]


def collect(tag):
    result = {}
    for name, enter, prefix, leave in CONTEXTS:
        for c in enter:
            line(c)
        text = help_listing(prefix)
        open(os.path.join(outdir, f"{name}.{tag}.txt"), "w").write(text)
        result[name] = keywords(text)
        for c in leave:
            line(c)
    return result


settle(0, limit=60)
send("\r")
settle(0, limit=30)
line("terminal length 0")
plain = collect("plain")
line("configure terminal")
line("service internal")
line("end")
internal = collect("internal")

md = ["# IOS commands revealed by `service internal`", ""]
total = 0
for name, *_ in CONTEXTS:
    hidden = {k: v for k, v in internal[name].items() if k not in plain[name]}
    total += len(hidden)
    md += [f"## {name} ({len(plain[name])} visible, {len(hidden)} more with service internal)", ""]
    if hidden:
        md += ["| keyword | help |", "|---|---|"] + [f"| `{k}` | {v} |" for k, v in sorted(hidden.items())]
    md.append("")
    if name == "show":
        for k in sorted(hidden):
            text = help_listing(f"show {k}")
            open(os.path.join(outdir, f"show-{k}.txt"), "w").write(text)
open(os.path.join(outdir, "hidden.md"), "w").write("\n".join(md))
print(f"{total} hidden keywords; see {outdir}/hidden.md")
send("\x1dq")
try:
    p.wait(30)
except subprocess.TimeoutExpired:
    p.kill()
