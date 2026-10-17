# 4. Hardware map

Everything the ROM touches, i.e. the specification of the emulator.

## 4.1 Physical address map (after `siu_init` and memory sizing)

| Address | Size / window | Chip select | Device | Port |
|---------|---------------|-------------|--------|------|
| 0x00000000 | sized (e.g. 32 MB) | CS1..CS4 (UPMA/UPMB) | SDRAM banks, largest first | 32-bit |
| 0x60000000 | 64 MB window | CS6 (GPCM) | boot flash: 2 × AMD Am29F016 = 4 MB | 16-bit |
| 0x68000000 | 1 MB window | CS7 (GPCM) | I/O bus: NVRAM, CPLD, UART | 8-bit |
| 0x80000000 | alias | CS1..CS4 | same SDRAM (OR AM bit 0 cleared) | |
| 0xFF000000 | 64 KB (16 KB used) | internal | IMMR: on-chip peripherals + DPRAM | |
| 0xFFF00000 | 1 MB window | CS0 (GPCM) | boot EPROM 512 KB (mirrored) | 8-bit |

How the MPC860 decodes an address (MPC860UM ch. 15): the internal IMMR
window wins; otherwise bank *n* is selected when `BRn.V = 1` and
`(addr & ORn.AM) == (BRn.BA & ORn.AM)`, with AM/BA = bits 0-16 (32 KB
granularity). No bank → no transfer acknowledge → the bus monitor (enabled
by SYPCR[BME]) asserts TEA → **machine check**. ROMMON's `berrscan` command
and the `reset` command both rely on that.

At reset, before `siu_init`, CS0 has `OR0.AM = 0`: the boot ROM answers at
*every* address, which is why the reset vector can be fetched at all.

## 4.2 On-chip peripherals actually used (IMMR = 0xFF000000)

Found by applying the `IMMR_t` structure offsets in IDA and listing every
field referenced (chapter 2):

| Block | Registers | Use |
|-------|-----------|-----|
| SIU | SYPCR, SIMASK, SIEL, SIPEND/SIVEC (via handler) | watchdog off, bus monitor on; only **IRQ1** unmasked |
| Memory controller | BR0-7/OR0-7, MCR, MDR, MAR, MAMR, MBMR, MPTPR, MSTAT | chip selects, UPM programming, SDRAM init |
| Timers | TBSCR (TB enable), PISCR/PITC (PIT), RTCSC | PIT = polled 4.03 ms tick (`g_pit_ticks`), the time base of ROMMON's network timeouts. Its clock comes from SCCR: RTSEL = EXTCLK, RTDIV = ÷512, and EXTCLK = SYSCLK (PLPRCR MF = 0) → 93.75 kHz |
| Clocks/reset | SCCR, PLPRCR, RSR + key registers (`0x55CCAA33`) | reset cause, checkstop-reset enable |
| CPM | CPCR, CICR/CIMR, ports A-D, timers, I2C/SPI/SDMA | **only reset/disabled** — the CPM serial channels are not used! |
| FEC (860T) | ECNTRL, IEVENT, IMASK, MII_DATA, MII_SPEED, buffer registers | MII management to the PHY; frame DMA for `tftpdnld` |

The CPM being unused is great news: modelling SMC/SCC buffer descriptors and
microcode would have been the hardest part of an MPC860 emulator.

## 4.3 The console: a 16550 on the I/O bus

Recognised from the polling pattern repeated ~190 times in the ROM:

```c
while ((*(u8*)0x68050005 & 0x20) == 0) ;   // LSR.THRE: transmitter empty?
*(u8*)0x68050000 = ch;                      // THR
```

LSR at offset 5, bit 5 = THRE, bit 0 = DR (data ready), registers at stride 1:
a textbook **16550**. `uart_init` confirms: DLAB, divisor = 3 686 400 /
(16 × baud) — 3.6864 MHz is the classic UART crystal — 8N1, DTR|RTS, FIFO.
IER = 4 enables the *line status* interrupt (that's how a BREAK from the
terminal is caught) and the interrupt output is wired to IRQ1 — the only
source ROMMON unmasks.

## 4.4 The I/O bus (CS7, 8-bit)

| Offset | Device | Evidence |
|--------|--------|----------|
| 0x00000 | NVRAM, 32 KB parallel EEPROM | `nvram_write` does 32-byte page writes + data polling; "NVRAM size: 32KB" |
| 0x20000 | CPLD | LEDs, NVRAM write protect, board id, misc control |
| 0x50000 | 16550 UART | see above |

**NVRAM layout**

| Offset | Size | Content |
|--------|------|---------|
| 0x000 | 132 | **cookie** (primary): u16 magic 0x8D74, 128 B data, u16 checksum (16-bit sum of halfwords) |
| 0x090 | 2 | configuration register |
| 0x092 | 2 | ~configuration register |
| 0x094 | 4 | 0xFEEDFACE (confreg valid) |
| ...   |   | ROMMON environment ("monitor variables"), written by `sync` |
| 0x65A | 132 | cookie backup copy |

**Cookie data**, from the ROMMON cookie editor's field table at
0xFFF2FCAC: entries of (name, length), offsets cumulative. Decoding it
programmatically gives:

