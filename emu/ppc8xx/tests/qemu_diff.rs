//! Differential testing against QEMU.
//!
//! The idea: QEMU's PowerPC emulation is mature and independent from ours, so
//! if both emulators run the *same instruction words* from the *same initial
//! state*, they must end in the same state. Any difference is a bug (almost
//! always ours).
//!
//! How it works:
//! 1. Generate random instruction sequences (seeded, reproducible). Each one is
//!    wrapped in a prologue that loads random values into r0-r28, CR, XER,
//!    CTR and LR, and an epilogue that dumps r0-r30, CR, XER, LR, CTR plus a
//!    1 KB scratch buffer to stdout with the Linux `write` syscall.
//! 2. Many sequences are concatenated into one tiny static Linux ELF,
//!    assembled with `llvm-mc` and linked with `ld.lld`.
//! 3. Run it under `qemu-ppc-static` (user-mode Linux emulation) and under our
//!    [`Cpu`] with a test platform that emulates `write`/`exit`.
//! 4. Compare the two stdout streams byte for byte.
//!
//! Register conventions inside a sequence:
//! * r29 = small signed offset (multiple of 4, -16..16), never written,
//! * r30 = pointer into the middle of the scratch buffer (only moved by
//!   update-form loads/stores, in multiples of 4),
//! * r31 = dump buffer pointer, never written.
//!
//! Excluded on purpose: architecturally undefined results (divide by zero,
//! 0x8000_0000 / -1; the generator forces a safe divisor first), `dcbz`
//! (QEMU's 750 has 32-byte cache lines, the 860 has 16), `mftb`, `mtxer` with
//! garbage bits, `lmw` (would clobber r29-r31), traps and privileged
//! instructions (we run in user mode, like QEMU).
//!
//! The same batches also check the JIT (`ppc8xx::jit`, docs chapter 17):
//! with a translation threshold of 1 every block is translated the first
//! time it runs, so the JIT must give the same output as QEMU, and
//! `jit_matches_interpreter` compares it with the interpreter on more
//! sequences (without needing QEMU).
//!
//! Environment knobs: `QEMU_DIFF_SEQUENCES` (default 400), `QEMU_DIFF_SEED`,
//! `JIT_DIFF_SEQUENCES` (default 3000).

mod common;

use common::asm;
use object::{Object, ObjectSegment};
use ppc8xx::jit::{CodeWatch, Jit};
use ppc8xx::{BusError, Cpu, Platform, msr};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const BATCH: usize = 100;
const SCRATCH_SIZE: usize = 1024;
const DUMP_SIZE: usize = 140; // r0-r30, CR, XER, LR, CTR
const CHUNK: usize = DUMP_SIZE + SCRATCH_SIZE;
const TEXT_BASE: u32 = 0x1000_0000;
const DATA_BASE: u32 = 0x1010_0000;
/// Registers a random instruction may write.
const DST_REGS: u32 = 29; // r0..r28

#[test]
fn qemu_differential() {
    let tools = ["qemu-ppc-static", "llvm-mc", "ld.lld"];
    if let Some(missing) = tools.iter().find(|t| which(t).is_none()) {
        eprintln!("SKIPPING qemu differential test: `{missing}` not found in PATH");
        return;
    }
    let total: usize = env_or("QEMU_DIFF_SEQUENCES", 400);
    let seed: u64 = env_or("QEMU_DIFF_SEED", 0x0860_1700);
    let workdir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("qemu_diff");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut rng = fastrand::Rng::with_seed(seed);
    let mut done = 0;
    let mut batch_no = 0;
    while done < total {
        let n = BATCH.min(total - done);
        let batch = Batch::generate(&mut rng, n);
        let elf = batch.build(&workdir, batch_no);
        let expected = run_qemu(&elf);
        let actual = run_ours(&elf, Engine::Interpreter);
        batch.compare(&expected, &actual, seed, batch_no, "qemu", "ours");
        let jit = run_ours(&elf, Engine::Jit);
        batch.compare(&expected, &jit, seed, batch_no, "qemu", "JIT");
        done += n;
        batch_no += 1;
    }
    eprintln!("qemu differential: {done} sequences OK, interpreter and JIT (seed {seed:#x})");
}

