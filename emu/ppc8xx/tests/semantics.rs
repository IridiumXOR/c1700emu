//! Focused tests for the tricky corners of PowerPC/MPC8xx semantics.
//! Each test assembles a few instructions, runs them, and checks the state.

mod common;
use common::*;
use ppc8xx::{Exception, StepOutcome, msr};

// ---------------------------------------------------------------------------
// Arithmetic and XER
// ---------------------------------------------------------------------------

#[test]
fn addo_sets_ov_and_sticky_so_and_cr0_copies_so() {
    let (cpu, _) = exec(&["addo. r3, r4, r5"], |c, _| {
        c.gpr[4] = 0x7FFF_FFFF;
        c.gpr[5] = 1;
    });
    assert_eq!(cpu.gpr[3], 0x8000_0000);
    assert_eq!(cpu.xer & (XER_SO | XER_OV), XER_SO | XER_OV);
    assert_eq!(crf(&cpu, 0), LT | SO);
}

#[test]
fn so_is_sticky_but_ov_is_not() {
    let (cpu, _) = exec(&["addo r3, r4, r5", "addo r6, r5, r5"], |c, _| {
        c.gpr[4] = 0x7FFF_FFFF;
        c.gpr[5] = 1;
    });
    assert_eq!(cpu.gpr[6], 2);
    assert_eq!(cpu.xer & XER_OV, 0, "second addo did not overflow");
    assert_ne!(cpu.xer & XER_SO, 0, "SO stays set");
}

#[test]
fn add_without_oe_leaves_xer_alone() {
    let (cpu, _) = exec(&["add. r3, r4, r5"], |c, _| {
        c.gpr[4] = 0x7FFF_FFFF;
        c.gpr[5] = 1;
    });
    assert_eq!(cpu.xer, 0);
    assert_eq!(crf(&cpu, 0), LT);
}

#[test]
fn addc_adde_chain_makes_64_bit_addition() {
    // (r4:r5) + (r6:r7) -> (r8:r9), r5/r7/r9 are the low words
    let (cpu, _) = exec(&["addc r9, r5, r7", "adde r8, r4, r6"], |c, _| {
        c.gpr[4] = 0x0000_0001;
        c.gpr[5] = 0xFFFF_FFFF;
        c.gpr[6] = 0x0000_0002;
        c.gpr[7] = 0x0000_0001;
    });
    assert_eq!((cpu.gpr[8], cpu.gpr[9]), (4, 0));
    assert_eq!(cpu.xer & XER_CA, 0, "high word did not carry out");
}

#[test]
fn subfc_carry_means_no_borrow() {
    // subfc rD,rA,rB = rB - rA
    let (cpu, _) = exec(&["subfc r3, r4, r5"], |c, _| {
        c.gpr[4] = 3;
        c.gpr[5] = 5;
    });
    assert_eq!(cpu.gpr[3], 2);
    assert_ne!(cpu.xer & XER_CA, 0);
    let (cpu, _) = exec(&["subfc r3, r4, r5"], |c, _| {
        c.gpr[4] = 5;
        c.gpr[5] = 3;
    });
    assert_eq!(cpu.gpr[3], (-2i32) as u32);
    assert_eq!(cpu.xer & XER_CA, 0, "borrow -> CA=0");
}

#[test]
fn subfe_and_extended_forms_use_carry_in() {
    let (cpu, _) = exec(&["subfe r3, r4, r5", "addze r6, r7", "addme r8, r9", "subfze r10, r11", "subfme r12, r13"], |c, _| {
        c.xer = XER_CA;
        c.gpr[4] = 1;
        c.gpr[5] = 10; // 10 - 1 - (1-CA) = 9, CA stays 1
        c.gpr[7] = 41; // 41 + CA(1) = 42, CA=0 afterwards
        c.gpr[9] = 5; // 5 + CA(0) - 1 = 4, CA=1
        c.gpr[11] = 0; // ~0 + CA(1) = 0, CA=1
        c.gpr[13] = 0; // ~0 + CA(1) - 1 = -1, CA=1
    });
    assert_eq!(cpu.gpr[3], 9);
    assert_eq!(cpu.gpr[6], 42);
    assert_eq!(cpu.gpr[8], 4);
    assert_eq!(cpu.gpr[10], 0);
    assert_eq!(cpu.gpr[12], u32::MAX);
    assert_ne!(cpu.xer & XER_CA, 0);
}