| Offset | Len | Field (ROM's own description) |
|--------|-----|-------------------------------|
| 0x00 | 1 | version |
| 0x01 | 1 | vendor (recommended 01 = cisco) |
| 0x02 | 6 | Ethernet MAC address |
| 0x08 | 1 | processor (recommended 33) — **must be 0x33** or "Unknown Processor type found in cookie" |
| 0x09 | 1 | NVRAM size (32K = 00, 64K = 01) |
| 0x0A | 1 | CPU speed (50 MHz = 01) |
| 0x0B | 1 | unused |
| 0x0C | 2 | on-board PM ID (1720: 00 B2) — the router model, chapter 11.5 |
| 0x0E | 2 | MAC addresses allocated |
| 0x10 | 8 | (unnamed) |
| 0x18 | 11 | (unnamed) — processor board serial, see below |
| 0x23 | 2 | deviation |
| 0x25 | 8 | (unnamed) |
| 0x2D | 1 | board config (recommended 04) |

**The serial** ("Processor board ID" in IOS's `show version`) is a Cisco
serial `LLLYYWWSSSS` (plant, year, week, unit) packed into 9 bytes at 0x18:
3 characters, YY and WW as one byte each, 4 characters. IOS 12.4(8)
rebuilds it at 0x8012377c, copying alphanumeric bytes 0x18–0x1A,
`sprintf("%02x")` of 0x1B and 0x1C, bytes 0x1D–0x20, then appends
`" (%lu)"`, a number computed from that string. Dynamips' default cookie
has `46 54 58 09 45 57 30 4D 59` there, i.e. "FTX0945W0MY". The emulator
writes `EMU1234C170` into a fresh cookie; `--serial` sets another one.

*Correction:* an earlier version of this table listed "interfaces" at 0x00,
a BCD serial at 0x10 and a "cookie version" at 0x1F. That was a generic
Cisco cookie layout, not this ROM's table. IOS showed the consequence:
`Processor board ID 0000 (1314672220)`, built from empty bytes.

**CPLD registers known so far**

| Reg | Use |
|-----|-----|
| 0x00 | bit 7: system LED (blinked by `fatal_error_blink`) |
| 0x04 | bits 0-2: board type (2, 3, 4 special-cased) |
| 0x14 | bit 0: NVRAM write protect; bit 2: flag set at every boot |
| 0x18 | bit 0: set before resetting the CPM |
| 0x24, 0x48 | initialised to 0xFF / 0xD8; 0x48 bit 3 = second LED on board type 2 |
| 0x4C | (IOS on the 1760) Xilinx FPGA configuration port: PROG_B 0x40, CCLK 0x20, DIN 0x10, INIT_B 0x08, DONE 0x02 — chapter 11.6 |

## 4.5 Flash (CS6, 16-bit)

The flash driver is generic (Intel and AMD parts, 17 entries in
`flash_device_table` at 0xFFF2FE64: Intel 28F0x0/28F008SA/016SA/S5/J3/J5,
AMD 28F010/020, Am29F080, Am29F016). The board-specific part is
`c1700_flash_setup`:

* **width = 2**: the bank is 16 bits wide. That fits two byte-wide chips
  side by side (chip 0 on the even byte lane, chip 1 on the odd one, each
  seeing `offset / 2`), my first model, or **one x16 chip**, which is what
  real boards carry (see the correction below and chapter 11);
* **swizzle** = rotate a 32-bit ID read by 16;
* **write enable** = clear BR6.WP (the memory controller's write-protect
  bit for the bank), write disable = set it.

ID detection first tries an Intel-style read (plain 0x90), then AMD
autoselect (unlock `0xAA@0x5555`, `0x55@0x2AAA`, `0x90@0x5555`, chip
addresses). With 2 × Am29F016 the AMD path yields mandev `0x01AD`; with one
x16 Intel 28F128J3 the Intel path yields `0x8918`. The emulator supports both
(`--flash-size 4` = AMD pair, 8/16/32 = Intel, default Intel).

**Correction / lesson (big-endian macros).** For a while I believed the
Intel path could not succeed on this board. I had read Hex-Rays' `BYTE1(x)`
as bits 8-15; on a *big-endian* target the macro means "the byte at memory
offset 1", i.e. bits 16-23. Redone correctly, one x16 Intel StrataFlash chip
answers the ID read with `0x0089_00DD`, the board's rotate-16 hook makes it
`0x00DD_0089`, and the ROM forms mandev `0x89DD` — e.g. `0x8918` "INTEL
28F128J3A". That is the configuration real C1700s use (Dynamips: "1x28F320 in
word mode"), and the emulator now defaults to it (chapter 11). Lesson: when a
decompiler macro decides your conclusion, check its definition for the
target's endianness — or the raw instructions.

**A lesson learned with the emulator:** for AMD parts (table flag 0x80)
`flash_probe_banks` creates one 40-byte descriptor *per bank* in an
8-entry array on its caller's stack and keeps probing at `base + n × bank
size` until the ID read fails. My first flash model mirrored the 4 MB across
the whole 64 MB window, so every probe "found" another bank; the 9th
descriptor overwrote the saved LR and the ROM returned to 0x62000000
(found with the GDB stub: `break *0x62000000`, then `info registers` showed
LR = PC = the last probe address). On the real board nothing drives the bus
above the fitted flash, so the emulator now returns 0xFF there.

The Cisco flash *file system* written by `tftpdnld` (magic 0xBAD00B1E, file
length, checksum, flags, name, data) is described in chapter 8.

## 4.6 FEC / PHY

`fec_mii_write`/`fec_mii_read` build IEEE 802.3 clause-22 management frames
(`0x6002_0000 | reg<<18` = read PHY 0 register *reg*) in MII_DATA and wait
for IEVENT[MII]. The ROM reads register 2 (PHY ID1); if it is `0x0013`
(Level One / Intel) it writes 0x4732 to register 20, the LED configuration
register of an **LXT971**. The emulator answers as an LXT971 with link up.
