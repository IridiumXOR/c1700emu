"""Annotations for IOS 12.4(8) c1700-adventerprisek9-mz (decompressed ELF,
e_machine patched to 20 so IDA loads it as PowerPC; docs chapters 9-12).

Run inside IDA (MCP execute_python or File > Script file) on a database of
that image. Every name below comes from reverse engineering done for the
emulator; the docs chapter is given in each comment.
"""
import idc
import ida_funcs
import ida_name

NAME_FLAGS = ida_name.SN_NOWARN | ida_name.SN_FORCE

FUNCS = {
    0x80122488: ("platform_get_string",
                 "platform_get_string(id): jump table at 0x82393370. id 5 = processor board ID "
                 "(buffer 0x8355D498, see build_processor_board_id), id 8 = 0x8355D48C. (docs ch. 4)"),
    0x8012377C: ("build_processor_board_id",
                 "Builds the 'Processor board ID' string from the NVRAM cookie copy (*0x83551B28): "
                 "cookie[0x18..0x1A] chars + %02x cookie[0x1B] + %02x cookie[0x1C] + cookie[0x1D..0x20] chars, "
                 "i.e. a Cisco serial LLLYYWWSSSS, then ' (%lu)' of a hash. (docs ch. 4)"),
    0x80123CEC: ("get_mainboard_pm_id",
                 "Returns the mainboard PM ID from the cookie (0x316 on a 1760). (docs ch. 11.5)"),
    0x801260F8: ("raise_interrupt_level",
                 "Probably IOS's raise_interrupt_level(level): called with 6 around the FPGA load, "
                 "returns the old level, paired with reset_interrupt_level."),
    0x8012609C: ("reset_interrupt_level", "Probably IOS's reset_interrupt_level(old_level)."),
    0x80615A00: ("busy_delay", "Busy-wait delay; units not established (called with 250 and 100)."),
    0x80617618: ("scheduler_find_next_process",
                 "IOS scheduler: picks the next process to run; when none is ready it loops here "
                 "(outer loop head 0x80617648) testing timers and blocked processes' wake-up predicates "
                 "(bctrl at 0x80617ebc). This loop is the emulator's idle anchor (--idle-pc 80617648, docs ch. 15)."),
    0x8061F708: ("scheduler_enter",
                 "Probably the entry a blocking process takes into the scheduler (calls "
                 "scheduler_find_next_process, with or without a stack switch)."),
    0x8061746C: ("mgd_timer_check",
                 "Managed-timer check (message '%SYS-1-MTNOTFENCED: Expired timer is not fenced')."),
    0x81BCF0C8: ("timer_expired", "timer_expired(t): *t != 0 && *t - now <= 0 (64-bit times)."),
    0x81BCFB70: ("get_system_time",
                 "Reads the 64-bit system clock (two words at *0x8333D52C/*0x8333D530, re-read until stable)."),
    0x81BD2BDC: ("process_get_byte_0x78", "Returns byte 0x78 of a process/queue structure (state flag)."),
    0x80130010: ("idma1_done_isr",
                 "IDMA1 completion (CPM interrupt): IDSR1 = 7 (clear), CISR = IDMA1, PCSO &= ~DREQ0 "
                 "(stop requesting), then completes the copied packets. (emu board/idma.rs)"),
    0x80130368: ("idma1_init",
                 "IDMA1 as a memory-copy engine: IBASE (IMMR-relative DPRAM offset), DCMR = 0 (mem->mem), "
                 "CPCR INIT IDMA (ch 1), IDSR1 = 7, RCCR DREQ0 mode bits, CIMR |= IDMA1, IDMR1 = DONE. "
                 "A copy starts when PCSO[DREQ0] is set (DREQ0 tied active on the board). (emu board/idma.rs)"),
    0x80130780: ("mmu_pin_dtlb_entry",
                 "Load a locked DTLB entry for EA r3 with the table-walk assist: MD_CTR[INDX] = counter++, "
                 "MD_EPN = ea|EV, mfspr M_TWB -> L1 desc -> MD_TWC, mfspr MD_TWC -> PTE -> MD_RPN. (docs ch. 12)"),
    0x80130810: ("mmu_pin_itlb_entry",
                 "Same as mmu_pin_dtlb_entry for the ITLB (MI_EPN/MI_TWC/MI_RPN, assists through MD_*). "
                 "IOS has no ITLB/DTLB miss handlers: everything it touches is pinned. (docs ch. 12)"),
    0x80141148: ("iomem_size_slots",
                 "Smart Init / IOMEM sizing: per slot, card_type[] (0x8306B214, 0xFF = empty) and "
                 "iomem_need[] (0x8306B224, -1 = unknown) -> prints 'Unknown Card in slot %d'. (docs ch. 10)"),
    0x80517C38: ("c1760_fpga_send_bit",
                 "Clock one bit into the FPGA: IO FPGA reg 0x4C: CCLK low, DIN = bit (0x10), CCLK high (0x20); "
                 "shadow byte at 0x835B6FF0. (docs ch. 11.6)"),
    0x80517C68: ("c1760_fpga_load_bitstream",
                 "Xilinx slave-serial configuration through IO FPGA reg 0x4C (buf, len): PROG_B pulse (0x40), "
                 "wait INIT_B (0x08, no timeout), send bytes MSB first, check DONE (0x02), retry x4. "
                 "Bitstream for the 1760 at 0x83426FD0 (XC2S200, 166980 bytes). (docs ch. 11.6)"),
}

