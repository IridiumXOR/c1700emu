# 17. A JIT compiler as a second CPU engine

Chapter 14 explained what a JIT is and chapter 16 showed that the
interpreter had no single hot spot left: what remains is the cost of
re-doing, for every instruction and every time, work whose result never
changes. This chapter adds a JIT that translates guest code to host machine
code with **Cranelift**. It is built step by step, and every step is
checked against the interpreter.

The JIT is an **alternative**, not a replacement:

```
c1700emu --cpu interp ...   # the interpreter (default): the reference
c1700emu --cpu jit ...      # the JIT
c1700emu --cpu jit --jit-threshold 200 ...   # translate later (default 50)
```

Both engines work on the same `Cpu` state through the same `Platform`
trait, and they must produce the same results instruction for instruction.
The code is in `emu/ppc8xx/src/jit/`:

| file | what |
|---|---|
| `mod.rs` | the dispatcher: entry table, when to translate, running regions, exactness rules |
| `scan.rs` | finding blocks and regions, classifying instructions |
| `codegen.rs` | guest instructions → Cranelift IR → x86-64 (or AArch64) |
| `runtime.rs` | what translated code runs against: context, fast memory tables, helpers |
| `watch.rs` | `CodeWatch`, for platforms: which instruction words hold translated code, which were written |

## 17.1 The idea in one picture

```
            ┌──────────────── dispatcher (Jit::run_batch) ───────────────┐
 cpu.pc ──▶ │ interrupt pending? ──yes──▶ interpreter takes it (1 step)  │
            │ ITLB translate pc  ──fault─▶ exception entry               │
            │ look up entry (pc, MSR mode)                               │
            │   cold   → count; interpret up to the next branch          │
            │   hot    → translate its region with Cranelift (once)      │
            │   ready  → call the host code with a budget; advance the   │
            │            clock by what it ran; precise exception if any  │
            └────────────────────────────────────────────────────────────┘
```

A **block** is straight-line guest code starting at one address. It ends
with a branch, before an instruction the translator leaves to the
interpreter, at the end of the page (1 KB with translation on, see 17.5) or
after 64 instructions. A **region** is a block plus the blocks reachable
from it by direct branches in the same page, translated as one function
in which loops run without returning to the dispatcher (17.10). A region
is translated once its first block has been reached 50 times
(`--jit-threshold`); code that runs fewer times is cheaper to interpret
than to compile. The first sections describe the JIT with one block per
function, as it was first built; 17.10 is the step to regions.

## 17.2 Step 1: what the core had to offer

The JIT lives inside the `ppc8xx` crate, so it can use the interpreter's
own functions. A few additions were needed:

* `Cpu::advance_clock(n)`: exactly the effect of `n` per-instruction
  `tick()`s (cycles, timebase prescaler, timebase, decrementer and its
  0 → -1 interrupt request) in one go. A block runs, then its time passes.
* `Cpu::instructions_until_decrementer_request()`: how many instructions
  can execute before the one whose tick requests the decrementer
  interrupt. The dispatcher uses it to never run a block the interrupt
  would fall into.
* `Mmu::cached()` and `Mmu::data_generation()`: the MMU's fast cache
  already decides when a whole 4 KB granule translates the same way
  (no 1 KB sub-page differences). The JIT reuses that decision instead of
  duplicating the 8xx protection rules, and a counter of cache flushes
  tells it when its own copies are stale.
* Five optional `Platform` methods (default: "not supported", in which
  case the JIT simply interprets everything):
  `host_page` (a host pointer to a page of plain memory),
  `memory_map_epoch` (changes when those pointers may be wrong),
  `watch_code` / `take_code_write` / `unwatch_code_pages` (17.7).

## 17.3 Step 2: what gets translated

`scan.rs` sorts every instruction into one of four kinds:

| kind | instructions | how |
|---|---|---|
| **Native** | integer arithmetic (OE=0), compares, logical, rotates and shifts, CR operations, all byte/half/word loads and stores (update, indexed, byte-reversed), `mfspr`/`mtspr` of LR, CTR, XER, `mftb` and `mfspr` of TBL/TBU/DEC (17.10), `sync`/`isync`/`eieio`/`dcbt` | translated to host instructions |
| **Helper** | `lmw`/`stmw`, string instructions, `lwarx`/`stwcx.`, `dcbz`, cache-line operations, traps, `mfmsr`, XO forms with OE=1, other `mfspr`, `mtspr` of SPRG/SRR/DAR/DSISR | a call to the interpreter's own code for that one instruction |
| **Branch** | `b`, `bc`, `bclr`, `bcctr` (with or without LK, with CTR decrement) | translated; ends the block |
| **Stop** | `sc`, `rfi`, `mtmsr`, `mtspr` of DEC and TB, of EIE/EID/NRI, of the MMU registers and IMMR, `mfspr` of DEC in user mode, `tlbie`, `tlbia`, floating point, illegal words | not translated: the block ends *before* it and the interpreter runs it |

