"""
Board-level annotations for the Cisco C1700 ROMMON (run AFTER mpc860_setup.py).

    exec(open("/path/to/cisco_rom/tools/ida/c1700_names.py").read())

Everything here is a *conclusion* from the reverse engineering described in
docs/03-boot-flow.md and docs/04-hardware-map.md. Each entry says why the name
was chosen, so you can verify it yourself in the decompiler.
"""

import idc
import ida_bytes
import ida_funcs
import ida_name
import ida_segment
import ida_typeinf
import ida_auto

FORCE = ida_name.SN_FORCE | ida_name.SN_NOWARN


def seg(start, size, name, sclass, comment):
    if ida_segment.getseg(start) is None:
        s = ida_segment.segment_t()
        s.start_ea, s.end_ea, s.bitness = start, start + size, 1
        s.perm = ida_segment.SEGPERM_READ | ida_segment.SEGPERM_WRITE
        ida_segment.add_segm_ex(s, name, sclass, ida_segment.ADDSEG_NOSREG | ida_segment.ADDSEG_SPARSE)
    ida_segment.set_segment_cmt(ida_segment.getseg(start), comment, 0)


def reg(ea, name, width, comment=None):
    ida_bytes.del_items(ea, 0, width)
    {1: ida_bytes.create_byte, 2: ida_bytes.create_word, 4: ida_bytes.create_dword}[width](ea, width)
    ida_name.set_name(ea, name, FORCE)
    if comment:
        idc.set_cmt(ea, comment, 1)


# ---------------------------------------------------------------------------
# Segments for the board devices (CS7 = 8-bit I/O bus at 0x68000000) and RAM
# ---------------------------------------------------------------------------
seg(0x68000000, 0x8000, "NVRAM", "DATA", "CS7: NVRAM (parallel EEPROM, 32-byte page writes + data polling)")
seg(0x68020000, 0x100, "CPLD", "IO", "CS7: board CPLD (LEDs, NVRAM write-protect, board id, resets)")
seg(0x68050000, 0x8, "UART", "IO", "CS7: 16550-compatible console UART, 3.6864 MHz clock")
seg(0x80000000, 0x8000, "RAM_HI", "DATA", "DRAM alias at 0x80000000 (ORx AM bit0 cleared): ROMMON data/bss")

# 16550 registers (DLAB=0 / DLAB=1 meanings)
for off, name, cmt in [
    (0, "UART_RBR_THR", "RBR (read) / THR (write) / DLL when LCR.DLAB=1"),
    (1, "UART_IER", "interrupt enable / DLM when LCR.DLAB=1"),
    (2, "UART_IIR_FCR", "IIR (read) / FCR (write)"),
    (3, "UART_LCR", "line control: 3 = 8N1, bit7 = DLAB"),
    (4, "UART_MCR", "modem control: 3 = DTR|RTS"),
    (5, "UART_LSR", "line status: bit0 DR (rx data ready), bit5 THRE (tx empty)"),
    (6, "UART_MSR", "modem status"),
    (7, "UART_SCR", "scratch"),
]:
    reg(0x68050000 + off, name, 1, cmt)

reg(0x68000000, "nvram_cookie", 1, "132-byte cookie: u16 magic 0x8D74, 128 B data, u16 checksum (sum of halfwords)")
reg(0x68000090, "nvram_confreg", 2, "configuration register")
reg(0x68000092, "nvram_confreg_inv", 2, "~configuration register (validity check)")
reg(0x68000094, "nvram_confreg_magic", 4, "0xFEEDFACE when confreg is valid")
reg(0x6800065A, "nvram_cookie_backup", 1, "backup copy of the cookie")

reg(0x68020000, "CPLD_LED", 1, "bit7 = system LED (blinked by fatal_error_blink)")
reg(0x68020004, "CPLD_BOARD_ID", 1, "bits0-2 = board type (2/3/4 handled specially)")
reg(0x68020014, "CPLD_CTRL14", 1, "bit0 = NVRAM write protect, bit2 = boot flag tested at reset")
reg(0x68020018, "CPLD_CTRL18", 1, "bit0 set before CPM reset")
reg(0x68020024, "CPLD_REG24", 1, None)
reg(0x68020048, "CPLD_REG48", 1, "bit3 = second LED on board type 2")

