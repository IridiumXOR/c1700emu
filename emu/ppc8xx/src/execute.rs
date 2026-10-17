//! Instruction semantics: one `match` arm per opcode.
//!
//! Decoding is done by the [`powerpc`] crate: `Ins::new(word, ..)` gives us an
//! [`Opcode`] plus accessors for every instruction field (`field_rd()`,
//! `field_simm()`, `field_bo()`, ...). The crate already handles the awkward
//! encodings (split SPR numbers, sign-extended branch offsets), so this file
//! only has to say *what each instruction does*.
//!
//! Contract with [`Cpu::step`]:
//! * on success, update the architectural state and set `self.pc` to the next
//!   instruction (sequential, or the branch target);
//! * on failure, leave `self.pc` untouched (it still points at the faulting
//!   instruction) and return the [`Exception`] to take.
//!
//! PowerPC notation reminders used in the comments below:
//! * `(rA|0)` means "the value of rA, or the constant 0 if the field is r0".
//!   That's how `li rD, x` is really `addi rD, 0, x`.
//! * Bits are numbered from the MSB: bit 0 is `0x8000_0000`.
//! * XER holds the sticky Summary Overflow (SO), Overflow (OV) and Carry (CA)
//!   flags, plus a byte count used by the string instructions.

use crate::mmu::Access as MemAccess;
use crate::{BusError, Cpu, Exception, Platform, ProgramReason, msr};
use powerpc::{Ins, Opcode};

const XER_SO: u32 = 0x8000_0000;
const XER_OV: u32 = 0x4000_0000;
const XER_CA: u32 = 0x2000_0000;
/// `XER[25-31]`: number of bytes moved by `lswx`/`stswx`.
const XER_BYTE_COUNT: u32 = 0x0000_007F;

/// `tlbia` (TLB invalidate all) is implemented by the MPC8xx but missing from
/// the `powerpc` crate's tables, so we recognise its raw encoding
/// (primary opcode 31, extended opcode 370).
const TLBIA_MASK: u32 = 0xFC00_07FE;
const TLBIA_PATTERN: u32 = 0x7C00_02E4;

/// The MPC860 data cache line is 16 bytes (4 words); `dcbz` clears one line.
const CACHE_LINE: u32 = 16;

/// Width/flavour of a load or store.
#[derive(Clone, Copy)]
enum Access {
    Byte,
    Half,
    /// Load half word *algebraic* = sign-extended (`lha`).
    HalfAlgebraic,
    Word,
    /// Byte-reversed forms (`lhbrx`/`sthbrx`): little-endian access.
    HalfReversed,
    WordReversed,
}

/// Addressing mode of a load or store.
#[derive(Clone, Copy)]
enum Addr {
    /// `d(rA|0)` – D-form, 16-bit signed displacement.
    D,
    /// `d(rA)` and rA is updated with the effective address (`lwzu`...).
    DUpdate,
    /// `(rA|0) + rB` – X-form, indexed.
    X,
    /// `rA + rB`, rA updated (`lwzux`...).
    XUpdate,
}

