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
    # ---- WIC/VIC slots and the IO FPGA (docs ch. 19) ----
    0x80504F1C: ("wic_slot_count",
                 "Number of WIC/VIC slots for the mainboard PM ID: 1720 (0xB2) / 1721 (0x35A) 2, "
                 "1750 (0xC9) / 1751 (0x24D) 3, 1760 (0x316) 4. (docs ch. 19)"),
    0x80140A08: ("slot_probe_voice_cards",
                 "Per slot (bases 0x68030000, 0x68040000, 0x68080000 on 1750/1751/1760, 0x680B0000 on 1760): "
                 "write 0x05 to the slot register, wait, read back; 0x0A (low nibble inverted) = a card with "
                 "its own logic (voice) -> bit in slot_voice_present. An empty slot floats at 0xFF. (docs ch. 19)"),
    0x80396348: ("wic_slot_status_poll",
                 "wic_slot_status_poll(bay, slot, base): reads the slot status register at base+5; bits 0x20/0x40 "
                 "signal interface up/down for card types 0x3FA-0x3FD, 0x473, 0x474; writes it back with bit 7 clear. "
                 "(docs ch. 19)"),
    0x8014B8F0: ("slot0_status_poll", "wic_slot_status_poll(0, 0, 0x68030000)"),
    0x8014B91C: ("slot1_status_poll", "wic_slot_status_poll(0, 1, 0x68040000)"),
    0x8014B948: ("slot2_status_poll", "wic_slot_status_poll(0, 2, 0x68080000)"),
    0x8014B974: ("slot3_status_poll", "wic_slot_status_poll(0, 3, 0x680B0000)"),
    0x801405BC: ("wic_slot_reset_pulse",
                 "Reset pulse for one slot: slot 0 = IO FPGA reg 0x1C bit 0x20, slots 1/2 = reg 0x18 bits 0/1, "
                 "slot 3 = reg 0x0A bit 0; high for busy_delay(20). (docs ch. 19)"),
    0x80504B8C: ("wic_slot_select",
                 "Set a slot's device select field from the table at 0x8268A74C: reg 0x18 for slots 0-2, "
                 "reg 0x0A for slot 3 (the 1760's fourth slot). (docs ch. 19)"),
    0x8013EF88: ("c17xx_slots_init",
                 "1751/1760 slot hardware init: load the FPGA bitstream (1760: XC2S200 166980 bytes, else XC2S100 "
                 "97652), set reg 0x14 bits 0x40/0x80 for voice cards in slots 0/1, voice_tdm_init, then pulse every "
                 "slot reset at once. (docs ch. 19)"),
    0x8013E4F4: ("voice_tdm_init",
                 "IO FPGA voice TDM init: reg 0x5C (low bits = 1 on 1751, bit 0x80 on 1760, bit 0x20 enable), "
                 "reg 0x6C = 0x80, reg 0x48 = 4. (docs ch. 19)"),
    0x800DC690: ("tsa_route_voice_port",
                 "Route a voice port to a TSA (MPC860 SI time slot assigner): reg 0x48 nibbles, reg 0x5C port bits "
                 "0x08/0x10, reg 0x14 bits 0x40/0x80; prints 'TSA %d <--- port %d/%d'. (docs ch. 19)"),
    0x8062826C: ("dsp_timeslot_program",
                 "Program DSP time slots: reg 0x70/0x74 masks, then reg 0x6C = slot | 0x40 per time slot. (docs ch. 19)"),
    0x80122110: ("iofpga_reg0c_init", "IO FPGA reg 0x0C = 0 on 1751/1760/0x288/0x496/0x1A8 (meaning unknown). (docs ch. 19)"),
    0x81BC7F80: ("probe_address",
                 "probe_address(addr): installs a temporary machine check handler (vector 0x200, via "
                 "sub_80613D8C), reads addr, returns 0 if the read faulted. Used by the memory dump (prints XX) "
                 "and by platform init to find the end of the flash (0x62000000 on a 32 MB 1760): these are "
                 "the machine checks seen during boot. (docs ch. 19)"),
    0x80123944: ("fpga_firmware_register",
                 "Registers the FPGA 'firmware' image: the longer length when no voice card is present "
                 "(slot_voice_present == 0) and IO FPGA reg 0x20 bits 0x40/0x80 are clear. (docs ch. 19)"),
    # ---- WIC detection and the WIC-1T serial port (docs ch. 23) ----
    0x80140E2C: ("smart_init_read_slot_eeproms",
                 "c1700_smart_init.c: per slot, read 64 words of the ID EEPROM (slot_eeprom_read); format 4 "
                 "(04 FF): TLV card type, else byte 1 = card type -> slot_card_type[], slot_iomem_need[]. (docs ch. 23)"),
    0x80140B40: ("slot_eeprom_read",
                 "slot_eeprom_read(spi_ctx, buf, words): SPI transfers 03 <2*word> 00 00 with the slot's EEPROM "
                 "selected (IO FPGA reg 0x18/0x0A device 3); words byte-swapped for voice slots. (docs ch. 23)"),
    0x805048E0: ("wic_eeprom_read", "Same SPI READ loop as slot_eeprom_read, for the WIC slot objects. (docs ch. 23)"),
    0x80504870: ("slot_spi_bit_reverse", "Voice slots (slot_voice_present) have their SPI bits reversed: table 0x82FEB300."),
    0x80504BF4: ("slot_eeprom_select", "slot_eeprom_select(on, ctx): wic_slot_select device 3 (selected) or 2 (idle)."),
    0x80516710: ("wic_slot_object_init", "If the slot is occupied (wic_slot_present), read its EEPROM; pick the vtable."),
    0x80516684: ("wic_slot_present", "Slot occupied? cards_present_mask bits: slot 0 0x02, 1 0x04, 2 0x01, 3 0x20. (docs ch. 23)"),
    0x801436A4: ("cards_present_mask", "Cached result of the platform's card-present function (pointer at 0x8306B9DC)."),
    0x80143694: ("c17xx_cards_present", "Default card-present function: IO FPGA reg 0x10 & 6 (slots 0 and 1)."),
    0x806293D8: ("c1760_cards_present",
                 "1751/1760 card-present function: reg 0x10 bits 1/2/0/5 for slots 0-3, plus the voice-card "
                 "presence test (0x05 -> 0x0A) per slot. (docs ch. 23)"),
    0x80516F04: ("show_diag_wic_slot", "show diag for one WIC/VIC slot: old (v1/v2) or v4 EEPROM layout. (docs ch. 23)"),
    0x80503BF4: ("wic_serial_analyze",
                 "WIC serial cards (types 2, 0x12, 0x13): SCC channels from wic_slot_scc_channels, create the "
                 "Serial interfaces (pquicc_serial_create). (docs ch. 23)"),
    0x80506AEC: ("wic_slot_scc_channels",
                 "Slot -> SCC channel(s): 1-port cards slot 0 -> 0 (SCC1), slot 1 -> 1 (SCC2); 2-port cards "
                 "[0,3] / [1,2]. (docs ch. 23)"),
    0x8050268C: ("pquicc_serial_create", "Allocate a serial IDB on SCC channel n and initialise the driver."),
    0x80501FDC: ("pquicc_serial_init", "'PowerQUICC Serial' driver setup for one SCC channel."),
    0x80127B40: ("pquicc_rxbd_base", "RxBD table offset per SCC channel: 0x2430 + 0x100*n (IMMR-relative)."),
    0x80127BE8: ("pquicc_txbd_base", "TxBD table offset per SCC channel: 0x24B0 + 0x100*n."),
    0x8050E868: ("wic_serial_modem_init",
                 "Modem control setup: PCDAT/PBDAT pointers, the card's registers at slot base +4 (cable-ID mux, "
                 "transceiver strobes) and +8; second port at +0x14/+0x18. (docs ch. 23)"),
    0x8050E33C: ("wic_cable_id_read", "Cable ID: select mux 3..0 in reg +4 bits 0-1, read bit 0x20 each, MSB first."),
    0x8050E410: ("wic_cable_id", "Cable ID, read until two reads agree; kept in the driver status word."),
    0x8050E4A4: ("wic_cable_type",
                 "Cable ID -> type: bit 0 DCE; 2 V.24, 4 V.35, 6 RS-449, 8 X.21, 14 530, 16 530A; 1 = no cable. (docs ch. 23)"),
    0x8050E9D8: ("wic_cable_is_dte", "DTE unless the cable ID's bit 0 says DCE."),
    0x8050E1AC: ("wic_transceiver_strobe", "Strobe a mode value into the card's line transceivers through reg +4."),
    0x8050DF64: ("serial_clock_detect", "show controllers: sample PADAT clock pins; toggling = 'TX and RX clocks detected'."),
    0x804FE6CC: ("show_controllers_serial_cable", "show controllers: 'DTE V.35 ...' or 'No serial cable attached'."),
    0x80504FC8: ("show_controllers_scc_regs", "show controllers: SCC, CPM interrupt and port registers."),
    0x8050DD44: ("show_serial_modem_signals", "'DCD=%s DSR=%s DTR=%s RTS=%s CTS=%s'"),
    0x8050D4D4: ("serial_dcd", "DCD: DTE channel 0 = PCDAT 0x20 (PC10/CD1), 1 = PCDAT 0x80; active low."),
    0x8050D5D8: ("serial_dsr", "DSR: DTE channel 0 = PBDAT 0x80, 1 = PBDAT 0x20; active low."),
    0x8050D6DC: ("serial_dtr", "DTR (output): DTE channel 0 = PBDAT 0x800, 1 = PCDAT 0x08; active low."),
    0x8050D8E4: ("serial_rts", "RTS (output)."),
    0x8050D918: ("serial_cts", "CTS: DTE channel 0 = PCDAT 0x10 (PC11/CTS1), 1 = PCDAT 0x40; active low."),
    0x8050DA1C: ("serial_set_dtr", "Drive DTR (channel 0: PBDAT 0x800, active low)."),
    0x8050DB2C: ("serial_set_rts", "Drive RTS (channel 0: PBDAT 0x1000, active low)."),
    0x819B5E9C: ("hdlc_send_keepalive",
                 "Cisco HDLC SLARP keepalive: code 2, myseq, yourseq, 0xFFFF, magic (keepalive_magic). (docs ch. 23)"),
    0x819BF000: ("slarp_input", "SLARP input: keepalive (code 2) records the peer's myseq, yourseq and magic."),
    0x819B5FF8: ("hdlc_keepalive_check",
                 "Keepalive timer: line looped if the peer's magic equals our last one; down after missed "
                 "keepalives. (docs ch. 23)"),
    0x8032F308: ("keepalive_magic", "counter++ + uptime (ms) + a constant: identical on two lockstep emulators."),
    # ROM monitor ("EMT") calls: `sc` with the number in r3 (docs ch. 24).
    0x806154B4: ("emt_call", "emt_call(n, a, b, c): `sc`; the ROM answers in r3. One wrapper per EMT at 0x81BCDE8C-0x81BCEAA4."),
    0x8061A190: ("format_reload_reason",
                 "Text of 'System returned to ROM by ...' from boot_data (EMT 13): checks BOOT_COUNT = 19, then the "
                 "reason in word 1 (0 power-on, 9 reload, 1 abort at PC ...). (docs ch. 24)"),
    0x81BCE2D0: ("emt_putc", "EMT 1: console output (before IOS's own console driver runs)."),
    0x81BCE1AC: ("emt_rom_level", "EMT 2: the real ROM answers 0xC00."),
    0x81BCE2AC: ("emt_getchar", "EMT 20: wait for a console character."),
    0x81BCDFB0: ("emt_memsize", "Memory size: EMT 132 (newer ROMs), EMT 4 when that answers -1."),
    0x81BCE31C: ("emt_nvram_confreg_inv", "EMT 6: the complemented configuration register in NVRAM, -1 if invalid."),
    0x81BCE42C: ("emt_platform", "EMT 7: platform code (0x33 = C1700, = ELF e_machine of its images)."),
    0x81BCDE8C: ("emt_reload", "EMT 8: reload (the ROM records reason 9 and resets)."),
    0x81BCDED4: ("emt_rom_banner", "EMT 10: the ROM's banner (show version prints its first line after 'ROM: ')."),
    0x81BCE240: ("emt_boot_data", "EMT 13: address of boot_data[20] {19, reason, PC, address}."),
    0x81BCE910: ("emt_cpm_reset", "EMT 21: reset the CPM."),
    0x81BCE140: ("emt_nvram_size", "EMT 24: NVRAM size."),
    0x81BCE21C: ("emt_platform_name", "EMT 25: \"C1700\"."),
    0x81BCE1D4: ("emt_boot_info", "EMT 26: {device type, \"flash:NAME\"} of the booted image."),
    0x81BCE2F8: ("emt_boot_confreg", "EMT 28: configuration register the image was booted with."),
    0x81BCE094: ("emt_cookie_ptr", "EMT 33: pointer to the ROM's cookie copy."),
    0x81BCE3E0: ("emt_image_string_get", "EMT 34: a 257-byte string kept by the ROM."),
    0x81BCE404: ("emt_image_string_set", "EMT 35."),
    0x81BCE4BC: ("emt_version_string_get", "EMT 36."),
    0x81BCE3B8: ("emt_version_string_set", "EMT 37: stored once per boot."),
    0x81BCE0B8: ("emt_cookie_copy", "EMT 44: copy the 128 cookie bytes (NVRAM + 2) to a buffer."),
    0x81BCE5C0: ("emt_monvar_set", "EMT 45: set ROMMON variable \"NAME=VALUE\" (BSI, RET_2_RCALTS, RANDOM_NUM...); saved in NVRAM."),
    0x81BCE5E8: ("emt_monvar_get", "EMT 46: get a ROMMON variable (length, -1 unset, -2 too long)."),
    0x81BCE164: ("emt_nvram_ios_offset", "EMT 47: where IOS's NVRAM area starts (0x800)."),
    0x81BCE728: ("emt_iomem_set", "EMT 59: IOMEM percentage into NVRAM 0x9F."),
    0x81BCE750: ("emt_iomem_get", "EMT 60: IOMEM percentage ('IOMEM set to: 25')."),
    0x81BCE700: ("emt_console_baud_set", "EMT 61: console speed into the NVRAM configuration register."),
    0x81BCE6DC: ("emt_console_baud_get", "EMT 62: console speed from the NVRAM configuration register."),
    0x81BCE850: ("emt_nvram_byte_a0_get", "EMT 71: NVRAM byte 0xA0."),
    0x81BCE878: ("emt_nvram_byte_a0_set", "EMT 72: NVRAM byte 0xA0."),
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
    0x8306B9E0: ("slot_voice_present", "bit n = slot n answered the 0x05 -> 0x0A presence test (voice card)"),
    0x8268A74C: ("wic_slot_select_table", "per slot and device: select bits for IO FPGA reg 0x18 / 0x0A"),
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
