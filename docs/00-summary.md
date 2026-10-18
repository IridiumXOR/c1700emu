# Project summary — Cisco C1700 boot ROM emulator

One-page overview of everything done so far. Each section links to the
chapter with the full story.

## 1. Status

| Feature | State |
|---------|-------|
| Boot the real ROM (`CISCO_bootrom_AT27C040.bin`, System Bootstrap 12.2(1r)XE1) to `rommon 1 >` | ✅ ~0.1 s |
| ROMMON commands (`help`, `confreg`, `meminfo`, `dev`, `set`, `cookie`, `dir`, `reset`, ...) | ✅ |
| Console on the terminal (raw mode) and over telnet | ✅ |
| BREAK from the terminal (`Ctrl-] b`) or telnet (`send brk`) | ✅ |
| `reset` command (checkstop → hard reset, like the real chip) | ✅ |
| NVRAM persisted to a file, valid default cookie | ✅ |
| `tftpdnld -r`: download over the emulated Ethernet and run from RAM | ✅ |
| `tftpdnld`: write the image to flash | ✅ |
| Flash persisted to a file; next run autoboots the image | ✅ |
| Boot an IOS image from the internal flash, no TFTP (`--flash-file`) | ✅ ([chapter 11](11-internal-flash-and-models.md)) |
| Router model 1720/1721/1750/1751/1760 (`--model`, cookie PM ID) | ✅ |
| Intel StrataFlash x16 (28F640J3/28F128J3, 8–32 MB), as on real boards | ✅ (AMD pair still available) |
| GDB remote debugging | ✅ |
| CPU verified against QEMU (100 000 random instruction sequences) | ✅ |
| Real IOS 12.3(11)T2: `tftpdnld -r` → self-decompress → IOS setup dialog | ✅ ~6.5 min ([chapter 9](09-ios-images.md)) |
| Real IOS 12.4(8) on a 1760, from internal flash → `Router>`; `show version/interfaces/diag/flash:` | ✅ ~4.8 min ([chapter 10](10-ios-roadmap.md)); FastEthernet0/0 detected, link up |
| IOS networking: ARP + `ping 10.0.2.2` from IOS to the virtual host | ✅ `.!!!!` ([chapter 13](13-ios-networking.md)): FEC + IDMA + MMU |
| Idle detection + interpreter fast paths: idle router ~0% host CPU, interpreter +47% | ✅ IOS boot to `Router>` 318 → 153 s ([chapters 15](15-profiling-and-idle.md), [16](16-interpreter-optimisations.md)) |
| 8xx MMU (ITLB/DTLB, table-walk assist, protection, TLB exceptions) | ✅ IOS boots with translation on ([chapter 12](12-mmu.md)) |
| JIT compiler as a second CPU engine (`--cpu jit`, Cranelift) | ✅ ~9.8× the interpreter on the benchmark, CPU-bound IOS boot 217.8 → 38.4 s, same results ([chapter 17](17-jit.md)) |
| Network bridge to the real LAN | ✅ TAP devices and UDP tunnels (`--net`); two emulated routers ping each other ([chapter 18](18-real-networks.md)) |

## 2. Quick start

The Cisco files (boot ROM, IDA database, IOS images) are proprietary, so
they live in a separate private repository, cloned as `roms/`:

```bash
git clone git@github.com:IridiumXOR/c1700emu.git && cd c1700emu
git clone git@github.com:IridiumXOR/c1700emu-roms.git roms
cd emu && cargo build --release && cd ..
emu/target/release/c1700emu --confreg 0x2100      # 0x2100 = stay in ROMMON
```

```text
System Bootstrap, Version 12.2(1r)XE1, RELEASE SOFTWARE (fc1)
...
C1700 platform with 32768 Kbytes of main memory

rommon 1 >
```

* `Ctrl-] q` quits (NVRAM and flash are saved), `Ctrl-] b` sends a BREAK.
* `--tcp 2323` adds a telnet console: `telnet localhost 2323`.
* `--confreg 0x8000` shows all the ROM's diagnostic init messages.
* NVRAM and flash live in `c1700-nvram.bin` and `c1700-flash.bin`.
* Full option list: [chapter 7](07-running-and-debugging.md).

### Debugging the guest with GDB

