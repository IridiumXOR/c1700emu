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
| 7 | [Running & debugging](07-running-and-debugging.md) | command line, console, GDB stub, tracing |
| 8 | [Network boot & flash](08-network-and-flash.md) | FEC DMA, virtual network + TFTP server, Cisco image format, flash file system, PIT clock, fast-forward |
| 9 | [Checking IOS images](09-ios-images.md) | why `file` says "MIPS-X", proving images are MPC8xx code, `-mz` = loader + ZIP |
| 10 | [Towards full IOS emulation](10-ios-roadmap.md) | what IOS uses that is still missing, with the evidence for each item |
| 11 | [Internal flash & router model](11-internal-flash-and-models.md) | the C1700's only boot device, Intel x16 flash, flash file system, `--flash-file`, PM ID / `--model` |

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
