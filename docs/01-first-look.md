# 1. First look at the ROM

## 1.1 What is it?

* File: `CISCO_bootrom_AT27C040.bin`, 524 288 bytes = 4 Mbit, i.e. the full
  contents of one AT27C040 (an 8-bit-wide one-time-programmable EPROM).
* `strings` immediately tells us it is a Cisco **ROMMON** ("System Bootstrap,
  Version 12.2(1r)XE1 … Compiled Fri 06-Apr-01 by ccai") and gives the internal
  platform codename via a file-system name: `ferrari-fslib-m`.
* The help strings (`tftpdnld`, `confreg`, `meminfo`, `cookie`, `berrscan`…)
  are the standard ROMMON command set.
* The CPU is an MPC860 (PowerQUICC); later we prove it is an **MPC860T** (it
  programs the Fast Ethernet Controller, which only the "T" variant has). That
  plus the strings matches a **Cisco 1720/1750**.

## 1.2 Where is it loaded? (base address)

Your notes contain the output of a base-address finder:

```
0x38610000: 512     <- false positive
0xfff00000: 218     <- real
```

Why is `0x38610000` a false positive? `0x3861xxxx` is the encoding of
`addi r3,r1,xxxx` — one of the most common PowerPC instructions (taking the
address of a stack variable). Any tool that looks for "values that look like
pointers into the image" gets fooled by instruction words.

The real argument for `0xFFF00000` comes from the CPU:

* After reset, the MPC860 fetches its first instruction at
  **exception prefix + 0x100**, the System Reset vector.
* The prefix is 0x00000000 or 0xFFF00000 depending on MSR[IP], which is
  loaded from the *hard reset configuration word*. Your notes list the
  four possible boot addresses (`00`→0, `01`→0x00F00000, `10`→0xFF000000,
  `11`→0xFFF00000).
* At reset, chip select 0 (the boot ROM) answers for *every* address, so the
  ROM is visible both at 0 and at 0xFFF00000 until software reprograms CS0.

Looking at offset 0x100 of the file:

```
00000100: 4800 2f00   b  +0x2F00      -> 0x3000 relative to the vector
```

A relative branch works at any base, but the code soon loads absolute
addresses like `0xFFF3xxxx` (pointers to strings) and programs CS0 as
`BR0 = 0xFFF00401` (base 0xFFF00000). So the ROM is linked at **0xFFF00000**.

Filler: unused space is filled with `0xBADDADDD` ("bad dad"), a Cisco habit.
It makes "jumped into nowhere" bugs easy to spot.

## 1.3 SPR census — what does the code expect from the CPU?

Before choosing how to emulate the CPU, I scanned every 32-bit word for
`mtspr`/`mfspr`/`mftb` and counted the special purpose registers used
(`tools`-free version: a 40-line Python script that decodes the SPR field —
PowerPC stores the 10-bit SPR number with its two 5-bit halves swapped).

Highlights:

| SPR | Name | Use in the ROM |
|-----|------|----------------|
| 638 | IMMR | written once with 0xFF000000 → relocates all on-chip peripherals; read 22× to find them |
| 80/81/82 | EIE/EID/NRI | 8xx-only: *writing* them enables/disables external interrupts |
| 560/568 | IC_CST/DC_CST | cache enable/disable/invalidate commands |
| 272–275 | SPRG0–3 | scratch in exception handlers |
| 799 | M_TW | MMU table-walk scratch — abused as one more scratch register in every vector |
| 22, 268/269 | DEC, TBL/TBU | timing (delays, decrementer interrupt) |
| 784–826 | MMU regs | **only read**, in a "dump registers" routine |

Conclusion: the ROM never writes the MMU control registers, so it runs with
translation off (real mode). Good news: the emulator does not need the 8xx
MMU (software-managed TLB) to run ROMMON.

The scan also reported ~600 "floating point" opcodes. Those are false
positives: data words such as pointers `0xFFF3xxxx` have primary opcode 63.
The MPC860 has no FPU at all.

## 1.4 The exception vector table

Each 256-byte slot at `0xFFF00000 + 0x100*n` contains the same trampoline:

```asm
mtspr  SPRG0, r4        ; park r4
mflr   r4
mtspr  SPRG1, r4        ; park LR
lis    r4, handler@h
ori    r4, r4, handler@l
mtlr   r4
blr                     ; jump to the real handler
```

The real handlers are therefore reachable from anywhere (a relative branch
from 0xFFF00x00 has limited range), and the handler can recover r4/LR from
SPRG0/SPRG1.

| Vector | Handler | |
|--------|---------|---|
| 0x100 System reset | 0xFFF03000 | the boot code (chapter 3) |
| 0x200 Machine check | 0xFFF0369C | bus errors (`berrscan`, memory probing, `reset`) |
| 0x500 External interrupt | 0xFFF037D0 | SIU interrupt controller |
| 0x900 Decrementer | 0xFFF03904 | timer tick |
| 0xC00 System call | 0xFFF0338C | `sc`-based "EMT" calls (how IOS calls into ROMMON) |
| everything else | 0xFFF03A38 | common "unexpected exception" handler |

Handlers that need a full C context use a neat trick: they put the address of
a second stage into SRR0, copy MSR[IR,DR] into SRR1 and execute `rfi` — a
"return" that is really a jump with a controlled MSR. IDA cannot follow that
(the target comes from a register), so those second stages need to be marked
as code by hand.

## 1.5 Decision: how to emulate

Because the ROM leans on so many 8xx-specific behaviours (magic SPRs,
exceptions delivered through its own vectors, timebase/decrementer, bus-error
machine checks), we decided to write our **own PowerPC interpreter in Rust**
instead of using Unicorn (whose PowerPC models lack all 8xx SPRs and which
does not deliver exceptions to the guest). Chapter 5 describes the core.
