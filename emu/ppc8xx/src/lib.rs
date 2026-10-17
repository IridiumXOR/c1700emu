//! The PowerPC core of the MPC860 (Part II of the MPC860 User's Manual).
//!
//! The MPC860 core is a 32-bit, big-endian PowerPC implementation with a few
//! peculiarities that matter for emulation:
//!
//! * **No FPU.** Every floating-point instruction raises the *Software
//!   Emulation* exception (vector 0x1000), as do unimplemented opcodes
//!   (MPC860UM §6.1.3.1). There is no "illegal instruction" Program exception
//!   for those, unlike desktop PowerPCs.
//! * **8xx-only SPRs**: `EIE`/`EID`/`NRI` (writing them toggles `MSR[EE]`/`MSR[RI]`,
//!   a cheap way to enable/disable interrupts), cache control (`IC_CST`/
//!   `DC_CST`), the MMU registers, debug registers and `IMMR`, which relocates
//!   the on-chip peripherals. See [`Cpu::read_spr`] / [`Cpu::write_spr`].
//! * The **timebase and decrementer** run from a clock derived from the system
//!   clock (TB enable lives in the SIU's TBSCR register, outside the core).
//!
//! Instruction decoding is done by the [`powerpc`] crate; this module owns the
//! architectural state, the exception model and the fetch/execute loop.
//! The per-instruction semantics live in the private `execute` module.

mod decode_cache;
mod execute;
pub mod mmu;

use decode_cache::DecodeCache;
use mmu::{Access, Fault, Mmu};

/// The bus answered "nobody is here" (no chip select matched, or a device
/// refused the access). On the MPC860 this is a TEA (transfer error
/// acknowledge) and the core takes a Machine Check exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusError;

/// Everything outside the core: memory, memory-mapped devices, the IMMR SPR
/// and the external interrupt line coming from the SIU interrupt controller.
///
/// The CPU is generic over this trait, so it can run against the real Cisco
/// board model or against a tiny test harness (see `tests/`).
pub trait Platform {
    fn read8(&mut self, addr: u32) -> Result<u8, BusError>;
    fn read16(&mut self, addr: u32) -> Result<u16, BusError>;
    fn read32(&mut self, addr: u32) -> Result<u32, BusError>;
    fn write8(&mut self, addr: u32, value: u8) -> Result<(), BusError>;
    fn write16(&mut self, addr: u32, value: u16) -> Result<(), BusError>;
    fn write32(&mut self, addr: u32, value: u32) -> Result<(), BusError>;

    /// Instruction fetch. Defaults to a normal 32-bit read.
    fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
        self.read32(addr)
    }

    /// SPRs that are implemented outside the core. On the MPC860 that is only
    /// `IMMR` (SPR 638), because its value decides where the SIU decodes the
    /// on-chip peripherals. Return `None` to let the core handle the SPR.
    fn read_spr(&mut self, _spr: u16) -> Option<u32> {
        None
    }
    /// See [`Platform::read_spr`]. Return `true` if the write was consumed.
    fn write_spr(&mut self, _spr: u16, _value: u32) -> bool {
        false
    }

    /// Level of the core's external interrupt input (driven by the SIU
    /// interrupt controller: `SIPEND & SIMASK != 0`).
    fn external_interrupt(&self) -> bool {
        false
    }

    /// The `sc` instruction. The real hardware simply takes the System Call
    /// exception; returning `true` here means "the platform handled it"
    /// (used by the qemu differential-test harness to emulate Linux syscalls).
    fn system_call(&mut self, _cpu: &mut Cpu) -> bool {
        false
    }
}