#[test]
fn nego_overflows_only_for_most_negative() {
    let (cpu, _) = exec(&["nego r3, r4"], |c, _| c.gpr[4] = 0x8000_0000);
    assert_eq!(cpu.gpr[3], 0x8000_0000);
    assert_ne!(cpu.xer & XER_OV, 0);
    let (cpu, _) = exec(&["nego r3, r4"], |c, _| c.gpr[4] = 5);
    assert_eq!(cpu.gpr[3], (-5i32) as u32);
    assert_eq!(cpu.xer & XER_OV, 0);
}

#[test]
fn addic_dot_records_and_subfic_sets_carry() {
    let (cpu, _) = exec(&["addic. r3, r4, -1", "subfic r5, r6, 10"], |c, _| {
        c.gpr[4] = 0; // 0 + -1 = -1, no carry
        c.gpr[6] = 3; // 10 - 3 = 7, no borrow -> CA=1
    });
    assert_eq!(cpu.gpr[3], u32::MAX);
    assert_eq!(crf(&cpu, 0), LT);
    assert_eq!(cpu.gpr[5], 7);
    assert_ne!(cpu.xer & XER_CA, 0);
}

#[test]
fn multiply_variants() {
    let (cpu, _) = exec(&["mullwo r3, r4, r5", "mulhw r6, r4, r5", "mulhwu r7, r4, r5", "mulli r8, r4, -3"], |c, _| {
        c.gpr[4] = 0xFFFF_FFFE; // -2
        c.gpr[5] = 0x8000_0000; // -2^31 (signed) / 2^31 (unsigned)
    });
    // -2 * -2^31 = 2^32: low word 0, overflow
    assert_eq!(cpu.gpr[3], 0);
    assert_ne!(cpu.xer & XER_OV, 0);
    assert_eq!(cpu.gpr[6], 1); // high word of +2^32
    assert_eq!(cpu.gpr[7], 0x7FFF_FFFF); // (2^32-2)*2^31 >> 32
    assert_eq!(cpu.gpr[8], 6);
}

#[test]
fn divide_variants_and_divide_by_zero() {
    let (cpu, _) = exec(&["divw r3, r4, r5", "divwu r6, r4, r5", "divwo. r7, r4, r8"], |c, _| {
        c.gpr[4] = (-7i32) as u32;
        c.gpr[5] = 2;
        c.gpr[8] = 0;
    });
    assert_eq!(cpu.gpr[3], (-3i32) as u32, "signed division truncates toward zero");
    assert_eq!(cpu.gpr[6], ((-7i32) as u32) / 2);
    assert_ne!(cpu.xer & XER_OV, 0);
    assert_ne!(crf(&cpu, 0) & SO, 0);
}

// ---------------------------------------------------------------------------
// Logical, rotate and shift
// ---------------------------------------------------------------------------

#[test]
fn srawi_carry_rules() {
    // negative, ones shifted out -> CA=1
    let (cpu, _) = exec(&["srawi r3, r4, 2"], |c, _| c.gpr[4] = (-5i32) as u32);
    assert_eq!(cpu.gpr[3], (-2i32) as u32);
    assert_ne!(cpu.xer & XER_CA, 0);
    // negative, only zeros shifted out -> CA=0
    let (cpu, _) = exec(&["srawi r3, r4, 2"], |c, _| c.gpr[4] = (-8i32) as u32);
    assert_eq!(cpu.gpr[3], (-2i32) as u32);
    assert_eq!(cpu.xer & XER_CA, 0);
    // positive -> CA=0 even with ones shifted out
    let (cpu, _) = exec(&["srawi r3, r4, 2"], |c, _| c.gpr[4] = 7);
    assert_eq!(cpu.gpr[3], 1);
    assert_eq!(cpu.xer & XER_CA, 0);
    // the classic idiom: srawi + addze = division rounding toward zero
    let (cpu, _) = exec(&["srawi r3, r4, 2", "addze r3, r3"], |c, _| c.gpr[4] = (-5i32) as u32);
    assert_eq!(cpu.gpr[3], (-1i32) as u32);
}

