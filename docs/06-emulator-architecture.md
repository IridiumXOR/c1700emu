# 6. Emulator architecture

```
emu/                        Cargo workspace
├── ppc8xx/                 the CPU core (no Cisco knowledge at all)
│   ├── src/lib.rs          state, Platform trait, exceptions, SPRs, timebase, step()
│   ├── src/execute.rs      one match arm per opcode; the load/store funnel
│   ├── src/decode_cache.rs memo of decoded instruction words
│   ├── src/mmu.rs          the 8xx MMU: TLBs, table-walk assist, protection (ch. 12)
│   ├── examples/decode_bench.rs
│   └── tests/              semantics, MMU end-to-end, differential vs qemu-ppc, throughput
└── c1700/                  the Cisco board
    ├── src/mpc860/         on-chip peripherals (IMMR window)
    │   ├── mod.rs          SIU (interrupts), memory controller/UPM, PIT, RSR,
    │   │                   CPM interrupt controller, CPCR, SPI registers, stats
    │   ├── regs.rs         register offsets      names.rs  register names (from IDA)
    │   └── fec.rs          Fast Ethernet Controller registers + LXT971 PHY
    ├── src/board/          what is soldered around the chip
    │   ├── mod.rs          chip-select decoder, Platform impl, --watch
    │   ├── iobus.rs        CS7 sub-decoder: NVRAM, IO FPGA, DUART (console + AUX)
    │   ├── uart16550.rs    nvram.rs  cookie.rs (model, serial)  cpld.rs (IO FPGA)
    │   ├── fpga_config.rs  IO FPGA reg 0x4C: Xilinx slave-serial port (1760, ch. 11)
    │   ├── flash/          amd.rs, intel.rs (x16 StrataFlash), mod.rs (persistence)
    │   ├── flashfs.rs      Cisco flash file system (--flash-file, ch. 11)
    │   ├── fec_dma.rs      the FEC's DMA engine (descriptor rings, ch. 8)
    │   ├── cpm_spi.rs      CPCR commands + SPI controller (WIC ID EEPROMs, ch. 10)
    │   └── idma.rs         IDMA memory-to-memory copies (IOS receive path, ch. 13)
    ├── src/net/            virtual network: smoltcp host + TFTP server
    ├── src/fastforward.rs  skips the ROM's timebase busy-wait loops
    ├── src/machine.rs      CPU + board + virtual clock + reset logic + stats
    ├── src/emulator.rs     machine + consoles, the interactive loop
    ├── src/console.rs      terminal (raw mode) and telnet servers (console, AUX)
    ├── src/gdb.rs          GDB stub (gdbstub crate)
    ├── src/main.rs         command line (clap)
    └── tests/              boot.rs (ROM → prompt, boot from flash), net.rs (tftpdnld)
```

## 6.1 Why two crates?

The split follows the silicon: the PowerPC core talks to the rest of the
MPC860 through an internal bus. In the emulator that boundary is the
`ppc8xx::Platform` trait:

```rust
pub trait Platform {
    fn read8/16/32(&mut self, addr) -> Result<_, BusError>;   // physical addresses:
    fn write8/16/32(&mut self, addr, value) -> Result<(), BusError>; // the MMU is in the core
    fn fetch(&mut self, addr) -> Result<u32, BusError>;
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
Only registers whose behaviour software depends on have hooks:

* **memory controller**: BR/OR (remap), MCR (UPM read/write via MDR);
* **SIU**: SIPEND/SIVEC computed, PISCR/PITC/PITR (periodic timer), RSR
  (reset cause, write-1-to-clear);
* **CPM**: CPCR (commands complete instantly; INIT for SCC/SMC/SPI, INIT/STOP
  IDMA), the CPM interrupt controller (CIPR computed from SPI and IDMA, CIVR
  = highest-priority code, CISR write-1-to-clear), SPI (SPCOM start, SPIE
  write-1-to-clear), IDSR1/2 (write-1-to-clear);
* **FEC**: registers, MII frames to the PHY, GTS→GRA.

Engines that need bus access (FEC DMA, SPI, IDMA) live on the board, which
calls `run_cpm()` after every IMMR write. Interrupt sources feed SIPEND
(IRQ0-7 pins and LVL0-7 internal levels: PIT, FEC via IVEC, the CPM via
CICR), `SIPEND & SIMASK` drives the core's external interrupt input, and
SIVEC encodes the highest-priority source × 4. With `--stats`, every IMMR
register access is counted; that table found most of the IOS bugs.

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
  burn a host core. IOS's idle loop is detected separately (`idle.rs`,
  chapter 15): virtual time jumps to the next timer interrupt, and in
  realtime mode the emulator sleeps instead of spinning.

## 6.6 Resets

`Cpu::step` returns `StepOutcome::Checkstop` when a machine check arrives
with MSR[ME]=0. `Machine` then checks PLPRCR[CSR]: set → internal hard reset
with RSR |= CSRS (RAM, NVRAM and RSR survive, like on the real chip); clear →
the machine halts (`StopReason::Checkstop`). Power-on sets RSR =
EHRS|ESRS (0xC0000000), the documented reset value.

## 6.7 Device models in one line each

| Device | Model |
|--------|-------|
| DUART | NS16552: two 16550 channels (A = console, B = AUX at +8, `--aux-tcp`); DLAB, 16-byte RX FIFO, LSR, IIR priorities, BREAK = BI + NUL, IRQ → SIU IRQ1 |
| NVRAM | 32 KB persisted to `c1700-nvram.bin`; a valid default cookie (model PM ID, MAC, serial) is written if missing |
| IO FPGA ("CPLD") | 256 latches, board-type strap, LED, WIC select for SPI; reg 0x4C = Xilinx configuration port (PROG_B/INIT_B/CCLK/DIN/DONE) |
| Flash | Intel StrataFlash x16 (8/16/32 MB, default) or 2 × AMD Am29F016 (4 MB): command state machines, instant completion, persisted to `c1700-flash.bin`; Cisco flash file system builder |
| FEC | registers, MII management with an LXT971 PHY, DMA engine walking the Tx/Rx descriptor rings |
| SPI | master transfers over the descriptor rings; the WIC/VIC ID EEPROMs answer 0xFF (empty slots) |
| IDMA | memory-to-memory transfers requested via PCSO[DREQ0/1], DONE/AD interrupts |
| MMU | in the core (`ppc8xx/src/mmu.rs`), see chapter 12 |
| Network | smoltcp virtual host (10.0.2.2 / 10.0.2.1): ARP, ping, read-only TFTP server |

## 6.8 Performance notes

* CPU core alone (flat RAM, release build): ~94 MIPS (~102 before the MMU
  checks were added).
* Full board, ROMMON: boot to the prompt ≈ 0.1 s with 32 MB (the ROM's
  byte-by-byte `bzero` of all RAM, 73% of the instructions, is done in bulk).
* Full board: **~32.5 million executed instructions per second** on the IOS
  decompression benchmark (`tools/bench.sh`; +47% from the fast paths of
  chapter 16). IOS reaches `Router>` in ~2.5 minutes (it
  was 5.3), mostly self-decompression.
* The `mftb` busy-waits of the ROM are fast-forwarded (`fastforward.rs`,
  chapter 8): boot 6.9 → 5.2 s, writing an image to flash ~70 → ~4 s.
* Measured profiles and idle detection (boot 318 → 206 s, idle router
  ~0% CPU): chapter 15. Further options, including a JIT: chapter 14.
