# 7. Running & debugging

## 7.1 Build and run

```bash
cd emu
cargo build --release
cd ..
emu/target/release/c1700emu                    # ROM found in the current or parent dir
```

First boot with a fresh NVRAM prints "monitor nvram area is corrupt ... using
default values" (true — there is no environment yet) and, with the default
confreg 0x101, tries to **autoboot** from the empty flash and resets in a
loop. Either type a key / send BREAK to stop it, or start in ROMMON:

```bash
emu/target/release/c1700emu --confreg 0x2100   # boot field 0 = stay in ROMMON
emu/target/release/c1700emu --confreg 0x8000   # diagnostic mode: all init messages
```

`--confreg` writes the value into the NVRAM file before booting (you can also
use the ROMMON `confreg` command; changes persist in `c1700-nvram.bin`).

| Option | Meaning |
|--------|---------|
| `--rom PATH` | ROM image (default `roms/CISCO_bootrom_AT27C040.bin`) |
| `--nvram PATH` | NVRAM file, created if missing, saved every 2 s and on exit |
| `--flash PATH` | flash image file (default `c1700-flash.bin`): loaded at start, guest writes saved back |
| `--model M` | chassis 1720 (default), 1721, 1750, 1751, 1760: sets the cookie's mainboard PM ID (chapter 11) |
| `--serial S` | processor board serial in the cookie, Cisco format `LLLYYWWSSSS` (default for a new NVRAM `EMU1234C170`; chapter 4) |
| `--flash-size MB` | internal flash: 4 (2 × AMD Am29F016), 8, 16, 32 (Intel 28F640J3/28F128J3 x16); default 32 for 1751/1760, else 16 |
| `--flash-file FILE` | (repeatable) build a fresh flash file system with these files, saved to `--flash`; confreg 0x2102 boots the first |
| `--tftp-root DIR` | serve DIR over TFTP on the virtual network (for `tftpdnld`) |
| `--net-server-ip`, `--net-gateway`, `--net-prefix` | virtual network addresses (default 10.0.2.2, 10.0.2.1, /24) |
| `--no-net` | leave the Ethernet port unplugged |
| `--no-fast-forward` | execute the ROM's delay loops instead of skipping them |
| `--ram MB` | SDRAM size, split into power-of-two banks (default 32) |
| `--board-id N` | CPLD board-type strap |
| `--confreg HEX` | set the configuration register in NVRAM |
| `--tcp PORT` | also serve the console on telnet |
| `--aux-tcp PORT` | serve the AUX port (DUART channel B) on telnet; without it AUX output goes to the log |
| `--watch START:LEN` | (hex) log every bus access to a physical range with the PC, or `DMA` — a watchpoint (chapter 13) |
| `--no-stdio` | don't use the terminal (TCP only) |
| `--gdb PORT` | wait for GDB before the first instruction |
| `-n N` | stop after N instructions |
| `--trace` | log every instruction (use with `--log`, very verbose) |
| `--profile` | per-address execution counts of the ROM, printed at exit |
| `--log PATH`, `--log-level L` | logging (`RUST_LOG` also works, e.g. `RUST_LOG=c1700emu::board=debug`) |

## 7.2 The console

On a terminal the emulator switches to raw mode: keys go straight to the
emulated 16550.

* `Ctrl-]` then `q` — quit (NVRAM is saved)
* `Ctrl-]` then `b` — send a serial BREAK
* `Ctrl-]` twice — send a literal `Ctrl-]`

With `--tcp 2323`: `telnet localhost 2323`. In telnet, `Ctrl-]` then
`send brk` sends a BREAK. Scripts can use plain sockets (send `\r` for Enter).

Piping works too, handy for automation:

```bash
(sleep 8; printf 'meminfo\r'; sleep 1) | c1700emu --confreg 0x2100 -n 300000000
```

## 7.3 Debugging the guest with GDB

