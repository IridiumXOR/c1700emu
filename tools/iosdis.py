"""Disassemble part of a decompressed IOS image (an ELF as produced by
`ciscoimg.py --extract`) as MPC860 code.

    python3 tools/iosdis.py IMAGE START END        (hex addresses)
"""
import subprocess, sys


def elf_offset(data, addr):
    """File offset of virtual address `addr` in a (Cisco) big-endian ELF image."""
    import struct
    phoff, = struct.unpack(">I", data[28:32])
    phentsize, phnum = struct.unpack(">HH", data[42:46])
    for i in range(phnum):
        _, off, vaddr, _, filesz, _, _, _ = struct.unpack(">8I", data[phoff + i * phentsize:][:32])
        if vaddr <= addr < vaddr + filesz:
            return addr - vaddr + off
    raise SystemExit(f"{addr:#x} is not in any loadable segment")
img, start, end = sys.argv[1], int(sys.argv[2], 16), int(sys.argv[3], 16)
data = open(img, 'rb').read()
off = elf_offset(data, start)
words = data[off:off + (end - start)]
hexs = " ".join(f"0x{b:02x}" for b in words)
r = subprocess.run(["llvm-mc", "--disassemble", "-triple=powerpc-unknown-unknown", "-mcpu=860"],
                   input=hexs, capture_output=True, text=True)
lines = [l.strip() for l in r.stdout.splitlines() if l.strip() and not l.strip().startswith('.')]
for i, l in enumerate(lines):
    print(f"{start + 4*i:08x}: {l}")