/// The JIT against the interpreter on the same random sequences.
#[test]
fn jit_matches_interpreter() {
    let tools = ["llvm-mc", "ld.lld"];
    if let Some(missing) = tools.iter().find(|t| which(t).is_none()) {
        eprintln!("SKIPPING JIT differential test: `{missing}` not found in PATH");
        return;
    }
    let total: usize = env_or("JIT_DIFF_SEQUENCES", 3000);
    let seed: u64 = env_or("QEMU_DIFF_SEED", 0x0860_1700) ^ 0x717;
    let workdir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("jit_diff");
    std::fs::create_dir_all(&workdir).unwrap();

    let mut rng = fastrand::Rng::with_seed(seed);
    let mut done = 0;
    let mut batch_no = 0;
    while done < total {
        let n = BATCH.min(total - done);
        let batch = Batch::generate(&mut rng, n);
        let elf = batch.build(&workdir, batch_no);
        let expected = run_ours(&elf, Engine::Interpreter);
        let actual = run_ours(&elf, Engine::Jit);
        batch.compare(&expected, &actual, seed, batch_no, "interpreter", "JIT");
        done += n;
        batch_no += 1;
    }
    eprintln!("JIT differential: {done} sequences OK (seed {seed:#x})");
}

// ---------------------------------------------------------------------------
// Sequence generation
// ---------------------------------------------------------------------------

/// One random test case: initial state + body instructions.
struct Sequence {
    gpr: [u32; 29],
    cr: u32,
    xer: u32,
    ctr: u32,
    lr: u32,
    small_offset: i32,
    /// (assembly text for humans, encoded word)
    body: Vec<(String, u32)>,
}

struct Batch {
    sequences: Vec<Sequence>,
    scratch: Vec<u8>,
}

impl Batch {
    fn generate(rng: &mut fastrand::Rng, count: usize) -> Batch {
        let sequences = (0..count).map(|_| Sequence::generate(rng)).collect();
        let scratch = (0..SCRATCH_SIZE).map(|_| rng.u8(..)).collect();
        Batch { sequences, scratch }
    }

    /// Write the assembly source, assemble and link it, return the ELF path.
    fn build(&self, dir: &Path, batch_no: usize) -> PathBuf {
        let mut s = String::new();
        s.push_str("    .text\n    .globl _start\n_start:\n");
        for (i, seq) in self.sequences.iter().enumerate() {
            writeln!(s, "# ---- sequence {i} ----").unwrap();
            seq.emit(&mut s);
        }
        s.push_str("    li 0, 1\n    li 3, 0\n    sc\n"); // exit(0)
        s.push_str("    .data\n    .p2align 4\ndump: .space 160\n    .p2align 4\nscratch:\n");
        for row in self.scratch.chunks(16) {
            let bytes: Vec<String> = row.iter().map(|b| b.to_string()).collect();
            writeln!(s, "    .byte {}", bytes.join(",")).unwrap();
        }

        let src = dir.join(format!("batch{batch_no}.s"));
        let obj = dir.join(format!("batch{batch_no}.o"));
        let elf = dir.join(format!("batch{batch_no}.elf"));
        std::fs::write(&src, s).unwrap();
        run_tool(Command::new("llvm-mc").args(["-triple=powerpc-unknown-linux-gnu", "-filetype=obj", "-o"]).arg(&obj).arg(&src));
        run_tool(
            Command::new("ld.lld")
                .args(["-e", "_start"])
                .arg(format!("--section-start=.text={TEXT_BASE:#x}"))
                .arg(format!("--section-start=.data={DATA_BASE:#x}"))
                .arg("-o")
                .arg(&elf)
                .arg(&obj),
        );
        elf
    }

