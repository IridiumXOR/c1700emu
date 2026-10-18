//! The SCC's HDLC engine without IOS (docs ch. 23): a synthetic ROM sets up
//! SCC1 with one TxBD and one RxBD in the dual-port RAM, enables it in
//! local loopback (GSMR_L[DIAG] = 01) and asks for transmission on demand.
//! The frame must come back in the RxBD followed by its CRC-16, with the
//! buffers closed and SCCE[TXB, RXF] set, with both CPU engines.

use c1700emu::board::{BoardConfig, nvram::Nvram};
use c1700emu::machine::{CpuEngine, Machine};
use powerpc_asm::{Argument, Arguments, assemble};

fn asm(line: &str) -> u32 {
    let (mnemonic, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    let mut args: Arguments = [Argument::None; 5];
    for (n, tok) in rest.split([',', '(', ')']).map(str::trim).filter(|t| !t.is_empty()).enumerate() {
        let tok = tok.strip_prefix('r').unwrap_or(tok);
        let value = match tok.strip_prefix("0x") {
            Some(hex) => i64::from_str_radix(hex, 16),
            None => tok.parse::<i64>(),
        }
        .unwrap_or_else(|_| panic!("bad operand {tok:?} in {line:?}"));
        args[n] = if value < 0 { Argument::Signed(value as i32) } else { Argument::Unsigned(value as u32) };
    }
    assemble(mnemonic, &args).unwrap_or_else(|e| panic!("cannot assemble {line:?}: {e}"))
}

const PROGRAM: &[&str] = &[
    "lis r1, 0xff00",
    "mtspr 638, r1",          // IMMR at 0xFF000000 (DPRAM at +0x2000)
    "lis r2, 0x0f00",         // the frame, 8 bytes at IMMR+0x2800:
    "ori r2, r2, 0x800",      //   0F 00 08 00  45 00 00 14
    "stw r2, 0x2800(r1)",
    "lis r2, 0x4500",
    "ori r2, r2, 0x14",
    "stw r2, 0x2804(r1)",
    "li r2, 0x100",           // SCC1 parameter RAM: RBASE = RBPTR = 0x100,
    "sth r2, 0x3c00(r1)",
    "sth r2, 0x3c10(r1)",
    "li r2, 0x200",           // TBASE = TBPTR = 0x200,
    "sth r2, 0x3c02(r1)",
    "sth r2, 0x3c20(r1)",
    "li r2, 64",              // MRBLR = 64,
    "sth r2, 0x3c06(r1)",
    "li r2, 1600",            // MFLR = 1600,
    "sth r2, 0x3c46(r1)",
    "li r2, 1",               // RFTHR = RFCNT = 1, HMASK = 0 (any address)
    "sth r2, 0x3c4a(r1)",
    "sth r2, 0x3c4c(r1)",
    "li r2, 0",
    "sth r2, 0x3c4e(r1)",
    "lis r2, 0xb000",         // RxBD at DPRAM 0x100: E|W|I, buffer IMMR+0x2900
    "stw r2, 0x2100(r1)",
    "lis r2, 0xff00",
    "ori r2, r2, 0x2900",
    "stw r2, 0x2104(r1)",
    "lis r2, 0xbc00",         // TxBD at DPRAM 0x200: R|W|I|L|TC, 8 bytes
    "ori r2, r2, 8",
    "stw r2, 0x2200(r1)",
    "lis r2, 0xff00",
    "ori r2, r2, 0x2800",
    "stw r2, 0x2204(r1)",
    "li r2, 0x70",            // GSMR_L1: HDLC, local loopback, ENT, ENR
    "stw r2, 0xa00(r1)",
    "li r2, 0",               // TODR1[TOD]: transmit now
    "ori r2, r2, 0x8000",
    "sth r2, 0xa0c(r1)",
    "b 0",                    // done: spin
];

fn run(engine: CpuEngine) -> Machine {
    let mut rom = vec![0u8; 64 << 10];
    for (i, line) in PROGRAM.iter().enumerate() {
        rom[0x100 + 4 * i..0x104 + 4 * i].copy_from_slice(&asm(line).to_be_bytes());
    }
    let cfg = BoardConfig { net: None, ..BoardConfig::default() };
    let mut m = Machine::new(rom, &cfg, Nvram::new(32 * 1024, None).unwrap());
    m.set_engine(engine, 1);
    m.run(10_000, false);
    m
}

/// CRC-16/X.25 of the frame, low byte first (what the SCC appends).
fn fcs16(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    (!crc).to_le_bytes()
}

#[test]
fn scc1_loops_a_frame_back_with_its_crc() {
    let frame = [0x0F, 0x00, 0x08, 0x00, 0x45, 0x00, 0x00, 0x14];
    for engine in [CpuEngine::Interp, CpuEngine::Jit] {
        let m = run(engine);
        let mpc = &m.board.mpc;
        assert_eq!(mpc.load16(0x2200), 0x3C00, "{engine:?}: TxBD closed (R clear)");
        assert_eq!(mpc.load16(0x2100), 0x3C00, "{engine:?}: RxBD closed (E clear, F and L set, no errors)");
        assert_eq!(mpc.load16(0x2102), 10, "{engine:?}: frame length includes the 2-byte CRC");
        let rx: Vec<u8> = (0..10).map(|i| mpc.load8(0x2900 + i)).collect();
        assert_eq!(&rx[..8], &frame, "{engine:?}: data");
        assert_eq!(&rx[8..], &fcs16(&frame), "{engine:?}: CRC");
        assert_eq!(mpc.load16(0xA10), 0x000A, "{engine:?}: SCCE = TXB | RXF");
        assert_eq!(mpc.load16(0x3C10), 0x100, "{engine:?}: RBPTR wrapped back to RBASE");
        assert_eq!(mpc.load16(0x3C20), 0x200, "{engine:?}: TBPTR wrapped back to TBASE");
    }
}
