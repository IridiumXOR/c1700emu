# 05 — The CPU core (`emu/ppc8xx`)

This chapter explains how the PowerPC core of the MPC860 is emulated: how an
instruction word becomes an action, how the flags (XER, CR) are computed, how
the 8xx exception model works, and how we *prove* the implementation is right
by comparing it against QEMU.

Code: `emu/ppc8xx/src/lib.rs` (state, exceptions, SPRs, timebase),
`emu/ppc8xx/src/execute.rs` (one `match` arm per instruction),
`emu/ppc8xx/src/decode_cache.rs` and `emu/ppc8xx/src/mmu.rs` (the 8xx MMU,
chapter 12).

---

## 1. Where the core sits

The core knows nothing about Cisco boards. Everything outside it is reached
through one trait:

```rust
pub trait Platform {
    fn read8/16/32(&mut self, addr) -> Result<_, BusError>;
    fn write8/16/32(&mut self, addr, value) -> Result<(), BusError>;
    fn fetch(&mut self, addr) -> Result<u32, BusError>;        // instruction fetch
    fn read_spr / write_spr(...)                                // only IMMR on the 860
    fn external_interrupt(&self) -> bool;                       // SIU interrupt line
    fn system_call(&mut self, cpu) -> bool;                     // test harness hook
}
```

That is the same boundary the silicon has: the PowerPC core talks to the SIU
over an internal bus, and the SIU decides what lives at each address. Because
of this split we can run the *same* core against the Cisco board model **and**
against a tiny "fake Linux" used by the tests.

`Cpu::step()` does, in order:

1. **Asynchronous interrupts** (only if `MSR[EE]=1`): external interrupt first,
   then decrementer. They are taken *between* instructions.
2. **Translate** `pc` through the ITLB when `MSR[IR]=1` (a miss or a
   protection fault becomes an ITLB miss/error exception), then **fetch**
   the word at the physical address (a bus error here = Machine Check).
3. **Decode** (decode cache in front of the `powerpc` crate) and **execute**.
   Every load/store goes through one funnel (`load8/16/32`, `store8/16/32`)
   that translates through the DTLB when `MSR[DR]=1`.
4. **Tick** the virtual clock (timebase/decrementer).

```
             ┌──────────── Cpu::step ────────────┐
 pending IRQ?│ EE && (external || decrementer)   │──► exception entry
             │ pc ──ITLB (IR)──► pa ──fetch──►   │
             │ word ──decode cache──► Ins        │
             │ execute(Ins): ALU / branch / SPR  │
             │   loads/stores: ea ──DTLB (DR)──► pa ──Platform──► board
             │ tick(): cycles++, TB/DEC          │
             └───────────────────────────────────┘
```

---

## 2. Decoding: the `powerpc` crate

PowerPC instructions are 32 bits. The top 6 bits are the *primary opcode*;
many instructions share primary opcode 31 and are told apart by a 10-bit
*extended opcode* in bits 21-30. Fields such as `rD`, `rA`, `rB` sit at fixed
positions. Example — `0x7C6802A6`:

| bits  | value | meaning                             |
|-------|-------|-------------------------------------|
| 0-5   | 31    | primary opcode "X-form group"       |
| 6-10  | 3     | rD = r3                             |
| 11-20 | 8 / 0 | SPR field, *halves swapped* → SPR 8 = LR |
| 21-30 | 339   | extended opcode = `mfspr`           |

So it's `mfspr r3, LR`, better known as `mflr r3`.

We don't hand-write that bit-twiddling. The `powerpc` crate (a table-driven
decoder generated from an ISA description, maintained by the GameCube/Wii
decompilation community) gives us:

```rust
let ins = Ins::new(word, Extensions::none());   // decode
match ins.op {                                   // Opcode enum
    Opcode::Mfspr => { let spr = ins.field_spr(); /* already un-swapped */ }
    ...
}
```