    fn compare(&self, expected: &[u8], actual: &[u8], seed: u64, batch_no: usize, them: &str, us: &str) {
        if expected == actual {
            return;
        }
        let index = (0..)
            .find(|i| {
                let r = i * CHUNK..((i + 1) * CHUNK).min(expected.len().max(actual.len()));
                expected.get(r.clone()) != actual.get(r)
            })
            .unwrap();
        let mut report = format!(
            "MISMATCH {us} vs {them}: seed {seed:#x}, batch {batch_no}, sequence {index}\n({them} output {} bytes, {us} {} bytes)\n",
            expected.len(),
            actual.len()
        );
        if let Some(seq) = self.sequences.get(index) {
            report += &seq.describe();
        }
        let chunk = |buf: &[u8]| buf.get(index * CHUNK..(index + 1) * CHUNK).map(<[u8]>::to_vec);
        match (chunk(expected), chunk(actual)) {
            (Some(e), Some(a)) => {
                report += &format!("register differences (name: {them} vs {us}):\n");
                for (k, name) in dump_names().iter().enumerate() {
                    let ev = u32::from_be_bytes(e[4 * k..4 * k + 4].try_into().unwrap());
                    let av = u32::from_be_bytes(a[4 * k..4 * k + 4].try_into().unwrap());
                    if ev != av {
                        writeln!(report, "  {name:>4}: {ev:#010x} vs {av:#010x}").unwrap();
                    }
                }
                for off in DUMP_SIZE..CHUNK {
                    if e[off] != a[off] {
                        writeln!(report, "  scratch[{:#05x}]: {:#04x} vs {:#04x}", off - DUMP_SIZE, e[off], a[off]).unwrap();
                    }
                }
            }
            _ => report += "  (one side produced less output: it probably took an exception)\n",
        }
        panic!("{report}");
    }
}

impl Sequence {
    fn generate(rng: &mut fastrand::Rng) -> Sequence {
        let mut gpr = [0u32; 29];
        for r in gpr.iter_mut() {
            // Mix fully random values with "interesting" ones (0, -1, sign bit...).
            *r = match rng.u8(0..8) {
                0 => [0, 1, u32::MAX, 0x8000_0000, 0x7FFF_FFFF, 0xFFFF, 0x8000][rng.usize(0..7)],
                1 => rng.u32(0..64),
                _ => rng.u32(..),
            };
        }
        let mut seq = Sequence {
            gpr,
            cr: rng.u32(..),
            // SO/OV/CA random, byte count 0..=32 (for lswx/stswx)
            xer: (rng.u32(..) & 0xE000_0000) | rng.u32(0..=32),
            ctr: [0, 1, 2, 3, rng.u32(..)][rng.usize(0..5)],
            lr: rng.u32(..),
            small_offset: 4 * rng.i32(-4..=4),
            body: Vec::new(),
        };
        let len = rng.usize(1..=24);
        while seq.body.len() < len {
            let last = seq.body.len() + 1 >= len;
            seq.random_instruction(rng, last);
        }
        seq
    }

    fn push(&mut self, text: String) {
        let word = asm(&text);
        self.body.push((text, word));
    }

