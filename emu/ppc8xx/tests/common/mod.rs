//! Shared helpers for the CPU tests: a flat-RAM platform and a tiny
//! one-line assembler built on the `powerpc-asm` crate.
#![allow(dead_code)]

use powerpc_asm::{Argument, Arguments, assemble};
use ppc8xx::jit::CodeWatch;
use ppc8xx::{BusError, Cpu, Platform, StepOutcome};

/// Where test programs are loaded.
pub const CODE: u32 = 0x1000;
/// A scratch data area inside RAM.
pub const DATA: u32 = 0x8000;
/// RAM covers 0..RAM_SIZE; anything above raises a bus error (machine check).
pub const RAM_SIZE: u32 = 0x1_0000;
/// A device register outside RAM: a 32-bit store sets the interrupt line
/// to (value != 0). Reads return the line.
pub const IRQ_PORT: u32 = 0x2_0000;

/// Flat RAM at address 0 plus an external-interrupt line we can drive.
pub struct TestBoard {
    pub ram: Vec<u8>,
    pub irq: bool,
    /// Count of accesses by width, to check that aligned accesses are not split.
    pub sized_accesses: [u32; 3],
    /// Pages (4 KB) holding JIT-translated code.
    pub code: CodeWatch,
}

impl TestBoard {
    pub fn new() -> Self {
        TestBoard { ram: vec![0; RAM_SIZE as usize], irq: false, sized_accesses: [0; 3], code: CodeWatch::new() }
    }
    fn range(&self, addr: u32, len: u32) -> Result<std::ops::Range<usize>, BusError> {
        let end = addr.checked_add(len).ok_or(BusError)?;
        if end > RAM_SIZE {
            return Err(BusError);
        }
        Ok(addr as usize..end as usize)
    }
    pub fn peek32(&self, addr: u32) -> u32 {
        let r = addr as usize;
        u32::from_be_bytes(self.ram[r..r + 4].try_into().unwrap())
    }
    pub fn poke32(&mut self, addr: u32, v: u32) {
        let r = addr as usize;
        self.ram[r..r + 4].copy_from_slice(&v.to_be_bytes());
    }
}

impl Platform for TestBoard {
    fn read8(&mut self, addr: u32) -> Result<u8, BusError> {
        self.sized_accesses[0] += 1;
        let r = self.range(addr, 1)?;
        Ok(self.ram[r.start])
    }
    fn read16(&mut self, addr: u32) -> Result<u16, BusError> {
        self.sized_accesses[1] += 1;
        let r = self.range(addr, 2)?;
        Ok(u16::from_be_bytes(self.ram[r].try_into().unwrap()))
    }
    fn read32(&mut self, addr: u32) -> Result<u32, BusError> {
        self.sized_accesses[2] += 1;
        if addr == IRQ_PORT {
            return Ok(self.irq as u32);
        }
        let r = self.range(addr, 4)?;
        Ok(u32::from_be_bytes(self.ram[r].try_into().unwrap()))
    }
    fn write8(&mut self, addr: u32, v: u8) -> Result<(), BusError> {
        self.sized_accesses[0] += 1;
        let r = self.range(addr, 1)?;
        self.code.note_write(addr >> 12, addr);
        self.ram[r.start] = v;
        Ok(())
    }
    fn write16(&mut self, addr: u32, v: u16) -> Result<(), BusError> {
        self.sized_accesses[1] += 1;
        let r = self.range(addr, 2)?;
        self.code.note_write(addr >> 12, addr);
        self.ram[r].copy_from_slice(&v.to_be_bytes());
        Ok(())
    }
    fn write32(&mut self, addr: u32, v: u32) -> Result<(), BusError> {
        self.sized_accesses[2] += 1;
        if addr == IRQ_PORT {
            self.irq = v != 0;
            return Ok(());
        }
        let r = self.range(addr, 4)?;
        self.code.note_write(addr >> 12, addr);
        self.ram[r].copy_from_slice(&v.to_be_bytes());
        Ok(())
    }
    fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
        let r = self.range(addr, 4)?;
        Ok(u32::from_be_bytes(self.ram[r].try_into().unwrap()))
    }
    fn external_interrupt(&self) -> bool {
        self.irq
    }

    fn host_page(&mut self, pa: u32, write: bool) -> Option<*mut u8> {
        let page = pa & !0xFFF;
        if page >= RAM_SIZE || (write && self.code.is_watched(pa >> 12)) {
            return None;
        }
        Some(self.ram[page as usize..].as_mut_ptr())
    }
    fn watch_code(&mut self, pa: u32, len: u32) -> Option<u32> {
        (pa < RAM_SIZE).then(|| {
            self.code.watch(pa >> 12, pa, len);
            pa >> 12
        })
    }
    fn take_code_write(&mut self) -> Option<u32> {
        self.code.take()
    }
    fn unwatch_code_pages(&mut self) {
        self.code.clear();
    }
}

