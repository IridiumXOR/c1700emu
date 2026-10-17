# 3. Boot flow

What happens between power-on and `rommon 1 >`, in order. Function names are
the ones applied by `tools/ida/c1700_names.py`.

```
0xFFF00100  vec_0100_Reset ──b──► 0xFFF03000 reset_entry          (assembly, no RAM yet)
                                       │
                                       ├─ soft reset only? ─► soft_reset_save_context (debug "break")
                                       ▼
                               dram_init_and_size(reset_type)     (C, stack in DPRAM)
                                       │   siu_init, uart_init, banner, UPM, SDRAM init, sizing
                                       ▼
                               rom_init_main(reset_type, 0)       (C)
                                       │   RAM test, bss, NVRAM/cookie, interrupts, caches,
                                       ▼   memory init, NVRAM/flash sizing
                               monitor main loop  →  "rommon 1 >"  (or autoboot)
```

## 3.1 `reset_entry` (0xFFF03000) — no RAM, no stack

| Step | Code | Why |
|------|------|-----|
| 1 | `mtspr SPRG1,r10; SPRG2,r11; SPRG3,CR` | preserve state in case this is a *soft* reset (used to inspect a hung system) |
| 2 | `MSR = 0x1042` | ME (machine checks on) + IP (vectors at 0xFFF00000) + RI |
| 3 | `DER = 0` | no debug-mode entry |
| 4 | `IMMR = 0xFF000000` | relocate the on-chip peripherals |
| 5 | `CPCR = 0x8000` | reset the CPM (communication co-processor) |
| 6 | `SYPCR = 0xFFFFFF88` | software watchdog **off**, bus monitor **on** (accesses to nowhere end in a machine check instead of hanging) |
| 7 | read `RSR` | `RSR & 0xC0000000 == 0x40000000` (external *soft* reset only) → save the CPU context to RAM and enter the debugger path |
| 8 | test DPRAM | write/read "data equals address" over the 4 KB at IMMR+0x2000 and 1 KB at IMMR+0x3C00, then zero them. Failure → `fatal_error_blink(2 or 3)` |
| 9 | `IC_CST/DC_CST` | disable, unlock all, invalidate all — caches off until memory is known |
| 10 | `r1 = 0xFF002800` | **first stack in the on-chip dual-port RAM**; r13/r2 = small-data base pointers in ROM |
| 11 | `RSR & CSRS` | checkstop reset (i.e. the `reset` command) → reset_type 2, else 1; call `dram_init_and_size`, then `rom_init_main` |

`fatal_error_blink(n)` never returns: it blinks the system LED (CPLD
register 0, bit 7) *n* times, pauses, repeats. That is how a board with no
console tells a technician "DRAM controller error = 6 blinks".

## 3.2 `siu_init` (0xFFF06D7C)

Masks every interrupt source (SIMASK, CIMR, PER...), programs the clocks
(SCCR/PLPRCR are *keyed*: write `0x55CCAA33` to the matching key register
first), and on a hard boot sets up the chip selects — the memory map of
chapter 4:

```
BR0 = 0xFFF00401  OR0 = 0xFFF005A6   ROM,   1 MB window, 8-bit port
BR1..4 = 0x80/0xC0 (invalid yet)     SDRAM banks, UPMA / UPMB
BR6 = 0x60000801  OR6 = 0xFC000190   flash, 64 MB window, 16-bit port
BR7 = 0x68000401  OR7 = 0xFFF001A8   I/O bus, 1 MB window, 8-bit port
```

## 3.3 `dram_init_and_size` (0xFFF0453C)

1. Reads the NVRAM confreg (if `0xFEEDFACE` + complement check pass, else
   0x101) and calls `uart_init` with the baud rate encoded in confreg bits
   12:11 and 5 → table `[9600, 4800, 1200, 2400, 19200, 38400, 57600, 115200]`.
   Then prints the first banner directly on the UART (no printf yet).
2. **UPM programming.** The MPC860 has no fixed DRAM controller: two
   *User-Programmable Machines* run 64-word micro-programs that define every
   bus cycle. The ROM writes `upm_table_sdram_init` into UPMA and UPMB via
   MDR/MCR, sets MAMR/MBMR, and **reads all 64 words back** —
   mismatch → "ERROR: Can not set up DRAM contoller" + 6 blinks.
