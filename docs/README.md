# Cisco C1700 ROMMON — reverse engineering & emulation notes

These notes document, in order, how the boot ROM `CISCO_bootrom_AT27C040.bin`
was analysed and how the emulator in `emu/` was built from that analysis.
Each chapter explains **what** was found, **how** it was found (so you can
reproduce it in IDA), and **what it means for the emulator**.

Start with the **[project summary](00-summary.md)** for a one-page overview.

| # | Chapter | Content |
|---|---------|---------|
| 1 | [First look at the ROM](01-first-look.md) | identification, load address, reset vector, SPR census, vector table |
| 2 | [IDA setup](02-ida-setup.md) | loading the ROM, the `mpc860_setup.py` annotation script, conventions |
| 3 | [Boot flow](03-boot-flow.md) | reset handler, SIU init, DRAM controller (UPM) + memory sizing, C init |
| 4 | [Hardware map](04-hardware-map.md) | chip selects, IMMR peripherals used, board devices (UART, NVRAM, CPLD, flash) |
| 5 | [CPU core](05-cpu-core.md) | the `ppc8xx` interpreter, 8xx quirks, differential testing vs QEMU |
| 6 | [Emulator architecture](06-emulator-architecture.md) | crates, `Platform` boundary, bus decoding, virtual clock, devices |
| 7 | [Running & debugging](07-running-and-debugging.md) | command line, console, GDB stub (also with the JIT), tracing, tests, `--config` files and what NVRAM keeps (confreg, ROMMON variables, startup-config) |
| 8 | [Network boot & flash](08-network-and-flash.md) | FEC DMA, virtual network + TFTP server, Cisco image format, flash file system, PIT clock, fast-forward |
| 9 | [Checking IOS images](09-ios-images.md) | why `file` says "MIPS-X", proving images are MPC8xx code, `-mz` = loader + ZIP |
| 10 | [Towards full IOS emulation](10-ios-roadmap.md) | what IOS uses that is still missing, with the evidence for each item |
| 11 | [Internal flash & router model](11-internal-flash-and-models.md) | the C1700's only boot device, Intel x16 flash, flash file system, `--flash-file`, PM ID / `--model` |
| 12 | [The MPC8xx MMU](12-mmu.md) | software-loaded TLBs, table-walk assist, how IOS pins its memory map, the emulation and its tests |
| 13 | [IOS networking](13-ios-networking.md) | why received frames got stuck, the `--watch` tool, the IDMA copy engine, the first ping |
| 14 | [Speed and JIT](14-speed-and-jit.md) | where an interpreter spends its time, how a JIT works, what it would take here, the plan |
| 15 | [Profiling and idle detection](15-profiling-and-idle.md) | guest and host profiles, how IOS idles, the idle-PC detector, results |
| 16 | [Making the interpreter faster](16-interpreter-optimisations.md) | the benchmark, each optimisation with its measured effect, exact ROM accelerators, what is left |
| 17 | [A JIT compiler](17-jit.md) | `--cpu jit`: Cranelift translation of hot blocks, exactness rules, self-modifying code, verification (incl. mutation testing), regions, chaining, background compiling, `--cpu jit-check`, breakpoints as hooks; ~700 MIPS |
| 18 | [Real networks](18-real-networks.md) | `--net tap:`/`udp:`: TAP devices, UDP tunnels between emulators (two IOS routers pinging), `--mac` |
| 19 | [Slots, IO FPGA, machine checks](19-slots-iofpga-machine-checks.md) | the WIC/VIC slot windows and their presence test, the IO FPGA registers IOS uses, why every boot takes five machine checks |
| 20 | [Snapshots and core dumps](20-snapshots-and-core-dumps.md) | `Ctrl-] s` / `--restore`: the whole machine in 0.2 s, IOS at `Router#` in 0.3 s; `Ctrl-] d`: ELF core files for GDB |
| 21 | [Warp mode and the silent console](21-warp-and-the-silent-console.md) | `--idle warp` back; why the console "died": IOS's exec-timeout logout, found with snapshot-based experiments |
| 22 | [ROMMON runs code from its stack](22-rommon-code-on-the-stack.md) | the "variable next to code" the JIT tripped over: ROMMON copies its checksum loop onto the stack to run it from RAM; traced with core dumps, `--watch` on the RAM alias, IDA |
| 23 | [A WAN card: the WIC-1T](23-wic-1t-serial.md) | `--wic0 wic-1t --serial0 udp:...`: card-present bits, the ID EEPROM over SPI, the SCC in HDLC mode, modem signals and cable ID, two routers pinging over Serial0/0; the "looped" line of two lockstep emulators |
| 24 | [A boot ROM of our own: `--rom builtin`](24-fake-boot-rom.md) | what IOS needs from ROMMON (entry state measured with GDB; 544 EMT calls of 22 kinds traced with `--log-level syscall=debug` and read in `emt_dispatch`); a built-in ROM that sets the machine up, loads the ELF from flash and answers the calls in Rust; reload, monitor variables in NVRAM; 24.7 s sooner to `Router>` in realtime mode |
| 25 | [Hidden commands and menus](25-hidden-commands.md) | ROMMON's command table, `priv` password (cookie sum), diagnostic menu (17 hardware tests pass on the emulator), MONRC startup script; IOS: 23 keywords from `service internal`, never-listed keywords (parser privilege flags), the hidden `test voip` shell |