The rule behind the last row: a block assumes that nothing changes the MSR
(privilege, translation, interrupt enable), the virtual time, the TLBs or
the memory map while it runs. Every instruction that could is left to the
interpreter, between blocks, where those changes are naturally safe. The
helpers cover instructions that are rare but harmless, so they don't cut
blocks short.

## 17.4 Step 3: code generation

A translated block is a function `fn(ctx, cpu) -> u32` that returns how
many instructions it executed (bit 16 set if the last one raised an
exception).

**Guest registers** (r0-r31, CR, XER, LR, CTR) are Cranelift *variables*.
A register is loaded from the `Cpu` struct the first time the block reads
it, then stays in a host register. In the first version every change was
also *written through* to the struct. That costs one store per register
write, but it makes every exit point trivially correct: when the block
leaves (end, fault, helper call), the struct already holds the exact
state, so exceptions are precise without any bookkeeping. (Today stores
are delayed to the end of the guest block and the exits, 17.13.)
Cranelift's register allocator and optimiser do the rest.

**Semantics** mirror `execute.rs` line by line. Some details that matter:

* XER[CA] comes from a 64-bit addition (`a + b + carry_in`, carry =
  bit 32), exactly as the interpreter's `add3`;
* host shifts take the amount modulo 32, PowerPC shifts by 32-63 give 0
  (or all sign bits): the translation selects those cases explicitly;
* `divw`/`divwu` undefined cases (÷0, 0x80000000 ÷ -1) would trap on x86:
  the divisor is replaced by 1 first and the result by 0, as in the
  interpreter;
* `blrl` jumps to the *old* LR: the target is read before LK writes it.

Cranelift 0.136 detail: memory flags are now `MemFlagsData`, `finalize`
takes the target configuration, and the `*_imm` builders exist as `_u`
(zero-extended) and `_s` (sign-extended) variants, which removes a
classic source of wrong constants.

### A real block, three ways

`--log-level warn,jit_ir=trace` logs every translated block as guest
code, Cranelift IR and host code. Here is the ROM's memory-clear loop
(`bzero_bytes`, chapter 16.7), almost exactly the loop chapter 14.2 used
to explain what a JIT would emit:

```
fff1683c: stb   r9, 0(r3)        ; clear one byte
fff16840: addi  r3, r3, 1
fff16844: cmplw cr1, r3, r0      ; reached the end?
fff16848: blt   cr1, -0xc
```

The host code Cranelift generated in the first version (x86-64, fast
path only; `%rsi` = the `Cpu` struct, `%rdi` = the context; comments
added; 17.13 shows the same loop today):

```
movq 8(%rdi), %rcx            ; store table of the current mode
movl 0x7cc(%rsi), %ebx        ; r3   (gpr[3] inside Cpu)
movl 0x7e4(%rsi), %edx        ; r9
mov  %ebx, %r8d ; shr $12 ; and $0x3ff ; shl $4      ; table index * 16
movl (%rcx,%r8), %eax         ; entry.tag
mov  %ebx, %esi ; and $0xfffff000                    ; key: r3's page
cmpl %esi, %eax ; jnz slow    ; not plain RAM (or not seen yet): helper
movq 8(%rcx,%r8), %rax        ; entry.addend
movb %dl, (%rcx,%rax)         ; THE STORE: host byte at r3 + addend
leal 1(%rbx), %edi            ; addi r3, r3, 1
movl %edi, 0x7cc(%rsi)        ;   written through to the Cpu struct
cmp/setb, cmp/seta, cmp/sete  ; cmplw: LT, GT, EQ, plus XER[SO]
...                           ;   merged into CR field 1, CR stored
shrl $0x1b ; andl $1          ; blt cr1: read CR bit 4 back
cmovel ..., %ecx              ; next PC: 0xfff1683c or 0xfff1684c
movl %ecx, 0x960(%rsi)        ; cpu.pc
movl $4, %eax ; ret           ; 4 instructions executed
```

About 40 host instructions for 4 guest instructions, against roughly
150 host cycles *per guest instruction* in the interpreter. It also shows
what is still wasteful: three compares to build one CR field, the branch
re-reading a CR bit it just computed, and a return to the dispatcher every
iteration (solved by regions, 17.10).

**The same loop today, as a region** (17.10). The region holds the loop
block and the `blr` after it (two blocks, so two entry points), and the
function now takes the budget and the entry number. The new parts of the
host code:

```
movl %ecx, %ecx ; cmpl $2 ; cmovb ; br_table   ; entry number → block (jump table)
xorl %r15d, %r15d                             ; instructions done so far = 0
...                                           ; stb / addi / cmplw as before
shrl $0x1b ; andl $1 ; cmpl $1 ; jz taken     ; blt cr1
taken:
leal 8(%r15), %eax                            ; done + this block (4) + the next (4)
cmpl budget, %eax ; jbe loop                  ; still fits: loop inside the function
movl $0xfff1683c, cpu.pc ; leal 4(%r15), %eax ; ret   ; else leave with the exact count
loop:
leal 4(%r15), %r15d ; jmp body                ; count the iteration, run it again
```

