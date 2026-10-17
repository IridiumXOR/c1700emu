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

pub(super) fn classify(ins: Ins) -> Kind {
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
            // Time: the block's clock only advances when the block ends.
            spr::DEC | spr::TBL_READ | spr::TBU_READ => Kind::Stop,
            _ => Kind::Helper,
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

/// The instructions of the block starting at `pc` (physical address `pa`).
/// It ends with a branch, before a [`Kind::Stop`] instruction, after
/// [`MAX_BLOCK`] instructions, at the next `boundary`-aligned address
/// (a power of two: the page or sub-page the start was translated in), or
/// before an address where the caller has a hook (`stop_before`).
pub(super) fn scan<P: Platform>(sys: &mut P, pc: u32, pa: u32, boundary: u32, stop_before: &mut impl FnMut(u32) -> bool) -> Vec<Insn> {
    let mut block = Vec::new();
    let end = (pc | (boundary - 1)).wrapping_add(1);
    let mut addr = pc;
    while block.len() < MAX_BLOCK && (block.is_empty() || (addr != end && !stop_before(addr))) {
        let Ok(word) = sys.fetch(pa.wrapping_add(addr.wrapping_sub(pc))) else { break };
        let ins = Ins::new(word, Extensions::none());
        let kind = classify(ins);
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