#[test]
fn shifts_by_32_or_more() {
    let (cpu, _) = exec(&["slw r3, r4, r5", "srw r6, r4, r5", "sraw r7, r8, r5", "sraw r9, r8, r10"], |c, _| {
        c.gpr[4] = 0xFFFF_FFFF;
        c.gpr[5] = 32;
        c.gpr[8] = 0x8000_0001;
        c.gpr[10] = 0x3F + 0x40; // only the low 6 bits count -> 63
    });
    assert_eq!(cpu.gpr[3], 0);
    assert_eq!(cpu.gpr[6], 0);
    assert_eq!(cpu.gpr[7], u32::MAX);
    assert_eq!(cpu.gpr[9], u32::MAX);
    assert_ne!(cpu.xer & XER_CA, 0);
}

#[test]
fn rotate_and_mask_forms() {
    let (cpu, _) = exec(
        &[
            "rlwinm r3, r4, 0, 30, 1",   // wrap-around mask 0xC0000003
            "rlwinm r5, r4, 8, 0, 31",   // rotlwi 8
            "rlwimi r6, r4, 16, 8, 15",  // insert bits 8-15 of rotl(r4,16)
            "rlwnm r7, r4, r8, 24, 31",  // rotl by r8 then keep low byte
            "rlwinm. r9, r4, 0, 31, 31", // test low bit, record
        ],
        |c, _| {
            c.gpr[4] = 0x1234_5678;
            c.gpr[6] = 0xAAAA_AAAA;
            c.gpr[8] = 4 + 32; // only low 5 bits used
        },
    );
    assert_eq!(cpu.gpr[3], 0x1234_5678 & 0xC000_0003);
    assert_eq!(cpu.gpr[5], 0x3456_7812);
    assert_eq!(cpu.gpr[6], 0xAA78_AAAA); // rotl(r4,16)=0x56781234, bits 8-15 = 0x78
    assert_eq!(cpu.gpr[7], 0x0000_0081);
    assert_eq!(cpu.gpr[9], 0);
    assert_eq!(crf(&cpu, 0), EQ);
}

#[test]
fn logical_and_count_leading_zeros() {
    let (cpu, _) = exec(
        &["cntlzw r3, r4", "cntlzw r5, r6", "extsb r7, r8", "extsh r9, r8", "nand r10, r8, r8", "andi. r11, r8, 0x80", "oris r12, r0, 0xdead"],
        |c, _| {
            c.gpr[4] = 0;
            c.gpr[6] = 0x0001_0000;
            c.gpr[8] = 0x1234_8080;
        },
    );
    assert_eq!(cpu.gpr[3], 32);
    assert_eq!(cpu.gpr[5], 15);
    assert_eq!(cpu.gpr[7], 0xFFFF_FF80);
    assert_eq!(cpu.gpr[9], 0xFFFF_8080);
    assert_eq!(cpu.gpr[10], !0x1234_8080);
    assert_eq!(cpu.gpr[11], 0x80);
    assert_eq!(crf(&cpu, 0), GT);
    assert_eq!(cpu.gpr[12], 0xDEAD_0000, "oris uses r0's value (not 0) as source");
}

// ---------------------------------------------------------------------------
// Compare and CR operations
// ---------------------------------------------------------------------------

#[test]
fn signed_vs_unsigned_compare() {
    let (cpu, _) = exec(&["cmpw cr1, r3, r4", "cmplw cr2, r3, r4", "cmpwi cr3, r3, -1", "cmplwi cr4, r4, 1"], |c, _| {
        c.gpr[3] = 0xFFFF_FFFF; // -1 signed, huge unsigned
        c.gpr[4] = 1;
        c.xer = XER_SO;
    });
    assert_eq!(crf(&cpu, 1), LT | SO);
    assert_eq!(crf(&cpu, 2), GT | SO);
    assert_eq!(crf(&cpu, 3), EQ | SO);
    assert_eq!(crf(&cpu, 4), EQ | SO);
}