and `ins.simplified()` prints the disassembly with simplified mnemonics
(`mr`, `clrlwi`, `bdnz`...) — handy for traces and test failure reports.

Accessors already do the annoying parts: `field_bd()`/`field_li()` return
sign-extended byte offsets, `field_spr()`/`field_tbr()` undo the swapped
halves, `field_oe()`/`field_rc()` give the XO/record bits.

One gap: the crate doesn't know `tlbia` (an 8xx instruction), so `execute`
recognises its raw encoding (`0x7C0002E4`).

### The decode cache (added after profiling the real boot)

`Opcode::detect` finds the opcode by scanning the candidate list for the
primary opcode. For opcode 31 that list is long: `examples/decode_bench.rs`
measured **~42 ns** per opcode-31 decode against ~4.5 ns on average — and
opcode-31 instructions (`mr`, `mflr`, `mtspr`, `cmpw`, `stwx`...) are about a
third of what the ROM executes. `decode_cache.rs` memoises results in a
16 Ki-entry direct-mapped table indexed by a Fibonacci hash of the
instruction word. Keying on the *word* (not the address) means self-modifying
code and code copied into RAM can never get a stale opcode. Every slot starts
as a valid memo of word 0, so there is no "empty" state to get wrong.

---

## 3. Arithmetic, XER and CR

### One adder for everything

The hardware has one adder computing `a + b + carry_in`. We do the same
(`add3` in `execute.rs`) and express every add/subtract through it:

| instruction | computed as            | why                             |
|-------------|------------------------|---------------------------------|
| `add`       | `rA + rB + 0`          |                                 |
| `adde`      | `rA + rB + CA`         | multi-word additions            |
| `subf rD,rA,rB` | `~rA + rB + 1`     | two's complement: `-x = ~x + 1` |
| `subfe`     | `~rA + rB + CA`        | multi-word subtraction          |
| `addme`     | `rA + 0xFFFFFFFF + CA` | adding `-1`                     |
| `neg`       | `~rA + 0 + 1`          |                                 |

Note PowerPC's subtraction convention: **CA = 1 means "no borrow"**.
`subfc` of `5 - 3` sets CA, `3 - 5` clears it.

**Signed overflow** happens only when both operands have the same sign and the
result has the other one. In bit-math: `((a ^ r) & (b ^ r)) >> 31`.

### XER: SO, OV, CA

* **CA** — carry, set by the "carrying" forms (`addc`, `adde`, `addic`,
  `subfic`, `srawi`, ...).
* **OV** — set/cleared only by instructions with **OE=1** (the `o` suffix:
  `addo`, `mullwo`, `divwo`...).
* **SO** — *summary* overflow: **sticky**. Set together with OV, but never
  cleared by arithmetic; only `mtxer` or `mcrxr` clear it. This lets you run a
  long computation and check once at the end whether anything overflowed.

### CR0 and the record form

The `.` suffix (Rc=1) writes CR0 from the result: LT / GT / EQ from a
*signed* comparison with zero, plus a copy of XER[SO]. Order matters:
`finish_xo()` updates OV/SO **first**, then CR0, so `addo.` that overflows
reports SO in CR0 — QEMU agrees.

`andi.`, `andis.` and `addic.` *always* record: they have no Rc bit (bit 0 is
part of their opcode).

### Compare

`cmpw` (signed) / `cmplw` (unsigned) and the immediate forms write LT/GT/EQ +
SO into any CR field (`crfD`). Same bits, two interpretations — `0xFFFFFFFF`
is "less than 1" for `cmpw` and "greater than 1" for `cmplw`.

---

## 4. Rotates, masks and the algebraic shift

`rlwinm rA,rS,SH,MB,ME` = rotate left by SH, AND with **MASK(MB, ME)**: ones
from bit MB to bit ME (MSB-first). If MB > ME the mask *wraps around*:
MASK(30, 1) = `0xC0000003`. `rlwinm` is the Swiss-army knife — `slwi`,
`srwi`, `clrlwi`, `clrrwi`, `extrwi`, `rotlwi` are all assembler aliases for
it (the Cisco ROM does `clrrwi r10,r10,15` to get the IMMR base).