    /// Append one random instruction (sometimes a short idiom of 2-3).
    fn random_instruction(&mut self, rng: &mut fastrand::Rng, is_last: bool) {
        let rd = rng.u32(0..DST_REGS);
        let ra = rng.u32(0..32);
        let rb = rng.u32(0..32);
        let simm = rng.i32(-0x8000..0x8000);
        let uimm = rng.u32(0..0x10000);
        let oe = if rng.bool() { "o" } else { "" };
        let rc = if rng.bool() { "." } else { "" };
        match rng.u8(0..20) {
            // XO-form arithmetic, 3 operands
            0 | 1 => {
                let op = ["add", "addc", "adde", "subf", "subfc", "subfe", "mullw"][rng.usize(0..7)];
                self.push(format!("{op}{oe}{rc} r{rd}, r{ra}, r{rb}"));
            }
            // XO-form arithmetic, 2 operands
            2 => {
                let op = ["addme", "addze", "subfme", "subfze", "neg"][rng.usize(0..5)];
                self.push(format!("{op}{oe}{rc} r{rd}, r{ra}"));
            }
            3 => {
                let op = ["mulhw", "mulhwu"][rng.usize(0..2)];
                self.push(format!("{op}{rc} r{rd}, r{ra}, r{rb}"));
            }
            // division with a guaranteed-safe divisor: positive and odd.
            // The leading nop absorbs a preceding "skip next instruction"
            // branch, which would otherwise skip the guard.
            4 => {
                let divisor = rng.u32(0..DST_REGS);
                self.push("nop".to_string());
                self.push(format!("ori r{divisor}, r{divisor}, 1"));
                self.push(format!("rlwinm r{divisor}, r{divisor}, 0, 1, 31"));
                let op = ["divw", "divwu"][rng.usize(0..2)];
                self.push(format!("{op}{oe}{rc} r{rd}, r{ra}, r{divisor}"));
            }
            // D-form arithmetic / logical / compare immediates
            5 | 6 => match rng.u8(0..12) {
                0 => self.push(format!("addi r{rd}, r{ra}, {simm}")),
                1 => self.push(format!("addis r{rd}, r{ra}, {}", simm as u16)),
                2 => self.push(format!("addic r{rd}, r{ra}, {simm}")),
                3 => self.push(format!("addic. r{rd}, r{ra}, {simm}")),
                4 => self.push(format!("subfic r{rd}, r{ra}, {simm}")),
                5 => self.push(format!("mulli r{rd}, r{ra}, {simm}")),
                6 => self.push(format!("andi. r{rd}, r{ra}, {uimm}")),
                7 => self.push(format!("andis. r{rd}, r{ra}, {uimm}")),
                8 => {
                    let op = ["ori", "oris", "xori", "xoris"][rng.usize(0..4)];
                    self.push(format!("{op} r{rd}, r{ra}, {uimm}"));
                }
                9 => self.push(format!("cmpwi cr{}, r{ra}, {simm}", rng.u8(0..8))),
                10 => self.push(format!("cmplwi cr{}, r{ra}, {uimm}", rng.u8(0..8))),
                _ => self.push(format!("cmp{} cr{}, r{ra}, r{rb}", ["w", "lw"][rng.usize(0..2)], rng.u8(0..8))),
            },
            // X-form logical (note: `op rA, rS, rB` -> rd is the destination)
            7 | 8 => {
                let op = ["and", "andc", "or", "orc", "xor", "nand", "nor", "eqv", "slw", "srw", "sraw"][rng.usize(0..11)];
                self.push(format!("{op}{rc} r{rd}, r{ra}, r{rb}"));
            }
            9 => {
                let op = ["extsb", "extsh", "cntlzw"][rng.usize(0..3)];
                self.push(format!("{op}{rc} r{rd}, r{ra}"));
            }
            10 => self.push(format!("srawi{rc} r{rd}, r{ra}, {}", rng.u8(0..32))),
            // rotates
            11 | 12 => {
                let (sh, mb, me) = (rng.u8(0..32), rng.u8(0..32), rng.u8(0..32));
                match rng.u8(0..3) {
                    0 => self.push(format!("rlwinm{rc} r{rd}, r{ra}, {sh}, {mb}, {me}")),
                    1 => self.push(format!("rlwimi{rc} r{rd}, r{ra}, {sh}, {mb}, {me}")),
                    _ => self.push(format!("rlwnm{rc} r{rd}, r{ra}, r{rb}, {mb}, {me}")),
                }
            }
            // condition register
            13 => match rng.u8(0..5) {
                0 => {
                    let op = ["crand", "crandc", "creqv", "crnand", "crnor", "cror", "crorc", "crxor"][rng.usize(0..8)];
                    self.push(format!("{op} {}, {}, {}", rng.u8(0..32), rng.u8(0..32), rng.u8(0..32)));
                }
                1 => self.push(format!("mcrf cr{}, cr{}", rng.u8(0..8), rng.u8(0..8))),
                2 => self.push(format!("mcrxr cr{}", rng.u8(0..8))),
                3 => self.push(format!("mfcr r{rd}")),
                _ => self.push(format!("mtcrf {}, r{ra}", rng.u8(..))),
            },
            // SPR moves available in user mode
            14 => match rng.u8(0..5) {
                0 => self.push(format!("mtctr r{ra}")),
                1 => self.push(format!("mfctr r{rd}")),
                2 => self.push(format!("mtlr r{ra}")),
                3 => self.push(format!("mflr r{rd}")),
                _ => self.push(format!("mfxer r{rd}")),
            },
            // conditional branch that may skip the next instruction
            15 if !is_last => {
                const BO: [u8; 10] = [0x00, 0x02, 0x04, 0x05, 0x08, 0x0A, 0x0C, 0x10, 0x12, 0x14];
                let bo = BO[rng.usize(0..BO.len())];
                let lk = if rng.bool() { "l" } else { "" };
                self.push(format!("bc{lk} {bo}, {}, 8", rng.u8(0..32)));
            }
            // loads from the scratch buffer (r30)
            16 => {
                let disp = rng.i32(-64..=64);
                let disp4 = 4 * rng.i32(-4..=4);
                match rng.u8(0..4) {
                    0 => {
                        let op = ["lbz", "lhz", "lha", "lwz"][rng.usize(0..4)];
                        self.push(format!("{op} r{rd}, {disp}(r30)"));
                    }
                    1 => {
                        let op = ["lbzu", "lhzu", "lhau", "lwzu"][rng.usize(0..4)];
                        self.push(format!("{op} r{rd}, {disp4}(r30)"));
                    }
                    2 => {
                        let op = ["lbzx", "lhzx", "lhax", "lwzx", "lhbrx", "lwbrx"][rng.usize(0..6)];
                        self.push(format!("{op} r{rd}, r30, r29"));
                    }
                    _ => {
                        let op = ["lbzux", "lhzux", "lhaux", "lwzux"][rng.usize(0..4)];
                        self.push(format!("{op} r{rd}, r30, r29"));
                    }
                }
            }
            // stores to the scratch buffer
            17 => {
                let disp = rng.i32(-64..=64);
                let disp4 = 4 * rng.i32(-4..=4);
                let rs = rng.u32(0..32);
                match rng.u8(0..5) {
                    0 => {
                        let op = ["stb", "sth", "stw"][rng.usize(0..3)];
                        self.push(format!("{op} r{rs}, {disp}(r30)"));
                    }
                    1 => {
                        let op = ["stbu", "sthu", "stwu"][rng.usize(0..3)];
                        self.push(format!("{op} r{rs}, {disp4}(r30)"));
                    }
                    2 => {
                        let op = ["stbx", "sthx", "stwx", "sthbrx", "stwbrx"][rng.usize(0..5)];
                        self.push(format!("{op} r{rs}, r30, r29"));
                    }
                    3 => {
                        let op = ["stbux", "sthux", "stwux"][rng.usize(0..3)];
                        self.push(format!("{op} r{rs}, r30, r29"));
                    }
                    _ => self.push(format!("stmw r{}, {disp4}(r30)", rng.u32(16..32))),
                }
            }
            // string instructions
            18 => {
                let xer_count = self.xer & 0x7F;
                match rng.u8(0..4) {
                    0 => {
                        let nb = rng.u32(1..=32);
                        let regs = nb.div_ceil(4);
                        let first = rng.u32(0..=(DST_REGS - regs));
                        self.push(format!("lswi r{first}, r30, {}", nb % 32));
                    }
                    1 => self.push(format!("stswi r{}, r30, {}", rng.u32(0..32), rng.u32(0..32))),
                    2 => {
                        let regs = xer_count.div_ceil(4);
                        let first = rng.u32(0..=(DST_REGS - regs.max(1)));
                        self.push(format!("lswx r{first}, r30, r29"));
                    }
                    _ => self.push(format!("stswx r{}, r30, r29", rng.u32(0..32))),
                }
            }
            // load-reserve / store-conditional, always as an adjacent pair
            _ => {
                if is_last {
                    self.push(format!("stwcx. r{}, 0, r30", rng.u32(0..32))); // no reservation: fails
                } else {
                    self.push(format!("lwarx r{rd}, 0, r30"));
                    self.push(format!("stwcx. r{}, 0, r30", rng.u32(0..32)));
                }
            }
        }
    }