#[test]
fn cr_logical_mcrf_mcrxr_mtcrf() {
    let (cpu, _) = exec(
        &[
            "creqv 31, 31, 31", // crset 31
            "crxor 30, 31, 31", // crclr 30
            "mcrf cr1, cr7",
            "mcrxr cr2",
            "mtcrf 0x01, r5", // only CR7
            "mfcr r6",
        ],
        |c, _| {
            c.xer = XER_SO | XER_CA | 0x12;
            c.gpr[5] = 0xFFFF_FFF5;
        },
    );
    assert_eq!(crf(&cpu, 1), 0b0001);
    assert_eq!(crf(&cpu, 2), 0b1010, "SO and CA copied, OV clear");
    assert_eq!(cpu.xer, 0x12, "mcrxr clears SO/OV/CA but not the byte count");
    assert_eq!(crf(&cpu, 7), 0x5);
    assert_eq!(cpu.gpr[6], cpu.cr);
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

#[test]
fn bdnz_loop_runs_ctr_times() {
    // r3 += 1, loop while --CTR != 0
    let (mut cpu, mut board) = setup(&["addi r3, r3, 1", "bdnz -4", "nop"]);
    cpu.ctr = 5;
    run(&mut cpu, &mut board, 10);
    assert_eq!(cpu.gpr[3], 5);
    assert_eq!(cpu.ctr, 0);
    assert_eq!(cpu.pc, CODE + 8);
}

#[test]
fn bdz_branches_when_ctr_reaches_zero() {
    let (mut cpu, mut board) = setup(&["bdz 8", "nop", "nop"]);
    cpu.ctr = 1;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, CODE + 8);
}

#[test]
fn conditional_branch_and_link_always_sets_lr() {
    // beql cr0 not taken still writes LR
    let (mut cpu, mut board) = setup(&["bcl 12, 2, 16"]);
    cpu.cr = 0; // EQ clear
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, CODE + 4);
    assert_eq!(cpu.lr, CODE + 4);
    // taken
    let (mut cpu, mut board) = setup(&["bcl 12, 2, 16"]);
    cpu.cr = 0x2000_0000; // CR0[EQ]
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, CODE + 16);
}

#[test]
fn blrl_jumps_to_old_lr_and_bctr() {
    let (mut cpu, mut board) = setup(&["blrl"]);
    cpu.lr = 0x2003; // low bits ignored
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x2000);
    assert_eq!(cpu.lr, CODE + 4);

    let (mut cpu, mut board) = setup(&["bctr"]);
    cpu.ctr = 0x3000;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x3000);
    assert_eq!(cpu.ctr, 0x3000, "bcctr never decrements CTR");
}

#[test]
fn absolute_branch() {
    let (mut cpu, mut board) = setup(&["bla 0x2000"]);
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x2000);
    assert_eq!(cpu.lr, CODE + 4);
}

// ---------------------------------------------------------------------------
// Loads and stores
// ---------------------------------------------------------------------------

#[test]
fn loads_with_update_and_sign_extension() {
    let (cpu, board) = exec(&["lwzu r3, 4(r4)", "lha r5, 0(r6)", "lhz r7, 0(r6)", "lbzx r8, r0, r6", "lwbrx r9, 0, r4", "lhbrx r10, 0, r6"], |c, b| {
        c.gpr[4] = DATA;
        c.gpr[6] = DATA + 0x10;
        b.poke32(DATA + 4, 0xCAFE_BABE);
        b.poke32(DATA + 0x10, 0x8001_0000);
    });
    assert_eq!(cpu.gpr[3], 0xCAFE_BABE);
    assert_eq!(cpu.gpr[4], DATA + 4, "update form writes EA back to rA");
    assert_eq!(cpu.gpr[5], 0xFFFF_8001);
    assert_eq!(cpu.gpr[7], 0x8001);
    assert_eq!(cpu.gpr[8], 0x80, "rA=r0 in indexed form means 0");
    assert_eq!(cpu.gpr[9], 0xBEBA_FECA);
    assert_eq!(cpu.gpr[10], 0x0180);
    let _ = board;
}

#[test]
fn stores_including_byte_reversed() {
    let (_, board) = exec(&["stw r3, 0(r4)", "sth r3, 4(r4)", "stb r3, 6(r4)", "stwbrx r3, r4, r5", "stwu r3, 16(r4)"], |c, _| {
        c.gpr[3] = 0x1122_3344;
        c.gpr[4] = DATA;
        c.gpr[5] = 8;
    });
    assert_eq!(board.peek32(DATA), 0x1122_3344);
    assert_eq!(board.peek32(DATA + 4), 0x3344_4400);
    assert_eq!(board.peek32(DATA + 8), 0x4433_2211);
    assert_eq!(board.peek32(DATA + 16), 0x1122_3344);
}

