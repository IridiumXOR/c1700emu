# 6. Emulator architecture

```
emu/                      Cargo workspace
├── ppc8xx/               the CPU core (no Cisco knowledge at all)
│   ├── src/lib.rs        state, Platform trait, exceptions, SPRs, timebase
│   ├── src/execute.rs    one match arm per opcode
│   ├── src/decode_cache.rs
│   └── tests/            semantics tests + differential tests vs qemu-ppc
└── c1700/                the Cisco board
    ├── src/mpc860/       on-chip peripherals (IMMR window)
    │   ├── mod.rs        SIU, memory controller/UPM, PIT, RSR, CPM...
    │   ├── regs.rs       register offsets
    │   └── fec.rs        Fast Ethernet Controller + LXT971 PHY
    ├── src/board/        what is soldered around the chip
    │   ├── mod.rs        chip-select decoder + Platform impl
    │   ├── iobus.rs      CS7 sub-decoder (NVRAM / CPLD / UART)
    │   ├── uart16550.rs  nvram.rs  cookie.rs  cpld.rs  flash.rs
    │   └── fec_dma.rs    the FEC's DMA engine (buffer-descriptor rings)
    ├── src/net/          virtual network: smoltcp + TFTP server
    ├── src/fastforward.rs  skips the ROM's timebase busy-wait loops
    ├── src/machine.rs    CPU + board + virtual clock + reset logic
    ├── src/emulator.rs   machine + console, the interactive loop
    ├── src/console.rs    terminal (raw mode) and telnet server
    ├── src/gdb.rs        GDB stub (gdbstub crate)
    ├── src/main.rs       command line (clap)
    └── tests/boot.rs     boots the real ROM to the prompt
```

## 6.1 Why two crates?

The split follows the silicon: the PowerPC core talks to the rest of the
MPC860 through an internal bus. In the emulator that boundary is the
`ppc8xx::Platform` trait:

```rust
pub trait Platform {
    fn read8/16/32(&mut self, addr) -> Result<_, BusError>;
    fn write8/16/32(&mut self, addr, value) -> Result<(), BusError>;
    fn read_spr / write_spr(...)   // IMMR lives outside the core
    fn external_interrupt(&self) -> bool;   // from the SIU
    fn system_call(&mut self, cpu) -> bool; // test hook (Linux syscalls)
}
```

Benefits: the CPU can be tested alone against QEMU (the test harness is just
another `Platform`), and it was developed in parallel with the board.

## 6.2 Engine choice: own interpreter, crates for the rest

