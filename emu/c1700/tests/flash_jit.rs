//! The JIT reads flash in read-array mode directly from host memory (docs
//! ch. 17.15). A flash command switches the bank to ID or status reads:
//! from then on those reads must reach the flash model again.
//!
//! A tiny synthetic ROM maps the flash, then loops: sum 1 KB of flash,
//! read the identifier, go back to read-array, program a word, read the
//! status and the programmed word. Interpreter, JIT and JIT in lockstep
//! check must agree on every register and on the flash contents.

use c1700emu::board::{BoardConfig, nvram::Nvram};
use c1700emu::machine::{CpuEngine, Machine};
use powerpc_asm::{Argument, Arguments, assemble};

/// Assemble `"lwz r6, 0x100(r3)"`-style lines (branch offsets in bytes).
fn asm(line: &str) -> u32 {
    let (mnemonic, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    let mut args: Arguments = [Argument::None; 5];
    for (n, tok) in rest.split([',', '(', ')']).map(str::trim).filter(|t| !t.is_empty()).enumerate() {
        let tok = tok.strip_prefix('r').unwrap_or(tok);
        let (neg, digits) = tok.strip_prefix('-').map_or((false, tok), |d| (true, d));
        let magnitude = match digits.strip_prefix("0x") {
            Some(hex) => i64::from_str_radix(hex, 16),
            None => digits.parse::<i64>(),
        }
        .unwrap_or_else(|_| panic!("bad operand {tok:?} in {line:?}"));
        let value = if neg { -magnitude } else { magnitude };
        args[n] = if value < 0 { Argument::Signed(value as i32) } else { Argument::Unsigned(value as u32) };
    }
    assemble(mnemonic, &args).unwrap_or_else(|e| panic!("cannot assemble {line:?}: {e}"))
}

const PROGRAM: &[&str] = &[
    "lis r1, 0xff00",
    "mtspr 638, r1",          // IMMR: internal registers at 0xFF000000
    "lis r2, 0xfff0",
    "ori r2, r2, 0x401",
    "stw r2, 0x100(r1)",      // BR0: this ROM at 0xFFF00000, 8-bit port
    "lis r2, 0xfff0",
    "stw r2, 0x104(r1)",      // OR0: 1 MB
    "lis r2, 0x6000",
    "ori r2, r2, 0x801",
    "stw r2, 0x130(r1)",      // BR6: the flash at 0x60000000, 16-bit port
    "lis r2, 0xfc00",
    "stw r2, 0x134(r1)",      // OR6: 64 MB window
    "lis r3, 0x6000",
    "li r13, -1",             // the word to program
    "li r4, 256",             // loop: sum 1 KB of flash (plain reads)
    "mtctr r4",
    "or r5, r3, r3",
    "lwz r6, 0(r5)",
    "add r7, r7, r6",
    "lbz r6, 1(r5)",
    "add r8, r8, r6",
    "addi r5, r5, 4",
    "bdnz -20",
    "li r6, 0x90",            // read identifier
    "sth r6, 0(r3)",
    "lhz r9, 0(r3)",          // manufacturer
    "lhz r10, 2(r3)",         // device
    "lwz r11, 0x100(r3)",     // block lock status: 0, not the array
    "add r16, r16, r9",
    "add r16, r16, r10",
    "add r16, r16, r11",
    "li r6, 0xff",            // read array again
    "sth r6, 0(r3)",
    "lwz r12, 0x100(r3)",
    "add r17, r17, r12",
    "li r6, 0x40",            // program the word at 0x200
    "sth r6, 0x200(r3)",
    "sth r13, 0x200(r3)",
    "lhz r14, 0x200(r3)",     // status
    "add r18, r18, r14",
    "li r6, 0xff",
    "sth r6, 0(r3)",
    "lhz r15, 0x200(r3)",     // the programmed word
    "add r19, r19, r15",
    "addi r13, r13, -3",
    "addi r20, r20, 1",
    "b -128",
];

fn run(engine: CpuEngine) -> Machine {
    let mut rom = vec![0u8; 64 << 10];
    for (i, line) in PROGRAM.iter().enumerate() {
        rom[0x100 + 4 * i..0x104 + 4 * i].copy_from_slice(&asm(line).to_be_bytes());
    }
    let cfg = BoardConfig { net: None, ..BoardConfig::default() };
    let mut m = Machine::new(rom, &cfg, Nvram::new(32 * 1024, None).unwrap());
    let pattern: Vec<u8> = (0..64 << 10).map(|i: u32| (i.wrapping_mul(37) >> 3) as u8).collect();
    m.board.flash.load(&pattern);
    m.set_engine(engine, 1);
    m.run(300_000, false);
    m
}

#[test]
fn jit_reads_flash_directly_and_sees_flash_commands() {
    let reference = run(CpuEngine::Interp);
    let loops = reference.cpu.gpr[20];
    assert!(loops > 100, "loops: {loops}");
    assert_eq!(reference.cpu.gpr[9], 0x89, "Intel manufacturer ID");
    assert_eq!(reference.cpu.gpr[11], 0, "ID mode reads at 0x100");
    for engine in [CpuEngine::Jit, CpuEngine::JitCheck] {
        let m = run(engine);
        assert_eq!(m.cpu.gpr, reference.cpu.gpr, "{engine:?}: registers");
        assert_eq!((m.cpu.pc, m.cpu.cr, m.cpu.cycles), (reference.cpu.pc, reference.cpu.cr, reference.cpu.cycles), "{engine:?}");
        assert!(m.board.flash.bytes() == reference.board.flash.bytes(), "{engine:?}: flash contents");
    }
}
