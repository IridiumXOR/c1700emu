# 16. Making the interpreter faster

Chapter 15 measured where the time goes. This chapter is the log of what
was then changed in the interpreter and the board, one step at a time: for
each step, the problem the profile showed, the idea, the code, and the
measured result. It ends with what is left and why the next step is a JIT
(chapter 17).

## 16.1 How to measure

**One benchmark, always the same.** `tools/bench.sh` boots a 1760 with
IOS 12.4(8) in the internal flash and stops after 3 billion instructions:
ROMMON, then almost all of IOS's self-decompression. It runs without a
console and prints the speed:

```
$ tools/bench.sh
3000M instructions in 92.4 s = 32.5 MIPS
```

The script turns the ROM accelerators and idle detection **off**
(`--no-fast-forward --idle off`). Then every counted instruction is really
executed, and the number measures the CPU engine. That matters: the
instruction budget counts `cpu.cycles`, and an accelerator that skips a
loop adds the skipped instructions to `cycles` without executing them.

*A correction.* The step-by-step numbers first published in chapter 15.8
(27.4 → 40.3 MIPS) were taken with the ROM's memory-clear accelerator on.
About 0.58 billion of the 3 billion "instructions" were the skipped clear,
so those figures were ~24% too high as instruction rates. The *ratios*
between the steps are right, because every step was measured the same way.
The table in 16.8 gives the wall time for that workload, which is
unambiguous, and the real rate of executed instructions.

**Host profile.** `PERF="perf record -F 999 -o bench.perf.data --" tools/bench.sh`,
then `perf report -i bench.perf.data --sort symbol`. The release profile
keeps debug info (`debug = 1` in `emu/Cargo.toml`).

**A/B, not line attribution.** Twice `perf annotate` pointed at a line that
wasn't the cost. Inside a loop where everything is inlined, samples land on
whichever instruction waits for a slow one, often several lines later. Every
change below was kept or reverted on the benchmark's wall time alone.

**Exactness.** A faster emulator must not be a different emulator.
`cargo test --release -p c1700emu --test boot` includes
`rom_accelerators_are_exact` (16.7). The QEMU differential tests (chapter 5)
cover the CPU core.

## 16.2 Bus fast paths

**Problem.** `Board::read32` (18% of host time) and `Board::region` (11%):
30% of the time was spent finding the memory behind an address, mostly to
fetch instructions.

**How the bus works** (chapter 6): the board keeps a table with one
`Region` per 32 KB page of the 4 GB space (`Dram { bank, base }`,
`Rom { base }`, `Flash`, `IoBus`, `Immr`, `Unmapped`, ...). It is filled
lazily from the memory controller's BR/OR registers. A write to those
registers marks it stale (`map_dirty`).

**Change.**

* `region()` is now two functions. The common case is "table not dirty,
  entry decoded": one array read, `#[inline(always)]`. Decoding a stale page
  moved into `region_slow`, `#[cold] #[inline(never)]`, so the hot function
  stays small enough to inline into every access.
* `read8/16/32`, `write8/16/32` and `fetch` (the `Platform` methods the
  CPU calls) have an inline fast path: if the page is plain DRAM (or ROM,
  for reads and fetches) and no `--watch` is active, it is a slice access.
  Devices, flash, IMMR and watch logging go to `*_slow` bodies kept out of
  line.
* The big-endian helpers became one load each
  (`u32::from_be_bytes(mem[o..o + 4].try_into())`) instead of four byte loads
  and shifts.

**Result:** 109.3 → 94.4 s (**+16%**).

## 16.3 Network polling: a step that gained nothing

**Problem, apparently.** `perf annotate` attributed a visible share of the
run loop to polling the virtual network (smoltcp) at every device sync.

**Change.** The network is polled only when there is something to do:
frames sent by the guest, a TFTP transfer in progress (`Tftp::busy`), or
every 20 ms of housekeeping (`IDLE_POLL_US`).

**Result:** 94.4 → 94.4 s, no gain. The samples belonged to the
instructions around the poll, not to the poll. The change is harmless and
was kept, but it is the reason for the "A/B, not line attribution" rule
above.

## 16.4 Hook pages

**Problem.** `Machine::run` called `before_step` for every instruction
(6.7% of host time). It checks breakpoints, tracing, the idle anchor, the
ROM delay loops, the memory clear and the profile, and almost always finds
nothing.

**Observation.** The per-address hooks live at a handful of addresses, on
three 4 KB pages: 0xFFF12xxx and 0xFFF16xxx (ROM delay loops, memory
clear) and the idle anchor's page (`Idle::hook_pages`).

**Change.** `run` first decides once whether this run needs a look at every
instruction (breakpoints set, `--trace`, `--profile`, `--watch`: the `slow`
flag). If not, an instruction only pays for one page comparison
(`is_hook_page`). The full `before_step` runs only on those pages.

**Result:** 94.4 → 90.6 s (**+4%**).

*Later refinement (chapter 17.9):* single-stepping whole pages also
single-stepped the ordinary ROM code that shares those pages with a hook.
Hooks are now exact addresses (the page test remains the cheap first
check).

## 16.5 Decode cache indexed by address

**Problem.** The hottest single spot of the interpreter was the decode
cache lookup.

