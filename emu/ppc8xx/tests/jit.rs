//! The JIT against the interpreter: the same program from the same initial
//! state must end in exactly the same state with both engines (registers,
//! clock, timebase, decrementer, memory), whatever happens on the way:
//! faults, interrupts, code that rewrites itself.

mod common;

use common::*;
use ppc8xx::jit::Jit;
use ppc8xx::{BatchEnd, Cpu, msr};

/// Steps per batch (the machine syncs its devices at least this often while
/// the FEC transmits; short batches exercise the batch-end paths).
const BATCH: u64 = 256;

/// Run `steps` steps, with the JIT if one is given.
fn run_engine(cpu: &mut Cpu, board: &mut TestBoard, mut jit: Option<&mut Jit>, steps: u64) {
    let mut done = 0;
    while done < steps {
        let max = BATCH.min(steps - done);
        let (n, end) = match jit.as_deref_mut() {
            Some(jit) => jit.run_batch(cpu, board, max, |_| false),
            None => cpu.run_batch(board, max, |_| false),
        };
        done += n;
        assert_ne!(end, BatchEnd::Checkstop, "checkstop at {:#x}", cpu.pc);
    }
}

fn assert_same(interp: &(Cpu, TestBoard), jit: &(Cpu, TestBoard), what: &str) {
    let ((a, ab), (b, bb)) = (interp, jit);
    for r in 0..32 {
        assert_eq!(a.gpr[r], b.gpr[r], "{what}: r{r} (interpreter vs JIT)");
    }
    let fields = [
        ("pc", a.pc, b.pc),
        ("cr", a.cr, b.cr),
        ("xer", a.xer, b.xer),
        ("lr", a.lr, b.lr),
        ("ctr", a.ctr, b.ctr),
        ("msr", a.msr, b.msr),
        ("srr0", a.srr0, b.srr0),
        ("srr1", a.srr1, b.srr1),
        ("dar", a.dar, b.dar),
        ("dsisr", a.dsisr, b.dsisr),
        ("dec", a.dec, b.dec),
    ];
    for (name, x, y) in fields {
        assert_eq!(x, y, "{what}: {name} (interpreter vs JIT)");
    }
    assert_eq!(a.cycles, b.cycles, "{what}: cycles");
    assert_eq!(a.tb, b.tb, "{what}: timebase");
    assert_eq!(a.dec_pending, b.dec_pending, "{what}: dec_pending");
    assert_eq!(a.sprg, b.sprg, "{what}: SPRG");
    assert_eq!(a.reservation, b.reservation, "{what}: reservation");
    assert_eq!(a.exception_counts, b.exception_counts, "{what}: exceptions taken");
    if let Some(i) = (0..ab.ram.len()).find(|&i| ab.ram[i] != bb.ram[i]) {
        panic!("{what}: RAM differs first at {i:#x}: {:#04x} vs {:#04x}", ab.ram[i], bb.ram[i]);
    }
    assert_eq!(ab.irq, bb.irq, "{what}: interrupt line");
}

/// Assemble `lines` into the board at `addr`.
fn place(board: &mut TestBoard, addr: u32, lines: &[&str]) {
    for (i, line) in lines.iter().enumerate() {
        board.poke32(addr + 4 * i as u32, asm(line));
    }
}

/// Run `program` (plus whatever `init` sets up) for `steps` steps with the
/// interpreter and with JITs of several thresholds; all must agree.
/// Returns the JIT run with threshold 1 (everything translated).
fn both(program: &[&str], init: impl Fn(&mut Cpu, &mut TestBoard), steps: u64) -> (Cpu, TestBoard, Jit) {
    let fresh = || {
        let (mut cpu, mut board) = setup(program);
        init(&mut cpu, &mut board);
        (cpu, board)
    };
    let mut reference = fresh();
    run_engine(&mut reference.0, &mut reference.1, None, steps);
    let mut last = None;
    // threshold 3, threshold 1, and threshold 1 with every region run
    // verified against the interpreter (lockstep check, which panics on
    // any difference)
    for (threshold, check) in [(3, false), (1, true), (1, false)] {
        let (mut cpu, mut board) = fresh();
        let mut jit = Jit::with_threshold(threshold);
        jit.set_check(check);
        run_engine(&mut cpu, &mut board, Some(&mut jit), steps);
        let run = (cpu, board);
        assert_same(&reference, &run, &format!("threshold {threshold}, check {check}"));
        if check {
            assert!(jit.stats.checked_runs > 0, "the checker ran");
        }
        last = Some((run.0, run.1, jit));
    }
    let last = last.unwrap();
    eprintln!("{}", last.2.report());
    last
}