    fn emit(&self, s: &mut String) {
        let load32 = |s: &mut String, r: u32, v: u32| {
            writeln!(s, "    lis {r}, {}\n    ori {r}, {r}, {}", (v >> 16) as u16 as i16, v & 0xFFFF).unwrap();
        };
        // special registers first, via r0
        load32(s, 0, self.cr);
        s.push_str("    mtcrf 255, 0\n");
        load32(s, 0, self.xer);
        s.push_str("    mtxer 0\n");
        load32(s, 0, self.ctr);
        s.push_str("    mtctr 0\n");
        load32(s, 0, self.lr);
        s.push_str("    mtlr 0\n");
        writeln!(s, "    li 29, {}", self.small_offset).unwrap();
        writeln!(s, "    lis 30, (scratch+{})@ha\n    addi 30, 30, (scratch+{})@l", SCRATCH_SIZE / 2, SCRATCH_SIZE / 2).unwrap();
        s.push_str("    lis 31, dump@ha\n    addi 31, 31, dump@l\n");
        for (r, &v) in self.gpr.iter().enumerate() {
            load32(s, r as u32, v);
        }
        for (text, word) in &self.body {
            writeln!(s, "    .long {word:#010x}   # {text}").unwrap();
        }
        // dump r0..r30, CR, XER, LR, CTR
        for r in 0..31 {
            writeln!(s, "    stw {r}, {}(31)", 4 * r).unwrap();
        }
        s.push_str("    mfcr 0\n    stw 0, 124(31)\n    mfxer 0\n    stw 0, 128(31)\n");
        s.push_str("    mflr 0\n    stw 0, 132(31)\n    mfctr 0\n    stw 0, 136(31)\n");
        writeln!(s, "    li 0, 4\n    li 3, 1\n    mr 4, 31\n    li 5, {DUMP_SIZE}\n    sc").unwrap();
        writeln!(s, "    li 0, 4\n    li 3, 1\n    lis 4, scratch@ha\n    addi 4, 4, scratch@l\n    li 5, {SCRATCH_SIZE}\n    sc").unwrap();
    }

