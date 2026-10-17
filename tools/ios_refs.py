"""Find a string in a decompressed IOS image and the code that loads its
address with `lis rX,hi` + `addi/ori rY,rX,lo` (PowerPC has no single
instruction for a 32-bit constant, and IDA/llvm-mc don't pair them).

    python3 tools/ios_refs.py IMAGE "text"
    python3 tools/ios_refs.py IMAGE 0x8355D498     (references to an address)
"""
import re, struct, sys


def segment(data):
    """(file offset, virtual address, size) of the first loadable ELF segment."""
    phoff, = struct.unpack(">I", data[28:32])
    _, off, vaddr, _, filesz, _, _, _ = struct.unpack(">8I", data[phoff:phoff + 32])
    return off, vaddr, filesz


def refs(body, base, target):
    words = struct.unpack(f">{len(body) // 4}I", body[:len(body) // 4 * 4])
    for i, w in enumerate(words):
        if (w >> 26) != 15 or ((w >> 16) & 0x1F) != 0:   # lis rD, hi
            continue
        rd, hi = (w >> 21) & 0x1F, w & 0xFFFF
        for k in range(1, 12):
            if i + k >= len(words):
                break
            w2 = words[i + k]
            op, ra, lo = w2 >> 26, (w2 >> 16) & 0x1F, w2 & 0xFFFF
            if op in (14, 24) and ra == rd:              # addi / ori
                val = (hi << 16) | lo if op == 24 else ((hi << 16) + (lo - 0x10000 if lo & 0x8000 else lo)) & 0xFFFFFFFF
                if val == target:
                    yield base + 4 * (i + k)
                break


data = open(sys.argv[1], "rb").read()
off, base, size = segment(data)
body = data[off:off + size]
if sys.argv[2].startswith("0x"):
    for r in refs(body, base, int(sys.argv[2], 16)):
        print(f"referenced at {r:#010x}")
    sys.exit()
for m in re.finditer(re.escape(sys.argv[2].encode()), body):
    start = m.start()
    while start > 0 and body[start - 1] != 0:
        start -= 1
    text = body[start:body.index(b"\0", start)].decode("latin-1")
    addr = base + start
    print(f"{addr:#010x} {text!r}")
    for r in refs(body, base, addr):
        print(f"    referenced at {r:#010x}")