#[test]
fn alu_loop() {
    let (_, _, jit) = both(
        &[
            "li r3, 0",
            "li r4, 0x1234",
            "addi r3, r3, 1",       // loop:
            "xor r4, r4, r3",
            "rlwinm r5, r4, 3, 0, 28",
            "add. r6, r5, r3",
            "subfc r7, r6, r4",
            "adde r8, r7, r3",
            "srawi r9, r8, 3",
            "addze r9, r9",
            "mullw r10, r9, r4",
            "mulhwu r11, r10, r3",
            "cntlzw r12, r11",
            "cmplw cr3, r12, r3",
            "b -48",
        ],
        |_, _| {},
        20_000,
    );
    assert!(jit.stats.jit_instructions > 19_000, "the loop should run translated: {}", jit.report());
}

#[test]
fn memory_loops() {
    both(
        &[
            "lis r3, 0",
            "ori r3, r3, 0x8000",   // source
            "addi r4, r3, 0x800",   // destination
            "li r5, 0x100",
            "mtctr r5",
            "lbzu r6, 1(r3)",       // loop: bytes, halves, words, reversed
            "stbu r6, 1(r4)",
            "lhz r7, 0(r3)",
            "lha r8, 2(r3)",
            "sth r8, 4(r4)",
            "lwz r9, 8(r3)",
            "lwbrx r10, r3, r5",
            "stwbrx r9, r4, r5",
            "add r11, r9, r10",
            "stw r11, 12(r4)",
            "lhbrx r12, r4, r5",
            "sthbrx r12, r3, r5",
            "lwzx r13, r3, r5",     // misaligned on odd r3: through the helper
            "bdnz -52",
            "b -72",
        ],
        |_, board| {
            for i in 0..0x400u32 {
                board.poke32(0x8000 + 4 * i, i.wrapping_mul(0x9E37_79B9));
            }
        },
        30_000,
    );
}

#[test]
fn branches_and_cr() {
    both(
        &[
            "li r3, 100",
            "mtctr r3",
            "li r4, 0",
            "addi r4, r4, 3",       // loop:
            "cmpwi cr1, r4, 50",
            "blt cr1, 8",
            "addi r4, r4, -45",
            "crxor 6, 5, 4",
            "mfcr r5",
            "mcrf cr2, cr1",
            "mtcrf 0x18, r4",
            "bl 8",
            "b 12",
            "mflr r6",              // subroutine: return via blr
            "blr",
            "bdnz -48",
            "b -60",
        ],
        |_, _| {},
        20_000,
    );
}

#[test]
fn machine_check_is_precise() {
    // The load in the middle of the block hits nothing on the bus.
    let (cpu, ..) = both(
        &["li r3, 1", "li r4, 2", "lis r5, 0x10", "lwz r6, 0(r5)", "li r7, 3", "b -20"],
        |cpu, board| {
            cpu.msr |= msr::ME;
            place(board, 0x200, &["addi r20, r20, 1", "mfsrr0 r21", "addi r21, r21, 4", "mtsrr0 r21", "rfi"]);
        },
        1_000,
    );
    assert!(cpu.gpr[20] > 10, "machine checks taken: {}", cpu.gpr[20]);
    assert_eq!(cpu.dar, 0x10_0000);
}

