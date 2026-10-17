"""Find where a guest is stuck: run the emulator under its GDB stub, wait
for a console marker plus DELAY seconds, interrupt, dump registers and the
code around the PC with gdb, then quit the emulator (so --stats print).

    python3 tools/gdb_sample.py OUT MARKER DELAY PORT c1700emu-command...

OUT gets the console, OUT.gdb the gdb dump, OUT.stderr the emulator stderr.
"""
import subprocess, sys, time, os, signal
out, marker, delay, port = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4]
emu_cmd = sys.argv[5:] + ["--gdb", port]
log = open(out, "w", buffering=1)
emu = subprocess.Popen(emu_cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(out + ".stderr", "w"))
os.set_blocking(emu.stdout.fileno(), False)
time.sleep(1)
gdbcmds = ["set pagination off", "set endian big", "set architecture powerpc:common",
           f"target remote :{port}", "continue",
           "info registers", "x/24i $pc-48", "x/16xw $r1", "p/x $lr", "x/8i $lr-16", "stepi 2000", "p/x $pc",
           "detach"]
gdb = subprocess.Popen(["gdb", "-nx", "-batch"] + sum((["-ex", c] for c in gdbcmds), []),
                       stdout=open(out + ".gdb", "w"), stderr=subprocess.STDOUT)
buf = b""; t0 = time.time(); seen = None
while True:
    try: d = emu.stdout.read()
    except BlockingIOError: d = None
    if d:
        buf += d; log.write(d.decode("latin-1").replace("\r", ""))
    if seen is None and marker.encode() in buf:
        seen = time.time(); log.write(f"\n[driver {seen-t0:.1f}s] marker seen\n")
    if seen and time.time() - seen > delay:
        break
    if emu.poll() is not None: break
    time.sleep(0.1)
gdb.send_signal(signal.SIGINT)
gdb.wait(120)
time.sleep(1)
emu.stdin.write(b"\x1dq"); emu.stdin.flush()
try: emu.wait(60)
except Exception: emu.kill()
try:
    d = emu.stdout.read()
    if d: log.write(d.decode("latin-1").replace("\r", ""))
except Exception: pass
log.write(f"\n[driver] done, emu exit {emu.returncode}\n")