/// Assemble one line such as `"addc. r3, r4, r5"`, `"lwz r3, -8(r4)"`,
/// `"cmpw cr1, r3, r4"` or `"bdnz 8"` (branch offsets are in bytes, relative).
pub fn asm(line: &str) -> u32 {
    let line = line.trim();
    let (mnemonic, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let mut args: Arguments = [Argument::None; 5];
    let mut n = 0;
    // "d(rA)" becomes two operands: d, rA
    for tok in rest.split([',', '(', ')']).map(str::trim).filter(|t| !t.is_empty()) {
        let tok = tok
            .strip_prefix("cr")
            .or_else(|| tok.strip_prefix('r'))
            .or_else(|| tok.strip_prefix('f'))
            .unwrap_or(tok);
        let (neg, digits) = match tok.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, tok),
        };
        let magnitude = match digits.strip_prefix("0x") {
            Some(hex) => i64::from_str_radix(hex, 16),
            None => digits.parse::<i64>(),
        }
        .unwrap_or_else(|_| panic!("bad operand {tok:?} in {line:?}"));
        let value = if neg { -magnitude } else { magnitude };
        args[n] = if value < 0 { Argument::Signed(value as i32) } else { Argument::Unsigned(value as u32) };
        n += 1;
    }
    assemble(mnemonic, &args).unwrap_or_else(|e| panic!("cannot assemble {line:?}: {e}"))
}

/// A CPU in supervisor mode with MSR[IP]=0 (vectors at 0x0000_0xxx, inside
/// test RAM) and `program` loaded at [`CODE`].
pub fn setup(program: &[&str]) -> (Cpu, TestBoard) {
    let mut board = TestBoard::new();
    for (i, line) in program.iter().enumerate() {
        board.poke32(CODE + 4 * i as u32, asm(line));
    }
    let mut cpu = Cpu::new();
    cpu.hard_reset(false);
    cpu.msr = ppc8xx::msr::ME; // machine checks enabled, everything else off
    cpu.pc = CODE;
    (cpu, board)
}

/// Step `n` instructions, asserting none of them checkstops.
pub fn run(cpu: &mut Cpu, board: &mut TestBoard, n: usize) {
    for _ in 0..n {
        assert_eq!(cpu.step(board), StepOutcome::Normal, "unexpected checkstop at {:#x}", cpu.pc);
    }
}

/// Assemble, load and run every instruction of `program` exactly once
/// (straight-line code).
pub fn exec(program: &[&str], init: impl FnOnce(&mut Cpu, &mut TestBoard)) -> (Cpu, TestBoard) {
    let (mut cpu, mut board) = setup(program);
    init(&mut cpu, &mut board);
    run(&mut cpu, &mut board, program.len());
    (cpu, board)
}

/// CR field `n` as a 4-bit value (LT GT EQ SO).
pub fn crf(cpu: &Cpu, n: u32) -> u32 {
    (cpu.cr >> (28 - 4 * n)) & 0xF
}

pub const LT: u32 = 0b1000;
pub const GT: u32 = 0b0100;
pub const EQ: u32 = 0b0010;
pub const SO: u32 = 0b0001;
pub const XER_SO: u32 = 0x8000_0000;
pub const XER_OV: u32 = 0x4000_0000;
pub const XER_CA: u32 = 0x2000_0000;