/// Machine State Register bits (MPC860UM Table 4-?/§4.1.2.1, PEM ch. 2).
/// PowerPC numbers bits from the MSB (bit 0 = 0x8000_0000).
pub mod msr {
    /// Power management enable (bit 13). Ignored by the emulator.
    pub const POW: u32 = 0x0004_0000;
    /// Exception little-endian mode (bit 15): copied into LE on exception.
    pub const ILE: u32 = 0x0001_0000;
    /// External interrupt enable (bit 16).
    pub const EE: u32 = 0x0000_8000;
    /// Problem state (user mode) when set (bit 17).
    pub const PR: u32 = 0x0000_4000;
    /// Floating-point available (bit 18). The 860 has no FPU anyway.
    pub const FP: u32 = 0x0000_2000;
    /// Machine check enable (bit 19). A machine check with ME=0 = checkstop.
    pub const ME: u32 = 0x0000_1000;
    pub const FE0: u32 = 0x0000_0800;
    /// Single-step trace enable (bit 21).
    pub const SE: u32 = 0x0000_0400;
    /// Branch trace enable (bit 22).
    pub const BE: u32 = 0x0000_0200;
    pub const FE1: u32 = 0x0000_0100;
    /// Exception prefix (bit 25): vectors at 0xFFF0_0000 instead of 0.
    pub const IP: u32 = 0x0000_0040;
    /// Instruction address translation (I-MMU) enable (bit 26).
    pub const IR: u32 = 0x0000_0020;
    /// Data address translation (D-MMU) enable (bit 27).
    pub const DR: u32 = 0x0000_0010;
    /// Recoverable interrupt (bit 30).
    pub const RI: u32 = 0x0000_0002;
    /// Little-endian mode (bit 31). Not supported by this emulator.
    pub const LE: u32 = 0x0000_0001;

    /// MSR bits that are saved to SRR1 on exception and restored by `rfi`
    /// (`MSR[16-31]` on the MPC8xx).
    pub const SAVED: u32 = 0x0000_FFFF;
}

/// SPR numbers we care about (MPC860UM Table 6-? "Special purpose registers").
pub mod spr {
    pub const XER: u16 = 1;
    pub const LR: u16 = 8;
    pub const CTR: u16 = 9;
    pub const DSISR: u16 = 18;
    pub const DAR: u16 = 19;
    pub const DEC: u16 = 22;
    pub const SRR0: u16 = 26;
    pub const SRR1: u16 = 27;
    pub const EIE: u16 = 80;
    pub const EID: u16 = 81;
    pub const NRI: u16 = 82;
    pub const TBL_READ: u16 = 268;
    pub const TBU_READ: u16 = 269;
    pub const SPRG0: u16 = 272;
    pub const SPRG3: u16 = 275;
    pub const TBL_WRITE: u16 = 284;
    pub const TBU_WRITE: u16 = 285;
    pub const PVR: u16 = 287;
    pub const IC_CST: u16 = 560;
    pub const DC_CST: u16 = 568;
    pub const IMMR: u16 = 638;
}

/// The exceptions the MPC8xx core can take, with their vector offsets
/// (MPC860UM Table 6-1). The vector address is `offset` plus 0xFFF0_0000 when
/// `MSR[IP]` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    SystemReset,
    /// TEA on a bus access (no device answered). `addr` is the data address
    /// for loads/stores (goes to DAR), `None` for instruction fetches.
    MachineCheck { addr: Option<u32> },
    External,
    Alignment { addr: u32, dsisr: u32 },
    Program(ProgramReason),
    Decrementer,
    SystemCall,
    Trace,
    /// Unimplemented instruction (including every floating-point opcode).
    SoftwareEmulation,
    /// No ITLB/DTLB entry for the address: software reloads the TLB (MPC860UM §6.1.3.2-3).
    ItlbMiss,
    DtlbMiss,
    /// A TLB entry matched but forbids the access (page faults, §6.1.3.4-5).
    /// `status` = SRR1 bits 1-4; `dsisr` = DSISR bits 1, 4, 6.
    ItlbError { status: u32 },
    DtlbError { addr: u32, dsisr: u32 },
}

/// Why a Program exception was raised; encoded into SRR1 bits 11-14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramReason {
    /// A privileged instruction executed with `MSR[PR]=1` (SRR1 bit 13).
    Privileged,
    /// `tw`/`twi` condition met (SRR1 bit 14).
    Trap,
}

impl Exception {
    /// Offset of the exception vector from the exception prefix.
    pub fn vector_offset(self) -> u32 {
        match self {
            Exception::SystemReset => 0x100,
            Exception::MachineCheck { .. } => 0x200,
            Exception::External => 0x500,
            Exception::Alignment { .. } => 0x600,
            Exception::Program(_) => 0x700,
            Exception::Decrementer => 0x900,
            Exception::SystemCall => 0xC00,
            Exception::Trace => 0xD00,
            Exception::SoftwareEmulation => 0x1000,
            Exception::ItlbMiss => 0x1100,
            Exception::DtlbMiss => 0x1200,
            Exception::ItlbError { .. } => 0x1300,
            Exception::DtlbError { .. } => 0x1400,
        }
    }
}

/// Why [`Cpu::run_batch`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchEnd {
    /// `max` instructions executed.
    Budget,
    /// The next instruction is one the caller wants to look at first.
    Hook,
    /// The last instruction ended in checkstop.
    Checkstop,
}

