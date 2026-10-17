"""
IDA setup script for the Cisco C1700 (MPC860T) System Bootstrap ROM.

Run inside IDA (File > Script file, or via the IDA MCP `execute_python`):

    exec(open("/path/to/cisco_rom/tools/ida/mpc860_setup.py").read())

It is idempotent: re-running it just re-applies names/comments.

What it does (and why) -- see docs/02-ida-setup.md for the long version:

 1. Forces the ROM segment to 32-bit (the MPC860 is a 32-bit PowerPC core).
 2. Creates an "IMMR" segment at 0xFF000000: the reset code writes
    0xFF000000 into SPR 638 (IMMR), which relocates the whole on-chip
    peripheral block (SIU, memory controller, timers, CPM, FEC...) there.
    Every register gets a name and a correctly-sized data item, so absolute
    accesses like  *(u32*)0xFF000014  decompile as  SIMASK.
 3. Builds a struct  IMMR_t  with the same layout and applies it as a
    "structure offset" to loads/stores whose base register was computed
    from  mfspr rX, IMMR  -- so  lwz r10, 0x288(r10)  shows  IMMR_t.RSR.
 4. Converts the exception vector table (0xFFF00100..0xFFF01F00) to code,
    names every trampoline (vec_*) and the real handler it jumps to (exc_*).
 5. Comments every mtspr/mfspr with the MPC8xx SPR name and meaning, because
    IDA's generic PPC profile prints them as bare numbers.
"""

import re
import idc
import idaapi
import ida_bytes
import ida_funcs
import ida_name
import ida_segment
import ida_ua
import ida_auto
import ida_typeinf

ROM_BASE = 0xFFF00000
IMMR_BASE = 0xFF000000
IMMR_SIZE = 0x10000

# ---------------------------------------------------------------------------
# MPC8xx special purpose registers (MPC860UM ch. 6 + ch. 11 + ch. 44)
# ---------------------------------------------------------------------------
SPRS = {
    1: ("XER", "integer exception register"),
    8: ("LR", "link register"),
    9: ("CTR", "count register"),
    18: ("DSISR", "DSI source / alignment info"),
    19: ("DAR", "data address of last DSI/alignment fault"),
    22: ("DEC", "decrementer (counts down at timebase rate, interrupt on 0->-1)"),
    26: ("SRR0", "save/restore 0: PC of interrupted instruction"),
    27: ("SRR1", "save/restore 1: MSR at interrupt time"),
    80: ("EIE", "8xx: any write sets MSR[EE] and MSR[RI] (enable ext. interrupts)"),
    81: ("EID", "8xx: any write clears MSR[EE], sets MSR[RI] (disable ext. interrupts)"),
    82: ("NRI", "8xx: any write clears MSR[EE] and MSR[RI] (non-recoverable)"),
    144: ("CMPA", "debug comparator A"), 145: ("CMPB", "debug comparator B"),
    146: ("CMPC", "debug comparator C"), 147: ("CMPD", "debug comparator D"),
    148: ("ICR", "interrupt cause register (debug)"),
    149: ("DER", "debug enable register"),
    150: ("COUNTA", "breakpoint counter A"), 151: ("COUNTB", "breakpoint counter B"),
    152: ("CMPE", "debug comparator E"), 153: ("CMPF", "debug comparator F"),
    154: ("CMPG", "debug comparator G"), 155: ("CMPH", "debug comparator H"),
    156: ("LCTRL1", "L-bus breakpoint control 1"), 157: ("LCTRL2", "L-bus breakpoint control 2"),
    158: ("ICTRL", "I-bus support control (show cycles / serialization)"),
    159: ("BAR", "breakpoint address register"),
    268: ("TBL", "timebase lower (read)"), 269: ("TBU", "timebase upper (read)"),
    272: ("SPRG0", "OS scratch 0"), 273: ("SPRG1", "OS scratch 1"),
    274: ("SPRG2", "OS scratch 2"), 275: ("SPRG3", "OS scratch 3"),
    284: ("TBL_W", "timebase lower (write)"), 285: ("TBU_W", "timebase upper (write)"),
    287: ("PVR", "processor version register (MPC860 = 0x0050xxxx)"),
    560: ("IC_CST", "I-cache control/status (commands: 0x02=enable 0x04=disable 0x0A=unlock-all 0x0C=invalidate-all, <<24)"),
    561: ("IC_ADR", "I-cache address"), 562: ("IC_DAT", "I-cache data port"),
    568: ("DC_CST", "D-cache control/status (same command encoding as IC_CST)"),
    569: ("DC_ADR", "D-cache address"), 570: ("DC_DAT", "D-cache data port"),
    630: ("DPDR", "development port data"), 631: ("DPIR", "development port instruction"),
    638: ("IMMR", "internal memory map register: [0:15]=ISB base of on-chip regs, [16:23]=PARTNUM, [24:31]=MASKNUM"),
    784: ("MI_CTR", "I-MMU control"), 786: ("MI_AP", "I-MMU access protection"),
    787: ("MI_EPN", "I-MMU effective page number"), 789: ("MI_TWC", "I-MMU tablewalk control"),
    790: ("MI_RPN", "I-MMU real page number"),
    792: ("MD_CTR", "D-MMU control"), 793: ("M_CASID", "MMU current address space ID"),
    794: ("MD_AP", "D-MMU access protection"), 795: ("MD_EPN", "D-MMU effective page number"),
    796: ("M_TWB", "MMU tablewalk base"), 797: ("MD_TWC", "D-MMU tablewalk control"),
    798: ("MD_RPN", "D-MMU real page number"),
    799: ("M_TW", "MMU tablewalk scratch (often abused as a free scratch SPR)"),
    816: ("MI_CAM", "I-TLB CAM read"), 817: ("MI_RAM0", "I-TLB RAM0 read"), 818: ("MI_RAM1", "I-TLB RAM1 read"),
    824: ("MD_CAM", "D-TLB CAM read"), 825: ("MD_RAM0", "D-TLB RAM0 read"), 826: ("MD_RAM1", "D-TLB RAM1 read"),
}