We considered Unicorn (QEMU's JIT as a library). For this ROM it would have
needed workarounds everywhere: its PowerPC models have no 8xx SPRs (as far
as I remember QEMU's translator, an unknown `mfspr` in supervisor mode is
treated as a no-op that leaves the target register unchanged — a silent
failure), it hands exceptions to a host callback instead of the guest's
vectors, and there is no API to inject the SIU interrupt or a machine check
on a bus timeout.
A small interpreter gets all of that exactly right, is fast enough
(~25–100 MIPS), and can be verified instruction by instruction.

Existing crates are used wherever possible: `powerpc` (decoder +
disassembler), `gdbstub`/`gdbstub_arch` (GDB protocol), `clap`, `crossterm`,
`env_logger`.

## 6.3 Address decoding (board/mod.rs)

The memory controller logic of chapter 4 is implemented literally in
`Board::decode(page)`: IMMR window first, then BR/OR of CS0..CS7. The result
is memoised per **32 KB page** (the finest granularity ORx.AM can express) in
a 131 072-entry table, so a normal access is one table lookup + one `match`.
Writing any BR/OR/IMMR sets `map_dirty` and the table is lazily rebuilt —
during memory sizing that happens a few dozen times.

Effects that fall out of modelling the hardware instead of hard-coding a map:

* SDRAM **aliasing**: a bank region stores `offset % bank_size`, so the
  ROM's sizing algorithm measures exactly the configured RAM (`--ram 16/32/48/64...`).
* **Empty UPM banks** read all-ones (the UPM still terminates the cycle):
  that's how empty sockets are detected.
* The **0x80000000 alias** of RAM appears by itself when the ROM clears
  bit 0 of ORx.AM.
* **Bus errors**: no bank → `BusError` → the CPU's machine check
  (`berrscan`, `reset`).
* 8-bit ports: a 32-bit access to the I/O bus is split into 4 byte accesses,
  as the memory controller does in hardware.

## 6.4 On-chip peripherals (mpc860/mod.rs): registers are memory

The 16 KB IMMR window is a byte array: unknown registers behave as latches.
Only registers whose behaviour the ROM depends on have hooks: BR/OR
(remap), MCR (UPM read/write via MDR), SIPEND/SIVEC (computed), PISCR/PITC/
PITR (periodic timer, PS write-1-to-clear), RSR (reset cause, w1c), CPCR
(command completes instantly), FEC (MII frames). Interrupt sources feed
SIPEND (IRQ0-7 pins and LVL0-7 internal levels), `SIPEND & SIMASK` drives
the core's external interrupt input, SIVEC encodes the highest-priority
source × 4.

## 6.5 Virtual time (machine.rs)

One executed instruction = one CPU cycle. The core advances the timebase
every 16 cycles (TB = SYSCLK/16, ~3 MHz like the real board, whose
`delay_ms` assumes 3024 ticks/ms) and counts the decrementer down with it,
but only while TBSCR[TBE] is set. Devices are advanced every 256
instructions (`DEVICE_SYNC_INTERVAL`). Consequences:

* runs are **deterministic** — the same inputs produce the same instruction
  count, which makes bugs reproducible;
* delays take *emulated* time: the ROMMON 20 ms decrementer tick happens
  every 960 000 instructions, regardless of host speed;
* when the guest spins waiting for a key (the UART counts consecutive
  "no data" LSR polls), the emulator sleeps 5 ms per slice so it does not
  burn a host core.

## 6.6 Resets

`Cpu::step` returns `StepOutcome::Checkstop` when a machine check arrives
with MSR[ME]=0. `Machine` then checks PLPRCR[CSR]: set → internal hard reset
with RSR |= CSRS (RAM, NVRAM and RSR survive, like on the real chip); clear →
the machine halts (`StopReason::Checkstop`). Power-on sets RSR =
EHRS|ESRS (0xC0000000), the documented reset value.

## 6.7 Device models in one line each

| Device | Model |
|--------|-------|
| UART | 16550 with DLAB, 16-byte RX FIFO, LSR (DR/OE/BI/THRE/TEMT), IIR priorities, BREAK = BI + NUL, IRQ → SIU IRQ1 |
| NVRAM | 32 KB byte array persisted to `c1700-nvram.bin`; blank = 0xFF; a valid default cookie is written if missing |
| CPLD | 256 latches, board id strap read-only, LED changes logged |
| Flash | 2 × Am29F016 interleaved, AMD command state machine per chip (unlock, autoselect, program, sector/chip erase), instant completion, persisted to `c1700-flash.bin` |
| FEC | registers, MII management with an LXT971 PHY, and a DMA engine walking the Tx/Rx descriptor rings (chapter 8) |
| Network | smoltcp-based virtual host (10.0.2.2 / 10.0.2.1): ARP, ping, read-only TFTP server |

## 6.8 Performance notes

* CPU core alone: ~108 MIPS (flat RAM, release build).
* Full board: ~28 MIPS; boot to the prompt = 183 M instructions ≈ 6.6 s.
* `--profile` shows 73 % of the boot is the ROM's byte-by-byte `bzero` of
  all RAM and 26 % its timebase delay loops — the ROM is slow, not the
  emulator. Using `--ram 16` halves the clearing time.
* The `mftb` busy-waits are now fast-forwarded (`fastforward.rs`, chapter 8):
  boot 6.9 → 5.2 s, writing an image to flash ~70 → ~4 s, same virtual time.
* Ideas if more speed is ever needed: a per-page fast path for instruction
  fetch, or avoiding the interrupt-line recomputation on every MMIO access.
