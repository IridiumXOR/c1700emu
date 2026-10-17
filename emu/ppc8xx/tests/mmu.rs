//! End-to-end MMU tests: real code takes TLB misses and the reload handlers
//! from MPC860UM Figures 8-23/8-24 refill the TLB through the table-walk
//! assist registers, exactly as an operating system does.

mod common;
use common::*;
use ppc8xx::mmu::spr;
use ppc8xx::{Cpu, msr};

/// Level-one table at 0x4000 (M_TWB), one level-two table at 0x5000:
/// EA 0x0000_0000..0x003F_FFFF uses L1 entry 0.
const L1: u32 = 0x4000;
const L2: u32 = 0x5000;
const L1_VALID: u32 = 1;
/// PTE bits: C (changed) | all four 1 KB sub-pages valid | V.
const PTE_C_SUB_V: u32 = 0x100 | 0xF0 | 1;
const PP_RW_ALL: u32 = 2 << 10; // data, basic encoding 10: RW for everyone
const PP_RO_ALL: u32 = 3 << 10; // data, basic encoding 11: RO for everyone
const PP_EXEC_ALL: u32 = 1 << 10; // instruction, basic encoding 01

/// Figure 8-23: DTLB reload.
const DTLB_RELOAD: &[&str] = &[
    "mtspr 799, r1", // M_TW = r1
    "mfspr r1, 796", // r1 = address of the level-one descriptor (M_TWB assist)
    "lwz r1, 0(r1)",
    "mtspr 797, r1", // MD_TWC = level-one descriptor
    "mfspr r1, 797", // r1 = address of the level-two descriptor (MD_TWC assist)
    "lwz r1, 0(r1)",
    "mtspr 798, r1", // MD_RPN = PTE: loads the DTLB entry
    "mfspr r1, 799",
    "rfi",
];

/// Figure 8-24: ITLB reload.
const ITLB_RELOAD: &[&str] = &[
    "mtspr 799, r1",
    "mfspr r1, 26",  // SRR0 = the missed instruction address
    "mtspr 795, r1", // MD_EPN, so the assists work for the instruction side
    "mfspr r1, 796",
    "lwz r1, 0(r1)",
    "mtspr 789, r1", // MI_TWC
    "mtspr 797, r1", // MD_TWC
    "mfspr r1, 797",
    "lwz r1, 0(r1)",
    "mtspr 790, r1", // MI_RPN: loads the ITLB entry
    "mfspr r1, 799",
    "rfi",
];

fn install(board: &mut TestBoard, vector: u32, code: &[&str]) {
    for (i, line) in code.iter().enumerate() {
        board.poke32(vector + 4 * i as u32, asm(line));
    }
}

/// Map the 4 KB page at `ea` to `pa` in the page table.
fn map(board: &mut TestBoard, ea: u32, pa: u32, pp: u32) {
    board.poke32(L1, L2 | L1_VALID);
    board.poke32(L2 + ((ea >> 12) & 0x3FF) * 4, pa | pp | PTE_C_SUB_V);
}

fn mmu_on(cpu: &mut Cpu, board: &mut TestBoard) {
    install(board, 0x1100, ITLB_RELOAD);
    install(board, 0x1200, DTLB_RELOAD);
    cpu.mmu.write_spr(spr::M_TWB, L1);
    cpu.mmu.write_spr(spr::MD_AP, 0x5555_5555); // all groups "client": page bits decide
    cpu.mmu.write_spr(spr::MI_AP, 0x5555_5555);
}

#[test]
fn data_tlb_miss_is_reloaded_by_the_handler_and_the_load_retried() {
    let (mut cpu, mut board) = setup(&["lis r4, 0", "ori r4, r4, 0x9008", "lwz r3, 0(r4)", "li r5, 1"]);
    mmu_on(&mut cpu, &mut board);
    map(&mut board, 0x9000, 0xA000, PP_RW_ALL);
    board.poke32(0xA008, 0xCAFE_BABE);
    cpu.msr |= msr::DR;

    run(&mut cpu, &mut board, 2);
    run(&mut cpu, &mut board, 1); // lwz misses
    assert_eq!(cpu.pc, 0x1200, "DTLB miss vector");
    assert_eq!(cpu.srr0, CODE + 8, "SRR0 = the faulting load");
    assert_eq!(cpu.msr & msr::DR, 0, "handlers run untranslated");
    assert_eq!(cpu.mmu.read_spr(spr::MD_EPN).unwrap() & 0xFFFF_F000, 0x9000);

    run(&mut cpu, &mut board, DTLB_RELOAD.len()); // handler, ending in rfi
    assert_eq!(cpu.pc, CODE + 8);
    run(&mut cpu, &mut board, 2); // the load again, then li
    assert_eq!(cpu.gpr[3], 0xCAFE_BABE, "virtual 0x9008 reads physical 0xA008");
    assert_eq!(cpu.gpr[5], 1);
    assert_eq!(cpu.exception_counts[0x12], 1, "exactly one DTLB miss");
}

#[test]
fn store_to_a_read_only_page_is_a_dtlb_error_with_dsisr_and_dar() {
    let (mut cpu, mut board) = setup(&["lis r4, 0", "ori r4, r4, 0x9010", "stw r3, 0(r4)"]);
    mmu_on(&mut cpu, &mut board);
    map(&mut board, 0x9000, 0xA000, PP_RO_ALL);
    cpu.msr |= msr::DR;

    run(&mut cpu, &mut board, 3); // stw misses
    run(&mut cpu, &mut board, DTLB_RELOAD.len());
    run(&mut cpu, &mut board, 1); // stw again: protection violation
    assert_eq!(cpu.pc, 0x1400, "DTLB error vector");
    assert_eq!(cpu.dar, 0x9010);
    assert_eq!(cpu.dsisr, 0x0800_0000 | 0x0200_0000, "protection + store");
}

#[test]
fn instruction_tlb_miss_maps_the_code_page() {
    let (mut cpu, mut board) = setup(&["li r5, 42"]);
    mmu_on(&mut cpu, &mut board);
    map(&mut board, CODE, CODE, PP_EXEC_ALL); // identity-map the code page
    cpu.msr |= msr::IR;

    run(&mut cpu, &mut board, 1); // fetch misses
    assert_eq!(cpu.pc, 0x1100, "ITLB miss vector");
    assert_eq!(cpu.srr0, CODE);
    assert_eq!(cpu.srr1 & 0xFFFF_0000, 0x0800_0000 | 0x0020_0000, "SRR1 bits 4 and 10");
    run(&mut cpu, &mut board, ITLB_RELOAD.len());
    assert_eq!(cpu.msr & msr::IR, msr::IR, "rfi turned translation back on");
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.gpr[5], 42);
    assert_eq!(cpu.exception_counts[0x11], 1);
}