#[test]
fn misaligned_word_access_is_split_into_bytes() {
    let (cpu, board) = exec(&["lwz r3, 1(r4)", "lwz r5, 4(r4)"], |c, b| {
        c.gpr[4] = DATA;
        b.poke32(DATA, 0x0011_2233);
        b.poke32(DATA + 4, 0x4455_6677);
    });
    assert_eq!(cpu.gpr[3], 0x1122_3344);
    assert_eq!(cpu.gpr[5], 0x4455_6677);
    assert_eq!(board.sized_accesses, [4, 0, 1], "4 byte reads + 1 aligned word read");
}

#[test]
fn load_store_multiple() {
    let (cpu, board) = exec(&["stmw r29, 0(r4)", "lmw r26, 0(r4)"], |c, _| {
        c.gpr[4] = DATA;
        c.gpr[29] = 29;
        c.gpr[30] = 30;
        c.gpr[31] = 31;
    });
    assert_eq!(board.peek32(DATA + 8), 31);
    assert_eq!(&cpu.gpr[26..29], &[29, 30, 31]);
}

#[test]
fn lmw_misaligned_takes_alignment_exception() {
    let (mut cpu, mut board) = setup(&["lmw r30, 2(r4)"]);
    cpu.gpr[4] = DATA;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x600);
    assert_eq!(cpu.srr0, CODE);
    assert_eq!(cpu.dar, DATA + 2);
    assert_eq!(cpu.dsisr & 0x3FF, (30 << 5) | 4, "rD and rA reported in DSISR");
}

#[test]
fn load_string_immediate_pads_and_wraps() {
    let (cpu, _) = exec(&["lswi r30, r4, 7", "lswi r5, r4, 0"], |c, b| {
        c.gpr[4] = DATA;
        for i in 0..32u32 {
            b.ram[(DATA + i) as usize] = i as u8 + 1;
        }
    });
    assert_eq!(cpu.gpr[30], 0x0102_0304);
    assert_eq!(cpu.gpr[31], 0x0506_0700, "partial last register is zero padded");
    assert_eq!(cpu.gpr[5], 0x0102_0304);
    assert_eq!(cpu.gpr[12], 0x1D1E_1F20, "NB=0 means 32 bytes -> 8 registers");
}

#[test]
fn string_wraps_from_r31_to_r0() {
    let (cpu, _) = exec(&["lswi r31, r4, 8"], |c, b| {
        c.gpr[4] = DATA;
        b.poke32(DATA, 0xAABB_CCDD);
        b.poke32(DATA + 4, 0x1122_3344);
    });
    assert_eq!(cpu.gpr[31], 0xAABB_CCDD);
    assert_eq!(cpu.gpr[0], 0x1122_3344);
}

#[test]
fn store_string_and_indexed_forms_use_xer_count() {
    let (_, board) = exec(&["stswi r5, r4, 6", "stswx r5, r4, r8", "lswx r9, r4, r8"], |c, _| {
        c.gpr[4] = DATA;
        c.gpr[5] = 0x4142_4344;
        c.gpr[6] = 0x4546_4748;
        c.gpr[8] = 0x20;
        c.xer = 3;
    });
    assert_eq!(board.peek32(DATA), 0x4142_4344);
    assert_eq!(board.peek32(DATA + 4) >> 16, 0x4546);
    assert_eq!(board.peek32(DATA + 0x20) >> 8, 0x41_4243);
}

#[test]
fn lswx_with_zero_count_is_a_noop() {
    let (cpu, _) = exec(&["lswx r9, r4, r8"], |c, _| {
        c.gpr[4] = DATA;
        c.gpr[9] = 0x1234;
        c.xer = 0;
    });
    assert_eq!(cpu.gpr[9], 0x1234);
}

#[test]
fn lwarx_stwcx_reservation() {
    let (cpu, board) = exec(&["lwarx r3, 0, r4", "addi r3, r3, 1", "stwcx. r3, 0, r4", "stwcx. r3, 0, r4"], |c, b| {
        c.gpr[4] = DATA;
        b.poke32(DATA, 41);
    });
    assert_eq!(board.peek32(DATA), 42);
    assert_eq!(crf(&cpu, 0), 0, "second stwcx. fails: reservation consumed");

    let (cpu, _) = exec(&["lwarx r3, 0, r4", "stwcx. r3, 0, r4"], |c, _| c.gpr[4] = DATA);
    assert_eq!(crf(&cpu, 0), EQ);
}

