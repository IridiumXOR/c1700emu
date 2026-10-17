//! Finding a block: which instructions starting at an address get translated
//! together, and how each one is handled.

use crate::{Platform, spr};
use powerpc::{Extensions, Ins, Opcode};

/// Longest block, in instructions. Bounds compile time and how long a block
/// can delay an interrupt.
pub(super) const MAX_BLOCK: usize = 64;

/// How the translator handles an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// Translated to host code.
    Native,
    /// Translated to a call of the interpreter's own code for this one
    /// instruction (rare instructions that touch nothing the block assumes
    /// constant: control flow, MSR, time, the MMU).
    Helper,
    /// A branch: translated, and it ends the block.
    Branch,
    /// Not translated: the block ends *before* it and the dispatcher lets
    /// the interpreter run it.
    Stop,
}

#[derive(Clone, Copy)]
pub(super) struct Insn {
    pub word: u32,
    pub ins: Ins,
    pub kind: Kind,
}

/// `user`: the block runs with `MSR[PR]=1` (part of its key).
pub(super) fn classify(ins: Ins, user: bool) -> Kind {
    use Opcode::*;
    match ins.op {
        B | Bc | Bclr | Bcctr => Kind::Branch,

        Addi | Addis | Addic | Addic_ | Subfic | Mulli | Mulhw | Mulhwu => Kind::Native,
        // With OE=1 they also compute overflow: rare, left to the interpreter.
        Add | Addc | Adde | Addme | Addze | Subf | Subfc | Subfe | Subfme | Subfze | Neg | Mullw | Divw | Divwu => {
            if ins.field_oe() { Kind::Helper } else { Kind::Native }
        }
        Cmp | Cmpl | Cmpi | Cmpli => Kind::Native,
        Andi_ | Andis_ | Ori | Oris | Xori | Xoris | And | Andc | Or | Orc | Xor | Nand | Nor | Eqv => Kind::Native,
        Extsb | Extsh | Cntlzw | Rlwinm | Rlwnm | Rlwimi | Slw | Srw | Sraw | Srawi => Kind::Native,
        Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor | Mcrf | Mfcr | Mtcrf => Kind::Native,

        Lbz | Lbzu | Lbzx | Lbzux | Lhz | Lhzu | Lhzx | Lhzux | Lha | Lhau | Lhax | Lhaux | Lwz | Lwzu | Lwzx | Lwzux
        | Lhbrx | Lwbrx => Kind::Native,
        Stb | Stbu | Stbx | Stbux | Sth | Sthu | Sthx | Sthux | Stw | Stwu | Stwx | Stwux | Sthbrx | Stwbrx => Kind::Native,

        // No cache model: ordering and cache hints do nothing.
        Sync | Isync | Eieio | Dcbt | Dcbtst => Kind::Native,

        Mfspr => match ins.field_spr() {
            spr::LR | spr::CTR | spr::XER => Kind::Native,
            // Time: computed from the clock at the block start plus the
            // instructions before this one (see codegen `time_now`).
            spr::TBL_READ | spr::TBU_READ => Kind::Native,
            // DEC is supervisor-only: from user mode it raises an exception.
            spr::DEC => if user { Kind::Stop } else { Kind::Native },
            _ => Kind::Helper,
        },
        Mftb => match ins.field_tbr() {
            spr::TBL_READ | spr::TBU_READ => Kind::Native,
            _ => Kind::Stop, // invalid TBR: Software Emulation exception
        },
        Mtspr => match ins.field_spr() {
            spr::LR | spr::CTR | spr::XER => Kind::Native,
            spr::SRR0 | spr::SRR1 | spr::DAR | spr::DSISR | spr::IC_CST | spr::DC_CST => Kind::Helper,
            spr::SPRG0..=spr::SPRG3 => Kind::Helper,
            // EIE/EID/NRI change MSR, DEC/TB time, the MMU registers
            // translations, IMMR the memory map: the interpreter does them.
            _ => Kind::Stop,
        },

        Mcrxr | Lmw | Stmw | Lswi | Lswx | Stswi | Stswx | Lwarx | Stwcx_ => Kind::Helper,
        Icbi | Dcbst | Dcbf | Dcbz | Dcbi | Tw | Twi | Mfmsr | Tlbsync => Kind::Helper,

        // sc, rfi, mtmsr, mftb, tlbie, tlbia, floating point, illegal words...
        _ => Kind::Stop,
    }
}