# ---------------------------------------------------------------------------
# IMMR register map. Names/offsets come from IDA's cfg/ppc.cfg (.mpc860),
# widths from the MPC860 User's Manual (registers not listed are 32-bit).
# ---------------------------------------------------------------------------
WIDTH16 = set("""SWSR MSTAT MPTPR TBSCR RTCSC PISCR CPCR RCCR RCTR1 RCTR2 RCTR3 RCTR4 RTER RTMR
PADIR PAPAR PAODR PADAT PCDIR PCPAR PCSO PCDAT PCINT PDDIR PDPAR PDDAT TGCR
TMR1 TMR2 TRR1 TRR2 TCR1 TCR2 TCN1 TCN2 TMR3 TMR4 TRR3 TRR4 TCR3 TCR4 TCN3 TCN4
TER1 TER2 TER3 TER4 CIVR SMCMR1 SMCMR2 DSR1 DSR2 DSR3 DSR4 TODR1 TODR2 TODR3 TODR4
SCCE1 SCCE2 SCCE3 SCCE4 SCCM1 SCCM2 SCCM3 SCCM4 SPMODE PIPC PTPR SDCR""".split())
WIDTH8 = set("""SMCE1 SMCE2 SMCM1 SMCM2 SCCS1 SCCS2 SCCS3 SCCS4 I2MOD I2ADD I2BRG I2COM I2CER I2CMR
SPIE SPIM SPCOM SIGMR SISTR SICMR SDSR SDMR IDSR1 IDMR1 IDSR2 IDMR2""".split())
# big areas: (name, offset, size)
IMMR_AREAS = [("SIRAM", 0xC00, 0x200), ("DPRAM", 0x2000, 0x1000),
              ("DPRAMEXP", 0x3000, 0xC00), ("PRAM", 0x3C00, 0x400)]


def parse_ppc_cfg_mpc860():
    """Return [(name, offset)] for the .mpc860 MMIO section of IDA's ppc.cfg."""
    path = idaapi.idadir("cfg/ppc.cfg")
    regs, in_dev = [], False
    for line in open(path, encoding="latin-1"):
        if line.startswith("."):
            in_dev = line.strip() == ".mpc860"
            continue
        if not in_dev or not line.strip() or line[0] in " ;\t":
            continue
        tok = line.split()
        if tok[0] in ("spr", "dcr") or "=" in tok[0] or len(tok) < 2:
            continue
        try:
            regs.append((tok[0], int(tok[1], 16)))
        except ValueError:
            pass
    return regs


def immr_regs():
    big = {n for n, _, _ in IMMR_AREAS}
    out = []
    for name, off in parse_ppc_cfg_mpc860():
        if name in big:
            continue
        w = 2 if name in WIDTH16 else 1 if name in WIDTH8 else 4
        out.append((name, off, w))
    out.sort(key=lambda r: r[1])
    return out