#[test]
fn dcbz_clears_a_16_byte_line() {
    let (_, board) = exec(&["dcbz r4, r5"], |c, b| {
        c.gpr[4] = DATA;
        c.gpr[5] = 0x1B; // inside the line DATA+0x10..0x20
        for i in 0..0x30 {
            b.ram[(DATA + i) as usize] = 0xFF;
        }
    });
    assert_eq!(board.peek32(DATA + 0x0C), 0xFFFF_FFFF);
    for off in (0x10..0x20).step_by(4) {
        assert_eq!(board.peek32(DATA + off), 0);
    }
    assert_eq!(board.peek32(DATA + 0x20), 0xFFFF_FFFF);
}

// ---------------------------------------------------------------------------
// Exceptions and the 8xx system model
// ---------------------------------------------------------------------------

#[test]
fn bus_error_is_a_machine_check() {
    let (mut cpu, mut board) = setup(&["lwz r3, 0(r4)"]);
    cpu.gpr[4] = 0x8000_0000; // outside test RAM
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x200);
    assert_eq!(cpu.srr0, CODE);
    assert_eq!(cpu.dar, 0x8000_0000);
    assert_eq!(cpu.gpr[3], 0, "destination untouched");
}

#[test]
fn machine_check_with_me_clear_is_a_checkstop() {
    let (mut cpu, mut board) = setup(&["lwz r3, 0(r4)"]);
    cpu.gpr[4] = 0x8000_0000;
    cpu.msr = 0;
    assert_eq!(cpu.step(&mut board), StepOutcome::Checkstop);
}

#[test]
fn system_call_saves_next_pc_and_old_msr() {
    let (mut cpu, mut board) = setup(&["sc"]);
    cpu.msr = msr::ME | msr::EE | msr::PR | msr::RI;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0xC00);
    assert_eq!(cpu.srr0, CODE + 4);
    assert_eq!(cpu.srr1, msr::ME | msr::EE | msr::PR | msr::RI);
    assert_eq!(cpu.msr, msr::ME, "only ME (and IP) survive exception entry");
}

#[test]
fn exception_prefix_selects_high_vectors() {
    let (mut cpu, mut board) = setup(&["sc"]);
    cpu.msr = msr::ME | msr::IP;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0xFFF0_0C00);
    assert_eq!(cpu.msr, msr::ME | msr::IP);
}

#[test]
fn rfi_restores_msr_and_pc() {
    let (cpu, _) = exec(&["rfi"], |c, _| {
        c.srr0 = 0x2002;
        c.srr1 = msr::EE | msr::PR | msr::ME;
    });
    assert_eq!(cpu.pc, 0x2000);
    assert_eq!(cpu.msr, msr::EE | msr::PR | msr::ME);
}

#[test]
fn privileged_instructions_trap_in_user_mode() {
    for program in [["mfmsr r3"], ["mtspr 26, r3"], ["rfi"], ["mfspr r3, 638"], ["tlbie r4"]] {
        let (mut cpu, mut board) = setup(&program);
        cpu.msr = msr::ME | msr::PR;
        run(&mut cpu, &mut board, 1);
        assert_eq!(cpu.pc, 0x700, "{program:?}");
        assert_eq!(cpu.srr0, CODE);
        assert_eq!(cpu.srr1 & 0x0004_0000, 0x0004_0000, "SRR1[13] = privileged");
    }
    // ...but LR/CTR/XER and the timebase are fine from user mode
    let (cpu, _) = exec(&["mflr r3", "mftb r4, 268"], |c, _| {
        c.msr = msr::PR;
        c.lr = 0x1234;
    });
    assert_eq!(cpu.gpr[3], 0x1234);
    assert_eq!(cpu.msr & msr::PR, msr::PR);
}

#[test]
fn trap_word() {
    let (mut cpu, mut board) = setup(&["tweq r3, r4"]);
    cpu.gpr[3] = 7;
    cpu.gpr[4] = 7;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x700);
    assert_eq!(cpu.srr1 & 0x0002_0000, 0x0002_0000, "SRR1[14] = trap");

    let (cpu, _) = exec(&["twi 0x10, r3, 0"], |c, _| c.gpr[3] = 5); // trap if r3 < 0
    assert_eq!(cpu.pc, CODE + 4);
}