#[test]
fn helper_instructions_and_alignment() {
    both(
        &[
            "lis r3, 0",
            "ori r3, r3, 0x9000",
            "stmw r26, 0(r3)",      // helpers: string/multiple, reservations, dcbz
            "lmw r28, 4(r3)",
            "lwarx r5, 0, r3",
            "stwcx. r5, 0, r3",
            "dcbz 0, r3",
            "addo. r6, r5, r4",
            "mfsprg r7, 1",
            "mtsprg 2, r6",
            "addi r8, r3, 2",
            "lwarx r9, 0, r8",      // misaligned: alignment exception
            "addi r10, r10, 1",
            "b -52",
        ],
        |cpu, board| {
            cpu.msr |= msr::ME;
            cpu.gpr[26] = 0x1111;
            cpu.gpr[31] = 0x5555;
            place(board, 0x600, &["addi r20, r20, 1", "mfsrr0 r21", "addi r21, r21, 4", "mtsrr0 r21", "rfi"]);
        },
        5_000,
    );
}

#[test]
fn self_modifying_code() {
    // The loop rewrites its own first instruction, from the same block.
    let (cpu, ..) = both(
        &[
            "li r3, 0",
            "lis r4, 0",
            "ori r4, r4, 0x100c",   // address of the patched instruction
            "addi r3, r3, 1",       // patched to addi r3, r3, 2 / back
            "lwz r6, 0(r4)",
            "xor r6, r6, r7",       // toggle the immediate between 1 and 2
            "stw r6, 0(r4)",
            "addi r8, r8, 1",
            "b -20",
        ],
        |cpu, _| cpu.gpr[7] = 1 ^ 2,
        3_000,
    );
    assert!(cpu.gpr[3] > 400, "r3={}", cpu.gpr[3]);
}

#[test]
fn decrementer_interrupts_land_on_the_same_instruction() {
    let (cpu, ..) = both(
        &[
            "addi r3, r3, 1",       // loop: blocks the decrementer fires into
            "xor r4, r4, r3",
            "rlwinm r5, r4, 3, 0, 28",
            "add. r6, r5, r3",
            "stw r6, 0x7000(r0)",
            "lwz r7, 0x7000(r0)",
            "b -24",
        ],
        |cpu, board| {
            cpu.msr = msr::ME | msr::EE;
            cpu.tb_enabled = true;
            cpu.dec = 7;
            place(board, 0x900, &["addi r10, r10, 1", "li r11, 13", "mtspr 22, r11", "rfi"]);
        },
        50_000,
    );
    assert!(cpu.gpr[10] > 100, "decrementer interrupts: {}", cpu.gpr[10]);
}

#[test]
fn interrupt_raised_by_a_store_is_taken_after_it() {
    let (cpu, ..) = both(
        &[
            "lis r9, 2",            // IRQ_PORT
            "li r8, 1",
            "addi r3, r3, 1",       // loop:
            "andi. r4, r3, 7",
            "bne 8",
            "stw r8, 0(r9)",        // raise the line (every 8th pass)
            "addi r5, r5, 1",       // must not run before the interrupt
            "b -20",
        ],
        |cpu, board| {
            cpu.msr = msr::ME | msr::EE;
            place(board, 0x500, &["li r12, 0", "stw r12, 0(r9)", "addi r10, r10, 1", "mfsrr0 r13", "rfi"]);
        },
        20_000,
    );
    assert!(cpu.gpr[10] > 100, "external interrupts: {}", cpu.gpr[10]);
}

#[test]
fn indirect_branches_with_link() {
    // `blrl` must jump to the *old* LR; `bctrl` to CTR; both set LR.
    let (cpu, ..) = both(
        &[
            "lis r20, 0",
            "ori r20, r20, 0x1040", // the subroutine
            "mtlr r20",             // loop:
            "blrl",
            "addi r3, r3, 1",
            "mtctr r20",
            "bctrl",
            "addi r4, r4, 1",
            "b -24",
        ],
        |_, board| place(board, 0x1040, &["addi r5, r5, 1", "mflr r6", "blr"]),
        5_000,
    );
    // Every call (r5) is followed by its return counter (r3 or r4).
    let (calls, returns) = (cpu.gpr[5], cpu.gpr[3] + cpu.gpr[4]);
    assert!(calls > 100 && (returns..=returns + 1).contains(&calls), "calls {calls}, returns {returns}");
}