```bash
emu/target/release/c1700emu --confreg 0x2100 --gdb 1234
gdb -ex "set architecture powerpc:common" -ex "set endian big" -ex "target remote :1234"
```

`set endian big` is required: without an ELF file GDB can't know the byte
order and shows every register byte-swapped.

### Network boot and flash

```bash
mkdir tftp && python3 tools/mkimage.py tftp/hello.bin "Hello from my own image!"
emu/target/release/c1700emu --confreg 0x2100 --tftp-root tftp
```

```text
rommon 1 > IP_ADDRESS=10.0.2.15
rommon 2 > IP_SUBNET_MASK=255.255.255.0
rommon 3 > DEFAULT_GATEWAY=10.0.2.1
rommon 4 > TFTP_SERVER=10.0.2.2
rommon 5 > TFTP_FILE=hello.bin
rommon 6 > tftpdnld -r                  # download and run from RAM
...
program load complete, entry point: 0x80008000, size: 0x94
Hello from my own image!
```

Plain `tftpdnld` (answer `y`) writes the image to flash. Quit, start again
with `--confreg 0x2102`, and ROMMON boots it from the saved flash file.
Details: [chapter 8](08-network-and-flash.md).

### Boot IOS straight from the internal flash

```bash
emu/target/release/c1700emu --model 1760 --ram 128 --confreg 0x2102 \
    --flash fl1760.bin --flash-file roms/c1700-adventerprisek9-mz.124-8.bin
```

`--flash-file` writes a Cisco flash file system with the image (saved to
`--flash`; later runs can drop the option). ROMMON finds the first file and
boots it. The C1700 has no PCMCIA or CompactFlash: the internal miniflash
is its only local boot device. Details: [chapter 11](11-internal-flash-and-models.md).

## 3. What the ROM taught us

### Identity and load address — [chapter 1](01-first-look.md)

* 512 KB AT27C040 dump, linked at **0xFFF00000**. The reset vector at
  0xFFF00100 is `b +0x2F00`. Base-address finders also suggested
  0x38610000, a false positive (0x3861 encodes `addi r3,r1,...`).
* CPU: **MPC860T** (it programs the Fast Ethernet Controller, which only the
  T variant has), board: Cisco 1720/1750, codename "ferrari".
* The ROM never writes the MMU control registers, so it runs untranslated.
  It uses many 8xx-only special registers (IMMR, EIE/EID/NRI, IC_CST/DC_CST,
  M_TW as a scratch register).

### Boot flow — [chapter 3](03-boot-flow.md)

1. `reset_entry`: IMMR = 0xFF000000 (on-chip peripherals), watchdog off, bus
   monitor on, test the on-chip dual-port RAM, first stack at 0xFF002800.
2. `dram_init_and_size`: programs the **UPM** (the 860's RAM-based memory
   controller micro-programs), runs the JEDEC SDRAM init sequence, then
   **sizes each bank by address aliasing**: it writes value X at address X
   for X = 64, 32, ... 2 MB, then reads address 0. Clearing the top bit of
   the address mask makes the RAM appear again at **0x80000000**, where
   ROMMON keeps its data.
3. `rom_init_main`: RAM test, NVRAM/cookie, interrupts (20 ms decrementer
   tick), clears all RAM **one byte at a time** (73 % of the boot), sizes
   NVRAM and flash, then the monitor.
4. `reset` command: sets PLPRCR[CSR], clears MSR[ME], reads address
   **0xBAD0ADD0**. That bus error becomes a machine check, then checkstop,
   then a hard reset.
5. Fatal errors blink the system LED N times (`fatal_error_blink`).

### Hardware map — [chapter 4](04-hardware-map.md)

| Address | Chip select | Device |
|---------|-------------|--------|
| 0x00000000 | CS1–CS4 (UPM) | SDRAM, sized at boot (also mirrored at 0x80000000) |
| 0x60000000 | CS6, 16-bit | flash: 2 × AMD Am29F016 side by side = 4 MB |
| 0x68000000 | CS7, 8-bit | NVRAM (32 KB parallel EEPROM) |
| 0x68020000 | CS7 | CPLD: LEDs, NVRAM write protect, board id |
| 0x68050000 | CS7 | **16550 UART** = the console |
| 0xFF000000 | internal | MPC860 peripherals (SIU, memory controller, timers, FEC) |
| 0xFFF00000 | CS0, 8-bit | boot ROM |