One call now clears up to `budget / 4` bytes (64 per batch of 256
instructions) instead of one, and the count in `%r15` is what every exit
returns, so the dispatcher advances the clock by exactly what ran.

## 17.5 Step 4: memory accesses and precise exceptions

Every load and store looks its page up in a **fast table** (one per mode:
real, translated supervisor, translated user; separate tables for loads
and stores). An entry says "guest page X is host memory at X + addend":

```
key   = ea & (0xFFFFF000 | (size - 1))   ; low bits: misaligned → no match
entry = table[(ea >> 12) & 1023]
if entry.tag == key:  host load/store at ea + entry.addend   (big-endian: bswap)
else:                 call the helper
```

The **helper** runs the interpreter's own `load32`/`store8`/...: devices,
misaligned accesses, MMU translation and faults, bus errors, all exactly
as in the interpreter. After a successful access it fills the table entry
if the page is plain memory (`Platform::host_page`) and, with translation
on, the MMU's fast cache holds a uniform translation for it.

On a fault the helper stores the exception and the block returns
`steps | RAISED`. The dispatcher then sets PC to that instruction and
enters the exception, exactly as `Cpu::step` would (the faulting
instruction's clock tick counts, as in the interpreter).

With translation on, a block never crosses a 1 KB boundary: the 8xx can
give the four 1 KB sub-pages of a 4 KB page different permissions, and the
one ITLB lookup done at the block start must be valid for every instruction
in it.

## 17.6 Step 5: exactly the interpreter's behaviour

The dispatcher reproduces `Cpu::step` at block granularity:

* **Interrupts** are recognised where `step` checks them, at the start of
  a block; the interpreter takes them (one step, no clock tick).
* **The decrementer**: with `MSR[EE]=1`, a block runs only if
  `instructions_until_decrementer_request() >= len`. Otherwise the
  interpreter runs the code, so the interrupt lands on the same
  instruction. (With regions this limit is part of the *budget* the
  region receives, 17.10.)
* **Device interrupts**: the only way the external interrupt line can
  change inside a batch is a device access, which goes through a helper.
  If it raised the line (with `EE=1`), the helper tells the block to stop
  right after that instruction.
* **Batch budget**: the machine syncs devices every 256 instructions; a
  block that doesn't fit before that point is interpreted instead (with
  regions: also part of the budget).
* **Hooks**: the machine has per-address hooks (ROM delay loops and
  memory clear, the idle anchor). The dispatcher stops before a hook
  address at a block start, and a translated block always ends before one,
  so the machine sees every hook address exactly as with the interpreter.

## 17.7 Step 6: self-modifying code

IOS decompresses itself (55 MB written, then executed), and ROMMON copies
code too. A translation is only valid while its guest code is unchanged.

The platform **watches** the instruction words that were translated
(`CodeWatch`: a bit per page as a quick first test, then a bit per word)
and reports any write to them: CPU stores, DMA (FEC, IDMA, SPI), the ROM
memory-clear accelerator and debugger writes all go through the board,
which checks it (one test while nothing is watched). The JIT then drops
the translations that contain the word; they are retranslated when they
run again. (Watching started with whole pages; why it became words is in
17.10.)

Three details make this airtight:

* the **fast tables never hold a store entry for a page with watched
  code**, so the JIT's own stores to code always take the helper path and
  get checked; every translation flushes the store tables;
* if a block writes its own page, the helper stops it after that store,
  so the next instruction is fetched fresh, as in the interpreter;
* pages are identified by **host memory**, not physical address: the
  board's SDRAM aliases (a bank answers at several addresses), and a write
  through an alias must still be seen.

A change of the memory map (BRx/ORx, IMMR) throws away everything: what is
behind an address may have changed.

## 17.8 Step 7: verification

The tests are all "JIT versus interpreter: same final state".

* `ppc8xx/tests/jit.rs` (11 tests): loops of arithmetic, memory and
  branches; a machine check in the middle of a block; helper instructions
  and an alignment exception; a loop that rewrites its own first
  instruction; decrementer interrupts falling into blocks; an interrupt
  raised by a store; indirect branches with link; translation on, with TLB
  misses refilled by the manual's reload handlers, `tlbie` every pass and a
  protection fault inside a block; timebase and decrementer reads inside
  blocks, in supervisor and user mode. Each runs with
  translation thresholds 1 and 3 and compares registers, clock, timebase,
  decrementer, exception counts and all of RAM.
