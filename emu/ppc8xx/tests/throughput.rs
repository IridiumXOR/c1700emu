//! Rough interpreter speed check. Ignored by default; run with
//! `cargo test --release -p ppc8xx --test throughput -- --ignored --nocapture`.

mod common;
use common::*;
use std::time::Instant;

#[test]
#[ignore]
fn interpreter_throughput() {
    // A memory-clearing loop like the ROM's BSS clear, plus some ALU work.
    let (mut cpu, mut board) = setup(&[
        "lis r3, 0",
        "ori r3, r3, 0x8000",
        "li r4, 0",
        "li r5, 0x400",
        "mtctr r5",
        "stw r4, 0(r3)",   // loop:
        "addi r3, r3, 4",
        "rlwinm r6, r3, 3, 0, 28",
        "add. r7, r6, r5",
        "bdnz -16",
        "b -40",           // restart the whole thing
    ]);
    let n: u64 = 50_000_000;
    let start = Instant::now();
    for _ in 0..n {
        cpu.step(&mut board);
    }
    let secs = start.elapsed().as_secs_f64();
    assert!(cpu.pc >= CODE && cpu.pc < CODE + 44, "benchmark escaped its loop (pc={:#x})", cpu.pc);
    eprintln!("{n} instructions in {secs:.2}s = {:.1} MIPS", n as f64 / secs / 1e6);
}