3. **SDRAM power-up, per bank (CS1-CS4).** Map the bank at address 0 and use
   MCR "run pattern" commands: precharge-all (pattern 0x20), 8 × refresh
   (0x30), mode-register-set (0x29 with `MAR = 0x88`), 4 more refreshes.
   That is the JEDEC SDRAM init sequence. Then load the normal-operation UPM
   table `upm_table_sdram_run`.
4. **Presence test.** Each bank mapped at 0 as a 64 MB window; write
   address-as-data to 16 words and read back. An empty socket reads back
   garbage → absent.
5. **Size by aliasing.** Write the value `X` at address `X` for X = 64 MB,
   32, 16, 8, 4, 2 MB (in that order, 64 MB written at address 0), then read
   address 0. A bank of size S ignores the address lines above S, so every
   write at a multiple of S lands on address 0; the last one to land there
   was written with **S** — the bank size. (The decompiler shows a constant
   here because it assumes memory doesn't change.)
6. **Layout.** Banks are placed contiguously from 0, largest first, with
   `ORx = (-size & 0x7FFFF9FF) | 0x600`. Clearing the top bit of the address
   mask means address bit 0 is not compared: **the RAM also appears at
   0x80000000**. ROMMON's data and bss live at 0x80005000+ — through that
   alias, no MMU involved.
7. Nothing found → "ERROR: No system memory detected" + 5 blinks.

## 3.4 `rom_init_main` (0xFFF060D8)

Messages in quotes are printed only when confreg bit 15 (0x8000,
"diagnostic mode") is set; try `c1700emu --confreg 0x8000`.

1. Read and clear RSR; `cpld_init`.
2. "Testing lower main memory" — data-equals-address, checkerboard and
   inverse checkerboard over the first 32 KB; failure prints
   `Bad RAM at location 0x... : wrote 0x..., read 0x...` + 9 blinks.
3. "Clearing lower 4K memory for cache initialization" (actually 64 KB) and
   "Clearing bss" (0x80005000–0x80007A48).
4. "Reading monitor variables from NVRAM", cookie (`cookie_get`): primary at
   NVRAM+0, else backup at NVRAM+0x65A, else "Warning: Cookie information is
   corrupt". Processor byte must be 0x33.
5. If PIPR bit 3 (a strap/jumper on a PCMCIA input pin) is clear on a
   power-on boot, the console baud bits are forced back to 9600.
6. "Enabling interrupts" (`enable_interrupts`: decrementer = 20 ms tick,
   SIMASK = IRQ1 = the UART), "Enabling instruction cache".
7. "Initializing main memory" — `bzero` of **all** DRAM, one byte at a time
   (`bzero_bytes`: 4 instructions per byte). With 32 MB that is 134 M
   instructions, 73 % of the whole boot (measured with `--profile`).
8. "Sizing NVRAM" (32 KB), "Sizing flash" (`flash_size_total`, see chapter 4),
   then BR6/OR6 are adjusted to the flash found; the FEC's MII is set up and
   the PHY ID is read (an LXT971 gets its LED register programmed).
9. "Exiting init" → the monitor prints the full banner and either autoboots
   (confreg boot field 1–15) or shows `rommon 1 >` (boot field 0).

## 3.5 The `reset` command

`system_reset_via_checkstop` (0xFFF03FEC) sets PLPRCR[CSR] (checkstop reset
enable) and clears MSR[ME] (and MSR[DR]). Then it provokes a machine check on
purpose:

```asm
fff04078  lis   r11, 0xBAD0
fff0407c  ori   r11, r11, 0xADD0     ; r11 = 0xBAD0ADD0  ("bad addr")
fff04080  mtmsr r10                  ; ME = 0
fff04084  lbz   r10, 0(r11)          ; nothing decodes 0xBAD0ADD0 → bus monitor
                                     ; timeout → TEA → machine check with ME=0
```

A machine check with ME=0 puts the core into *checkstop*, which the SIU
converts into a hard reset with RSR[CSRS] set. On the way back up,
`reset_entry` sees CSRS and passes reset_type 2, which skips the RAM test.
The emulator implements exactly this chain (chapter 6).