```bash
emu/target/release/c1700emu --confreg 0x2100 --gdb 1234
```

then in another terminal:

```text
$ gdb
(gdb) set architecture powerpc:common
(gdb) set endian big            # needed: without an ELF, GDB can't know
(gdb) target remote :1234
(gdb) break *0xfff060d8         # rom_init_main
(gdb) continue
(gdb) info registers
(gdb) x/8i $pc
(gdb) stepi
```

Notes:

* Breakpoints are kept in the emulator (no code patching), so they work in ROM.
* `Ctrl-C` in GDB stops the CPU; the console keeps working while attached.
* Memory reads from GDB have no side effects (reading the UART data
  register from GDB does not consume a received character).
* The FPU/AltiVec registers GDB shows are always 0: the MPC860 has neither.
* When GDB detaches, emulation continues normally.

Real example from development (chapter 4): the ROM crashed at 0x62000000.

```text
(gdb) break *0x62000000
(gdb) continue
Breakpoint 1, 0x62000000 in ?? ()
(gdb) info registers pc lr r1
pc  0x62000000   lr 0x62000000   r1 0xff0027c0
```

LR == PC means a `blr` returned to a corrupted saved LR — which led straight
to the flash-probing stack overflow.

## 7.4 Finding hot spots and tracing

```bash
c1700emu --confreg 0x2100 -n 183300000 --profile < /dev/null
execution profile: 183398691 instructions in ROM
  0xfff16800..+0x100     134088369   73.1%    <- bzero_bytes
  0xfff12a00..+0x100      28803338   15.7%    <- delay_us
  0xfff12900..+0x100      19353363   10.6%    <- delay_ms
```

`--trace --log trace.txt -n 100000` writes `pc: word  disassembly` for every
instruction (disassembly by the `powerpc` crate).

## 7.5 Tests

```bash
cd emu
cargo test --release                       # everything
cargo test --release -p c1700emu           # boots the ROM; tftpdnld to RAM and flash, autoboot
QEMU_DIFF_SEQUENCES=20000 cargo test --release -p ppc8xx --test qemu_diff
```

## 7.5b Why is the boot so quiet?

With a normal configuration register (0x2100, 0x2102...) ROMMON prints only
the banner, warnings and the memory size. That's the ROM's design, not
missing output: `rom_init_main` prints every init step only when confreg
**bit 15 (0x8000, "diagnostic mode")** is set — the code is literally
`if (confreg & 0x8000) puts("Testing lower main memory - ...")`.

```bash
c1700emu --confreg 0xA100    # 0x8000 diagnostics + 0x2100 stay in ROMMON
c1700emu --confreg 0xA102    # diagnostics + autoboot
```

(or type `confreg 0xA100` at the ROMMON prompt and `reset`; it is saved in
NVRAM.) You then get "Testing lower main memory - data equals address",
"Clearing bss", "Reading cookie", "Initializing main memory", "Sizing
flash", "Exiting init", etc.

The second banner ending in "Compiled Fri 06-Apr-01 11:27 by ccai" is never
printed by ROMMON: neither that string nor its table slot (0xFFF3047C) is
referenced by any code (checked with IDA xrefs, `lis`/`addi` scans and
pointer scans). It sits right after the first banner in the ROM image.

## 7.6 Network boot

See chapter 8 for a complete `tftpdnld` session (`tools/mkimage.py` builds
a test image). Frame-level logging: `RUST_LOG=c1700emu::net=debug --log net.log`.

## 7.7 Known limitations / next steps

* The virtual network only offers ARP, ping (since chapter 13) and a read-only TFTP server
  (no bridging to the host network).
* No MMU emulation (identity mapping): fine for ROMMON, and enough for IOS
  12.3(11)T2 to boot to its setup dialog (chapter 9), but not guaranteed for
  everything IOS does.
* `xmodem`/`dnld` serial downloads should work through the console but are
  untested.