/// One straight-line run of a region.
pub(super) struct Block {
    pub start: u32,
    pub insns: Vec<Insn>,
}

/// Most guest instructions translated together in one region.
pub(super) const MAX_REGION: usize = 256;

/// A region: the block starting at `pc` (physical address `pa`) and the
/// blocks reachable from it by direct branches and fall-through, all inside
/// the same `boundary`-aligned piece of memory (a power of two: the page or
/// 1 KB sub-page the start was translated in), at most [`MAX_REGION`]
/// instructions. `blocks[0]` starts at `pc`. Translated as one function,
/// a loop inside the region becomes a jump inside the host code.
///
/// No block starts at an address where the caller has a hook
/// (`stop_before`), except the region's own start, which the dispatcher has
/// already checked; branches to such addresses leave the region.
pub(super) fn scan_region<P: Platform>(
    sys: &mut P,
    pc: u32,
    pa: u32,
    boundary: u32,
    user: bool,
    stop_before: &mut impl FnMut(u32) -> bool,
) -> Vec<Block> {
    let base = pc & !(boundary - 1);
    let inside = |a: u32| a.wrapping_sub(base) < boundary;
    let mut blocks: Vec<Block> = Vec::new();
    let mut queue = std::collections::VecDeque::from([pc]);
    let mut total = 0;
    while let Some(start) = queue.pop_front() {
        if total >= MAX_REGION || blocks.iter().any(|b| b.start == start) || (start != pc && stop_before(start)) {
            continue;
        }
        let insns = scan(sys, start, pa.wrapping_add(start.wrapping_sub(pc)), boundary, user, stop_before);
        let Some(last) = insns.last() else { continue };
        let next = start.wrapping_add(4 * insns.len() as u32);
        let mut successors = Vec::new();
        if last.kind == Kind::Branch {
            let (target, can_fall_through) = branch_targets(last.ins, next.wrapping_sub(4));
            successors.extend(target);
            if can_fall_through {
                successors.push(next);
            }
        } else {
            successors.push(next); // ended before a Stop/hook or at MAX_BLOCK
        }
        total += insns.len();
        blocks.push(Block { start, insns });
        queue.extend(successors.into_iter().filter(|&a| inside(a)));
    }
    blocks
}

/// The static target of a branch at `pc` (`None` for `bclr`/`bcctr`), and
/// whether it can fall through (a condition or CTR test is involved).
fn branch_targets(ins: Ins, pc: u32) -> (Option<u32>, bool) {
    let conditional = |bo: u8| bo & 0x14 != 0x14; // not "branch always"
    match ins.op {
        Opcode::B => {
            let offset = ins.field_li() as u32;
            (Some(if ins.field_aa() { offset } else { pc.wrapping_add(offset) }), false)
        }
        Opcode::Bc => {
            let offset = ins.field_bd() as i32 as u32;
            let target = if ins.field_aa() { offset } else { pc.wrapping_add(offset) };
            (Some(target), conditional(ins.field_bo()))
        }
        // bcctr never decrements CTR: only the CR condition matters.
        Opcode::Bcctr => (None, ins.field_bo() & 0x10 == 0),
        _ => (None, conditional(ins.field_bo())),
    }
}

/// The instructions of the block starting at `pc` (physical address `pa`).
/// It ends with a branch, before a [`Kind::Stop`] instruction, after
/// [`MAX_BLOCK`] instructions, at the next `boundary`-aligned address
/// (a power of two: the page or sub-page the start was translated in), or
/// before an address where the caller has a hook (`stop_before`).
pub(super) fn scan<P: Platform>(
    sys: &mut P,
    pc: u32,
    pa: u32,
    boundary: u32,
    user: bool,
    stop_before: &mut impl FnMut(u32) -> bool,
) -> Vec<Insn> {
    let mut block = Vec::new();
    let end = (pc | (boundary - 1)).wrapping_add(1);
    let mut addr = pc;
    while block.len() < MAX_BLOCK && (block.is_empty() || (addr != end && !stop_before(addr))) {
        let Ok(word) = sys.fetch(pa.wrapping_add(addr.wrapping_sub(pc))) else { break };
        let ins = Ins::new(word, Extensions::none());
        let kind = classify(ins, user);
        if kind == Kind::Stop {
            break;
        }
        block.push(Insn { word, ins, kind });
        if kind == Kind::Branch {
            break;
        }
        addr = addr.wrapping_add(4);
    }
    block
}