#[test]
fn eie_eid_nri_toggle_msr() {
    let (cpu, _) = exec(&["mtspr 80, r0"], |_, _| {});
    assert_eq!(cpu.msr & (msr::EE | msr::RI), msr::EE | msr::RI);
    let (cpu, _) = exec(&["mtspr 80, r0", "mtspr 81, r0"], |_, _| {});
    assert_eq!(cpu.msr & (msr::EE | msr::RI), msr::RI);
    let (cpu, _) = exec(&["mtspr 80, r0", "mtspr 82, r0"], |_, _| {});
    assert_eq!(cpu.msr & (msr::EE | msr::RI), 0);
}

#[test]
fn decrementer_interrupt_waits_for_ee() {
    let (mut cpu, mut board) = setup(&["nop", "nop", "nop", "nop", "nop", "nop"]);
    cpu.tb_enabled = true;
    cpu.tb_divider = 1;
    cpu.dec = 1;
    run(&mut cpu, &mut board, 2); // 1 -> 0 -> 0xFFFF_FFFF
    assert!(cpu.dec_pending);
    assert_eq!(cpu.tb, 2);
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, CODE + 12, "EE=0: interrupt stays pending");
    cpu.msr |= msr::EE;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x900);
    assert_eq!(cpu.srr0, CODE + 12, "resume at the instruction not yet executed");
    assert!(!cpu.dec_pending);
}

#[test]
fn timebase_stops_when_disabled() {
    let (mut cpu, mut board) = setup(&["nop", "nop"]);
    cpu.tb_enabled = false;
    cpu.tb_divider = 1;
    run(&mut cpu, &mut board, 2);
    assert_eq!(cpu.tb, 0);
    assert_eq!(cpu.cycles, 2);
}

#[test]
fn external_interrupt_enters_0x500() {
    let (mut cpu, mut board) = setup(&["nop", "nop"]);
    cpu.msr |= msr::EE;
    run(&mut cpu, &mut board, 1);
    board.irq = true;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x500);
    assert_eq!(cpu.srr0, CODE + 4);
    assert_eq!(cpu.msr & msr::EE, 0, "EE cleared so the handler isn't re-entered");
}

#[test]
fn floating_point_and_illegal_words_go_to_software_emulation() {
    let (mut cpu, mut board) = setup(&["fadd f1, f2, f3"]);
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x1000);
    assert_eq!(cpu.srr0, CODE);

    let (mut cpu, mut board) = setup(&["nop"]);
    board.poke32(CODE, 0); // all-zero word is not an instruction
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x1000);
    assert_eq!(Exception::SoftwareEmulation.vector_offset(), 0x1000);
}

#[test]
fn tlbia_is_recognised() {
    let (mut cpu, mut board) = setup(&["nop"]);
    board.poke32(CODE, 0x7C00_02E4); // tlbia
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, CODE + 4);

    let (mut cpu, mut board) = setup(&["nop"]);
    board.poke32(CODE, 0x7C00_02E4);
    cpu.msr |= msr::PR;
    run(&mut cpu, &mut board, 1);
    assert_eq!(cpu.pc, 0x700);
}

#[test]
fn mftb_and_timebase_writes() {
    let (cpu, _) = exec(&["mtspr 284, r3", "mtspr 285, r4", "mftb r5, 268", "mftb r6, 269"], |c, _| {
        c.gpr[3] = 0x1000;
        c.gpr[4] = 0x2;
    });
    assert_eq!(cpu.gpr[5], 0x1000);
    assert_eq!(cpu.gpr[6], 0x2);
}

#[test]
fn cache_control_commands() {
    let (cpu, _) = exec(&["mtspr 560, r3", "mfspr r4, 560", "mtspr 568, r3", "mtspr 568, r5", "mfspr r6, 568"], |c, _| {
        c.gpr[3] = 0x0200_0000; // enable
        c.gpr[5] = 0x0400_0000; // disable
    });
    assert_eq!(cpu.gpr[4] & 0x8000_0000, 0x8000_0000, "IC enabled");
    assert_eq!(cpu.gpr[6] & 0x8000_0000, 0, "DC enabled then disabled");
}

#[test]
fn generic_spr_storage_round_trips() {
    // M_TW (799) is used by the Cisco ROM as a plain scratch register
    let (cpu, _) = exec(&["mtspr 799, r3", "mfspr r4, 799", "mfspr r5, 287"], |c, _| c.gpr[3] = 0xFEED_FACE);
    assert_eq!(cpu.gpr[4], 0xFEED_FACE);
    assert_eq!(cpu.gpr[5], 0x0050_0000, "PVR of the MPC860");
}
