# 14. Speed: how the interpreter spends its time, and what a JIT would mean

The emulator runs IOS at about **23 million instructions per second**, about
half the speed of a real 48 MHz MPC860, so IOS needs ~5 minutes to reach
`Router>`. This chapter explains why, what a JIT (just-in-time compiler)
is and how one would work for *this* emulator, and the plan I'd follow,
cheaper steps first. Nothing in this chapter is implemented yet.

## 14.1 What one emulated instruction costs today

`Cpu::step()` (chapter 5) does this for **every** guest instruction:

```
1. interrupt check        EE? external line? decrementer pending?
2. ITLB translate         MSR[IR]? fast-cache lookup (or TLB search)
3. fetch                  board: 32 KB-page region table → match → DRAM read
4. decode                 decode cache: hash the 32-bit word → cached Ins
5. dispatch               big `match` on the opcode (~150 arms)
6. operands               extract rD/rA/rB/immediates from the word
7. execute                the actual work: often a single add or compare
8. data access (if any)   DTLB translate → board region table → device or RAM
9. tick                   cycles++, timebase prescaler, decrementer
```

Step 7 is the only "useful" part; steps 1-6 and 9 repeat identical work
every time the same instruction runs. In a loop executed a million times,
the same word is fetched, translated and decoded a million times. On this
machine that comes to ~43 ns, roughly 150 host clock cycles, per guest
instruction. A real MPC860 does about one instruction per clock.

Measured facts so far:

* core alone, flat RAM: ~88 MIPS (~102 before the MMU checks, which cost
  ~14% even with translation off, more than they should);
* full board under IOS: ~23 MIPS. The difference is the board's address
  decoding, device accesses and TLB lookups;
* an idle IOS still runs its scheduler loop flat out.

Where exactly the 150 cycles go has to be **measured** with a profiler
(`perf`) before optimising. The list above only names the suspects.

## 14.2 What a JIT is

An interpreter re-reads the program every time. A JIT **translates** guest
code into host machine code once, and from then on just runs the
translation.

The unit of translation is usually a **basic block**: straight-line guest
code up to and including a branch. Take this IOS-style loop:

```
loop:  lbz   r9, 0(r3)        ; load a byte
       addi  r3, r3, 1
       cmpwi r9, 0
       bne   loop             ; until the terminating zero
```

The interpreter executes these four instructions with ~600 host cycles of
overhead per iteration. A JIT would emit roughly this x86-64 once (`rbx`
points at the `Cpu` struct, where the guest registers live):

```
block_loop:
    mov   eax, [rbx + r3]          ; guest r3
    ; --- lbz: translate + load, inline fast path ---
    mov   ecx, eax
    shr   ecx, 12                  ; page number
    ...   lookup in the TLB fast cache → host pointer to the RAM page
    movzx edx, byte [host_page + offset]   ; or call the slow path for I/O
    mov   [rbx + r9], edx
    ; --- addi ---
    add   eax, 1
    mov   [rbx + r3], eax
    ; --- cmpwi + bne ---
    test  edx, edx
    ... set CR0 bits ...
    add   qword [rbx + cycles], 4  ; time: 4 instructions
    jne   block_loop               ; (after checking for pending interrupts)
```

That's about 20 host instructions per iteration instead of hundreds. Real
JIT-based emulators (QEMU, Dolphin) reach 10-50× an interpreter on hot code.

## 14.3 The hard parts, for this emulator specifically

**Precise exceptions.** Any load or store can raise a DTLB miss, a DTLB
error, a machine check (bus error) or an alignment exception, and IOS
relies on all of them being exact. The guest state must then be precisely
what it was at the faulting instruction: earlier instructions done,
SRR0 = that instruction. So guest registers stay in memory (the `Cpu`
struct), each memory operation has an exit path, and on a fault the
generated code stores the guest PC and returns an "exception" code to the
dispatcher.