* The console is a plain **16550**, not one of the MPC860's own serial
  controllers. Those (the CPM) are only switched off, which removed the
  hardest part of an MPC860 emulator.
* **NVRAM**: cookie at 0x000 (magic 0x8D74, 128 bytes, 16-bit checksum) with
  a backup at 0x65A; confreg + ~confreg + 0xFEEDFACE at 0x090.
* **Cookie**: MAC at bytes 2–7; byte 8 must be **0x33** (the C1700
  processor code). The ROM's cookie editor recommends values for other
  fields too (PM ID 00 B2, 50 MHz, board config 04).
* **Flash**: a generic Intel/AMD driver with a 17-chip table. The board hooks
  say "two byte-wide chips"; write-protect is a bit in the chip-select
  register (BR6.WP).
* **Clocks**: timebase = 48 MHz / 16. The PIT (ROMMON's network timer) =
  48 MHz / 512 with a period of 4.03 ms.
* **PHY**: Level One / Intel **LXT971** (ID 0x0013).

### Network boot and images — [chapter 8](08-network-and-flash.md)

* `tftpdnld` reads IP_ADDRESS, IP_SUBNET_MASK, DEFAULT_GATEWAY, TFTP_SERVER,
  TFTP_FILE (and TFTP_CHECKSUM). The FEC driver polls with interrupts off.
* A bootable image is an ELF with:
  * `e_machine` = **0x33** (the platform code, not the PowerPC one; `readelf`
    calls it "Stanford MIPS-X");
  * one program header and the section headers at offset 84;
  * file size = 84 + 40 × sections + section sizes;
  * a last section starting with the Cisco trailer `0xFEEDFACE`, sizes, and
    an end-around-carry checksum.
* ROMMON loads every *section* to its address and jumps to the entry point.
* Flash files start with magic **0xBAD00B1E**, then length, checksum, flags,
  name, data.

## 4. How the emulator is built — [chapters 5](05-cpu-core.md), [6](06-emulator-architecture.md)

```text
emu/
├── ppc8xx/   PowerPC core of the MPC860: state, exceptions, SPRs, timebase,
│             instruction semantics. Decoding via the `powerpc` crate.
└── c1700/    MPC860 peripherals (mpc860/), board devices (board/), FEC DMA,
              virtual network (net/), machine loop, console, GDB stub, CLI
```

Main decisions:

* **Own interpreter in Rust** rather than Unicorn: the 8xx special
  registers, exceptions delivered to the ROM's own vectors, bus-error
  machine checks and timers all need exact behaviour.
* **Existing crates** wherever possible: `powerpc` (decoder and
  disassembler), `gdbstub`, `smoltcp`, `tftp-packet`, `crc32fast`, `clap`,
  `crossterm`, `env_logger`.
* **`Platform` trait** between the CPU and the board, following the boundary
  on the silicon. The CPU is tested on its own against QEMU.
* **Real chip-select decoding** (BR/OR registers, cached per 32 KB page)
  instead of a hard-coded memory map. The ROM's own memory sizing, aliasing
  and bus-error probing then work unchanged.
* **"Registers are memory, hooks keep them honest"**: peripheral registers
  are plain storage, plus hooks only where the ROM depends on behaviour (UPM,
  interrupts, PIT, reset cause, FEC...).
* **Virtual time**: 1 instruction = 1 cycle at 48 MHz, which makes runs
  deterministic. The network uses the same clock.

## 5. Bugs found along the way (and what they taught)

| Symptom | Cause | Lesson |
|---------|-------|--------|
| Crash at 0x62000000 during "Sizing flash" | my flash model mirrored 4 MB across the 64 MB window. The ROM kept finding "more banks" and overflowed an 8-entry array on its stack, overwriting the saved return address | the GDB stub found it in two commands (LR = PC = the last probe address) |
| GDB showed garbage registers | GDB assumed little-endian | `set endian big` |
| `tftpdnld` silent for tens of seconds after the transfer | PIT clock guessed (32 kHz ÷ 4), really 48 MHz ÷ 512: 11× too slow | derive clocks from the registers the ROM programs (SCCR, PLPRCR) |
| Decompiler showed `g_cookie[128]` | a stale label from an address I had mis-converted (`-2147457820` = 0x800064E4) | check the raw instruction (`lwz r9, 0x6464(r9)`) |
| Boot slower than the CPU benchmark | decoding opcode-31 instructions took 42 ns (linear table scan) | memo cache keyed by the instruction word |
| Flash write took ~70 s | half the time in the ROM's `mftb` busy-wait loops | fast-forward the clock through them (exact, never past an interrupt): ~4 s |

## 6. Performance

* CPU core alone: ~108 MIPS. Full board: ~28 MIPS.
* Boot to the prompt: **5.2 s** (6.9 s without fast-forward). What's left is
  mostly the ROM's byte-by-byte RAM clear; `--ram 16` halves it.
* Writing an image to flash: **~4 s** after answering `y`.
* `--profile` prints the hottest ROM addresses. That is how both speedups
  were found.

## 7. Verification

| Test | What it proves |
|------|----------------|
| `ppc8xx/tests/qemu_diff.rs` | random instruction sequences give the same registers in our CPU and in `qemu-ppc` (400 per run by default, 100 000 checked) |
| `ppc8xx/tests/semantics.rs` | 48 focused tests: carries, overflow, rotates, branches, string ops, exceptions, 8xx special registers |
| `c1700/tests/boot.rs` | the real ROM boots to the prompt and answers `meminfo` and `dev` |
| `c1700/tests/net.rs` | TFTP boot from RAM, write to flash, power-cycle, autoboot from flash |
| unit tests | cookie checksum, flash commands, flash file persistence |

Run everything with `cd emu && cargo test --release` (about 30 s).

## 8. Tools

| File | Purpose |
|------|---------|
| `tools/ida/mpc860_setup.py` | IDA: IMMR segment and `IMMR_t` struct, vector table, SPR comments |
| `tools/ida/c1700_names.py` | IDA: 77 functions and 12 globals named, typed and commented, with the evidence |
| `tools/ida/decompile.py` | headless IDA (idalib): decompile, disassemble, apply scripts, save |
| `tools/find_refs.py` | finds code that builds an address with `lis`+`addi` (who uses this string?) |
| `tools/mkimage.py` | builds a minimal bootable C1700 image that prints a message |
| `tools/ciscoimg.py` | checks a real IOS image like the ROM does (header, trailer, checksums), unzips it, identifies the CPU ([chapter 9](09-ios-images.md)) |

The annotated database is `roms/CISCO_bootrom_AT27C040.bin.i64` (in the
private `c1700emu-roms` repository, see §2).

## 9. Next steps

* **IOS**: reach the router from the host (bridge/port forward, telnet in),
  the slot registers (0x68030000/40000/80000/B0000) and WIC cards (SCC/HDLC),
  the remaining small inaccuracies (5 machine checks),
  and snapshots to skip the ~5-minute boot
  ([chapter 10](10-ios-roadmap.md)).
* **Bridge to the host network** (e.g. UDP NAT) to TFTP from a real server.
* **Interpreter speed**: a fast path for instruction fetch.
* More CPLD registers, and the remaining ROMMON commands (`xmodem`, `dnld`)
  tested.

## 10. Chapter map

| # | Chapter |
|---|---------|
| 1 | [First look at the ROM](01-first-look.md) |
| 2 | [IDA setup](02-ida-setup.md) |
| 3 | [Boot flow](03-boot-flow.md) |
| 4 | [Hardware map](04-hardware-map.md) |
| 5 | [CPU core](05-cpu-core.md) |
| 6 | [Emulator architecture](06-emulator-architecture.md) |
| 7 | [Running & debugging](07-running-and-debugging.md) |
| 8 | [Network boot & flash](08-network-and-flash.md) |
| 9 | [Checking IOS images](09-ios-images.md) |
| 10 | [Towards full IOS emulation](10-ios-roadmap.md) |
| 11 | [Internal flash & router model](11-internal-flash-and-models.md) |
| 12 | [The MPC8xx MMU](12-mmu.md) |
| 13 | [IOS networking](13-ios-networking.md) |
| 14 | [Speed and JIT](14-speed-and-jit.md) |
| 15 | [Profiling and idle detection](15-profiling-and-idle.md) |