def ensure_32bit_rom():
    s = ida_segment.getseg(ROM_BASE)
    if s and s.bitness != 1:
        ida_segment.set_segm_addressing(s, 1)
    idc.set_segm_name(ROM_BASE, "ROM")


def make_segment(start, size, name, sclass="DATA", comment=None):
    if ida_segment.getseg(start) is None:
        seg = ida_segment.segment_t()
        seg.start_ea, seg.end_ea = start, start + size
        seg.bitness = 1
        seg.perm = ida_segment.SEGPERM_READ | ida_segment.SEGPERM_WRITE
        ida_segment.add_segm_ex(seg, name, sclass, ida_segment.ADDSEG_NOSREG | ida_segment.ADDSEG_SPARSE)
    else:
        idc.set_segm_name(start, name)
    if comment:
        ida_segment.set_segment_cmt(ida_segment.getseg(start), comment, 0)


def setup_immr():
    make_segment(IMMR_BASE, IMMR_SIZE, "IMMR", "IO",
                 "MPC860 internal memory map (IMMR=0xFF000000 set at reset). SIU/memctl/timers/clocks/CPM/FEC/DPRAM")
    regs = immr_regs()
    mk = {1: ida_bytes.create_byte, 2: ida_bytes.create_word, 4: ida_bytes.create_dword}
    for name, off, w in regs:
        ea = IMMR_BASE + off
        ida_bytes.del_items(ea, 0, w)
        mk[w](ea, w)
        ida_name.set_name(ea, name, ida_name.SN_FORCE | ida_name.SN_NOWARN)
    for name, off, size in IMMR_AREAS:
        ea = IMMR_BASE + off
        ida_name.set_name(ea, name, ida_name.SN_FORCE | ida_name.SN_NOWARN)

    # C struct with explicit padding, so op_stroff can display IMMR_t.<REG>
    ctype = {1: "unsigned __int8", 2: "unsigned __int16", 4: "unsigned __int32"}
    fields, cur, pad = [], 0, 0
    for name, off, w in regs:
        if off < cur:
            continue  # overlapping definition, skip
        if off > cur:
            fields.append(f"  unsigned __int8 _pad{pad}[{off - cur:#x}];")
            pad += 1
        fields.append(f"  {ctype[w]} {name};")
        cur = off + w
    for name, off, size in IMMR_AREAS:
        if off > cur:
            fields.append(f"  unsigned __int8 _pad{pad}[{off - cur:#x}];")
            pad += 1
        fields.append(f"  unsigned __int8 {name}[{size:#x}];")
        cur = off + size
    decl = "struct IMMR_t {\n" + "\n".join(fields) + "\n};"
    ida_typeinf.idc_parse_types(decl, 0)
    idc.import_type(-1, "IMMR_t")
    return idc.get_struc_id("IMMR_t")


def name_and_comment_spr_accesses():
    """Annotate every mtspr/mfspr with the 8xx SPR name."""
    n = 0
    for seg_ea in (ROM_BASE,):
        s = ida_segment.getseg(seg_ea)
        ea = s.start_ea
        while ea < s.end_ea:
            w = ida_bytes.get_dword(ea)
            if (w >> 26) == 31 and ((w >> 1) & 0x3FF) in (339, 467, 371) and ida_bytes.is_code(ida_bytes.get_flags(ea)):
                spr = ((w >> 16) & 0x1F) | (((w >> 11) & 0x1F) << 5)
                if spr in SPRS:
                    nm, desc = SPRS[spr]
                    idc.set_cmt(ea, f"{nm}: {desc}", 0)
                    n += 1
            ea += 4
    return n


VEC_NAMES = {0x100: "Reset", 0x200: "MachineCheck", 0x300: "DSI", 0x400: "ISI", 0x500: "External",
             0x600: "Alignment", 0x700: "Program", 0x800: "FPUnavailable", 0x900: "Decrementer",
             0xC00: "Syscall", 0xD00: "Trace", 0xE00: "FPAssist", 0x1000: "SWEmulation",
             0x1100: "ITLBMiss", 0x1200: "DTLBMiss", 0x1300: "ITLBError", 0x1400: "DTLBError",
             0x1C00: "DataBreakpoint", 0x1D00: "InstrBreakpoint", 0x1E00: "PeriphBreakpoint",
             0x1F00: "NMDevPort"}