* `ppc8xx/tests/qemu_diff.rs`: the random instruction sequences of
  chapter 5 now also run through the JIT with threshold 1 (every block
  translated): against QEMU (400 sequences) and against the interpreter
  (`jit_matches_interpreter`, 3000 sequences, no QEMU needed).
* `c1700/tests/boot.rs::jit_boots_rommon_exactly_like_the_interpreter`:
  the real boot ROM to `rommon 1 >`; same virtual time, registers, console
  output and RAM after the same number of steps.

**Are the tests good enough? Mutation testing.** I planted bugs in the
code generator on purpose, one at a time:

| planted bug | hand-written tests | random sequences |
|---|---|---|
| `subf` operands swapped | **not caught** (no test used `subf`) | caught |
| `srawi` carry mask off by one bit | **not caught** (rare operand values) | caught |
| `bdnz` testing CTR ≠ 1 instead of ≠ 0 | | caught |
| `blrl` reading LR after the link | | **not caught** (random code has no indirect branches) |

The hand-written tests check the machinery (exits, faults, interrupts,
invalidation); the random ones check semantics. The last row led to the
`indirect_branches_with_link` test, which catches it. The same was done
for the region code of 17.10: a wrong instruction count on an internal
jump, a budget check against the count before the next block instead of
after it, and a timebase read one tick late were all caught.

## 17.9 Step 8: first measurement, and a surprise

The first benchmark run (`tools/bench.sh --cpu jit`) was **slower** than
the interpreter: 27.7 against 32.5 MIPS. `--stats` showed why: 59 111
blocks translated in 24.7 s of compile time, and **2104 complete flushes**.

The flushes were memory-map changes, but only 56 came from chip-select
registers. The rest: ROMMON writes the IMMR register 558 times **with the
value it already holds**, and each write counted as a new memory map. Now
only a change of the IMMR base counts (`Mpc860::set_immr`). This also
spares the board's own address decoder needless rebuilds. Result: 39
flushes, 3388 blocks, 0.9 s of compiling.

**A real IOS boot.** The scenario of chapter 15.6 (boot to `Router>`,
`show clock`, 30 s idle, configure the interface, ping 10.0.2.2) works
with `--cpu jit`: same console output, `show clock` advancing 30 s in a
30 s sleep, ping 4/5 (the first lost to ARP, as always). `Router>` came at
153.4 s, **exactly the interpreter's time**. That isn't a disappointment
but a measurement limit: with idle detection, virtual time may not run
ahead of the wall clock, and the boot was already paced by IOS's own
timers (147 s of virtual time were skipped as idle). To compare the
engines on IOS, the boot must be CPU-bound (`--idle off`).

**The hook pages.** A profile of the JIT run still showed half of the
time in the *interpreter*: `Machine::run`'s single-step path. The machine
single-stepped every instruction on the three 4 KB pages that contain a
hook (chapter 16.4), and the ROM's ordinary code on page 0xFFF16 (for
example the copy of the 21 MB image from flash to RAM) shares that page
with the memory clear. Hooks are now exact addresses: the page test stays
the cheap first check, the exact address is compared only on those pages,
and the JIT ends a block before a hook address (`stop_before` is asked
while translating). With the accelerators off, as in the benchmark,
there are no ROM hooks at all, so the memory clear itself is now batched
by the interpreter and translated by the JIT.