    fn describe(&self) -> String {
        let mut s = String::from("initial state:\n");
        for (r, v) in self.gpr.iter().enumerate() {
            write!(s, "  r{r:<2}={v:#010x}").unwrap();
            if r % 4 == 3 {
                s.push('\n');
            }
        }
        writeln!(s, "\n  cr={:#010x} xer={:#010x} ctr={:#010x} lr={:#010x} r29={}", self.cr, self.xer, self.ctr, self.lr, self.small_offset).unwrap();
        s.push_str("body:\n");
        for (text, word) in &self.body {
            let ins = powerpc::Ins::new(*word, powerpc::Extensions::none());
            writeln!(s, "  {word:08x}  {text:<32} ; decoded as `{}`", ins.simplified()).unwrap();
        }
        s
    }
}

fn dump_names() -> Vec<String> {
    let mut names: Vec<String> = (0..31).map(|r| format!("r{r}")).collect();
    names.extend(["cr", "xer", "lr", "ctr"].map(String::from));
    names
}

// ---------------------------------------------------------------------------
// Running both emulators
// ---------------------------------------------------------------------------

fn run_qemu(elf: &Path) -> Vec<u8> {
    let out = Command::new("qemu-ppc-static").args(["-cpu", "750"]).arg(elf).output().expect("run qemu");
    assert!(out.status.success(), "qemu failed: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Minimal user-mode "Linux" for our CPU: flat memory for the ELF segments
/// and the two syscalls the test programs use.
struct LinuxUser {
    base: u32,
    mem: Vec<u8>,
    stdout: Vec<u8>,
    exited: bool,
    /// Pages holding JIT-translated code.
    code: CodeWatch,
}

impl LinuxUser {
    fn range(&self, addr: u32, len: u32) -> Result<std::ops::Range<usize>, BusError> {
        let start = addr.checked_sub(self.base).ok_or(BusError)? as usize;
        let end = start + len as usize;
        if end > self.mem.len() {
            return Err(BusError);
        }
        Ok(start..end)
    }
}

impl Platform for LinuxUser {
    fn read8(&mut self, a: u32) -> Result<u8, BusError> {
        let r = self.range(a, 1)?;
        Ok(self.mem[r.start])
    }
    fn read16(&mut self, a: u32) -> Result<u16, BusError> {
        let r = self.range(a, 2)?;
        Ok(u16::from_be_bytes(self.mem[r].try_into().unwrap()))
    }
    fn read32(&mut self, a: u32) -> Result<u32, BusError> {
        let r = self.range(a, 4)?;
        Ok(u32::from_be_bytes(self.mem[r].try_into().unwrap()))
    }
    fn write8(&mut self, a: u32, v: u8) -> Result<(), BusError> {
        let r = self.range(a, 1)?;
        self.code.note_write(a >> 12);
        self.mem[r.start] = v;
        Ok(())
    }
    fn write16(&mut self, a: u32, v: u16) -> Result<(), BusError> {
        let r = self.range(a, 2)?;
        self.code.note_write(a >> 12);
        self.mem[r].copy_from_slice(&v.to_be_bytes());
        Ok(())
    }
    fn write32(&mut self, a: u32, v: u32) -> Result<(), BusError> {
        let r = self.range(a, 4)?;
        self.code.note_write(a >> 12);
        self.mem[r].copy_from_slice(&v.to_be_bytes());
        Ok(())
    }
    fn host_page(&mut self, pa: u32, write: bool) -> Option<*mut u8> {
        let r = self.range(pa & !0xFFF, 0x1000).ok()?;
        if write && self.code.is_watched(pa >> 12) {
            return None;
        }
        Some(self.mem[r].as_mut_ptr())
    }
    fn watch_code_page(&mut self, pa: u32) -> Option<u32> {
        self.range(pa & !0xFFF, 0x1000).ok()?;
        self.code.watch(pa >> 12);
        Some(pa >> 12)
    }
    fn take_code_write(&mut self) -> Option<u32> {
        self.code.take()
    }
    fn unwatch_code_pages(&mut self) {
        self.code.clear();
    }
    /// PowerPC Linux ABI: syscall number in r0, arguments in r3.., result in r3.
    fn system_call(&mut self, cpu: &mut Cpu) -> bool {
        match cpu.gpr[0] {
            1 => self.exited = true, // exit
            4 => {
                // write(fd, buf, len)
                let (buf, len) = (cpu.gpr[4], cpu.gpr[5]);
                let r = self.range(buf, len).expect("write() buffer outside memory");
                self.stdout.extend_from_slice(&self.mem[r]);
                cpu.gpr[3] = len;
            }
            n => panic!("unexpected syscall {n}"),
        }
        cpu.cr &= !0x1000_0000; // CR0[SO]=0: success
        true
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Interpreter,
    /// The JIT, translating every block the first time it runs.
    Jit,
}

fn run_ours(elf: &Path, engine: Engine) -> Vec<u8> {
    let data = std::fs::read(elf).unwrap();
    let file = object::File::parse(&*data).unwrap();
    let mut os = LinuxUser { base: TEXT_BASE, mem: vec![0; 0x20_0000], stdout: Vec::new(), exited: false, code: CodeWatch::new() };
    // Load segments in program-header order (later ones win where they overlap).
    for seg in file.segments() {
        let start = (seg.address() as u32 - TEXT_BASE) as usize;
        let bytes = seg.data().unwrap();
        os.mem[start..start + bytes.len()].copy_from_slice(bytes);
    }
    let mut cpu = Cpu::new();
    cpu.hard_reset(false);
    cpu.msr = msr::PR | msr::ME | msr::EE;
    cpu.pc = file.entry() as u32;

    if engine == Engine::Jit {
        let mut jit = Jit::with_threshold(1);
        let mut steps = 0u64;
        while !os.exited {
            let (n, _) = jit.run_batch(&mut cpu, &mut os, 256, |_| false);
            steps += n;
            // (After exit() the batch runs on into whatever follows: ignore it.)
            assert!(os.exited || cpu.pc >= 0x2000, "the JIT run took exception vector {:#x}; SRR0={:#x} SRR1={:#x}", cpu.pc, cpu.srr0, cpu.srr1);
            assert!(steps < 50_000_000, "runaway execution");
        }
        return os.stdout;
    }
    let mut steps = 0u64;
    while !os.exited {
        let pc = cpu.pc;
        cpu.step(&mut os);
        steps += 1;
        if cpu.pc < 0x2000 {
            let word = os.read32(pc).unwrap_or(0);
            let ins = powerpc::Ins::new(word, powerpc::Extensions::none());
            panic!(
                "our CPU took exception vector {:#x} at {pc:#x} (`{}`, {word:#010x}); SRR1={:#x} DAR={:#x}",
                cpu.pc,
                ins.simplified(),
                cpu.srr1,
                cpu.dar
            );
        }
        assert!(steps < 50_000_000, "runaway execution");
    }
    os.stdout
}

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

fn run_tool(cmd: &mut Command) {
    let out = cmd.output().unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    assert!(out.status.success(), "{cmd:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(tool)).find(|p| p.is_file())
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            let parsed = match v.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok().and_then(|n| n.to_string().parse().ok()),
                None => v.parse().ok(),
            };
            parsed.unwrap_or_else(|| panic!("bad value for {name}: {v}"))
        }
        Err(_) => default,
    }
}