**Background** (chapter 5): the `powerpc` crate's `Opcode::detect` scans a
candidate list per primary opcode. For opcode 31 (`mr`, `mflr`, `stwx`,
`cmpw`...) that takes ~42 ns instead of ~4.5 ns. So decoded opcodes are
memoised in a direct-mapped table of 16 Ki entries; the tag is the
instruction word itself, so self-modifying code can never get a stale
opcode.

**The mistake.** The slot was chosen by a hash of the word. Even
straight-line code then touched random slots of the 128 KB table, and
nearly every lookup missed the host's L1 cache.

**Change.** The slot is `(pc >> 2) & 0x3FFF`: consecutive instructions use
consecutive slots, 8 per 64-byte cache line. A loop's working set stays in
L1. The tag is still the word, so correctness doesn't depend on the slot.

**Result:** 90.6 → 79.4 s (**+14%**; +8% on the core-only benchmark).

## 16.6 `Cpu::run_batch`

**Problem.** The machine called `cpu.step()` once per guest instruction.
`step` is too big to inline, so every guest instruction paid a call with a
full prologue/epilogue (~12% of the core's samples).

**Change.** `Cpu::run_batch(sys, max, stop_before)` runs up to `max`
instructions in one loop with the step body `#[inline(always)]`. It returns
`(executed, BatchEnd)` where `BatchEnd` is `Budget`, `Hook` (the next
instruction is one for which `stop_before(pc)` is true: the machine's hook
pages) or `Checkstop`. The machine uses it for everything except the hook
pages and the `slow` mode, with `max` = the instructions left until the next
device sync (every 256).

**Result:** 79.4 → 74.4 s (**+7%**).

## 16.7 ROM accelerators, and proving them exact

Two loops of the boot ROM are recognised by their address *and* their
instruction words, and replaced by their effect (`fastforward.rs`):

* **Delay loops** (chapter 8.6): `mftb` busy-waits. Virtual time jumps to
  the loop's exit condition.
* **The memory clear** (`bzero_bytes`, 0xFFF1683C, 4 instructions per byte):
  at power-on the ROM clears all of RAM, one byte at a time. That is 0.54
  billion instructions for 128 MB. The accelerator does one bulk fill of
  plain DRAM and leaves registers, CR1, PC and virtual time exactly as the
  loop would. It only runs in whole iterations, never with an interrupt
  pending, and stops one timebase tick before the next decrementer
  interrupt, so the interrupt is taken by the real loop at the right time.

ROMMON boot to `rommon 1 >`: 8.0 → 0.1 s with 32 MB, 25.7 → 0.2 s with
128 MB.

**Exactness is tested, not claimed.** `rom_accelerators_are_exact` boots
the ROM twice, with and without accelerators, and stops both at ROMMON's
first key poll (0xFFF0BCAC). Cycle count, timebase, all registers, CR, the
console output and all of RAM must be identical. The bulk fill was exact
from the start. The test found three bugs in the older delay-loop
fast-forward:

1. it could skip while a decrementer interrupt was already pending (that
   interrupt's period was then lost);
2. it skipped *to* the decrementer event instead of one tick before it;
3. for 3-instruction loops it jumped a number of instructions that wasn't a
   whole number of iterations (fixed with a gcd step).

Fixing the PIT bound made the network test 3× slower. ROMMON polls a PIT
whose interrupt is *masked*, and bounding every skip by that PIT cut the
skips into tiny pieces. Now a PIT expiry bounds a skip only when the PIT
interrupt is unmasked. With it masked, the sticky PS flag makes merged
expiries indistinguishable on the real chip too.

## 16.8 Results

Benchmark workload of 16.1 (3 billion counted instructions, of which ~2.42
billion executed and ~0.58 billion skipped by the memory-clear accelerator,
as when these steps were measured):

| step | wall time | executed instructions per second | gain |
|---|---|---|---|
| start | 109.3 s | 22.1 M | |
| bus fast paths (16.2) | 94.4 s | 25.6 M | +16% |
| network polled only when needed (16.3) | 94.4 s | 25.6 M | none |
| hook pages (16.4) | 90.6 s | 26.7 M | +4% |
| decode cache by address (16.5) | 79.4 s | 30.5 M | +14% |
| `run_batch` (16.6) | **74.4 s** | **32.5 M** | +7% |

Total **+47%**. `tools/bench.sh` (no accelerators at all) agrees: 32.5
MIPS. The core alone on flat RAM (`cargo test --release -p ppc8xx --test
throughput -- --ignored --nocapture`) does ~94 MIPS. The difference is the
board: address decoding, devices, the device sync every 256 instructions.

For the user:

| | before | after |
|---|---|---|
| ROMMON boot, 128 MB | 25.7 s | 0.2 s |
| IOS 12.4(8) boot to `Router>` (with idle detection, chapter 15) | 216 s | **153 s** |

## 16.9 What is left

A `perf` profile of the benchmark now:

| share | what |
|---|---|
| ~40% | the instruction cycle around the work: interrupt check, ITLB, fetch, decode cache, dispatch |
| ~34% | instruction semantics (`execute`) |
| ~13% | loads and stores (translation, bus) |
| ~8% | device sync |

There is no single hot spot left. The remaining cost is the interpreter
doing, for every instruction and every time, work whose result is the same
each time: fetch the same word, look up the same decode result, extract the
same register numbers, branch on the same opcode. Removing that requires
translating code once and running the translation, which is what a JIT does.
Chapter 17 adds one as a second CPU engine next to this interpreter.