/// Translation on: TLB misses refilled by the reload handlers of MPC860UM
/// Figures 8-23/8-24 (as in tests/mmu.rs), a `tlbie` every pass so the
/// misses keep coming, and a store to a read-only page in the middle of a
/// block (DTLB error, skipped by its handler).
// Translation on: a two-level page table at L1/L2, refilled by the
// reload handlers of MPC860UM Figures 8-23/8-24 (as in tests/mmu.rs).
const L1: u32 = 0x4000;
const L2: u32 = 0x5000;
const DTLB_RELOAD: &[&str] =
    &["mtspr 799, r1", "mfspr r1, 796", "lwz r1, 0(r1)", "mtspr 797, r1", "mfspr r1, 797", "lwz r1, 0(r1)", "mtspr 798, r1", "mfspr r1, 799", "rfi"];
const ITLB_RELOAD: &[&str] = &[
    "mtspr 799, r1", "mfspr r1, 26", "mtspr 795, r1", "mfspr r1, 796", "lwz r1, 0(r1)", "mtspr 789, r1",
    "mtspr 797, r1", "mfspr r1, 797", "lwz r1, 0(r1)", "mtspr 790, r1", "mfspr r1, 799", "rfi",
];

/// PTE: page | PP | C | four valid sub-pages | V
fn pte(pa: u32, pp: u32) -> u32 {
    pa | pp << 10 | 0x100 | 0xF0 | 1
}

/// The reload handlers and an L1 entry for the first 4 MB; the L2 entries
/// are the test's.
fn page_table(cpu: &mut Cpu, board: &mut TestBoard) {
    use ppc8xx::mmu::spr as mmu_spr;
    place(board, 0x1100, ITLB_RELOAD);
    place(board, 0x1200, DTLB_RELOAD);
    board.poke32(L1, L2 | 1);
    cpu.mmu.write_spr(mmu_spr::M_TWB, L1);
    cpu.mmu.write_spr(mmu_spr::MD_AP, 0x5555_5555);
    cpu.mmu.write_spr(mmu_spr::MI_AP, 0x5555_5555);
}

#[test]
fn translated_mode_with_tlb_misses_and_faults() {
    let (cpu, ..) = both(
        &[
            "lis r3, 2",            // loop: EA 0x20000.., 8 pages
            "li r4, 8",
            "mtctr r4",
            "lwz r5, 0(r3)",        // inner:
            "add r6, r6, r5",
            "stw r6, 4(r3)",        // page 5 is read-only
            "addi r3, r3, 0x1000",
            "bdnz -16",
            "lis r3, 2",
            "tlbie r3",
            "addi r7, r7, 1",
            "b -44",
        ],
        |cpu, board| {
            page_table(cpu, board);
            place(board, 0x1400, &["mfsrr0 r21", "addi r21, r21, 4", "mtsrr0 r21", "rfi"]);
            board.poke32(L2 + 4, pte(0x1000, 1)); // the code page, executable
            for i in 0..8 {
                board.poke32(L2 + (0x20 + i) * 4, pte(0x8000 + i * 0x1000, if i == 5 { 3 } else { 2 }));
                board.poke32(0x8000 + i * 0x1000, 0x100 + i);
            }
            cpu.msr = msr::ME | msr::IR | msr::DR;
        },
        30_000,
    );
    assert!(cpu.exception_counts[0x12] > 100, "DTLB misses: {}", cpu.exception_counts[0x12]);
    assert!(cpu.exception_counts[0x14] > 100, "DTLB errors: {}", cpu.exception_counts[0x14]);
}