/// What happened during one [`Cpu::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// An instruction retired (or an exception was entered).
    Normal,
    /// Machine check while `MSR[ME]=0`: the core stops ("checkstop state").
    /// On the MPC860 the SIU turns this into a hard reset if `PLPRCR[CSR]=1`;
    /// that decision belongs to the machine, not the core.
    Checkstop,
}

/// Architectural state of the MPC8xx core.
#[derive(Clone)]
pub struct Cpu {
    /// General purpose registers r0..r31.
    pub gpr: [u32; 32],
    /// Address of the next instruction to execute.
    pub pc: u32,
    /// Condition register: 8 four-bit fields CR0..CR7 (LT, GT, EQ, SO).
    pub cr: u32,
    /// Fixed-point exception register: SO (bit 0), OV (1), CA (2), byte count (25-31).
    pub xer: u32,
    pub lr: u32,
    pub ctr: u32,
    pub msr: u32,
    /// Save/restore registers written on exception entry, read by `rfi`.
    pub srr0: u32,
    pub srr1: u32,
    pub dar: u32,
    pub dsisr: u32,
    /// Operating-system scratch registers SPRG0..SPRG3.
    pub sprg: [u32; 4],
    /// 64-bit timebase (TBU:TBL).
    pub tb: u64,
    /// Decrementer: counts down at the timebase rate.
    pub dec: u32,
    /// Processor version register. MPC860 = version 0x0050.
    pub pvr: u32,
    /// Instruction cache control/status: bit 0 = IEN (cache enabled).
    pub ic_cst: u32,
    /// Data cache control/status: bit 0 = DEN (cache enabled).
    pub dc_cst: u32,
    /// Storage for SPRs we accept but do not model (debug comparators,
    /// ICTRL...). Indexed by SPR number.
    pub spr_misc: Box<[u32; 1024]>,
    /// Reservation address set by `lwarx`, consumed by `stwcx.`.
    pub reservation: Option<u32>,