# ---------------------------------------------------------------------------
# Functions: (address, name, C prototype or None, comment)
# ---------------------------------------------------------------------------
FUNCS = [
    (0xFFF03000, "reset_entry", None, "System reset handler: save r10/r11/CR in SPRG1-3, MSR=ME|IP|RI, IMMR=0xFF000000, test DPRAM, caches off, stack in DPRAM"),
    (0xFFF03244, "soft_reset_save_context", None, "RSR says external soft reset: dump CPU state to 0x80006D54 (debug 'break')"),
    (0xFFF0453C, "dram_init_and_size", "int __fastcall dram_init_and_size(int reset_type);",
     "Program UPMA/UPMB, SDRAM power-up sequence per bank (CS1-4), size banks by address aliasing, lay them out contiguously"),
    (0xFFF04528, "delay_loop_20000", "void delay_loop_20000(void);", "busy loop, used after BR/OR changes"),
    (0xFFF060D8, "rom_init_main", "int __fastcall rom_init_main(int reset_type, int arg);",
     "C-level init: RAM test, clear bss, cookie/NVRAM, interrupts, caches, sizing; prints the diag messages when confreg bit15"),
    (0xFFF06D7C, "siu_init", "int __fastcall siu_init(int reset_type);",
     "Mask interrupts, timers, clocks (SCCR/PLPRCR with keys), chip selects BR0-7/OR0-7, ports"),
    (0xFFF04474, "uart_init", "int __fastcall uart_init(int baud_index, int enable_ier, int reset_type);",
     "16550 init: divisor = 3686400/(16*baud), 8N1, DTR|RTS, FIFO on"),
    (0xFFF06FD4, "fatal_error_blink", "void __fastcall __noreturn fatal_error_blink(int code);",
     "never returns: blink CPLD LED `code` times, pause, repeat"),
    (0xFFF12218, "cpld_test_and_set_bootflag", "int cpld_test_and_set_bootflag(void);", "returns old CPLD_CTRL14 bit2 and sets it"),
    (0xFFF06D34, "io_sync", "int io_sync(void);", "sync; return 1"),
    (0xFFF040FC, "cpld_init", "void cpld_init(void);", None),
    (0xFFF06CE8, "cpm_reset", "int cpm_reset(void);", "CPLD_CTRL18|=1, CPCR = RST|FLG"),
    (0xFFF0B5D0, "nvram_write_enable", "int nvram_write_enable(void);", "clear CPLD_CTRL14 bit0 (remember old state)"),
    (0xFFF0B630, "nvram_write_restore", "int nvram_write_restore(void);", "restore NVRAM write protect"),
    (0xFFF0BC7C, "uart_putc_raw", "int __fastcall uart_putc_raw(char c);", "wait LSR.THRE, write THR"),
    (0xFFF0BCAC, "uart_getc_nowait", "int uart_getc_nowait(void);", "LSR.DR ? RBR : -1"),
    (0xFFF06F10, "rsr_read_and_clear", "unsigned int rsr_read_and_clear(void);", "return RSR & 0xFF000000 and clear it (needs RSRK key)"),
    (0xFFF03FEC, "system_reset_via_checkstop", "void __fastcall __noreturn system_reset_via_checkstop(unsigned int);",
     "PLPRCR.CSR=1 then MSR[ME]=0: next machine check = checkstop = hard reset"),
    (0xFFF12AC4, "delay_us", "int __fastcall delay_us(int us);", "timebase busy-wait, TB ~3 ticks/us"),
    (0xFFF1299C, "delay_ms", "int __fastcall delay_ms(int ms);", "timebase busy-wait, 3024 ticks/ms"),
    (0xFFF12784, "dec_reload", "unsigned int __fastcall dec_reload(char defer);", "DEC = 60480 (20 ms tick at 3.024 MHz)"),
    (0xFFF19920, "pit_start", "void pit_start(void);", "PISCR = PS|PIE|PITF|PTE, PITC = 0x17A"),
    (0xFFF19948, "pit_poll_tick", "void pit_poll_tick(void);", "if PISCR.PS: ++pit_ticks, clear PS"),
    (0xFFF0D6D4, "enable_interrupts", "int enable_interrupts(void);", "dec_reload, SIMASK = IRQ1, MSR[EE]=1"),
    (0xFFF0D5E0, "caches_disable", "int caches_disable(void);", "flush D-cache if enabled, disable I/D caches, clear MSR[IR,DR]"),
    (0xFFF03E58, "msr_set_ee", "void msr_set_ee(void);", None),
    (0xFFF03E80, "icache_enable", "unsigned int icache_enable(void);", "IC_CST = enable command"),
    (0xFFF0B234, "cookie_get", "unsigned char *cookie_get(void);", "load cookie from NVRAM (primary, else backup) into RAM, return pointer to data"),
    (0xFFF0B1D8, "cookie_is_valid", "int __fastcall cookie_is_valid(unsigned __int16 *c);", "magic 0x8D74 and checksum16 over 130 bytes"),
    (0xFFF0B324, "cookie_store", "int __fastcall cookie_store(unsigned char *data);", "recompute checksum and write primary + backup copies to NVRAM"),
    (0xFFF0BA6C, "checksum16", "int __fastcall checksum16(unsigned __int16 *p, int len);", "sum of halfwords, truncated to 16 bits"),
    (0xFFF0BB00, "nvram_write", "int __fastcall nvram_write(unsigned char *src, unsigned char *dst, int len);",
     "32-byte page writes + data polling (parallel EEPROM protocol)"),
    (0xFFF0B52C, "confreg_write", "int __fastcall confreg_write(unsigned __int16 value);", "writes value, ~value, 0xFEEDFACE to NVRAM+0x90"),
    (0xFFF0B4FC, "confreg_read_raw", "int confreg_read_raw(void);", None),
    (0xFFF08C58, "bcopy", "void *__fastcall bcopy(const void *src, void *dst, int len);", "BSD argument order"),
    (0xFFF24974, "bcmp", "int __fastcall bcmp(const void *a, const void *b, int len);", "0 if equal"),
    (0xFFF246D0, "puts_nonl", "int __fastcall puts_nonl(const char *s);", None),
    (0xFFF24CE0, "printf", "int printf(const char *fmt, ...);", None),
    (0xFFF24D88, "vprintf_internal", None, None),
    (0xFFF1B5C4, "fec_mii_read", "int __fastcall fec_mii_read(unsigned int frame, int reg);", "MII_DATA = frame | reg<<18, wait IEVENT.MII"),
    (0xFFF1B540, "fec_mii_write", "int __fastcall fec_mii_write(unsigned int frame, int reg, unsigned __int16 value);", None),
    # --- memory helpers (found with the emulator's --profile: bzero is 73% of boot) ---
    (0xFFF1682C, "bzero_bytes", "void __fastcall bzero_bytes(void *p, unsigned int len);",
     "naive byte loop (stb/addi/cmplw/blt): clearing 32 MB takes 134M instructions"),
    (0xFFF16850, "bzero", "void __fastcall bzero(void *p, unsigned int len);", None),
    # --- flash driver (docs/04-hardware-map.md, flash section) ---
    (0xFFF15EB4, "c1700_flash_setup", "void c1700_flash_setup(void);",
     "install board hooks: width=2, swizzle=rotate16, write enable/disable via BR6.WP"),
    (0xFFF15E64, "c1700_flash_write_enable", "void c1700_flash_write_enable(void);", "BR6 &= ~WP"),
    (0xFFF15E78, "c1700_flash_write_protect", "void c1700_flash_write_protect(void);", "BR6 |= WP"),
    (0xFFF15E8C, "c1700_flash_width", "int c1700_flash_width(void);", "two byte-wide chips side by side"),
    (0xFFF15E94, "c1700_flash_swizzle", "unsigned int __fastcall c1700_flash_swizzle(unsigned int v);", "rotate by 16"),
    (0xFFF15FCC, "flash_size_total", "int flash_size_total(void);", "probe flash at 0x60000000, returns total bytes"),
    (0xFFF15DFC, "flash_info_read", None, None),
    (0xFFF13CEC, "flash_probe_banks", None,
     "ID every bank; AMD parts (flag 0x80) get one descriptor per bank in a caller stack array of 8 (overflows if flash mirrors!)"),
    (0xFFF138F0, "flash_read_id", None, "Intel-style ID (0x90 w/o unlock) first, then AMD autoselect"),
    (0xFFF135DC, "amd_flash_cmd", None, "unlock + command; 0x90 autoselect, 0xA0 program, 0x80/0x30 erase, 0xF0 reset"),
    (0xFFF13548, "amd_flash_unlock", None, "0xAA @ 0x5555*w, 0x55 @ 0x2AAA*w, then command"),
    (0xFFF134F0, "flash_read_w", None, "read 1/2/4 bytes depending on width"),
    (0xFFF1351C, "flash_write_w", None, None),
    (0xFFF113A0, "cmd_cookie", None, "ROMMON 'cookie' command: hex dump or field editor"),
    # --- network / tftpdnld (docs/08-network-and-flash.md) ---
    (0xFFF1EF1C, "cmd_tftpdnld", None, "options -r (RAM only, then boot); validates IP_ADDRESS/IP_SUBNET_MASK/DEFAULT_GATEWAY/TFTP_SERVER/TFTP_FILE"),
    (0xFFF1D384, "tftp_download", "int tftp_download(void);", None),
    (0xFFF1E4A0, "tftp_image_received", None, "header check, size check, checksum (TFTP_CHECKSUM), then boot from RAM or copy to flash"),
    (0xFFF18878, "image_header_check", "int __fastcall image_header_check(void *img, int *size, unsigned int len);",
     "accepts compress (0x1F9D) or ELF: e_type 2, e_machine == g_platform_code, e_phnum 1, e_shoff 84 layout; size = 84+40*shnum+sum(sh_size)"),
    (0xFFF1DC90, "image_checksum_bad", "int __fastcall image_checksum_bad(void *img);",
     "last section starts with {0xFEEDFACE, usize, csize, ccksum, ucksum}: end-around-carry sum of csize bytes at +20"),
    (0xFFF1AEE0, "eth_init_fec_based", None, "(name from its own debug strings) FEC reset, MAC from cookie, R_BUFF_SIZE=1520, rings, IMASK=0 (polled)"),
    (0xFFF1A38C, "fec_transmit", None, "fill next TxBD (L|TC|R); write X_DES_ACTIVE only if it reads 0"),
    (0xFFF1A4DC, "fec_receive_poll", None, "consume RxBDs with E clear (len-4 = strip CRC), re-arm with E, poke R_DES_ACTIVE if 0"),
    (0xFFF19978, "pit_ticks_get", "unsigned int pit_ticks_get(void);", "network timeouts are measured in PIT ticks (4.03 ms)"),
    (0xFFF2040C, "getenv_ip_address", None, None),
    (0xFFF20434, "getenv_ip_subnet_mask", None, None),
    (0xFFF2045C, "getenv_default_gateway", None, None),
    (0xFFF20484, "getenv_tftp_server", None, None),
    (0xFFF204AC, "getenv_tftp_file", None, None),
    (0xFFF20580, "getenv_tftp_checksum", "int getenv_tftp_checksum(void);", "TFTP_CHECKSUM, default 1"),
    (0xFFF0336C, "exc_MachineCheck_unused", None, None) if False else (0xFFF0369C, "exc_MachineCheck", None, None),
    (0xFFF037D0, "exc_External", None, None),
    (0xFFF03904, "exc_Decrementer", None, "stage 1: stash SRR0/1 in SPRG2/3, rfi to stage 2 (0xFFF03948)"),
    (0xFFF0338C, "exc_Syscall", None, None),
    (0xFFF03A38, "exc_Unexpected", None, "shared by every vector without a dedicated handler"),
    # ---- found while chasing JIT code invalidations (docs ch. 17.10, 22) ----
    (0xFFF21C44, "checksum16_oc", "int __fastcall checksum16_oc(int sum, unsigned short *buf, int len);",
     "16-bit ones'-complement checksum (end-around carry). bootdev_checksum copies it onto its stack (0x800043A4 during boot) and runs it from RAM"),
    (0xFFF23C34, "bootdev_range", "unsigned int __fastcall bootdev_range(int dev, int offset, int len, unsigned int *addr);",
     "Device table at 0x800079BC (32-byte entries: +8 base, +16 limit, +20 flags): *addr = base+offset, length clipped to the limit"),
    (0xFFF23C88, "bootdev_read", "int __fastcall bootdev_read(int dev, int offset, void *buf, int len);",
     "Read through bootdev_range: copy with *0x8000791C (memcpy-like), or a special routine when entry flag 0x40000000 is set"),
    (0xFFF21C78, "bootdev_checksum", "int __fastcall bootdev_checksum(int dev, int offset, int len);",
     "Ones'-complement checksum of a boot device range (flash files, the IOS image before booting it). For len > 64 KB it "
     "copies checksum16_oc (52 bytes) into a local array on its stack (copy16), syncs the caches and runs it from RAM, "
     "much faster than from the 8-bit boot ROM: the 'code next to data' at 0x800043A4 the JIT had to cope with (docs ch. 22)"),
    (0xFFF08C80, "copy16", "void __fastcall copy16(const void *src, void *dst, int len);", "copy by halfwords"),
    (0xFFF16AE0, "sync_caches_for_code", "void sync_caches_for_code(void);",
     "dcache_flush_all + icache_invalidate_all: what real hardware needs before running freshly written code"),
    (0xFFF03F40, "dcache_flush_all", "void dcache_flush_all(void);", "sync, then flush every D-cache line through DC_CST/DC_ADR"),
    (0xFFF03EB8, "icache_invalidate_all", "void icache_invalidate_all(void);", "IC_CST = invalidate all (0x0C << 24), isync"),
]