**Dispatcher.** Two small changes: a 4096-entry direct-mapped *jump
cache* (by PC) in front of the block hash table, and no cache
synchronisation after a block unless a helper ran (translated code alone
can't change the memory map, the MMU or watched pages): +5%.

The same session showed a second effect: with the JIT present the
**interpreter** was 4% slower (32.4 → 31.0 MIPS), even when not used. An
A/B bisection (old and new binaries back to back, then removing one change
at a time) cleared the store-path check and the MMU refactoring; removing
the JIT call from `Machine::run` restored the speed, but moving it out of
line didn't. Building both versions with forced 64-byte code alignment
shrank the gap to 2% (31.7 vs 31.0). So it is mostly where the hot loops
land in the binary, not work the interpreter does; the remaining 2% is not
explained yet.

**Explained later** (TODO item 13). By the time of 17.15 the gap had grown
to 7%: HEAD 33.5 MIPS, the same source with the JIT call removed from
`Machine::run` 36.2. Two more builds narrowed it down. With one codegen
unit and fat LTO both variants run at 34.0-34.1 MIPS, so the JIT's
*presence* costs nothing at run time; it changes the compiler's decisions.
A `perf` profile of the two default builds showed which decision:

| default build | `Cpu::execute` | `Cpu::run_batch` | `Board::fetch` |
|---|---|---|---|
| without the JIT call | 45% | 36% | inlined |
| with it | 44% | 25% | **14%, a separate function** |

The interpreter fetches every instruction through `Board::fetch`. While
the interpreter loop was its only real user, LLVM inlined it. The JIT
calls it too (the scanner, the checker's code capture), and with several
callers the inliner's cost model said no: one call, one return and the
lost optimisations across the boundary, per instruction. `fetch` is a
table lookup and a load, so it is now `#[inline(always)]`:

| | interpreter (1 G instructions) | JIT (3 G) |
|---|---|---|
| before | 33.5 MIPS | 408.7 MIPS |
| `fetch` always inlined | **36.6 MIPS** (+9%; the build without JIT call: 36.2) | 414.4 MIPS |

`tools/bench.sh` (3 G instructions): interpreter **35.8 MIPS** (was 32.0).
The lesson from 17.9 holds, with a sharper form: when unrelated code makes
a hot loop slower, look at what got *inlined*, and compare a profile of
both builds function by function.

## 17.10 Step 9: regions, and what compiling really costs

At 137 MIPS a profile showed the dispatcher as a big share again: blocks
average five instructions, and every block returned to it. This step
went in four moves, each measured on the benchmark.

**`mftb`/`mfdec` translated.** The time at the *i*-th instruction of a
region is the time at its entry plus *i* ticks (the clock advances once
per instruction, see `Cpu::tick`), so the generated code computes
`tb + (prescaler + i) / divider` from the `Cpu` struct. `mfdec` is
supervisor-only; in user mode it stays with the interpreter, which raises
the Program exception. The ROM's delay loops and IOS's timer reads no
longer cut loops in two.

**Regions.** A region is the block at the hot address plus every block
reachable from it by direct branches (taken and fall-through) inside the
same page (1 KB with translation on), at most 256 instructions, never
through a hook address. It becomes one function `fn(ctx, cpu, budget,
entry)`:

* `budget` = `min(steps left in the batch, instructions until the
  decrementer request)`; before every jump to another block of the region
  the code checks that the next block still fits, otherwise it returns
  with the exact PC and count, and the dispatcher continues;
* the count of instructions executed so far is a Cranelift variable, so a
  fault or helper exit anywhere returns the exact total (the faulting PC
  goes to `JitCtx::fault_pc`);
* each guest block re-reads the registers it uses from the `Cpu` struct
  (it can be entered from several places; write-through keeps the struct
  current).

Region runs went from 5 to ~120 instructions per dispatcher call. But the
benchmark barely moved: **compiling** now took 12 of 21 seconds.

**What compiling costs.** Three findings, in order:

1. Cranelift's IR verifier runs on every function by default
   (`enable_verifier`): off, 12 → 7.7 s.
2. 70% of the translations were thrown away by code writes, all on one
   physical page, 0x4000. With the `--watch` tool and a backtrace on the
   first hits: a halfword at 0x43a4 is written over and over during the
   boot, and translated code starts at 0x43a8, **four bytes later**.
   (It turned out to be ROMMON copying its checksum routine onto its stack
   before running it: chapter 22.) Watching whole pages, then 32-byte lines, kept throwing that
   code away. Now the platform watches exactly the translated instruction
   words; the JIT indexes its regions by 32-byte lines. Invalidations:
   2476 → 10.
3. Only **4 062 distinct instructions** had been compiled **141 734
   times**: every hot address got its own region with everything
   reachable from it, so each loop body was compiled once per entry into
   it. Now a region is a **multi-entry** function: a jump table on the
   `entry` parameter, every block start of a region becomes a ready entry
   in the dispatcher's table, and a new region stops where an existing
   one already has an entry. Each piece of code is compiled once;
   execution passes between regions through the dispatcher. Compiling:
   4.35 → 0.3 s.

(A bug on the way: Cranelift panicked in `remove_constant_phis`. The jump
table used the same `BlockCall` as its default and as entry 0; the SSA
builder appends the variables' arguments to each edge's argument list, so
the shared list got them twice. Each edge needs its own `BlockCall`.)

With compiling cheap, Cranelift's `opt_level` hardly matters any more
(`none` 305, `speed` 306 MIPS); it stays at `speed`.

## 17.11 Results

`tools/bench.sh` (3 billion executed instructions: ROMMON, then IOS's
self-decompression; accelerators and idle detection off):

| step | interpreter | JIT |
|---|---|---|
| first working version | 32.5 MIPS | 27.7 MIPS (slower!) |
| IMMR rewrites no longer flush everything (17.9) | 31.0 | 59.6 |
| jump cache, sync only after helpers | 31.0 | 62.5 |
| exact hook addresses (17.9) | 32.0 | 136.9 |
| `mftb` native, regions | | 142.2 (compiling: 12 s) |
| Cranelift verifier off | | 176.6 |
| code watched per word | | 197.9 |
| multi-entry regions (no duplicate compiling) | | 302.2 (compiling: 0.3 s) |
| clock helpers inlined | **32.0** | **312.6** (93.8 s vs **9.6 s**) |
| registers per guest block, comparisons feed branches (17.13) | | +1.5% (297.4 → 301.6 in one A/B session) |
| chaining (17.14) | | +17% (301.1 → 352.9 in one A/B session) |
| flash read directly (17.15) | | +17% (349.6 → 408.7 in one A/B session) |
| `Board::fetch` always inlined (17.9) | **35.8** | 414 |

(Absolute numbers move by a few percent from one day to the next with
the machine's state: the old binary gave 297-300 MIPS in the session of
the last row. Compare steps only within one A/B session.)

The JIT is **~9.8×** the interpreter on this benchmark. 98.9% of the
instructions run as translated code, a region run averages ~49
instructions, and 788 regions (1572 entry points, 357 KB of host code)
were compiled in 0.4 s.

**IOS**, boot to `Router>` with `--idle off` (everything CPU-bound, the
idle loop included):

| engine | `Router>` after |
|---|---|
| interpreter | 217.8 s |
| JIT, first version (block per function) | 66.2 s |
| JIT, regions | **38.4 s** (5.7×) |

In the same session, configuring the interface and pinging the virtual
host work (4/5, 1 ms), and `show clock` advanced 96 s during a 30 s wait:
with nothing holding it back, the emulated router runs about 3× faster
than the real one. With idle detection on (the default), both engines
reach `Router>` at about the same time: virtual time may not run ahead of
the wall clock, and IOS's boot waits on its own timers (17.9).

Where the time went at this point (`perf`, benchmark; 17.15 has today's
numbers): translated code 43%, the
dispatcher ~14%, reading the IOS image out of flash through the device
path ~10%, the interpreter for the rest (tails, cold code, untranslatable
instructions).

## 17.12 Lockstep checking: `--cpu jit-check`

The tests compare the JIT with the interpreter on chosen programs. To
verify it on everything IOS does, `--cpu jit-check` checks **every region
run** against the interpreter while the machine runs (`jit/check.rs`):

1. the region runs on the real platform through a **recorder**, with the
   fast memory tables off, so every memory access goes through the helpers
   and is logged with its value (and the interrupt line after it);
2. the CPU is put back in its state before the region (`Cpu::arch_state`),
   and the **interpreter** executes the same number of steps against a
   **replayer**: reads are answered from the log, writes are compared
   with it, so the devices never see an access twice; instruction fetches
   come from a copy of the region's page taken before it ran (a region
   can rewrite its own code);
3. registers, clock, reservation and exception counts must be identical
   and the log fully consumed, otherwise the emulator stops:

   ```
   JIT check failed: region at 0x00001000, 7 instructions: interpreter vs JIT: r3: 0x00000002 vs 0x00000001;
   ```

   (That one is real output, from the checker's own first version: it
   replayed fetches from memory the region had already rewritten. The fix
   is the page copy of step 2.)

`read_spr` queries (does the platform implement this SPR? only IMMR on
this board) are not logged: the interpreter asks for every `mfspr`, the
JIT only for helper ones, and the answer has no side effects.

The JIT tests also run in check mode, and the exact ROMMON boot test
boots with `--cpu jit-check` too. A whole IOS boot in check mode
(`SCALE=20 LIMIT=30000 tools/ios_session.py ... --cpu jit-check`) reaches
`Router>` after 503 s, against 30 s unchecked: **102,754,115 region runs
verified, all identical**. (With `--idle off` the idle loop runs too: a
first run verified 291,591,636 region runs, about 6 billion instructions,
before the session driver gave up waiting for the setup dialog.)
Regions are checked one run at a time, so chaining (17.14) is off in
check mode.

## 17.13 Step 10: registers in host registers, comparisons feeding branches

The listing of 17.4 shows two wastes: every register write is stored to
the struct at once (write-through), and a conditional branch extracts a
CR bit that the compare right before it has just computed.

**First attempt: registers live for the whole region.** Load every
register the region uses once, at entry; keep them in variables
everywhere; store the ones it writes at the exits and around interpreter
helper calls. The used and written sets must be known before any code is
emitted, so the region was translated twice (a first pass only collecting
them). It worked, and it was **slower**: 283 MIPS against 300, twice the
host code (789 KB against 357), twice the compile time. x86-64 has 15
usable registers (5 of them preserved across calls), a region touches 20
or more guest registers, and now all of them are live everywhere: the
register allocator spills them, every loop edge passes all of them as
block arguments, every slow-path call saves the live ones, and every exit
stub stores all the written ones.

**What works is QEMU's TCG scheme, per guest block.** Inside a guest
block (straight-line code up to its branch) a register is loaded when
first read; a write only changes the variable and marks the register
*dirty*. Dirty registers are stored:

* at the end of the guest block, before its branch, so both successors
  (and other regions, and the dispatcher) find the struct up to date;
* before an interpreter helper call, which works on the struct;
  afterwards every register is re-read when next used, since the helper
  may have changed anything;
* in each **exit stub**: a stub records the dirty set at its own exit
  point, so a fault in the third instruction of a block stores what the
  first two wrote and nothing else. Exceptions stay precise.

One subtlety, found by the lockstep checker (17.12) on the first run. A
memory access splits the code into a fast path (table hit) and a slow
path (helper call) that join again, and "loaded" and "dirty" describe one
path. The first version carried the fast path's state into the slow path:
after `lwz r5` the fast path had marked r5 dirty, so the slow path's
fault stub stored r5, a value never defined on that path, which Cranelift
silently makes 0. In check mode,
`translated_mode_with_tlb_misses_and_faults` stopped with:

```
JIT check failed: region at 0x0000100c, 1 instructions: interpreter vs JIT: r5: 0x00000105 vs 0x00000000;
```

Each side now starts from the state at the split. At the join, a register
counts as loaded only if both sides loaded it, and as dirty if either
side dirtied it.

**Comparison results feed the branch.** `compare()` remembers the three
booleans (LT, GT, EQ) it computed for the CR field it wrote. A following
`bc` testing bit 0, 1 or 2 of that field uses the boolean directly
instead of extracting the bit from CR. The cache is forgotten at every CR
write, at helper calls and at the start of every guest block (its values
only exist on the code path that computed them). The CR field itself is
still built and stored, because it is architectural state: the next block
or an interrupt handler may read it.

The loop of 17.4 today (comments added):

```
leal 1(%r14), %edx            ; addi r3, r3, 1 (r3 has been in %r14 since the stb)
movl %edx, 0x7cc(%r12)        ; r3 stored: once per block, at its end (scheduled early)
cmpl %ecx, %edx ; setb/seta/sete ...  ; cmplw: CR field 1 built ...
movl %esi, 0x964(%r12)        ;   ... and CR stored, once
cmpl %ecx, %edx ; jb taken    ; blt cr1: the comparison itself, no CR bit read back
```

Measured in one A/B session (`tools/bench.sh --cpu jit`, three runs
each): **297.4 → 301.6 MIPS** (+1.5%), with 11% more host code (stubs
store their dirty registers). A small gain, because the translated code
is only ~40% of the benchmark's time; the next sections are about the
other 60%.

**Tests.** The JIT tests (also in check mode), the random differential
test against the interpreter, and the ROMMON boot tests pass. Mutations
are caught: removing the stub stores, the store before a helper or the
reload after it, or inverting the cached comparison, each makes JIT tests
fail.

## 17.14 Step 11: chaining regions

After 17.13 the dispatcher (`run_batch`) took 14% of the benchmark: every
time a region left (a call, a return, a loop spanning two regions), Rust
code found the next region (interrupt check, MMU translation of the PC,
jump cache), checked the budget, advanced the clock and called it. With
regions averaging 49 instructions per run, that adds up.

**Chaining** jumps from one region straight into the next, as QEMU does
between its translation blocks:

1. Every region exit to a known address outside the region gets a
   **link**, a small record the generated code reads: `{func, entry, len,
   epoch, target}`.
2. The exit code checks the link: current (`epoch` equals
   `JitCtx::chain_epoch`) and the next block fits in the budget (`total +
   len <= budget`)? Then it **jumps** into `func` at block `entry`.
   Otherwise it returns to the dispatcher, leaving the link's address in
   `JitCtx::exit_link`.
3. The dispatcher fills that link in when the very next thing it does is
   run a region at the link's target: `{that region, its entry, its first
   block's length, the current epoch}`. From then on, that exit chains.

The jump is a **tail call**: regions use Cranelift's `tail` calling
convention and leave with `return_call_indirect`, so a chain of a
thousand regions uses one stack frame. The steps executed so far are
passed along as a fifth argument (`done`), so each region continues
counting from there and the last one returns the total for the whole
chain. Rust can't call a `tail`-convention function, so the dispatcher
calls regions through a five-instruction **trampoline** compiled once per
code module (C convention in, `tail` convention out, `done` = 0). (One
surprise: Cranelift's x86-64 tail calls need frame pointers, the
`preserve_frame_pointers` flag; without it the first compile panics.)

**Computed branches** (`blr`, `bctr`) have a link too, remembering the
target it was last filled in for; the exit compares the branch's address
with it before jumping. It is a one-entry inline cache: a loop calling a
function returns to the same place every time.

**Why nothing is skipped.** Between two regions the dispatcher checks
hooks, interrupts, the decrementer, the MMU and its own caches. On a
chained path none of these can change:

* interrupts appear only through device accesses, and a helper that sees
  one raised (with `MSR[EE]`=1) already ends the region;
* the decrementer can't fire: the budget the chain shares was limited to
  the instructions before it fires, and every chained jump checks it;
* `MSR` and the MMU only change in instructions left to the interpreter
  (`mtmsr`, `rfi`, `sc`, `tlbie`, MMU `mtspr`s), which always go back to
  the dispatcher, so a whole chain runs in one mode;
* hook addresses never start a region (17.6), and the hook set can only
  shrink (idle detection narrows candidates down to one anchor).

**Invalidation.** A link is current only while `chain_epoch` hasn't
moved, and it moves when a region is dropped (rewritten code: a link may
lead into it), on a flush, and when the instruction MMU's translations may
have changed (`Mmu::instruction_generation`: a link doesn't translate its
target address again). Each of these kills all links at once; they are
filled in again lazily, one dispatcher round trip each, which is cheap
next to how rarely it happens.

**Tests.** `chained_jump_into_rewritten_code` (a caller rewrites the
region it calls every 64 calls) and `chained_jump_follows_an_instruction_remap`
(translation on: the called virtual page is switched between two physical
pages with different code, `tlbie` after each switch) check the two
invalidation rules; without the epoch bump each fails, as do the existing
tests when the budget check or the computed-target comparison is removed.
The IOS boot works as before.

**Measured** (one A/B session, three runs each):

| | MIPS | regions entered by the dispatcher |
|---|---|---|
| before chaining | 301.1 | 60.6 million |
| known exits chained | 347.5 | 16.2 million |
| computed branches too | **352.9** | 11.8 million |

+17%. 80% of region runs now start with a jump from another region, and
the dispatcher's share of the time fell from 14% to 6%. IOS boots to
`Router>` with `--idle off` in **32.0 s** (38.4 s before). The host code
grew from 399 to 555 KB (frame pointers, the exit checks).

## 17.15 Step 12: reading flash directly

ROMMON copies the IOS image (21 MB) out of flash into RAM before booting
it, and the benchmark's first ~600 million instructions are mostly that
copy plus a checksum over it (chapter 22). Every flash read went through
a helper, the interpreter's `load8`, the board's slow path and the flash
model's `read16`: ~16% of the benchmark's time.

In read-array mode the flash *is* memory: a read returns the array byte,
exactly what the backing `Vec<u8>` holds. The only difficulty is that the
mode changes. Any command (read ID, read status, program, erase) switches
the bank to returning something else, and a direct pointer handed out
earlier would keep returning the array.

* The board's `host_page` now also hands out flash pages, for reads only
  and only while their bank is in read-array mode. Writes always go
  through the model, because on flash every write is a command.
* A new `Platform::host_page_epoch` moves whenever a flash write takes a
  bank **out of** read-array mode. It is the same idea as
  `memory_map_epoch`, but cheaper: that one throws all translations away,
  this one only empties the fast memory tables (`JitCtx::host_epoch`, as
  for an MMU change). A store helper that moves it ends the region, so no
  translated code runs between the command and the flush.
* Programming a flash file needs one bump per session, not one per word:
  once the bank is in status mode its pages aren't handed out, and the
  `0xFF` (read array) at the end needs nothing; the next reads fill the
  tables again.

The AMD model (`--flash-size 4`) is readable unless a chip is in
autoselect (ID) mode; its program and erase change the array at once, and
direct readers see that like the model's own reads.

**Tests.** `c1700/tests/flash_jit.rs` boots a 47-instruction synthetic
ROM that maps the flash, then loops: sum 1 KB of flash, read the
identifier (and a data address in ID mode), back to read-array, program a
word, read the status and the programmed word. Interpreter, JIT and JIT
in check mode must agree on all registers and on the flash contents;
without the epoch bump the JIT reads array data in ID mode and the test
fails. In IOS, `copy running-config flash:test.cfg`, `show flash:` and
`more flash:test.cfg` work under the JIT, and the file is in the host's
flash image afterwards.

**Measured** (one A/B session): **349.6 → 408.7 MIPS** (+17%); the
`--stats` output (instruction counts, exceptions, devices) is
byte-identical before and after.

With 17.13-17.15 together the JIT runs the benchmark at ~410 MIPS,
**~11.5×** the interpreter (35.8 MIPS since the inlining fix at the end of
17.9), and IOS reaches `Router>` with `--idle off` in **30.4 s**
(interpreter: 217.8 s).

## 17.16 What limits it, and next steps

`perf record -e cycles:u` of the benchmark (after 17.15):

| where | share |
|---|---|
| translated code | 59% |
| the interpreter (`execute`, `fetch`, `interpret`: tails at batch ends, helper instructions) | ~15% |
| the dispatcher (`run_batch`, `sync`) | ~9% |
| device and machine bookkeeping per batch (`sipend`, `advance`, `Machine::run`) | ~6% |

* **Tails**: a region only starts if its first block fits in what is
  left of the batch (256 instructions between device syncs), so the last
  instructions of almost every batch are interpreted: 1.1% of the
  instructions, but ~15% of the time. Splitting a region's entry block at
  the budget (or longer batches when no device needs attention) would
  remove most of it.
* **Per-batch bookkeeping**: the machine syncs the devices every 256
  instructions; with the JIT that granularity costs more than the
  instructions themselves in some loops.