def setup_vectors():
    """Each vector slot is a trampoline: save r4/LR in SPRG0/1, load handler, blr."""
    found = {}
    for off, nm in VEC_NAMES.items():
        ea = ROM_BASE + off
        if ida_bytes.get_dword(ea) == 0xBADDADDD:
            continue
        ida_bytes.del_items(ea, 0, 0x40)
        idc.create_insn(ea)
        ida_auto.auto_wait()
        ida_funcs.add_func(ea)
        ida_name.set_name(ea, f"vec_{off:04X}_{nm}", ida_name.SN_FORCE | ida_name.SN_NOWARN)
        # look for lis rX,hi / ori rX,rX,lo in the first 8 insns -> handler
        hi = None
        for i in range(8):
            w = ida_bytes.get_dword(ea + 4 * i)
            op = w >> 26
            if op == 15 and ((w >> 16) & 0x1F) == 0:      # lis
                hi = (w & 0xFFFF) << 16
            elif op == 24 and hi is not None:            # ori
                tgt = hi | (w & 0xFFFF)
                found[off] = tgt
                break
        if off == 0x100:
            tgt = ea + 0x2F00  # 'b +0x2F00'
            found[off] = tgt
        if off in found:
            tgt = found[off]
            idc.create_insn(tgt)
            ida_auto.auto_wait()
            ida_funcs.add_func(tgt)
            idc.set_cmt(ea, f"exception vector {off:#x} ({nm}) -> {tgt:#x}", 1)
    # name handlers; one shared by several vectors is the generic catch-all
    users = {}
    for off, tgt in found.items():
        users.setdefault(tgt, []).append(off)
    for tgt, offs in users.items():
        name = f"exc_{VEC_NAMES[offs[0]]}" if len(offs) == 1 else "exc_Unexpected"
        cur = ida_name.get_name(tgt)
        if not cur or cur.startswith("sub_") or cur.startswith("exc_"):
            ida_name.set_name(tgt, name, ida_name.SN_FORCE | ida_name.SN_NOWARN)
    return found


def apply_immr_stroff(strid):
    """Track registers holding the IMMR base inside each function, then display
    D-form memory operands based on them as IMMR_t.<field>."""
    done = 0
    for f_ea in list(idautils_functions()):
        f = ida_funcs.get_func(f_ea)
        immr_regs_live = set()
        ea = f.start_ea
        while ea < f.end_ea:
            if not ida_bytes.is_code(ida_bytes.get_flags(ea)):
                ea += 4
                continue
            # basic-block boundary: anything that is a jump target resets tracking
            if idc.get_first_cref_to(ea) not in (idc.BADADDR, ea - 4) and ea != f.start_ea:
                immr_regs_live = set()
            insn = ida_ua.insn_t()
            if ida_ua.decode_insn(insn, ea) == 0:
                ea += 4
                continue
            mn = idc.print_insn_mnem(ea)
            o0, o1 = insn.ops[0], insn.ops[1]
            if mn == "mfspr" and o1.type == 8 and o1.value == 638:
                immr_regs_live.add(o0.reg)
            elif mn in ("clrrwi", "rlwinm") and o1.type == 1 and o1.reg in immr_regs_live:
                immr_regs_live.add(o0.reg)
            elif mn == "lis" and o1.type == 5 and (o1.value & 0xFFFFFFFF) == 0xFFFFFF00:
                immr_regs_live.add(o0.reg)
            else:
                for i in range(4):
                    o = insn.ops[i]
                    if o.type == 4 and o.phrase in immr_regs_live and 0 <= o.addr < IMMR_SIZE:
                        idc.op_stroff(ea, i, strid, 0)
                        done += 1
                # destination register overwritten by anything else -> forget
                if o0.type == 1 and mn not in ("stw", "sth", "stb", "stwu", "cmpw", "cmplw", "cmpwi", "cmplwi",
                                                "mtspr", "mtmsr", "mtcrf", "mtlr", "mtctr"):
                    immr_regs_live.discard(o0.reg)
            if mn in ("b", "blr", "bctr", "rfi"):
                immr_regs_live = set()
            ea += insn.size
    return done


def idautils_functions():
    import idautils
    return idautils.Functions()


def run():
    ensure_32bit_rom()
    strid = setup_immr()
    vecs = setup_vectors()
    ida_auto.auto_wait()
    nspr = name_and_comment_spr_accesses()
    nst = apply_immr_stroff(strid)
    print(f"IMMR_t id={strid:#x}; vectors={len(vecs)}; SPR comments={nspr}; IMMR stroffs={nst}")
    return vecs


if __name__ == "__main__" or True:
    _vecs = run()