for ea, name, proto, cmt in FUNCS:
    if not ida_funcs.get_func(ea):
        idc.create_insn(ea)
        ida_funcs.add_func(ea)
    ida_name.set_name(ea, name, FORCE)
    if proto:
        idc.SetType(ea, proto)
    if cmt:
        idc.set_func_cmt(ea, cmt, 1)

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------
GLOBALS = [
    (0xFFF256E4, "upm_table_sdram_init", "unsigned int upm_table_sdram_init[64];", "UPM RAM image used for SDRAM power-up"),
    (0xFFF257E4, "upm_table_sdram_run", "unsigned int upm_table_sdram_run[64];", "UPM RAM image for normal operation"),
    (0xFFF2F368, "console_baud_table", "unsigned int console_baud_table[8];", "index = (confreg>>11 & 3) | (confreg>>3 & 4)"),
    (0xFFF2EE8C, "p_nvram_base", "unsigned char *p_nvram_base;", "= 0x68000000"),
    (0x80005000, "g_reset_type", "int g_reset_type;", "argument of rom_init_main"),
    (0x800058C8, "g_cookie_loaded", "int g_cookie_loaded;", None),
    (0x800079BC, "bootdev_table", None, "boot device table, 32-byte entries (bootdev_range): +8 base, +16 limit, +20 flags"),
    (0x8000791C, "p_bootdev_copy", None, "copy routine used by bootdev_read"),
    (0x800058CC, "g_nvram_was_protected", "int g_nvram_was_protected;", None),
    (0x80005B6C, "g_pit_ticks", "unsigned int g_pit_ticks;", "incremented every PIT period (4.03 ms); ROMMON network timeouts use it"),
    (0x80006464, "g_platform_code", "int g_platform_code;", "51 (0x33) if cookie processor == 0x33 else 101; must equal ELF e_machine of boot images"),
    (0x800064E4, "g_cookie", "unsigned char g_cookie[132];", "RAM copy of the NVRAM cookie (magic, 128 B data, checksum)"),
    (0xFFF2FCAC, "cookie_field_table", None, "u16 count, ptr to 12-byte {name, len, nvals, value-names} descriptors"),
    (0xFFF2FE64, "flash_device_table", None, "20-byte entries {u16 mandev, u16, u32 chip size, u32 sector size, char* name, u32 flags}"),
]
# An early version of this script put g_cookie at 0x800063E4 (I mis-converted
# -2147457820); the real buffer is at 0x800064E4. Drop the stale name.
if ida_name.get_name(0x800063E4) == "g_cookie":
    ida_name.set_name(0x800063E4, "", ida_name.SN_NOWARN)
    idc.del_items(0x800063E4, 0, 132)