**Interrupts and virtual time.** Time is instruction-count based. A block
adds its length to `cycles` and checks for pending interrupts at its end.
Bounding blocks (say ≤ 64 instructions) bounds the interrupt latency. Runs
stay deterministic, but interrupts land a few instructions later than in
the interpreter, which matters when comparing the two.

**Self-modifying code.** IOS writes its own code: the self-decompressor
writes 55 MB, then jumps into it. Translations must be keyed by **physical**
address, and a store to a page that has translations must invalidate them.
That means a per-page "contains code" bit checked on the store path. The
8xx's explicit `icbi`/`IC_CST` cache invalidations are useful hints.

**The MMU.** Blocks never cross a 4 KB page boundary, so one ITLB lookup
covers a whole block. The block lookup is keyed by (physical address, MSR
bits that change semantics: IR/DR/PR). TLB changes (`tlbie`, `tlbia`,
`Mx_RPN` writes) flush the virtual → block lookup, not the code itself.

**Memory accesses.** Most of the speed is in an inline fast path: EA →
TLB fast cache → host pointer into DRAM → load/store. Everything else (I/O
bus, IMMR registers, flash) calls a slow-path helper written in Rust,
the same code the interpreter uses.

**Branches.** Direct branches can be *chained*: the end of a block jumps
straight to the next translated block. Indirect branches (`blr`, `bctr`,
`rfi`) look up their target in a hash table.

**Everything rare** (SPR accesses, `rfi`, `sc`, `tlbie`, string
instructions, `mtmsr`…) can simply end the block and run in the
interpreter. The JIT only has to handle the common integer/branch/load/
store subset well.

## 14.4 How it could be built in Rust

| Approach | Idea | Expected gain | Cost |
|----------|------|---------------|------|
| **Pre-decoded blocks** ("threaded interpreter") | decode a block once into a list of ops with pre-extracted operands; run the list | 2-3× | moderate; no machine code, no `unsafe` |
| **Cranelift** (the code generator used by Wasmtime) | build Cranelift IR per hot block; it does register allocation and emits x86-64 or AArch64 | 5-15× on hot code | large; generated-code `unsafe`, compile time per block (interpret first, compile after N executions) |
| **dynasm-rs** | hand-written x86-64 templates per instruction | similar to Cranelift, faster compiles | large; x86-64 only, more code to get right |
| reuse QEMU/Unicorn | rejected in chapter 6 (no 8xx SPRs, wrong exception model) | | |

Cranelift fits the project's "prefer maintained crates" rule. Pre-decoded
blocks are worth doing first anyway: they need the same block discovery,
invalidation and exit machinery a JIT needs, without generating code.

## 14.5 The plan I would follow

The bigger wins for the *experience* of using the emulator aren't the JIT:

1. **Snapshots**: save the whole machine at `Router>` and restore in
   seconds. That turns the 5-minute boot into a few seconds no matter how
   fast the CPU is.
2. **Profile and fix the cheap overheads** with `perf`. **Done**
   (chapter 15.8): +47% interpreter speed (27 → 40 MIPS), the ROM's memory
   clear in bulk, IOS boot 216 → 153 s.
3. **IOS idle detection**: recognise the idle loop and advance virtual time
   to the next timer interrupt. **Done** (chapter 15): boot 318 → 206 s, an
   idle router uses ~0% CPU.
4. **Pre-decoded blocks** (14.4), 2-3× on top, and the foundation for:
5. **A Cranelift JIT for hot blocks**, verified in **lockstep**: run each
   block in both the JIT and the interpreter on a copy of the state and
   compare registers and memory writes. Plus the existing QEMU differential
   tests and the IOS boot as the end-to-end test.

Honest expectations: the self-decompressor (tight loops) would gain the
most, maybe 10×; IOS as a whole less, because device accesses, MMIO and
idle time don't get faster with a JIT. Steps 1-3 give most of the practical
benefit for a fraction of the risk; steps 4-5 are a substantial project.