`srawi`/`sraw` (shift right *algebraic*) sets **CA only if the value is
negative and at least one 1-bit was shifted out**. That odd-looking rule makes
this idiom a correct signed division by 2ⁿ rounding toward zero:

```asm
srawi r3, r4, 2     # -5 >> 2 = -2 (floor), CA=1 because 1-bits fell off
addze r3, r3        # -2 + CA = -1  (= trunc(-5/4))
```

Shift amounts come from 6 bits of rB: 32..63 shift everything out.

---

## 5. Branches and the BO field

Conditional branches (`bc`, `bclr`, `bcctr`) are controlled by 5-bit **BO**
and a CR bit index **BI**:

| BO bit | mask   | meaning                                         |
|--------|--------|-------------------------------------------------|
| 0      | `0x10` | ignore the CR condition                         |
| 1      | `0x08` | required value of CR bit BI                     |
| 2      | `0x04` | do **not** decrement CTR                        |
| 3      | `0x02` | (if decrementing) branch when CTR == 0, else when CTR != 0 |
| 4      | `0x01` | static prediction hint — no architectural effect |

Common aliases:

| alias     | BO   | BI          |
|-----------|------|-------------|
| `blr`     | 0x14 | (ignored)   |
| `beq crN` | 0x0C | 4·N + 2 (EQ)|
| `bne crN` | 0x04 | 4·N + 2     |
| `bdnz`    | 0x10 | (ignored)   |
| `bdz`     | 0x12 | (ignored)   |

Details that tests check: LK=1 writes LR **even if the branch is not taken**;
`blrl` jumps to the **old** LR (read before LK overwrites it); `bcctr` never
decrements CTR (that form is invalid).

---

## 6. Loads and stores

* `(rA|0)`: in address computations r0 reads as **zero** — that's why `li`
  is `addi rD,0,imm` and why `lbzx r8,0,r6` means "address = r6".
* **Update forms** (`lwzu`, `stwux`...) write the effective address back to rA
  — the classic way to walk an array or push onto a stack (`stwu r1,-16(r1)`).
* **Byte-reversed** forms (`lwbrx`, `sthbrx`) access little-endian data.
* **Misaligned** half/word accesses are legal on the 860 (the bus interface
  splits them); we split them into byte accesses, but aligned accesses reach
  the platform as single sized accesses — important for memory-mapped
  registers.
* `lmw`/`stmw` move rD..r31 (function prologues!). Misaligned → Alignment
  exception. `lmw` reads everything before committing, so a fault leaves the
  registers intact.
* **String instructions** `lswi/lswx/stswi/stswx` pack bytes MSB-first into
  consecutive registers, wrapping r31 → r0; the last register is zero padded.
  `lswx`/`stswx` take the byte count from XER[25-31].
* `lwarx`/`stwcx.` implement atomics: `stwcx.` stores only if the reservation
  from `lwarx` is still held for that address, and reports success in CR0[EQ].