for ea, name, decl, cmt in GLOBALS:
    ida_name.set_name(ea, name, FORCE)
    if decl:
        idc.SetType(ea, decl)
    if cmt:
        idc.set_cmt(ea, cmt, 1)

# second stage of exception handlers reached through `rfi` (target in SRR0):
# pattern  lis rX,hi ; ori rX,rX,lo ; mtsrr0 rX ; ... ; rfi
def find_rfi_stage2():
    out = []
    ea = 0xFFF03000
    while ea < 0xFFF04000:
        w0, w1, w2 = (ida_bytes.get_dword(ea + 4 * i) for i in range(3))
        if (w0 >> 26) == 15 and (w1 >> 26) == 24 and (w2 & 0xFC1FFFFF) == 0x7C1A03A6:  # mtspr SRR0
            tgt = ((w0 & 0xFFFF) << 16) | (w1 & 0xFFFF)
            if 0xFFF00000 <= tgt < 0xFFF80000:
                out.append(tgt)
        ea += 4
    return out


for tgt in find_rfi_stage2():
    idc.create_insn(tgt)
    ida_funcs.add_func(tgt)
    if ida_name.get_name(tgt).startswith("sub_"):
        ida_name.set_name(tgt, f"exc_stage2_{tgt:08X}", FORCE)

