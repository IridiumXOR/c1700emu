#!/usr/bin/env python3
"""
Inspect a Cisco C1700 IOS image the way the C1700 boot ROM does, and find
out which CPU the code is really for.

    python3 -I tools/ciscoimg.py c1700-k9o3sy7-mz.123-11.T2.bin [--extract OUT]

Checks (see docs/09-ios-images.md):
  1. ELF header + ROMMON's own acceptance rules (image_header_check, 0xFFF18878),
     including e_machine == 0x33, the C1700 *platform code* that `file`
     misreads as "Stanford MIPS-X".
  2. The Cisco trailer at the start of the last section (0xFEEDFACE, sizes,
     checksums) and both checksums (image_checksum_bad, 0xFFF1DC90).
  3. The payload: a PKZIP archive -> decompressed in memory, its size and
     checksum compared with the trailer.
  4. Code at the entry point disassembled as PowerPC and as MIPS (llvm-objdump).
  5. Tell-tale strings in the (decompressed) image: CPU and platform names.
  6. Accesses to MPC8xx-only special registers (IMMR, the 8xx MMU, EIE/EID/NRI,
     IC_CST/DC_CST): proves "PowerQUICC (MPC8xx)", not just "some PowerPC".
"""

import argparse
import io
import struct
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

PLATFORM_C1700 = 0x33
OK, BAD = "OK ", "!! "


def parse_elf(d: bytes) -> dict:
    if d[:4] != b"\x7fELF":
        raise SystemExit("not an ELF file")
    (e_type, e_machine, e_version, e_entry, e_phoff, e_shoff, e_flags,
     e_ehsize, e_phentsize, e_phnum, e_shentsize, e_shnum, e_shstrndx) = struct.unpack(">HHIIIIIHHHHHH", d[16:52])
    phdrs = [struct.unpack(">IIIIIIII", d[e_phoff + 32 * i:e_phoff + 32 * i + 32]) for i in range(e_phnum)]
    shdrs = [struct.unpack(">IIIIIIIIII", d[e_shoff + 40 * i:e_shoff + 40 * i + 40]) for i in range(e_shnum)]
    return dict(cls=d[4], data=d[5], type=e_type, machine=e_machine, entry=e_entry, phoff=e_phoff,
                shoff=e_shoff, ehsize=e_ehsize, phentsize=e_phentsize, phnum=e_phnum,
                shentsize=e_shentsize, shnum=e_shnum, phdrs=phdrs, shdrs=shdrs)


