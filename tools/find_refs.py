"""Find code that builds a given address with `lis rX,hi` + `addi/ori/lwz/... lo(rX)`.

    python3 tools/find_refs.py roms/CISCO_bootrom_AT27C040.bin 0xFFF2A641 [...]

IDA doesn't create cross-references for pointers built at run time this way,
so this is how strings (and therefore the code printing them) were located.
"""
import struct
import sys

BASE = 0xFFF00000
LO_OPS = {14: "addi", 24: "ori", 32: "lwz", 34: "lbz", 40: "lhz", 36: "stw", 38: "stb", 44: "sth"}


def refs(rom, target):
    words = struct.unpack(f">{len(rom) // 4}I", rom)
    for i, w in enumerate(words):
        if (w >> 26) != 15 or ((w >> 16) & 0x1F) != 0:  # lis rD, imm == addis rD, 0, imm
            continue
        rd, hi = (w >> 21) & 0x1F, w & 0xFFFF
        for k in range(1, 12):
            if i + k >= len(words):
                break
            w2 = words[i + k]
            op, ra, lo = w2 >> 26, (w2 >> 16) & 0x1F, w2 & 0xFFFF
            if op in LO_OPS and ra == rd:
                val = (hi << 16) | lo if op == 24 else ((hi << 16) + (lo - 0x10000 if lo & 0x8000 else lo)) & 0xFFFFFFFF
                if val == target:
                    yield BASE + 4 * (i + k), LO_OPS[op]
                break


if __name__ == "__main__":
    rom = open(sys.argv[1], "rb").read()
    for t in sys.argv[2:]:
        t = int(t, 0)
        print(f"{t:#010x}:", ", ".join(f"{a:#010x} ({op})" for a, op in refs(rom, t)) or "no references")