* `dcbz` clears one **16-byte** cache line (the 860's line size).

A data access nobody answers becomes a **Machine Check** with DAR = address.

With `MSR[DR]=1` the effective address is first translated by the DTLB
(chapter 12): no matching entry → **DTLB miss** (0x1200); a matching entry
that forbids the access → **DTLB error** (0x1400) with DSISR/DAR. An
aligned half/word can't cross a page and is translated once; misaligned
accesses are split into bytes, each translated on its own. `icbi`, `dcbst`,
`dcbf` and `dcbi` have no cache to act on but still translate their address,
so they can fault like on the real core.

---

## 7. The 8xx exception model

On an exception the core does (MPC860UM ch. 6):

1. **SRR0** ← where to resume: the faulting instruction (so the handler can
   retry it), or the *next* one for `sc` and for asynchronous interrupts
   (decrementer, external) which happen *between* instructions.
2. **SRR1** ← MSR[16-31] + exception-specific bits (Program: bit 13 =
   privileged, bit 14 = trap).
3. **MSR** ← everything cleared except **ME** and **IP**: interrupts off
   (EE=0), supervisor mode (PR=0), MMU off (IR=DR=0), RI=0.
4. **PC** ← prefix + vector offset; prefix = `0xFFF00000` if MSR[IP]=1
   (how the Cisco ROM runs), else 0.

`rfi` reverses it: MSR ← SRR1, PC ← SRR0.

8xx specifics:

* **No FPU, and no "illegal instruction" program exception.** Floating point,
  unimplemented and illegal opcodes all go to **Software Emulation (0x1000)**
  — designed so an OS can emulate FP in software.
* **Machine check with MSR[ME]=0 = checkstop**: the core stops. `step()`
  returns `StepOutcome::Checkstop`; whether that becomes a reset
  (PLPRCR[CSR]) is the machine's decision — the Cisco ROM's `reset` command
  relies on exactly this (see chapter 03).
* Privilege: any SPR whose number has bit 4 (`0x10`) set is supervisor-only
  (SRR0/1, SPRGs, DEC, IMMR...). From user mode → Program exception.
* **MMU exceptions** (chapter 12): ITLB miss 0x1100 (SRR1 bits 4 and 10),
  DTLB miss 0x1200, ITLB error 0x1300 (SRR1 bit 1 = not mapped, 3 =
  guarded, 4 = protection), DTLB error 0x1400 (DSISR bits 1/4/6, DAR = EA).
  Exception entry clears IR/DR, so handlers run untranslated; `rfi` restores
  them from SRR1.

### SPR quirks

| SPR | quirk |
|-----|-------|
| `EIE` (80) / `EID` (81) / `NRI` (82) | the value written is ignored; *the write itself* sets EE+RI / clears EE, sets RI / clears EE+RI. A one-instruction interrupt enable/disable. |
| `IC_CST` (560) / `DC_CST` (568) | writes are *commands* (0x02000000 enable, 0x04000000 disable, 0x0A.. unlock all, 0x0C.. invalidate all); reads show the enable bit in bit 0. |
| `IMMR` (638) | delegated to the platform — its value moves the whole on-chip peripheral block. |
| MMU registers (784-799, 816-826) | modelled by `mmu.rs` (chapter 12): reading `M_TWB`/`MD_TWC` returns table-walk addresses, writing `Mx_RPN` loads a TLB entry. `M_TW` is a plain scratch register (the Cisco ROM's handlers use it). |
| debug comparators, `ICTRL`... | stored, not modelled. |
| `PVR` (287) | `0x00500000` = MPC860. |

### Virtual time

`cycles` counts executed instructions. Every `tb_divider` (16) instructions
the 64-bit **timebase** increments and the **decrementer** decrements; when
DEC goes from 0 to `0xFFFFFFFF` a decrementer interrupt becomes *pending*
until MSR[EE] allows it. Both only count when `tb_enabled` (TBSCR[TBE],
owned by the SIU model) is set. Being instruction-driven, runs are fully
deterministic — great for debugging.

---

## 8. Testing

### Unit tests (`tests/semantics.rs`)

48 focused tests, written as real assembly thanks to a one-line assembler
built on the `powerpc-asm` crate:

```rust
let (cpu, _) = exec(&["srawi r3, r4, 2", "addze r3, r3"], |c, _| c.gpr[4] = (-5i32) as u32);
assert_eq!(cpu.gpr[3], (-1i32) as u32);
```

They cover carries/overflow, SO stickiness, rotate masks, shift-by-32, BO
combinations, string/multiple ops, reservations, `dcbz`, every exception
entry path, EIE/EID/NRI, decrementer pending logic, checkstop.

### Differential testing against QEMU (`tests/qemu_diff.rs`)

Hand-written tests only check what you thought of. Differential testing finds
what you *didn't*:

1. Generate random instruction sequences (seeded → reproducible). Each is
   wrapped in a **prologue** loading random values into r0-r28, CR, XER, CTR,
   LR and an **epilogue** dumping r0-r30, CR, XER, LR, CTR and a 1 KB scratch
   buffer with the Linux `write` syscall.
2. Batch 100 sequences into one static PowerPC Linux ELF (`llvm-mc` +
   `ld.lld`); the random instructions are emitted as `.long` words.
3. Run it under **`qemu-ppc-static`** (user-mode Linux emulation) and under
   **our core**, with a 40-line fake Linux (`LinuxUser`) implementing `write`
   and `exit` through `Platform::system_call`.
4. Compare stdout byte for byte. On mismatch the test prints the sequence, the
   initial state and a per-register diff.

The generator covers XO arithmetic with all OE/Rc variants, immediates,
logical/rotate/shift, compares, CR ops, CTR/LR/XER moves, "skip next
instruction" conditional branches with every BO class, loads/stores (all
widths, update/indexed/byte-reversed, misaligned), `stmw`, all string
instructions and `lwarx`/`stwcx.` pairs. Excluded on purpose: architecturally
undefined results (divide by zero, `0x80000000 / -1`), `dcbz` (QEMU's 750 has
32-byte lines, the 860 has 16), `mftb`, privileged instructions.

Running it:

```sh
cargo test -p ppc8xx                                   # 400 sequences, ~0.3 s
QEMU_DIFF_SEQUENCES=25000 QEMU_DIFF_SEED=7 cargo test -p ppc8xx --test qemu_diff -- --nocapture
```

The test skips itself (with a message) if `qemu-ppc-static`, `llvm-mc` or
`ld.lld` is missing.

**What it caught:**

* **100,000 sequences (4 seeds × 25,000) match QEMU byte for byte.**
* The first big run reported a mismatch in `divwu`. Digging in showed a
  *generator* bug: a random "skip next instruction" branch had skipped the
  `ori rB,rB,1` that guarantees a non-zero divisor, so the program divided by
  zero — undefined behaviour, where QEMU and we legitimately return different
  garbage. Fixed by starting the idiom with a `nop` that absorbs the skip.
  Lesson: the oracle is only as good as the inputs; when a diff test fails,
  first check that the program is *defined*.
* To make sure the harness can actually fail, I deliberately broke the
  `srawi` carry rule (CA = "negative" instead of "negative and 1-bits shifted
  out"): the very first batch flagged it, with the exact instruction.
* The unit tests caught three mistakes — all in my *expectations* (e.g. the
  value of an `rlwimi` I had computed by hand), none in the CPU.

### MMU tests

8 unit tests in `mmu.rs` and 3 end-to-end tests in `tests/mmu.rs`, where
real code takes TLB misses and the reload handlers from the manual
(Figures 8-23/8-24) refill the TLB through the table-walk assist (chapter 12).

### Speed

`cargo test --release -p ppc8xx --test throughput -- --ignored --nocapture`
on a flat-RAM platform: **~88 million instructions/second** today, **~102**
at the commit before the MMU (same machine, measured back to back). The
translation checks cost ~14% even with translation off, more than they
should; that's on the speed list (chapter 14). On the real board, under
IOS with translation on, the whole emulator does **~23 MIPS** (address
decoding, devices, TLB lookups).

---

## 9. Known limitations

* No trace (MSR[SE]/[BE]) or debug-breakpoint exceptions; no cache model
  (so the MMU's write-through/cache-inhibit attributes have no effect).
* `divw`/`divwu` undefined cases return 0 with OV set.
* `dcbz` always clears the line (no cache-inhibited alignment exception).
* Big-endian only (MSR[LE] is ignored).
