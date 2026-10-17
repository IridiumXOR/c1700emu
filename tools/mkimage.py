#!/usr/bin/env python3
"""
Build a tiny Cisco-C1700-style boot image: an ELF that ROMMON accepts with
`tftpdnld` / `boot`, containing a few PowerPC instructions that print a
message on the console UART.

    python3 tools/mkimage.py hello.bin "Hello from a TFTP-booted image!"

What ROMMON checks (image header check at 0xFFF18878, see docs/08-network.md):

    e_ident  "\\x7fELF", 32-bit, big-endian
    e_type   = 2 (executable)
    e_machine= 0x33   <- the *platform code*, = the cookie's processor byte
    e_ehsize = 52, e_phentsize = 32, e_phnum = 1, e_shentsize = 40
    e_shoff  = 84     <- section headers right after the single program header;
                         then file size must be 84 + 40*e_shnum + sum(sh_size)

and, when checksum verification is on (the default), the LAST section must
start with the classic Cisco self-decompressing-image trailer (0xFFF1DC90):

    +0  0xFEEDFACE
    +4  uncompressed size      +8  compressed size (bytes checksummed)
    +12 compressed checksum    +16 uncompressed checksum
    +20 payload ...

checksum = 32-bit sum of big-endian words with end-around carry.
Here the "payload" is simply our (uncompressed) code, loaded by the single
PT_LOAD program header.

The payload is assembled with llvm-mc (position independent: it finds its
message with `bl`/`mflr`, so no linker is needed).
"""

import struct
import subprocess
import sys
import tempfile
from pathlib import Path

LOAD_ADDR = 0x80008000  # where C1700 IOS images are linked (RAM alias at 0x80000000)
PLATFORM_C1700 = 0x33

ASM = r"""
    .text
_start:
    lis     3, 0x6805           # r3 = 0x68050000, the 16550 console UART
    bl      1f                  # LR = address of label 1 = our own position
1:  mflr    4
    addi    4, 4, msg - 1b      # r4 = &msg (position independent)
2:  lbz     5, 0(4)             # next character
    cmpwi   5, 0
    beq     4f
3:  lbz     6, 5(3)             # LSR
    andi.   6, 6, 0x20          # THRE: transmitter empty?
    beq     3b
    stb     5, 0(3)             # THR = character
    addi    4, 4, 1
    b       2b
4:  b       4b                  # done: spin (Ctrl-] q to quit the emulator)
msg:
    .asciz  "\r\n\r\n*** MESSAGE ***\r\n"
    .balign 4
"""


def assemble(message: str) -> bytes:
    src = ASM.replace("*** MESSAGE ***", message.replace('"', "'"))
    with tempfile.TemporaryDirectory() as d:
        s, o, b = Path(d, "p.s"), Path(d, "p.o"), Path(d, "p.bin")
        s.write_text(src)
        subprocess.run(["llvm-mc", "-triple=powerpc-unknown-unknown", "-filetype=obj", "-o", str(o), str(s)], check=True)
        subprocess.run(["llvm-objcopy", "-O", "binary", "--only-section=.text", str(o), str(b)], check=True)
        return b.read_bytes()


def cisco_checksum(data: bytes) -> int:
    """32-bit sum with end-around carry, over big-endian words."""
    total = 0
    for (w,) in struct.iter_unpack(">I", data):
        total += w
        if total > 0xFFFFFFFF:
            total = (total & 0xFFFFFFFF) + 1
    return total


def cisco_elf(code: bytes) -> bytes:
    """Lay the image out like a real IOS image: a code section loaded at
    LOAD_ADDR, and a LAST section that starts with the checksummed trailer.

    ROMMON's loader copies every *section* to its sh_addr and jumps to
    e_entry (it ignores the program header, which we still provide so that
    standard ELF tools are happy)."""
    ehsize, phentsize, shentsize = 52, 32, 40
    shnum = 2
    shoff = ehsize + phentsize  # 84, the layout the ROM expects
    code = code + bytes(-len(code) % 16)
    payload = b"c1700emu test image".ljust(32, b"\0")  # stands in for compressed IOS
    cks = cisco_checksum(payload)
    last = struct.pack(">IIIII", 0xFEEDFACE, len(payload), len(payload), cks, cks) + payload

    code_off = shoff + shentsize * shnum
    last_off = code_off + len(code)
    last_addr = LOAD_ADDR + len(code)

    ident = b"\x7fELF" + bytes([1, 2, 1]) + bytes(9)  # ELFCLASS32, ELFDATA2MSB, EV_CURRENT
    ehdr = ident + struct.pack(
        ">HHIIIIIHHHHHH",
        2,  # e_type: ET_EXEC
        PLATFORM_C1700,  # e_machine: Cisco platform code, not EM_PPC!
        1,  # e_version
        LOAD_ADDR,  # e_entry
        ehsize,  # e_phoff
        shoff,  # e_shoff
        0,  # e_flags
        ehsize, phentsize, 1,  # e_ehsize, e_phentsize, e_phnum
        shentsize, shnum, 0,  # e_shentsize, e_shnum, e_shstrndx
    )
    phdr = struct.pack(">IIIIIIII", 1, code_off, LOAD_ADDR, LOAD_ADDR, len(code), len(code), 7, 4)  # PT_LOAD RWX
    sh_code = struct.pack(">IIIIIIIIII", 0, 1, 6, LOAD_ADDR, code_off, len(code), 0, 0, 4, 0)  # PROGBITS AX
    sh_last = struct.pack(">IIIIIIIIII", 0, 1, 2, last_addr, last_off, len(last), 0, 0, 4, 0)  # PROGBITS A
    image = ehdr + phdr + sh_code + sh_last + code + last
    assert len(image) == 84 + 40 * shnum + len(code) + len(last)  # the ROM's size rule
    return image


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    msg = sys.argv[2] if len(sys.argv) > 2 else "Hello from a TFTP-booted image!"
    image = cisco_elf(assemble(msg))
    out.write_bytes(image)
    print(f"{out}: {len(image)} bytes, entry {LOAD_ADDR:#x}, e_machine {PLATFORM_C1700:#x}")


if __name__ == "__main__":
    main()