/// `mftb`/`mftbu`/`mfdec` inside translated blocks must read the clock the
/// interpreter would see at that instruction (block start + instructions
/// before it). In user mode `mfdec` raises a Program exception.
#[test]
fn timebase_and_decrementer_reads() {
    for user in [false, true] {
        let (cpu, ..) = both(
            &[
                "addi r4, r4, 1",       // loop:
                "mftb r3, 268",
                "xor r9, r9, r3",
                "mftb r5, 269",       // mftbu
                "add r9, r9, r5",
                "mfspr r6, 22",         // mfdec
                "add r7, r3, r6",
                "subf r8, r7, r9",
                "b -32",
            ],
            |cpu, board| {
                cpu.msr = msr::ME | if user { msr::PR } else { 0 };
                cpu.tb_enabled = true;
                cpu.tb = 0xFFFF_FF00; // TBU changes during the run
                cpu.dec = 5_000;
                place(board, 0x700, &["addi r20, r20, 1", "mfsrr0 r21", "addi r21, r21, 4", "mtsrr0 r21", "rfi"]);
            },
            40_000,
        );
        assert!(cpu.tb > 0x1_0000_0000, "the timebase wrapped into TBU");
        assert_eq!(cpu.gpr[20] > 0, user, "program exceptions in user mode only");
    }
}

/// Chaining (docs ch. 17.14): a region jumps straight into the region it
/// calls, and the callee's `blr` straight back. Every 64 calls the caller
/// rewrites the callee's first instruction: the link into the dropped
/// region must not be used any more.
#[test]
fn chained_jump_into_rewritten_code() {
    let (cpu, _, jit) = both(
        &[
            "addi r8, r8, 1",       // loop:
            "bl 0xffc",             // the callee at 0x2000, in another region
            "andi. r9, r8, 63",
            "bne -12",
            "lwz r6, 0(r4)",        // toggle its immediate between 1 and 2
            "xor r6, r6, r7",
            "stw r6, 0(r4)",
            "b -28",
        ],
        |cpu, board| {
            place(board, 0x2000, &["addi r3, r3, 1", "blr"]);
            cpu.gpr[4] = 0x2000;
            cpu.gpr[7] = 1 ^ 2;
        },
        20_000,
    );
    assert!(cpu.gpr[8] > 2000, "calls: {}", cpu.gpr[8]);
    assert!(jit.stats.chained_runs > 1000, "chained runs: {}", jit.stats.chained_runs);
}

/// Chaining with translation on: the caller calls virtual 0x20000, which
/// maps alternately to two physical pages holding different code (the PTE
/// is switched and the old translation invalidated with `tlbie` every 64
/// calls). A link made under the old mapping must not be used.
#[test]
fn chained_jump_follows_an_instruction_remap() {
    let (cpu, _, jit) = both(
        &[
            "addi r8, r8, 1",       // loop:
            "bl 0x1effc",           // virtual 0x20000
            "andi. r9, r8, 63",
            "bne -12",
            "lwz r6, 0(r4)",        // switch the PTE between the two pages
            "xor r6, r6, r7",
            "stw r6, 0(r4)",
            "tlbie r5",
            "b -32",
        ],
        |cpu, board| {
            page_table(cpu, board);
            board.poke32(L2 + 4, pte(0x1000, 1)); // the caller's page
            board.poke32(L2 + 0x20 * 4, pte(0x8000, 1)); // virtual 0x20000
            place(board, 0x8000, &["addi r3, r3, 1", "blr"]);
            place(board, 0x9000, &["addi r3, r3, 16", "blr"]);
            cpu.gpr[4] = L2 + 0x20 * 4;
            cpu.gpr[5] = 0x20000;
            cpu.gpr[7] = 0x8000 ^ 0x9000;
            cpu.msr = msr::ME | msr::IR; // data accesses untranslated
        },
        20_000,
    );
    assert!(cpu.gpr[8] > 1000, "calls: {}", cpu.gpr[8]);
    assert!(cpu.gpr[3] > 16 * 500, "the second page's code ran too: r3={}", cpu.gpr[3]);
    assert!(jit.stats.chained_runs > 1000, "chained runs: {}", jit.stats.chained_runs);
}