impl Cpu {
    pub(crate) fn execute<P: Platform>(&mut self, ins: Ins, sys: &mut P) -> Result<(), Exception> {
        use Opcode::*;

        let pc = self.pc;
        // Where execution continues; branches overwrite it.
        let mut next = pc.wrapping_add(4);

        match ins.op {
            // ================================================================
            // Integer arithmetic, immediate forms (D-form)
            // ================================================================
            Addi => {
                // addi rD,(rA|0),SIMM   (also `li` when rA=0)
                let v = self.ra_or_zero(ins).wrapping_add(simm(ins));
                self.set_rd(ins, v);
            }
            Addis => {
                // addis rD,(rA|0),SIMM<<16   (also `lis` when rA=0)
                let v = self.ra_or_zero(ins).wrapping_add(simm(ins) << 16);
                self.set_rd(ins, v);
            }
            Addic | Addic_ => {
                // addic[.] rD,rA,SIMM — like addi but rA is never "0" and CA is set.
                let (v, ca, _) = add3(self.ra(ins), simm(ins), 0);
                self.set_rd(ins, v);
                self.set_ca(ca);
                if ins.op == Addic_ {
                    self.record_cr0(v); // `addic.` always records (bit 0 is part of the opcode)
                }
            }
            Subfic => {
                // subfic rD,rA,SIMM = SIMM - rA = ~rA + SIMM + 1, sets CA.
                let (v, ca, _) = add3(!self.ra(ins), simm(ins), 1);
                self.set_rd(ins, v);
                self.set_ca(ca);
            }
            Mulli => {
                // Low 32 bits of rA * SIMM (signed; low half is sign-agnostic).
                let v = self.ra(ins).wrapping_mul(simm(ins));
                self.set_rd(ins, v);
            }

            // ================================================================
            // Integer arithmetic, register forms (XO-form: OE and Rc variants)
            //
            // Everything is expressed as a + b + carry_in, which is exactly how
            // the hardware adder works: subtraction rB - rA is ~rA + rB + 1.
            // ================================================================
            Add => {
                let (v, _, ov) = add3(self.ra(ins), self.rb(ins), 0);
                self.finish_xo(ins, v, ov);
            }
            Addc => {
                let (v, ca, ov) = add3(self.ra(ins), self.rb(ins), 0);
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Adde => {
                let (v, ca, ov) = add3(self.ra(ins), self.rb(ins), self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Addme => {
                // rA + CA - 1  (adding 0xFFFF_FFFF is adding -1)
                let (v, ca, ov) = add3(self.ra(ins), u32::MAX, self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Addze => {
                let (v, ca, ov) = add3(self.ra(ins), 0, self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Subf => {
                // subf rD,rA,rB = rB - rA  (note the operand order!)
                let (v, _, ov) = add3(!self.ra(ins), self.rb(ins), 1);
                self.finish_xo(ins, v, ov);
            }
            Subfc => {
                let (v, ca, ov) = add3(!self.ra(ins), self.rb(ins), 1);
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Subfe => {
                let (v, ca, ov) = add3(!self.ra(ins), self.rb(ins), self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Subfme => {
                let (v, ca, ov) = add3(!self.ra(ins), u32::MAX, self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Subfze => {
                let (v, ca, ov) = add3(!self.ra(ins), 0, self.ca());
                self.set_ca(ca);
                self.finish_xo(ins, v, ov);
            }
            Neg => {
                // -rA = ~rA + 1. Overflows only for 0x8000_0000.
                let (v, _, ov) = add3(!self.ra(ins), 0, 1);
                self.finish_xo(ins, v, ov);
            }
            Mullw => {
                let product = self.ra(ins) as i32 as i64 * self.rb(ins) as i32 as i64;
                let v = product as u32;
                // OV: the full product does not fit in a signed 32-bit result.
                self.finish_xo(ins, v, product != v as i32 as i64);
            }
            Mulhw => {
                // High 32 bits of the signed 64-bit product (no OE form).
                let product = self.ra(ins) as i32 as i64 * self.rb(ins) as i32 as i64;
                self.finish_xo_no_oe(ins, (product >> 32) as u32);
            }
            Mulhwu => {
                let product = self.ra(ins) as u64 * self.rb(ins) as u64;
                self.finish_xo_no_oe(ins, (product >> 32) as u32);
            }
            Divw => {
                let (a, b) = (self.ra(ins) as i32, self.rb(ins) as i32);
                // Division by zero and 0x8000_0000 / -1 are "undefined" in the
                // architecture: OV is set and rD is garbage. We return 0.
                let (v, ov) = match a.checked_div(b) {
                    Some(q) => (q as u32, false),
                    None => (0, true),
                };
                self.finish_xo(ins, v, ov);
            }
            Divwu => {
                let (a, b) = (self.ra(ins), self.rb(ins));
                let (v, ov) = match a.checked_div(b) {
                    Some(q) => (q, false),
                    None => (0, true),
                };
                self.finish_xo(ins, v, ov);
            }

            // ================================================================
            // Compare: result goes to CR field crfD as LT/GT/EQ + copy of SO.
            // (The L bit selects 64-bit compares; it must be 0 on the 860.)
            // ================================================================
            Cmp => {
                let (a, b) = (self.ra(ins) as i32, self.rb(ins) as i32);
                self.compare(ins.field_crfd(), a < b, a > b);
            }
            Cmpl => {
                let (a, b) = (self.ra(ins), self.rb(ins));
                self.compare(ins.field_crfd(), a < b, a > b);
            }
            Cmpi => {
                let (a, b) = (self.ra(ins) as i32, ins.field_simm() as i32);
                self.compare(ins.field_crfd(), a < b, a > b);
            }
            Cmpli => {
                let (a, b) = (self.ra(ins), ins.field_uimm() as u32);
                self.compare(ins.field_crfd(), a < b, a > b);
            }

            // ================================================================
            // Logical. Note the operand order: rA is the *destination*, rS the
            // source (e.g. `or rA,rS,rB`, `mr rA,rS` = `or rA,rS,rS`).
            // ================================================================
            Andi_ => {
                let v = self.rs(ins) & uimm(ins);
                self.set_ra(ins, v);
                self.record_cr0(v); // andi. always records
            }
            Andis_ => {
                let v = self.rs(ins) & (uimm(ins) << 16);
                self.set_ra(ins, v);
                self.record_cr0(v);
            }
            Ori => {
                let v = self.rs(ins) | uimm(ins); // `ori 0,0,0` is the canonical nop
                self.set_ra(ins, v);
            }
            Oris => {
                let v = self.rs(ins) | (uimm(ins) << 16);
                self.set_ra(ins, v);
            }
            Xori => {
                let v = self.rs(ins) ^ uimm(ins);
                self.set_ra(ins, v);
            }
            Xoris => {
                let v = self.rs(ins) ^ (uimm(ins) << 16);
                self.set_ra(ins, v);
            }
            And => self.finish_logical(ins, self.rs(ins) & self.rb(ins)),
            Andc => self.finish_logical(ins, self.rs(ins) & !self.rb(ins)),
            Or => self.finish_logical(ins, self.rs(ins) | self.rb(ins)),
            Orc => self.finish_logical(ins, self.rs(ins) | !self.rb(ins)),
            Xor => self.finish_logical(ins, self.rs(ins) ^ self.rb(ins)),
            Nand => self.finish_logical(ins, !(self.rs(ins) & self.rb(ins))),
            Nor => self.finish_logical(ins, !(self.rs(ins) | self.rb(ins))),
            Eqv => self.finish_logical(ins, !(self.rs(ins) ^ self.rb(ins))),
            Extsb => self.finish_logical(ins, self.rs(ins) as u8 as i8 as i32 as u32),
            Extsh => self.finish_logical(ins, self.rs(ins) as u16 as i16 as i32 as u32),
            Cntlzw => self.finish_logical(ins, self.rs(ins).leading_zeros()),

            // ================================================================
            // Rotate and mask. MASK(mb, me) has ones from bit mb to bit me
            // (MSB-first numbering); if mb > me the mask wraps around.
            // ================================================================
            Rlwinm => {
                // rotate left by SH, keep the masked bits (the Swiss-army knife:
                // slwi, srwi, clrlwi, clrrwi, extrwi... are all rlwinm).
                let rotated = self.rs(ins).rotate_left(ins.field_sh() as u32);
                self.finish_logical(ins, rotated & mask(ins.field_mb(), ins.field_me()));
            }
            Rlwnm => {
                let rotated = self.rs(ins).rotate_left(self.rb(ins) & 31);
                self.finish_logical(ins, rotated & mask(ins.field_mb(), ins.field_me()));
            }
            Rlwimi => {
                // Insert: masked bits come from the rotated rS, the rest stay from rA.
                let m = mask(ins.field_mb(), ins.field_me());
                let rotated = self.rs(ins).rotate_left(ins.field_sh() as u32);
                let v = (rotated & m) | (self.ra(ins) & !m);
                self.finish_logical(ins, v);
            }

            // ================================================================
            // Shifts. The shift amount is 6 bits (rB[26-31]): amounts 32..63
            // shift everything out.
            // ================================================================
            Slw => {
                let n = self.rb(ins) & 0x3F;
                let v = if n >= 32 { 0 } else { self.rs(ins) << n };
                self.finish_logical(ins, v);
            }
            Srw => {
                let n = self.rb(ins) & 0x3F;
                let v = if n >= 32 { 0 } else { self.rs(ins) >> n };
                self.finish_logical(ins, v);
            }
            Sraw => {
                let n = self.rb(ins) & 0x3F;
                let v = self.shift_right_algebraic(self.rs(ins), n);
                self.finish_logical(ins, v);
            }
            Srawi => {
                let v = self.shift_right_algebraic(self.rs(ins), ins.field_sh() as u32);
                self.finish_logical(ins, v);
            }

            // ================================================================
            // Branches
            // ================================================================
            B => {
                // LI is a sign-extended byte offset; AA=1 means absolute.
                let offset = ins.field_li() as u32;
                next = if ins.field_aa() { offset } else { pc.wrapping_add(offset) };
                if ins.field_lk() {
                    self.lr = pc.wrapping_add(4);
                }
            }
            Bc => {
                let taken = self.branch_condition(ins.field_bo(), ins.field_bi(), true);
                let offset = ins.field_bd() as i32 as u32;
                let target = if ins.field_aa() { offset } else { pc.wrapping_add(offset) };
                if ins.field_lk() {
                    self.lr = pc.wrapping_add(4);
                }
                if taken {
                    next = target;
                }
            }
            Bclr => {
                // Read the target *before* LK overwrites LR (`blrl` must jump
                // to the old LR).
                let taken = self.branch_condition(ins.field_bo(), ins.field_bi(), true);
                let target = self.lr & !3;
                if ins.field_lk() {
                    self.lr = pc.wrapping_add(4);
                }
                if taken {
                    next = target;
                }
            }
            Bcctr => {
                // bcctr cannot decrement CTR (that form is invalid): only the
                // CR condition is evaluated.
                let taken = self.branch_condition(ins.field_bo(), ins.field_bi(), false);
                let target = self.ctr & !3;
                if ins.field_lk() {
                    self.lr = pc.wrapping_add(4);
                }
                if taken {
                    next = target;
                }
            }

            // ================================================================
            // Condition register logical operations (bit granularity)
            // ================================================================
            Crand => self.cr_op(ins, |a, b| a & b),
            Crandc => self.cr_op(ins, |a, b| a & !b),
            Creqv => self.cr_op(ins, |a, b| !(a ^ b)),
            Crnand => self.cr_op(ins, |a, b| !(a & b)),
            Crnor => self.cr_op(ins, |a, b| !(a | b)),
            Cror => self.cr_op(ins, |a, b| a | b),
            Crorc => self.cr_op(ins, |a, b| a | !b),
            Crxor => self.cr_op(ins, |a, b| a ^ b),
            Mcrf => {
                let field = (self.cr >> (28 - 4 * ins.field_crfs() as u32)) & 0xF;
                self.set_cr_field(ins.field_crfd(), field);
            }
            Mcrxr => {
                // Copy XER[SO,OV,CA] into a CR field, then clear them in XER.
                self.set_cr_field(ins.field_crfd(), (self.xer >> 28) & 0xE);
                self.xer &= !(XER_SO | XER_OV | XER_CA);
            }
            Mfcr => self.set_rd(ins, self.cr),
            Mtcrf => {
                // CRM is an 8-bit mask, one bit per CR field (MSB = CR0).
                let crm = ins.field_crm();
                let mut field_mask = 0u32;
                for field in 0..8 {
                    if crm & (0x80 >> field) != 0 {
                        field_mask |= 0xF000_0000 >> (4 * field);
                    }
                }
                self.cr = (self.cr & !field_mask) | (self.rs(ins) & field_mask);
            }

            // ================================================================
            // Loads and stores (byte, half, word; plain/update/indexed)
            // ================================================================
            Lbz => self.load(sys, ins, Access::Byte, Addr::D)?,
            Lbzu => self.load(sys, ins, Access::Byte, Addr::DUpdate)?,
            Lbzx => self.load(sys, ins, Access::Byte, Addr::X)?,
            Lbzux => self.load(sys, ins, Access::Byte, Addr::XUpdate)?,
            Lhz => self.load(sys, ins, Access::Half, Addr::D)?,
            Lhzu => self.load(sys, ins, Access::Half, Addr::DUpdate)?,
            Lhzx => self.load(sys, ins, Access::Half, Addr::X)?,
            Lhzux => self.load(sys, ins, Access::Half, Addr::XUpdate)?,
            Lha => self.load(sys, ins, Access::HalfAlgebraic, Addr::D)?,
            Lhau => self.load(sys, ins, Access::HalfAlgebraic, Addr::DUpdate)?,
            Lhax => self.load(sys, ins, Access::HalfAlgebraic, Addr::X)?,
            Lhaux => self.load(sys, ins, Access::HalfAlgebraic, Addr::XUpdate)?,
            Lwz => self.load(sys, ins, Access::Word, Addr::D)?,
            Lwzu => self.load(sys, ins, Access::Word, Addr::DUpdate)?,
            Lwzx => self.load(sys, ins, Access::Word, Addr::X)?,
            Lwzux => self.load(sys, ins, Access::Word, Addr::XUpdate)?,
            Lhbrx => self.load(sys, ins, Access::HalfReversed, Addr::X)?,
            Lwbrx => self.load(sys, ins, Access::WordReversed, Addr::X)?,

            Stb => self.store(sys, ins, Access::Byte, Addr::D)?,
            Stbu => self.store(sys, ins, Access::Byte, Addr::DUpdate)?,
            Stbx => self.store(sys, ins, Access::Byte, Addr::X)?,
            Stbux => self.store(sys, ins, Access::Byte, Addr::XUpdate)?,
            Sth => self.store(sys, ins, Access::Half, Addr::D)?,
            Sthu => self.store(sys, ins, Access::Half, Addr::DUpdate)?,
            Sthx => self.store(sys, ins, Access::Half, Addr::X)?,
            Sthux => self.store(sys, ins, Access::Half, Addr::XUpdate)?,
            Stw => self.store(sys, ins, Access::Word, Addr::D)?,
            Stwu => self.store(sys, ins, Access::Word, Addr::DUpdate)?,
            Stwx => self.store(sys, ins, Access::Word, Addr::X)?,
            Stwux => self.store(sys, ins, Access::Word, Addr::XUpdate)?,
            Sthbrx => self.store(sys, ins, Access::HalfReversed, Addr::X)?,
            Stwbrx => self.store(sys, ins, Access::WordReversed, Addr::X)?,

            // ---- load/store multiple: registers rD..r31 <-> consecutive words ----
            Lmw => {
                let ea = self.effective_address(ins, Addr::D);
                if ea & 3 != 0 {
                    return Err(alignment(ins, ea));
                }
                // Read everything first so a fault leaves the registers intact.
                let first = ins.field_rd() as usize;
                let mut words = [0u32; 32];
                for (i, w) in words[first..].iter_mut().enumerate() {
                    *w = self.load32(sys, ea.wrapping_add(4 * i as u32))?;
                }
                self.gpr[first..].copy_from_slice(&words[first..]);
            }
            Stmw => {
                let ea = self.effective_address(ins, Addr::D);
                if ea & 3 != 0 {
                    return Err(alignment(ins, ea));
                }
                let first = ins.field_rs() as usize;
                for (i, r) in (first..32).enumerate() {
                    self.store32(sys, ea.wrapping_add(4 * i as u32), self.gpr[r])?;
                }
            }

            // ---- string instructions: N bytes packed big-endian into registers ----
            Lswi => {
                // NB=0 means 32 bytes. Address is (rA|0), no displacement.
                let count = if ins.field_nb() == 0 { 32 } else { ins.field_nb() as u32 };
                let ea = self.ra_or_zero(ins);
                self.load_string(sys, ea, ins.field_rd(), count)?;
            }
            Lswx => {
                let count = self.xer & XER_BYTE_COUNT;
                let ea = self.effective_address(ins, Addr::X);
                self.load_string(sys, ea, ins.field_rd(), count)?;
            }
            Stswi => {
                let count = if ins.field_nb() == 0 { 32 } else { ins.field_nb() as u32 };
                let ea = self.ra_or_zero(ins);
                self.store_string(sys, ea, ins.field_rs(), count)?;
            }
            Stswx => {
                let count = self.xer & XER_BYTE_COUNT;
                let ea = self.effective_address(ins, Addr::X);
                self.store_string(sys, ea, ins.field_rs(), count)?;
            }

            // ---- atomics: load-reserve / store-conditional ----
            Lwarx => {
                let ea = self.effective_address(ins, Addr::X);
                if ea & 3 != 0 {
                    return Err(alignment(ins, ea));
                }
                let v = self.load32(sys, ea)?;
                self.reservation = Some(ea);
                self.set_rd(ins, v);
            }
            Stwcx_ => {
                // Store only if we still hold a reservation for this address;
                // CR0[EQ] tells software whether the store happened.
                let ea = self.effective_address(ins, Addr::X);
                if ea & 3 != 0 {
                    return Err(alignment(ins, ea));
                }
                let success = self.reservation == Some(ea);
                if success {
                    self.store32(sys, ea, self.rs(ins))?;
                }
                self.reservation = None;
                let so = (self.xer >> 31) & 1;
                self.set_cr_field(0, if success { 0b0010 } else { 0 } | so);
            }

            // ================================================================
            // Cache and synchronisation. Without a cache model most of these
            // are no-ops; dcbz is the exception because it writes memory.
            // ================================================================
            Sync | Isync | Eieio | Dcbt | Dcbtst => {}
            // No cache model, but these still translate their address: with
            // the MMU on they can take DTLB misses/errors (MPC860UM §8.9).
            Icbi | Dcbst | Dcbf => {
                let ea = self.effective_address(ins, Addr::X);
                self.data_address(ea, MemAccess::Load)?;
            }
            Dcbz => {
                let line = self.effective_address(ins, Addr::X) & !(CACHE_LINE - 1);
                for offset in (0..CACHE_LINE).step_by(4) {
                    self.store32(sys, line + offset, 0)?;
                }
            }
            Dcbi => {
                self.require_supervisor()?;
                let ea = self.effective_address(ins, Addr::X);
                self.data_address(ea, MemAccess::Store)?;
            }
            Tlbie => {
                self.require_supervisor()?;
                self.mmu.tlbie(self.rb(ins));
            }
            Tlbsync => self.require_supervisor()?, // a no-op on the 8xx

            // ================================================================
            // System / supervisor
            // ================================================================
            Sc => {
                // A test harness may emulate the syscall itself; on the real
                // board `sc` always enters the System Call exception (0xC00).
                if !sys.system_call(self) {
                    return Err(Exception::SystemCall);
                }
            }
            Rfi => {
                self.require_supervisor()?;
                self.return_from_interrupt();
                return Ok(()); // pc was set from SRR0
            }
            Mfmsr => {
                self.require_supervisor()?;
                self.set_rd(ins, self.msr);
            }
            Mtmsr => {
                self.require_supervisor()?;
                self.set_msr(self.rs(ins));
            }
            Mfspr => {
                let v = self.read_spr(sys, ins.field_spr())?;
                self.set_rd(ins, v);
            }
            Mtspr => self.write_spr(sys, ins.field_spr(), self.rs(ins))?,
            Mftb => {
                let tbr = ins.field_tbr();
                if tbr != crate::spr::TBL_READ && tbr != crate::spr::TBU_READ {
                    return Err(Exception::SoftwareEmulation);
                }
                let v = self.read_spr(sys, tbr)?;
                self.set_rd(ins, v);
            }
            Tw => {
                if trap_condition(ins.field_to(), self.ra(ins), self.rb(ins)) {
                    return Err(Exception::Program(ProgramReason::Trap));
                }
            }
            Twi => {
                if trap_condition(ins.field_to(), self.ra(ins), simm(ins)) {
                    return Err(Exception::Program(ProgramReason::Trap));
                }
            }

            Opcode::Illegal if ins.code & TLBIA_MASK == TLBIA_PATTERN => {
                self.require_supervisor()?;
                self.mmu.tlbia();
            }

            // ================================================================
            // Everything else: floating point (no FPU on the 860), segment
            // registers (the 8xx MMU has none), 64-bit, AltiVec, paired
            // singles, eciwx/ecowx and truly illegal words. The 8xx reports
            // all of them through the Software Emulation exception (0x1000).
            // ================================================================
            _ => return Err(Exception::SoftwareEmulation),
        }

        self.pc = next;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Register access helpers
    // ------------------------------------------------------------------

    #[inline]
    fn ra(&self, ins: Ins) -> u32 {
        self.gpr[ins.field_ra() as usize]
    }
    #[inline]
    fn rb(&self, ins: Ins) -> u32 {
        self.gpr[ins.field_rb() as usize]
    }
    #[inline]
    fn rs(&self, ins: Ins) -> u32 {
        self.gpr[ins.field_rs() as usize]
    }
    /// `(rA|0)`: r0 in the rA position reads as zero for address/immediate forms.
    #[inline]
    fn ra_or_zero(&self, ins: Ins) -> u32 {
        match ins.field_ra() {
            0 => 0,
            r => self.gpr[r as usize],
        }
    }
    #[inline]
    fn set_rd(&mut self, ins: Ins, v: u32) {
        self.gpr[ins.field_rd() as usize] = v;
    }
    #[inline]
    fn set_ra(&mut self, ins: Ins, v: u32) {
        self.gpr[ins.field_ra() as usize] = v;
    }

    // ------------------------------------------------------------------
    // XER / CR helpers
    // ------------------------------------------------------------------

    /// `XER[CA]` as 0 or 1, ready to be used as a carry-in.
    #[inline]
    fn ca(&self) -> u32 {
        (self.xer >> 29) & 1
    }
    #[inline]
    fn set_ca(&mut self, carry: bool) {
        if carry {
            self.xer |= XER_CA;
        } else {
            self.xer &= !XER_CA;
        }
    }
    /// OV reflects the last OE-instruction; SO is *sticky*: once set it stays
    /// set until software clears it (mtxer / mcrxr).
    #[inline]
    fn set_ov(&mut self, overflow: bool) {
        if overflow {
            self.xer |= XER_OV | XER_SO;
        } else {
            self.xer &= !XER_OV;
        }
    }

    /// Common tail of XO-form arithmetic: write rD, update OV/SO if OE=1,
    /// then CR0 if Rc=1 (in that order, so `CR0[SO]` sees the new SO).
    #[inline]
    fn finish_xo(&mut self, ins: Ins, v: u32, overflow: bool) {
        self.set_rd(ins, v);
        if ins.field_oe() {
            self.set_ov(overflow);
        }
        if ins.field_rc() {
            self.record_cr0(v);
        }
    }
    /// XO-form instructions without an OE bit (mulhw, mulhwu).
    #[inline]
    fn finish_xo_no_oe(&mut self, ins: Ins, v: u32) {
        self.set_rd(ins, v);
        if ins.field_rc() {
            self.record_cr0(v);
        }
    }
    /// Common tail of X-form logical/rotate/shift ops: write rA, CR0 if Rc=1.
    #[inline]
    fn finish_logical(&mut self, ins: Ins, v: u32) {
        self.set_ra(ins, v);
        if ins.field_rc() {
            self.record_cr0(v);
        }
    }

    /// Write a compare result (LT, GT, EQ, SO) into CR field `crf`.
    #[inline]
    fn compare(&mut self, crf: u8, less: bool, greater: bool) {
        let bits = if less {
            0b1000
        } else if greater {
            0b0100
        } else {
            0b0010
        };
        self.set_cr_field(crf, bits | (self.xer >> 31));
    }

    /// CR bit `n`, MSB-first (bit 0 = `CR0[LT]`).
    #[inline]
    fn cr_bit(&self, n: u8) -> bool {
        (self.cr >> (31 - n as u32)) & 1 != 0
    }

    /// `crXXX crbD,crbA,crbB`: crbD = op(crbA, crbB).
    #[inline]
    fn cr_op(&mut self, ins: Ins, op: impl Fn(bool, bool) -> bool) {
        let value = op(self.cr_bit(ins.field_crba()), self.cr_bit(ins.field_crbb()));
        let bit = 0x8000_0000u32 >> ins.field_crbd();
        if value {
            self.cr |= bit;
        } else {
            self.cr &= !bit;
        }
    }

    /// Evaluate the BO/BI fields of a conditional branch.
    ///
    /// BO is five bits, MSB first:
    /// * `0x10` – ignore the CR condition,
    /// * `0x08` – the value the CR bit BI must have,
    /// * `0x04` – do NOT decrement CTR,
    /// * `0x02` – when decrementing: branch if CTR == 0 (else CTR != 0),
    /// * `0x01` – static prediction hint (no architectural effect).
    ///
    /// e.g. `bdnz` is BO=0x10 (decrement, branch if CTR != 0, ignore CR),
    /// `beq` is BO=0x0C with BI = 4*crf + 2.
    fn branch_condition(&mut self, bo: u8, bi: u8, can_decrement_ctr: bool) -> bool {
        let ctr_ok = if bo & 0x04 != 0 || !can_decrement_ctr {
            true
        } else {
            self.ctr = self.ctr.wrapping_sub(1);
            (self.ctr != 0) != (bo & 0x02 != 0)
        };
        let cond_ok = bo & 0x10 != 0 || self.cr_bit(bi) == (bo & 0x08 != 0);
        ctr_ok && cond_ok
    }

    /// Arithmetic shift right, setting CA as the architecture requires:
    /// CA = 1 only if the source is negative AND at least one 1-bit was
    /// shifted out. That makes `srawi rD,rS,n ; addze rD,rD` a correct
    /// signed division by 2^n rounding toward zero.
    fn shift_right_algebraic(&mut self, value: u32, amount: u32) -> u32 {
        let negative = (value as i32) < 0;
        if amount >= 32 {
            self.set_ca(negative);
            return if negative { u32::MAX } else { 0 };
        }
        let shifted_out = value & ((1u32 << amount) - 1);
        self.set_ca(negative && shifted_out != 0);
        ((value as i32) >> amount) as u32
    }

    fn require_supervisor(&self) -> Result<(), Exception> {
        if self.msr & msr::PR != 0 {
            Err(Exception::Program(ProgramReason::Privileged))
        } else {
            Ok(())
        }
    }

    // ------------------------------------------------------------------
    // Memory helpers
    // ------------------------------------------------------------------

    /// Effective address for the four addressing modes. Update forms always
    /// use rA itself (rA=0 is an invalid form; we just read r0).
    #[inline]
    fn effective_address(&self, ins: Ins, mode: Addr) -> u32 {
        match mode {
            Addr::D => self.ra_or_zero(ins).wrapping_add(simm(ins)),
            Addr::DUpdate => self.ra(ins).wrapping_add(simm(ins)),
            Addr::X => self.ra_or_zero(ins).wrapping_add(self.rb(ins)),
            Addr::XUpdate => self.ra(ins).wrapping_add(self.rb(ins)),
        }
    }

    fn load<P: Platform>(&mut self, sys: &mut P, ins: Ins, access: Access, mode: Addr) -> Result<(), Exception> {
        let ea = self.effective_address(ins, mode);
        let value = match access {
            Access::Byte => self.load8(sys, ea)? as u32,
            Access::Half => self.load16(sys, ea)? as u32,
            Access::HalfAlgebraic => self.load16(sys, ea)? as i16 as i32 as u32,
            Access::Word => self.load32(sys, ea)?,
            Access::HalfReversed => self.load16(sys, ea)?.swap_bytes() as u32,
            Access::WordReversed => self.load32(sys, ea)?.swap_bytes(),
        };
        if matches!(mode, Addr::DUpdate | Addr::XUpdate) {
            self.set_ra(ins, ea);
        }
        self.set_rd(ins, value); // written last: wins if rD == rA (invalid form)
        Ok(())
    }

    fn store<P: Platform>(&mut self, sys: &mut P, ins: Ins, access: Access, mode: Addr) -> Result<(), Exception> {
        let ea = self.effective_address(ins, mode);
        let value = self.rs(ins);
        match access {
            Access::Byte => self.store8(sys, ea, value as u8)?,
            Access::Half | Access::HalfAlgebraic => self.store16(sys, ea, value as u16)?,
            Access::Word => self.store32(sys, ea, value)?,
            Access::HalfReversed => self.store16(sys, ea, (value as u16).swap_bytes())?,
            Access::WordReversed => self.store32(sys, ea, value.swap_bytes())?,
        }
        if matches!(mode, Addr::DUpdate | Addr::XUpdate) {
            self.set_ra(ins, ea);
        }
        Ok(())
    }

    /// `lswi`/`lswx`: bytes fill registers MSB first, wrapping r31 -> r0;
    /// the unused low bytes of the last register are zeroed.
    fn load_string<P: Platform>(&mut self, sys: &mut P, ea: u32, first_reg: u8, count: u32) -> Result<(), Exception> {
        // Read all bytes first (at most 127) so a fault leaves registers intact.
        let mut bytes = [0u8; 128];
        for i in 0..count {
            bytes[i as usize] = self.load8(sys, ea.wrapping_add(i))?;
        }
        let mut reg = first_reg as usize;
        for (i, chunk) in bytes[..count as usize].chunks(4).enumerate() {
            if i > 0 {
                reg = (reg + 1) % 32;
            }
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.gpr[reg] = u32::from_be_bytes(word);
        }
        Ok(())
    }

    /// `stswi`/`stswx`: the inverse of [`Cpu::load_string`].
    fn store_string<P: Platform>(&mut self, sys: &mut P, ea: u32, first_reg: u8, count: u32) -> Result<(), Exception> {
        for i in 0..count {
            let reg = (first_reg as usize + (i / 4) as usize) % 32;
            let byte = self.gpr[reg].to_be_bytes()[(i % 4) as usize];
            self.store8(sys, ea.wrapping_add(i), byte)?;
        }
        Ok(())
    }

    // The MPC860 performs misaligned half/word accesses in hardware (as
    // several bus cycles), so we split them into byte accesses. Aligned
    // accesses go to the platform as a single sized access, which matters for
    // memory-mapped registers.
    //
    // With MSR[DR]=1 every access is translated first (`data_address`); an
    // aligned half/word never crosses a page, a misaligned one is split into
    // bytes that are translated one by one.

    pub(crate) fn load8<P: Platform>(&mut self, sys: &mut P, ea: u32) -> Result<u8, Exception> {
        let pa = self.data_address(ea, MemAccess::Load)?;
        sys.read8(pa).map_err(|BusError| machine_check(ea))
    }
    pub(crate) fn load16<P: Platform>(&mut self, sys: &mut P, ea: u32) -> Result<u16, Exception> {
        if ea & 1 == 0 {
            let pa = self.data_address(ea, MemAccess::Load)?;
            return sys.read16(pa).map_err(|BusError| machine_check(ea));
        }
        let hi = self.load8(sys, ea)?;
        let lo = self.load8(sys, ea.wrapping_add(1))?;
        Ok(u16::from_be_bytes([hi, lo]))
    }
    pub(crate) fn load32<P: Platform>(&mut self, sys: &mut P, ea: u32) -> Result<u32, Exception> {
        if ea & 3 == 0 {
            let pa = self.data_address(ea, MemAccess::Load)?;
            return sys.read32(pa).map_err(|BusError| machine_check(ea));
        }
        let mut b = [0u8; 4];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = self.load8(sys, ea.wrapping_add(i as u32))?;
        }
        Ok(u32::from_be_bytes(b))
    }
    pub(crate) fn store8<P: Platform>(&mut self, sys: &mut P, ea: u32, v: u8) -> Result<(), Exception> {
        let pa = self.data_address(ea, MemAccess::Store)?;
        sys.write8(pa, v).map_err(|BusError| machine_check(ea))
    }
    pub(crate) fn store16<P: Platform>(&mut self, sys: &mut P, ea: u32, v: u16) -> Result<(), Exception> {
        if ea & 1 == 0 {
            let pa = self.data_address(ea, MemAccess::Store)?;
            return sys.write16(pa, v).map_err(|BusError| machine_check(ea));
        }
        let [hi, lo] = v.to_be_bytes();
        self.store8(sys, ea, hi)?;
        self.store8(sys, ea.wrapping_add(1), lo)
    }
    pub(crate) fn store32<P: Platform>(&mut self, sys: &mut P, ea: u32, v: u32) -> Result<(), Exception> {
        if ea & 3 == 0 {
            let pa = self.data_address(ea, MemAccess::Store)?;
            return sys.write32(pa, v).map_err(|BusError| machine_check(ea));
        }
        for (i, byte) in v.to_be_bytes().into_iter().enumerate() {
            self.store8(sys, ea.wrapping_add(i as u32), byte)?;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Free helper functions
// ----------------------------------------------------------------------

/// Sign-extended 16-bit immediate, as the 32-bit pattern the ALU sees.
#[inline]
fn simm(ins: Ins) -> u32 {
    ins.field_simm() as i32 as u32
}

/// Zero-extended 16-bit immediate (logical ops: andi., ori, xori...).
#[inline]
fn uimm(ins: Ins) -> u32 {
    ins.field_uimm() as u32
}

/// `a + b + carry_in` with the carry out of bit 0 and signed overflow.
///
/// Signed overflow happens when both operands have the same sign and the
/// result has the other one; `(a ^ r) & (b ^ r)` has its MSB set exactly then.
#[inline]
fn add3(a: u32, b: u32, carry_in: u32) -> (u32, bool, bool) {
    let wide = a as u64 + b as u64 + carry_in as u64;
    let result = wide as u32;
    let carry = wide > u32::MAX as u64;
    let overflow = ((a ^ result) & (b ^ result)) & 0x8000_0000 != 0;
    (result, carry, overflow)
}

/// The rotate mask MASK(mb, me): ones from bit `mb` through bit `me`
/// (bit 0 = MSB). When `mb > me` the run of ones wraps around, e.g.
/// MASK(30, 1) = 0xC000_0003.
#[inline]
pub(crate) fn mask(mb: u8, me: u8) -> u32 {
    let from_mb = u32::MAX >> mb; // bits mb..31
    let to_me = u32::MAX << (31 - me); // bits 0..me
    if mb <= me { from_mb & to_me } else { from_mb | to_me }
}

/// TO field of `tw`/`twi`: trap if any selected comparison is true.
/// 0x10 = signed less, 0x08 = signed greater, 0x04 = equal,
/// 0x02 = unsigned less, 0x01 = unsigned greater.
fn trap_condition(to: u8, a: u32, b: u32) -> bool {
    let (sa, sb) = (a as i32, b as i32);
    (to & 0x10 != 0 && sa < sb)
        || (to & 0x08 != 0 && sa > sb)
        || (to & 0x04 != 0 && a == b)
        || (to & 0x02 != 0 && a < b)
        || (to & 0x01 != 0 && a > b)
}

/// A data access nobody answered: Machine Check with DAR = effective address.
#[inline]
fn machine_check(ea: u32) -> Exception {
    Exception::MachineCheck { addr: Some(ea) }
}

/// Build an Alignment exception. DSISR describes the faulting instruction
/// (PowerPC PEM §6.4.6): bits 15-21 identify the opcode, 22-26 rD/rS,
/// 27-31 rA. Bits are MSB-first, so DSISR bit n is value bit 31-n.
fn alignment(ins: Ins, ea: u32) -> Exception {
    let code = ins.code;
    let regs = (code >> 16) & 0x3FF; // rD/rS and rA, already in the right place
    let opcode_bits = if code >> 26 == 31 {
        // X-form: DSISR[15-16] = insn[29-30], [17] = insn[25], [18-21] = insn[21-24]
        (((code >> 1) & 0x3) << 15) | (((code >> 6) & 0x1) << 14) | (((code >> 7) & 0xF) << 10)
    } else {
        // D-form: DSISR[15-16] = 0, [17] = insn[5], [18-21] = insn[1-4]
        (((code >> 26) & 0x1) << 14) | (((code >> 27) & 0xF) << 10)
    };
    Exception::Alignment { addr: ea, dsisr: opcode_bits | regs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_examples() {
        assert_eq!(mask(0, 31), 0xFFFF_FFFF);
        assert_eq!(mask(16, 31), 0x0000_FFFF);
        assert_eq!(mask(0, 15), 0xFFFF_0000);
        assert_eq!(mask(31, 31), 0x0000_0001);
        assert_eq!(mask(30, 1), 0xC000_0003); // wrap-around
        assert_eq!(mask(5, 5), 0x0400_0000);
    }

    #[test]
    fn add3_flags() {
        assert_eq!(add3(0xFFFF_FFFF, 1, 0), (0, true, false));
        assert_eq!(add3(0x7FFF_FFFF, 1, 0), (0x8000_0000, false, true));
        assert_eq!(add3(0x7FFF_FFFF, 0, 1), (0x8000_0000, false, true));
        assert_eq!(add3(0x8000_0000, 0x8000_0000, 0), (0, true, true));
        // subtract 5 - 3 = ~3 + 5 + 1, carry means "no borrow"
        assert_eq!(add3(!3, 5, 1), (2, true, false));
        assert_eq!(add3(!5, 3, 1), (0xFFFF_FFFE, false, false));
    }

    #[test]
    fn trap_conditions() {
        assert!(trap_condition(0x1F, 1, 1));
        assert!(!trap_condition(0x00, 1, 2));
        assert!(trap_condition(0x10, 0xFFFF_FFFF, 0)); // -1 < 0 signed
        assert!(!trap_condition(0x02, 0xFFFF_FFFF, 0)); // but not unsigned
    }
}