DATA = {
    0x82393370: ("platform_string_jumptab", None),
    0x83053130 - 4: ("fpga_bitstream_xc2s100", "Xilinx Spartan-II configuration stream, FLR=0x0D (XC2S100-sized)"),
    0x83426FD0: ("fpga_bitstream_c1760_xc2s200", "Xilinx Spartan-II stream loaded on a 1760: FLR=0x11, 166980 bytes"),
    0x8306B214: ("slot_card_type", "per-slot card type from the WIC/VIC ID EEPROMs (SPI); 0xFF = no card"),
    0x8306B224: ("slot_iomem_need", "per-slot IOMEM requirement; -1 = unknown card type"),
    0x83551B28: ("cookie_copy_ptr", "pointer to IOS's copy of the NVRAM cookie data (128 bytes)"),
    0x8355D48C: ("platform_string_8", None),
    0x8355D498: ("processor_board_id_str", "'Processor board ID' text, built from the cookie serial"),
    0x835B6FF0: ("iofpga_reg4c_shadow", "shadow of IO FPGA register 0x4C (FPGA configuration port)"),
}

COMMENTS = {
    0x80617648: "scheduler outer loop head: reached once per idle pass (emulator idle anchor)",
    0x80617EBC: "call the blocked process's wake-up predicate (0xC0(r24), argument 0xC4(r24))",
    0x80517D10: "wait for INIT_B (reg 0x4C bit 3): hangs forever if the FPGA never answers",
    0x801307E4: "M_TWB read = address of the level-one descriptor (hardware assist)",
    0x801307F0: "MD_TWC read = address of the level-two descriptor (PTE)",
}


def apply():
    for ea, (name, cmt) in FUNCS.items():
        if not ida_funcs.get_func(ea):
            ida_funcs.add_func(ea)
        ida_name.set_name(ea, name, NAME_FLAGS)
        if cmt:
            idc.set_func_cmt(ea, cmt, 0)
    for ea, (name, cmt) in DATA.items():
        ida_name.set_name(ea, name, NAME_FLAGS)
        if cmt:
            idc.set_cmt(ea, cmt, 1)
    for ea, cmt in COMMENTS.items():
        idc.set_cmt(ea, cmt, 0)
    return len(FUNCS), len(DATA), len(COMMENTS)


if __name__ in ("__main__", "__ida_script__") or "idaapi" in globals():
    print(apply())