Reference material:

* `MPC860UM.pdf` — MPC860 PowerQUICC User's Manual. Useful PDF pages:
  instruction set p.153, exceptions p.177–196, MSR p.145, IMMR p.287,
  SIU interrupts p.296–302, PIT p.315, RSR p.323, BR/OR p.449–451, UPM/MCR p.455.
* `info` — the original notes and boot log.
* `tools/ida/mpc860_setup.py`, `tools/ida/c1700_names.py` — reproducible IDA annotations;
  `tools/ida/decompile.py` — headless decompiler (idalib) for when the IDA GUI/MCP is unavailable.
* `tools/find_refs.py` — finds code that builds an address with `lis`+`addi` (string references).
* `tools/mkimage.py` — builds a minimal bootable C1700 image for `tftpdnld`/`boot`.
* `tools/ciscoimg.py` — checks an IOS image like the boot ROM does and identifies its CPU.
* `tools/iosdis.py` — disassembles a range of a decompressed IOS image.
* `tools/xbit.py` — parses a Xilinx configuration bitstream found inside an image.
* `tools/gdb_sample.py` — finds where a guest hangs: runs it under the GDB stub, interrupts, dumps the PC and code.
* `tools/ios_session.py` — drives an IOS boot and a list of commands unattended (setup dialog, `show`, `ping`...).
* `tools/ios_refs.py` — finds code referencing a string or an address in a decompressed IOS image.
* `tools/bench.sh` — the CPU engine benchmark (power-on to 3 billion executed instructions with IOS in flash).
* `tools/ida/ios_124_8_names.py` — IDA annotations for IOS 12.4(8) (the database stays local in `ida-ios/`): scheduler, MMU, IDMA, FPGA loader, WIC/VIC slots, IO FPGA, `probe_address`, WIC detection, the PowerQUICC serial driver, HDLC keepalives, the EMT (ROM monitor call) wrappers.
* `tools/tests/` — end-to-end scripts: `tap_test.sh` (TAP, chapter 18), `serial_test.sh` and `serial_2t_test.sh` (two routers over WIC serial ports, chapter 23), `startup_config_test.sh` (chapter 7.7), `fakerom_test.sh` (chapter 24), `warp_test.py` and `exec_timeout_test.py` (chapter 21); `tools/rommon_session.py`, `tools/ios_hidden_commands.py` (chapter 25).