    /// Number of instructions executed since power-on: our virtual clock.
    pub cycles: u64,
    /// Instructions per timebase tick. With a ~48 MHz core doing roughly
    /// one instruction per clock and TB = SYSCLK/16, that's 16.
    pub tb_divider: u32,
    /// `TBSCR[TBE]` from the SIU: timebase and decrementer only count when set.
    pub tb_enabled: bool,
    tb_prescaler: u32,
    /// Decrementer went from 0 to 0xFFFF_FFFF and the exception wasn't taken yet.
    pub dec_pending: bool,
    /// Instruction and data MMUs (TLBs, table-walk assist, protection).
    pub mmu: Mmu,
    /// Memo of recently decoded instruction words (see `decode_cache`).
    decoder: DecodeCache,
    /// How many times each exception vector was taken (index = vector / 0x100).
    pub exception_counts: [u64; 32],
    /// First addresses that raised Software Emulation (unimplemented instructions).
    pub soft_emulation_pcs: Vec<u32>,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        let mut cpu = Cpu {
            gpr: [0; 32],
            pc: 0,
            cr: 0,
            xer: 0,
            lr: 0,
            ctr: 0,
            msr: 0,
            srr0: 0,
            srr1: 0,
            dar: 0,
            dsisr: 0,
            sprg: [0; 4],
            tb: 0,
            dec: 0,
            pvr: 0x0050_0000,
            ic_cst: 0,
            dc_cst: 0,
            spr_misc: Box::new([0; 1024]),
            reservation: None,
            cycles: 0,
            tb_divider: 16,
            tb_enabled: false,
            tb_prescaler: 0,
            dec_pending: false,
            mmu: Mmu::new(),
            decoder: DecodeCache::new(),
            exception_counts: [0; 32],
            soft_emulation_pcs: Vec::new(),
        };
        cpu.hard_reset(true);
        cpu
    }

    /// Hard reset. `exception_prefix_high` mirrors the IP bit of the hard
    /// reset configuration word: the Cisco board boots with IP=1, so the
    /// first instruction is fetched from 0xFFF0_0100.
    pub fn hard_reset(&mut self, exception_prefix_high: bool) {
        self.msr = if exception_prefix_high { msr::IP } else { 0 };
        self.pc = self.exception_prefix() + Exception::SystemReset.vector_offset();
        self.reservation = None;
        self.dec_pending = false;
        self.ic_cst = 0;
        self.dc_cst = 0;
        self.tb_enabled = false;
    }

    /// Base address of the exception vector table, selected by `MSR[IP]`.
    pub fn exception_prefix(&self) -> u32 {
        if self.msr & msr::IP != 0 { 0xFFF0_0000 } else { 0 }
    }

    /// Run up to `max` instructions with `step` inlined into one loop (no
    /// call per guest instruction). Stops early, *before* executing it, when
    /// `stop_before(pc)` is true for the next instruction (the caller has a
    /// hook there), or after a checkstop. Returns the instructions executed.
    pub fn run_batch<P: Platform>(&mut self, sys: &mut P, max: u64, mut stop_before: impl FnMut(u32) -> bool) -> (u64, BatchEnd) {
        let mut n = 0;
        while n < max {
            if stop_before(self.pc) {
                return (n, BatchEnd::Hook);
            }
            let outcome = self.step_inline(sys);
            n += 1;
            if outcome == StepOutcome::Checkstop {
                return (n, BatchEnd::Checkstop);
            }
        }
        (n, BatchEnd::Budget)
    }

    /// Execute one instruction (or enter one pending interrupt).
    pub fn step<P: Platform>(&mut self, sys: &mut P) -> StepOutcome {
        self.step_inline(sys)
    }

    #[inline(always)]
    fn step_inline<P: Platform>(&mut self, sys: &mut P) -> StepOutcome {
        // 1. Asynchronous interrupts are recognised between instructions,
        //    only when MSR[EE]=1. External has priority over the decrementer
        //    (MPC860UM §6.1.2 "Exception Ordering").
        if self.msr & msr::EE != 0 {
            if sys.external_interrupt() {
                return self.enter_exception(Exception::External, self.pc);
            }
            if self.dec_pending {
                self.dec_pending = false;
                return self.enter_exception(Exception::Decrementer, self.pc);
            }
        }

        // 2. Translate (MSR[IR]) and fetch. A fetch that nobody answers is a
        //    machine check.
        let pc = self.pc;
        let pa = if self.msr & msr::IR != 0 {
            match self.mmu.translate(pc, Access::Fetch, self.msr & msr::PR != 0) {
                Ok(pa) => pa,
                Err(Fault::Miss) => return self.enter_exception(Exception::ItlbMiss, pc),
                Err(Fault::Error(status)) => return self.enter_exception(Exception::ItlbError { status }, pc),
            }
        } else {
            pc
        };
        let word = match sys.fetch(pa) {
            Ok(w) => w,
            Err(BusError) => {
                return self.enter_exception(Exception::MachineCheck { addr: None }, pc);
            }
        };

        // 3. Decode (powerpc crate) and execute. `execute` advances self.pc on
        //    success; on failure it returns the exception to take and leaves
        //    self.pc pointing at the faulting instruction.
        let ins = self.decoder.decode(word, pc);
        let outcome = match self.execute(ins, sys) {
            Ok(()) => StepOutcome::Normal,
            Err(exc) => self.enter_exception(exc, pc),
        };

        // 4. Advance the virtual clock.
        self.tick();
        outcome
    }

    /// Advance timebase/decrementer by one instruction's worth of time.
    #[inline]
    fn tick(&mut self) {
        self.cycles += 1;
        if !self.tb_enabled {
            return;
        }
        self.tb_prescaler += 1;
        if self.tb_prescaler >= self.tb_divider {
            self.tb_prescaler = 0;
            self.tb = self.tb.wrapping_add(1);
            let old = self.dec;
            self.dec = self.dec.wrapping_sub(1);
            // The decrementer interrupt is requested when DEC[0] goes 0 -> 1,
            // i.e. when counting down past zero.
            if old & 0x8000_0000 == 0 && self.dec & 0x8000_0000 != 0 {
                self.dec_pending = true;
            }
        }
    }

    /// Let `ticks` timebase ticks pass without executing instructions, with
    /// exactly the effect `ticks * tb_divider` executed instructions would
    /// have on the cycle counter, TB and DEC. Used to fast-forward busy-wait
    /// loops; the caller must not skip past a decrementer interrupt it wants
    /// taken on time (see [`Cpu::ticks_until_decrementer`]).
    pub fn skip_timebase(&mut self, ticks: u64) {
        if ticks == 0 {
            return;
        }
        self.cycles += ticks * self.tb_divider as u64;
        self.tb = self.tb.wrapping_add(ticks);
        let old = self.dec;
        self.dec = self.dec.wrapping_sub(ticks as u32);
        // DEC[0] goes 0 -> 1 when counting down past zero.
        if old & 0x8000_0000 == 0 && ticks > old as u64 {
            self.dec_pending = true;
        }
    }

    /// Timebase ticks until the decrementer requests its next interrupt
    /// (`None` if it already did, or never will without wrapping around).
    pub fn ticks_until_decrementer(&self) -> Option<u64> {
        (!self.dec_pending && self.dec & 0x8000_0000 == 0).then_some(self.dec as u64 + 1)
    }

    /// Exception entry, as described in MPC860UM chapter 6:
    ///
    /// * SRR0 = address to resume at (the faulting instruction, or the next
    ///   one for `sc` and asynchronous interrupts),
    /// * SRR1 = `MSR[16-31]` plus exception-specific status bits,
    /// * MSR  = everything cleared except ME and IP; LE takes the value of ILE,
    /// * PC   = exception prefix + vector offset.
    ///
    /// `pc` is the address of the instruction that was executing (or about to
    /// execute, for asynchronous interrupts).
    pub fn enter_exception(&mut self, exc: Exception, pc: u32) -> StepOutcome {
        let mut srr1_status = 0;
        let srr0 = match exc {
            // `sc` resumes after itself; everything else re-executes / resumes at `pc`.
            Exception::SystemCall => pc.wrapping_add(4),
            _ => pc,
        };

        match exc {
            Exception::MachineCheck { addr } => {
                if self.msr & msr::ME == 0 {
                    log::warn!("checkstop: machine check at pc={pc:#010x} with MSR[ME]=0");
                    return StepOutcome::Checkstop;
                }
                if let Some(a) = addr {
                    self.dar = a;
                }
            }
            Exception::Alignment { addr, dsisr } => {
                self.dar = addr;
                self.dsisr = dsisr;
            }
            Exception::DtlbError { addr, dsisr } => {
                self.dar = addr;
                self.dsisr = dsisr;
            }
            // SRR1 bits 4 and 10 are set on an ITLB miss (Table 6-13).
            Exception::ItlbMiss => srr1_status = 0x0800_0000 | 0x0020_0000,
            Exception::ItlbError { status } => srr1_status = status,
            Exception::Program(ProgramReason::Privileged) => srr1_status = 0x0004_0000,
            Exception::Program(ProgramReason::Trap) => srr1_status = 0x0002_0000,
            _ => {}
        }

        self.exception_counts[(exc.vector_offset() >> 8) as usize & 31] += 1;
        if exc == Exception::SoftwareEmulation
            && self.soft_emulation_pcs.len() < 32
            && !self.soft_emulation_pcs.contains(&pc)
        {
            self.soft_emulation_pcs.push(pc);
        }
        self.srr0 = srr0;
        self.srr1 = (self.msr & msr::SAVED) | srr1_status;
        let keep = self.msr & (msr::ME | msr::IP | msr::ILE);
        self.msr = keep | if keep & msr::ILE != 0 { msr::LE } else { 0 };
        self.reservation = None;
        self.pc = self.exception_prefix() + exc.vector_offset();
        StepOutcome::Normal
    }

    /// `rfi`: return from interrupt.
    pub fn return_from_interrupt(&mut self) {
        self.set_msr((self.msr & !msr::SAVED) | (self.srr1 & msr::SAVED));
        self.pc = self.srr0 & !3;
    }

    /// Write the MSR.
    pub fn set_msr(&mut self, value: u32) {
        if (value ^ self.msr) & (msr::IR | msr::DR) != 0 {
            log::debug!(target: "mmu", "translation {} (MSR={value:#010x}) at pc={:#010x}",
                if value & (msr::IR | msr::DR) != 0 { "on" } else { "off" }, self.pc);
        }
        self.msr = value;
    }

    /// Physical address for a data access at `ea` (identity when `MSR[DR]=0`).
    #[inline]
    pub(crate) fn data_address(&mut self, ea: u32, access: Access) -> Result<u32, Exception> {
        if self.msr & msr::DR == 0 {
            return Ok(ea);
        }
        self.mmu.translate(ea, access, self.msr & msr::PR != 0).map_err(|fault| match fault {
            Fault::Miss => Exception::DtlbMiss,
            Fault::Error(dsisr) => Exception::DtlbError { addr: ea, dsisr },
        })
    }

    /// `mfspr` (and `mftb`) semantics. `Err` means the access must raise an
    /// exception (privileged SPR from user mode).
    pub fn read_spr<P: Platform>(&mut self, sys: &mut P, n: u16) -> Result<u32, Exception> {
        // SPRs with bit 4 of the (swapped) number set are supervisor-only.
        if n & 0x10 != 0 && self.msr & msr::PR != 0 {
            return Err(Exception::Program(ProgramReason::Privileged));
        }
        if let Some(v) = sys.read_spr(n) {
            return Ok(v);
        }
        if let Some(v) = self.mmu.read_spr(n) {
            return Ok(v);
        }
        Ok(match n {
            spr::XER => self.xer,
            spr::LR => self.lr,
            spr::CTR => self.ctr,
            spr::DSISR => self.dsisr,
            spr::DAR => self.dar,
            spr::DEC => self.dec,
            spr::SRR0 => self.srr0,
            spr::SRR1 => self.srr1,
            spr::TBL_READ => self.tb as u32,
            spr::TBU_READ => (self.tb >> 32) as u32,
            spr::SPRG0..=spr::SPRG3 => self.sprg[(n - spr::SPRG0) as usize],
            spr::PVR => self.pvr,
            spr::IC_CST => self.ic_cst,
            spr::DC_CST => self.dc_cst,
            _ => self.spr_misc[n as usize & 0x3FF],
        })
    }

    /// `mtspr` semantics, including the 8xx "magic" SPRs.
    pub fn write_spr<P: Platform>(&mut self, sys: &mut P, n: u16, v: u32) -> Result<(), Exception> {
        if n & 0x10 != 0 && self.msr & msr::PR != 0 {
            return Err(Exception::Program(ProgramReason::Privileged));
        }
        if sys.write_spr(n, v) {
            return Ok(());
        }
        if self.mmu.write_spr(n, v) {
            log::trace!(target: "mmu", "mtspr {n} = {v:#010x} at pc={:#010x}", self.pc);
            return Ok(());
        }
        match n {
            spr::XER => self.xer = v & 0xE000_007F,
            spr::LR => self.lr = v,
            spr::CTR => self.ctr = v,
            spr::DSISR => self.dsisr = v,
            spr::DAR => self.dar = v,
            spr::DEC => self.dec = v,
            spr::SRR0 => self.srr0 = v,
            spr::SRR1 => self.srr1 = v,
            // 8xx: the value written is ignored, the *act of writing* changes MSR.
            spr::EIE => self.msr |= msr::EE | msr::RI,
            spr::EID => self.msr = (self.msr & !msr::EE) | msr::RI,
            spr::NRI => self.msr &= !(msr::EE | msr::RI),
            spr::SPRG0..=spr::SPRG3 => self.sprg[(n - spr::SPRG0) as usize] = v,
            spr::TBL_WRITE => self.tb = (self.tb & 0xFFFF_FFFF_0000_0000) | v as u64,
            spr::TBU_WRITE => self.tb = (self.tb & 0xFFFF_FFFF) | ((v as u64) << 32),
            spr::PVR => {} // read-only
            spr::IC_CST => self.ic_cst = cache_command(self.ic_cst, v),
            spr::DC_CST => self.dc_cst = cache_command(self.dc_cst, v),
            _ => self.spr_misc[n as usize & 0x3FF] = v,
        }
        Ok(())
    }

    // ----- condition register helpers, shared by the instruction semantics -----

    /// Set CR field `field` (0..7) to the 4-bit value `bits` (LT GT EQ SO).
    #[inline]
    pub fn set_cr_field(&mut self, field: u8, bits: u32) {
        let shift = (7 - field as u32) * 4;
        self.cr = (self.cr & !(0xF << shift)) | ((bits & 0xF) << shift);
    }

    /// Record form (`.` suffix): CR0 = signed comparison of `result` with 0,
    /// plus a copy of `XER[SO]`.
    #[inline]
    pub fn record_cr0(&mut self, result: u32) {
        let r = result as i32;
        let bits = if r < 0 { 0b1000 } else if r > 0 { 0b0100 } else { 0b0010 };
        let so = (self.xer >> 31) & 1;
        self.set_cr_field(0, bits | so);
    }
}

/// Apply an `IC_CST`/`DC_CST` command (bits 4-7 of the written value,
/// MPC860UM §9.4.3): 1 = cache enable, 2 = cache disable. The other commands
/// (lock/unlock/invalidate/flush) have no visible effect without a cache model.
fn cache_command(current: u32, written: u32) -> u32 {
    const ENABLED: u32 = 0x8000_0000;
    match (written >> 24) & 0xF {
        0x2 => current | ENABLED,
        0x4 => current & !ENABLED,
        _ => current,
    }
}