# ---------------------------------------------------------------------------
# The monitor's command table (docs ch. 25): 55 entries of {char *name,
# handler, char *help, u32 flags}; flags 1 = privileged (refused, and hidden
# by `help`, until `priv`), 2 = `priv` itself (never listed). Every handler
# becomes cmd_<name>; handlers shared by two commands get both in a comment.
# ---------------------------------------------------------------------------
CMD_TABLE, CMD_COUNT = 0xFFF2EEC0, 55
ida_name.set_name(CMD_TABLE, "monitor_command_table", FORCE)
idc.set_cmt(CMD_TABLE, "55 x {name, handler, help, flags}; flags 1 = privileged, 2 = priv (docs ch. 25)", 1)
ida_name.set_name(0xFFF2F230, "monitor_command_count", FORCE)


def _cstr(ea):
    return (ida_bytes.get_bytes(ea, 80) or b"").split(b"\0")[0].decode(errors="replace")


_handlers = {}
for i in range(CMD_COUNT):
    e = CMD_TABLE + 16 * i
    name, handler, helptext, flags = (ida_bytes.get_dword(e + k) for k in (0, 4, 8, 12))
    _handlers.setdefault(handler, []).append((_cstr(name), _cstr(helptext), flags))
for handler, cmds in _handlers.items():
    if not ida_funcs.get_func(handler):
        idc.create_insn(handler)
        ida_funcs.add_func(handler)
    ida_name.set_name(handler, "cmd_" + cmds[0][0], FORCE)
    level = {0: "public", 1: "privileged (after priv)", 2: "never listed"}
    idc.set_func_cmt(handler, "; ".join(f"monitor command '{n}': {h} [{level.get(f, f)}]" for n, h, f in cmds), 1)