def cisco_checksum(data: bytes) -> int:
    """32-bit sum of big-endian words with end-around carry (as the ROM computes it)."""
    total = 0
    for (w,) in struct.iter_unpack(">I", data[: len(data) // 4 * 4]):
        total += w
        if total > 0xFFFFFFFF:
            total = (total & 0xFFFFFFFF) + 1
    return total


def report(ok: bool, text: str):
    print(f"   {OK if ok else BAD}{text}")
    return ok


def check_rommon_rules(d: bytes, e: dict) -> bool:
    print("1. ELF header vs. the C1700 ROMMON rules (image_header_check)")
    good = True
    good &= report(e["cls"] == 1 and e["data"] == 2, "32-bit, big-endian")
    good &= report(e["type"] == 2, f"e_type = {e['type']} (2 = executable)")
    good &= report(e["machine"] == PLATFORM_C1700,
                   f"e_machine = {e['machine']:#x} (ROM wants the platform code 0x33; `file` prints 'Stanford MIPS-X' for 51)")
    good &= report((e["ehsize"], e["phentsize"], e["shentsize"]) == (52, 32, 40),
                   f"header sizes {e['ehsize']}/{e['phentsize']}/{e['shentsize']} (52/32/40)")
    good &= report(e["phnum"] == 1, f"{e['phnum']} program header(s) (1)")
    if e["shoff"] == 84:
        expected = 84 + 40 * e["shnum"] + sum(s[5] for s in e["shdrs"])
        good &= report(expected == len(d),
                       f"Cisco layout: e_shoff = 84, size rule 84 + 40*{e['shnum']} + sum(sh_size) = {expected} vs file {len(d)}")
    else:
        report(True, f"e_shoff = {e['shoff']} (not 84: ROM skips the size rule and trusts the received length)")
    return good


def check_trailer(d: bytes, e: dict):
    print("2. Cisco trailer at the start of the last section (image_checksum_bad)")
    last = e["shdrs"][-1]
    off = last[4]
    magic, usize, csize, ccks, ucks = struct.unpack(">IIIII", d[off:off + 20])
    if magic != 0xFEEDFACE:
        report(False, f"no 0xFEEDFACE trailer (last section starts with {d[off:off+8].hex()}): "
                      "not a ROMMON-bootable image as is; with TFTP_CHECKSUM=1 the ROM would read garbage sizes")
        return None
    report(True, f"magic 0xFEEDFACE, uncompressed {usize} bytes, compressed {csize} bytes")
    payload = d[off + 20:off + 20 + csize]
    got = cisco_checksum(payload)
    report(got == ccks, f"compressed checksum {got:#010x} (trailer says {ccks:#010x})")
    return dict(payload=payload, usize=usize, ucks=ucks)


def unpack_payload(t: dict):
    print("3. Payload")
    p = t["payload"]
    if p[:4] != b"PK\x03\x04":
        report(False, f"unknown payload format {p[:4].hex()}")
        return None
    z = zipfile.ZipFile(io.BytesIO(p))
    infos = z.infolist()
    for i in infos:
        report(True, f"PKZIP member {i.filename!r}: {i.compress_size} -> {i.file_size} bytes, method {i.compress_type} (8 = deflate)")
    data = z.read(infos[0])
    report(len(data) == t["usize"], f"decompressed size {len(data)} (trailer says {t['usize']})")
    got = cisco_checksum(data)
    report(got == t["ucks"], f"uncompressed checksum {got:#010x} (trailer says {t['ucks']:#010x})")
    return data


def disassemble(code: bytes, triple: str, n: int = 12) -> list:
    """Disassemble raw bytes with llvm-mc (one line per 4-byte word)."""
    lines = []
    for i in range(0, min(len(code), 4 * n), 4):
        word = code[i:i + 4]
        r = subprocess.run(["llvm-mc", "--disassemble", f"-triple={triple}"],
                           input=" ".join(f"0x{b:02x}" for b in word), capture_output=True, text=True)
        asm = [l.strip() for l in r.stdout.splitlines() if l.strip() and not l.strip().startswith(".text")]
        lines.append(f"{word.hex()}  {asm[0] if asm else '<invalid instruction>'}")
    return lines


def show_entry_code(d: bytes, e: dict, label: str):
    print(f"4. Code at the entry point {e['entry']:#x} ({label})")
    # find the section containing the entry point
    for s in e["shdrs"]:
        addr, off, size = s[3], s[4], s[5]
        if s[1] == 1 and addr <= e["entry"] < addr + size:
            code = d[off + e["entry"] - addr: off + e["entry"] - addr + 48]
            for triple in ("powerpc", "mips"):
                print(f"   as {triple.upper()}:")
                for l in disassemble(code, triple):
                    print("     ", l)
            return code
    print("   entry point not inside a PROGBITS section")
    return None


def show_strings(data: bytes, what: str):
    print(f"5. Tell-tale strings in {what}")
    needles = [b"MPC860", b"MPC8", b"PowerPC", b"PowerQUICC", b"IOS (tm) C1700", b"Cisco IOS Software, C1700",
               b"cisco 17", b"C1700 Software", b"MIPS"]
    for n in needles:
        i = data.find(n)
        if i < 0:
            print(f"   -- {n.decode()!r}: not found")
            continue
        s = data.rfind(b"\0", max(0, i - 120), i) + 1
        end = data.find(b"\0", i, i + 160)
        text = data[s:end if end > 0 else i + 80].decode("latin-1").replace("\r", " ").replace("\n", " ")
        print(f"   {data.count(n):5d}x {n.decode()!r}: ...{text[:110]}")


SPR_8XX = {80: "EIE", 81: "EID", 82: "NRI", 560: "IC_CST", 568: "DC_CST", 638: "IMMR",
           784: "MI_CTR", 787: "MI_EPN", 789: "MI_TWC", 790: "MI_RPN", 792: "MD_CTR", 793: "M_CASID",
           795: "MD_EPN", 796: "M_TWB", 797: "MD_TWC", 798: "MD_RPN", 799: "M_TW"}


def count_8xx_sprs(data: bytes, elf_text=None):
    """Scan code for mtspr/mfspr of SPRs that only exist on the MPC8xx core."""
    print("6. MPC8xx-only special registers used by the code (mtspr/mfspr)")
    start, end = elf_text if elf_text else (0, len(data))
    counts = {}
    for (w,) in struct.iter_unpack(">I", data[start:end - (end - start) % 4]):
        if w >> 26 == 31 and (w >> 1) & 0x3FF in (339, 467):  # mfspr / mtspr
            spr = ((w >> 16) & 0x1F) | (((w >> 11) & 0x1F) << 5)
            if spr in SPR_8XX:
                counts[SPR_8XX[spr]] = counts.get(SPR_8XX[spr], 0) + 1
    if counts:
        print("   " + ", ".join(f"{k} x{v}" for k, v in sorted(counts.items(), key=lambda kv: -kv[1])))
    else:
        print("   none found")


def text_range(d: bytes):
    """File range of the first executable section, if the file is an ELF."""
    if d[:4] != b"\x7fELF":
        return None
    e = parse_elf(d)
    for s in e["shdrs"]:
        if s[1] == 1 and s[2] & 4:  # PROGBITS + EXECINSTR
            return (s[4], s[4] + s[5])
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("image")
    ap.add_argument("--extract", help="write the decompressed payload to this file")
    a = ap.parse_args()
    d = Path(a.image).read_bytes()
    e = parse_elf(d)
    print(f"== {a.image}: {len(d)} bytes, {e['shnum']} sections, entry {e['entry']:#x}")
    check_rommon_rules(d, e)
    t = check_trailer(d, e)
    data = unpack_payload(t) if t else None
    show_entry_code(d, e, "loader stub" if t else "IOS itself")
    show_strings(data if data is not None else d, "the decompressed IOS" if data is not None else "the file")
    body = data if data is not None else d
    count_8xx_sprs(body, text_range(body))
    if data is not None and a.extract:
        Path(a.extract).write_bytes(data)
        print(f"decompressed payload written to {a.extract}")


if __name__ == "__main__":
    sys.exit(main())
