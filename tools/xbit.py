"""Parse a Xilinx Virtex/Spartan-II configuration stream (type 1/2 packets)
stored inside a decompressed IOS image, e.g. the FPGA bitstream IOS loads
on a 1760 through IO FPGA register 0x4C (docs chapter 11.6).

    python3 tools/xbit.py IMAGE ADDRESS     (hex address of the stream)
"""
import sys, struct


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
REG = {0:'CRC',1:'FAR',2:'FDRI',3:'FDRO',4:'CMD',5:'CTL',6:'MASK',7:'STAT',8:'LOUT',9:'COR',10:'MFWR',11:'FLR'}
CMD = {1:'WCFG',3:'LFRM',4:'RCFG',5:'START',6:'RCAP',7:'RCRC',8:'AGHIGH',9:'SWITCH',0xD:'DESYNCH'}
d = open(sys.argv[1], 'rb').read()
addr = int(sys.argv[2], 16); o = elf_offset(d, addr)
words = lambda i: struct.unpack('>I', d[o+4*i:o+4*i+4])[0]
i = 0
while words(i) != 0xAA995566: i += 1
print(f"sync word at +{4*i:#x}")
i += 1; last_reg = None; n = 0
while n < 200:
    w = words(i); typ = w >> 29
    if typ == 1:
        op = (w >> 27) & 3; reg = (w >> 13) & 0x3FFF; cnt = w & 0x7FF
        last_reg = reg
    elif typ == 2:
        op = (w >> 27) & 3; reg = last_reg; cnt = w & 0x7FFFFFF
    else:
        print(f"+{4*i:#07x}: {w:08x} (filler)"); i += 1; n += 1
        if w == 0 and words(i) == 0 and words(i+1) == 0: break
        continue
    vals = [words(i+1+k) for k in range(min(cnt, 2))]
    desc = REG.get(reg, reg)
    extra = f" = {CMD.get(vals[0], hex(vals[0]))}" if reg == 4 and cnt == 1 else (f" = {vals[0]:#x}" if cnt == 1 else f" ({cnt} words)")
    print(f"+{4*i:#07x}: type{typ} {'write' if op==2 else 'op'+str(op)} {desc}{extra}")
    i += 1 + cnt; n += 1
    if reg == 4 and cnt == 1 and vals[0] == 0xD:
        print(f"end of stream after DESYNCH at +{4*i:#x} bytes"); break