MONITOR = [
    (0xFFF070E0, "monitor_main", None,
     "Prompt loop. At power-on runs the MONRC monitor variable as a command line (a startup script); the prompt "
     "is the PS1 variable ('rommon ! > ', '!' = command number). docs ch. 25"),
    (0xFFF07BEC, "cmd_priv", None,
     "Password: 4 hex digits = sum of the cookie's first five 16-bit words (per box!); right -> g_monitor_flags |= 8"),
    (0xFFF0E138, "cmd_menu", None, "Main Diagnostic Menu: 17 hardware tests, Diagnostic Utilities Menu (cookie editor, MPC860 register dump...)"),
]
for ea, name, proto, cmt in MONITOR:
    if not ida_funcs.get_func(ea):
        idc.create_insn(ea)
        ida_funcs.add_func(ea)
    ida_name.set_name(ea, name, FORCE)
    if cmt:
        idc.set_func_cmt(ea, cmt, 1)
ida_name.set_name(0x800064DC, "g_monitor_flags", FORCE)
idc.set_cmt(0x800064DC, "bit 3 (0x08) = privileged commands unlocked (cmd_priv)", 1)

ida_auto.auto_wait()
print("c1700 names applied:", len(FUNCS), "functions,", len(GLOBALS), "globals, rfi stage2:", [hex(t) for t in find_rfi_stage2()])
