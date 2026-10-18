//! The on-chip peripherals of the MPC860 that live in the IMMR window.
//!
//! `IMMR` (SPR 638) holds the base of a 64 KB window (only the first 16 KB are
//! populated): system interface unit (SIU) registers, the memory controller,
//! timers, clock/reset control, the CPM (communication processor) and the
//! FEC (Fast Ethernet Controller), followed by 8 KB of dual-port RAM.
//!
//! **Modelling approach — "registers are memory, hooks keep them honest":**
//! the whole 16 KB window is a byte array, so every register reads back what
//! was written, exactly like real latches. Only registers whose behaviour the
//! ROM depends on get a hook:
//!
//! | register     | hook                                                     |
//! |--------------|----------------------------------------------------------|
//! | BRx / ORx    | address map changed → board must rebuild its decoder    |
//! | MCR          | UPM RAM read/write through MDR (DRAM init verifies it)   |
//! | SIPEND/SIVEC | computed from the interrupt sources                      |
//! | PISCR/PITR   | periodic interrupt timer, PS is write-1-to-clear         |
//! | TBSCR        | TBE gates the core's timebase                            |
//! | RSR          | reset cause, write-1-to-clear                            |
//! | CPCR         | CPM command: FLG (busy) clears immediately               |
//! | FEC          | MII management frames answered by an LXT971 PHY model    |
//!
//! Register offsets and names follow IDA's `cfg/ppc.cfg` (.mpc860) and the
//! MPC860 User's Manual; see `docs/04-hardware-map.md`.

pub mod fec;
pub mod names;
pub mod regs;

use fec::Fec;
use regs::*;

/// Size of the populated part of the IMMR window (registers + DPRAM).
pub const IMMR_POPULATED: usize = 0x4000;

/// IMMR[16:31] = PARTNUM:MASKNUM. 0x0502 identifies an MPC860 rev D.4
/// (value table from U-Boot's `cpu/mpc8xx/cpu.c`). The ROM tests
/// `PARTNUM > 4 && MASKNUM > 0` to decide how to configure port D.
pub const PART_AND_MASK: u32 = 0x0502;

/// Write this to a "key" register to unlock the protected register it guards.
pub const UNLOCK_KEY: u32 = 0x55CC_AA33;

/// RSR bits (MPC860UM §11.2).
pub mod rsr {
    /// External hard reset (HRESET pin). Set at power-on.
    pub const EHRS: u32 = 0x8000_0000;
    /// External soft reset (SRESET pin). Set at power-on.
    pub const ESRS: u32 = 0x4000_0000;
    /// Loss of PLL lock reset.
    pub const LLRS: u32 = 0x2000_0000;
    /// Software watchdog reset.
    pub const SWRS: u32 = 0x1000_0000;
    /// Checkstop reset (machine check with MSR[ME]=0 and PLPRCR[CSR]=1).
    pub const CSRS: u32 = 0x0800_0000;
}

/// Periodic interrupt timer (MPC860UM §10.9.4).
#[derive(Default)]
pub struct Pit {
    /// Current count (PITR[0:15]).
    pub count: u16,
    /// Fractional progress towards the next PIT clock, in CPU cycles.
    acc: u64,
}

pub struct Mpc860 {
    /// Value of the IMMR SPR: ISB in the upper half, PARTNUM:MASKNUM below.
    pub immr: u32,
    /// The register file + DPRAM, big-endian, offset = address - ISB.
    mem: Box<[u8; IMMR_POPULATED]>,
    /// UPM A and UPM B RAM arrays (64 words each) of the memory controller.
    pub upm: [[u32; 64]; 2],
    /// Set when BRx/ORx/IMMR change; the board rebuilds its address decoder.
    pub map_dirty: bool,
    /// Counts those changes (the JIT's direct memory pointers depend on them).
    pub map_epoch: u64,
    pub pit: Pit,
    /// Level of the external IRQ0..IRQ7 pins (bit n = IRQn asserted).
    pub irq_pins: u8,
    /// Edge-triggered IRQ latches (SIPEND IRQ bits when SIEL[EDn]=1).
    irq_edge_latched: u32,
    pub fec: Fec,
    /// Per-word access counters for `--stats` (index = offset / 4).
    pub stats: Option<Box<RegStats>>,
    /// SPCOM[STR] was written: the board must run an SPI transfer.
    spi_start: bool,
    /// A CPCR command (opcode, channel) waiting for the board to execute it.
    cpm_command: Option<(u8, u8)>,
}

/// CIPR bits of the CPM interrupt sources we model (MPC860UM Table 35-?:
/// bit n = 0x8000_0000 >> n; SPI is bit 26, IDMA1/IDMA2 bits 10/11).
const CIPR_SPI: u32 = 0x0000_0020;
const CIPR_IDMA: [u32; 2] = [0x0020_0000, 0x0010_0000];
/// IDMA status registers (write-1-to-clear) and masks.
const IDSR: [u32; 2] = [0x910, 0x918];
const IDMR: [u32; 2] = [0x914, 0x91C];
/// CICR: IEN (CPM interrupts enabled) and the IRL field (SIU level used).
const CICR_IEN: u32 = 0x0000_0080;

/// How often software read/wrote each 32-bit word of the IMMR window.
pub struct RegStats {
    pub reads: Vec<u64>,
    pub writes: Vec<u64>,
}

impl RegStats {
    pub fn new() -> Self {
        RegStats { reads: vec![0; IMMR_POPULATED / 4], writes: vec![0; IMMR_POPULATED / 4] }
    }
}

impl Default for RegStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Parameter RAM blocks of the CPM (MPC860UM Table 18-?): who owns IMMR+0x3C00..0x3FFF.
const PRAM_BLOCKS: &[(u32, &str)] = &[
    (0x3C00, "SCC1"), (0x3C80, "I2C/IDMA1"), (0x3D00, "SCC2"), (0x3D80, "SPI/RISC timers/IDMA2"),
    (0x3E00, "SCC3"), (0x3E80, "SMC1"), (0x3F00, "SCC4"), (0x3F80, "SMC2/PIP"),
];

/// SCCR bits selecting the PIT/RTC clock (PITRTCLK), MPC860UM §14.
const SCCR_RTDIV: u32 = 0x0100_0000; // divide by 512 (else by 4)
const SCCR_RTSEL: u32 = 0x0080_0000; // source = EXTCLK (else the 32 kHz oscillator)
/// CPU cycles per period of a 32.768 kHz oscillator at 48 MHz.
const CYCLES_PER_OSC32K: u64 = 48_000_000 / 32_768;

impl Mpc860 {
    /// Snapshot state (docs ch. 20). Register statistics aren't state.
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        use ppc8xx::state;
        v.u32(&mut self.immr);
        v.bytes_fixed(&mut self.mem[..]);
        for bank in self.upm.iter_mut() {
            state::u32s(v, bank);
        }
        v.u64(&mut self.map_epoch);
        v.u16(&mut self.pit.count);
        v.u64(&mut self.pit.acc);
        v.u8(&mut self.irq_pins);
        v.u32(&mut self.irq_edge_latched);
        self.fec.visit_state(v);
        v.bool(&mut self.spi_start);
        let mut cmd = self.cpm_command.map_or(0u32, |(op, ch)| 0x1_0000 | (op as u32) << 8 | ch as u32);
        v.u32(&mut cmd);
        self.cpm_command = (cmd & 0x1_0000 != 0).then_some(((cmd >> 8) as u8, cmd as u8));
        // The memory map may differ from what the board decoded: rebuild it.
        self.map_changed();
    }

    pub fn new() -> Self {
        let mut m = Mpc860 {
            immr: 0,
            mem: Box::new([0; IMMR_POPULATED]),
            upm: [[0; 64]; 2],
            map_dirty: true,
            map_epoch: 0,
            pit: Pit::default(),
            irq_pins: 0,
            irq_edge_latched: 0,
            fec: Fec::new(),
            stats: None,
            spi_start: false,
            cpm_command: None,
        };
        m.power_on_reset();
        m
    }

    /// Power-on reset: everything (including the keep-alive RSR) to defaults.
    pub fn power_on_reset(&mut self) {
        self.mem.fill(0);
        self.store32(RSR, rsr::EHRS | rsr::ESRS);
        self.hard_reset();
    }

    /// Hard reset (HRESET): RSR is kept (it is powered by keep-alive).
    pub fn hard_reset(&mut self) {
        let rsr_val = self.load32(RSR);
        let dpram: Vec<u8> = self.mem[0x2000..].to_vec(); // DPRAM content survives reset
        self.mem.fill(0);
        self.mem[0x2000..].copy_from_slice(&dpram);
        self.store32(RSR, rsr_val);

        // The hard reset configuration word on this board selects ISB=0 at
        // reset; the ROM moves IMMR to 0xFF000000 in its first instructions.
        self.immr = PART_AND_MASK;
        // CS0 (boot ROM) is valid and matches every address until reprogrammed:
        // BR0.V=1 with OR0.AM=0. Port size 8-bit (from the reset config word).
        self.store32(BR0, 0x0000_0401);
        self.store32(OR0, 0x0000_0FF6);
        self.store32(SYPCR, 0xFFFF_FF0F); // watchdog enabled at reset, bus monitor on
        self.store32(SIVEC, 0x3C00_0000);
        self.upm = [[0; 64]; 2];
        self.pit = Pit::default();
        self.irq_edge_latched = 0;
        self.fec = Fec::new();
        self.map_changed();
    }

    /// The memory map (chip selects or IMMR) changed.
    fn map_changed(&mut self) {
        self.map_dirty = true;
        self.map_epoch += 1;
    }

    /// Base address of the IMMR window.
    pub fn isb(&self) -> u32 {
        self.immr & 0xFFFF_0000
    }

    pub fn set_immr(&mut self, value: u32) {
        // PARTNUM/MASKNUM are read-only.
        let old_base = self.isb();
        self.immr = (value & 0xFFFF_0000) | PART_AND_MASK;
        // The ROM rewrites IMMR with the same value hundreds of times; only
        // a new base moves the on-chip window.
        if self.isb() != old_base {
            self.map_changed();
        }
    }

    /// Chip-select registers for bank `n`: (BRn, ORn).
    pub fn chip_select(&self, n: usize) -> (u32, u32) {
        (self.load32(BR0 + 8 * n as u32), self.load32(OR0 + 8 * n as u32))
    }

    pub fn sypcr(&self) -> u32 {
        self.load32(SYPCR)
    }
    pub fn plprcr(&self) -> u32 {
        self.load32(PLPRCR)
    }
    /// TBSCR[TBE]: the core's timebase/decrementer count only when set.
    pub fn timebase_enabled(&self) -> bool {
        self.load16(TBSCR) & 0x0001 != 0
    }

    /// Record why we are (re)starting; RSR bits accumulate until software clears them.
    pub fn add_reset_cause(&mut self, bits: u32) {
        let v = self.load32(RSR) | bits;
        self.store32(RSR, v);
    }

    // ------------------------------------------------------------------
    // Interrupt controller (SIU, MPC860UM §10.5.4)
    // ------------------------------------------------------------------

    /// SIPEND bit for internal interrupt level `n` (LVL0 = bit 1, LVL1 = bit 3...).
    fn lvl_bit(level: u32) -> u32 {
        0x4000_0000 >> (2 * level)
    }
    /// SIPEND bit for external pin IRQn (IRQ0 = bit 0, IRQ1 = bit 2...).
    fn irq_bit(n: u32) -> u32 {
        0x8000_0000 >> (2 * n)
    }

    /// Convert a one-hot "interrupt request level" field (PIRQ, TBIRQ...: bit
    /// 0x80 = level 0, 0x40 = level 1, ...) into SIPEND LVL bits.
    fn level_field_to_sipend(field: u8) -> u32 {
        (0..8).filter(|l| field & (0x80 >> l) != 0).map(Self::lvl_bit).fold(0, |a, b| a | b)
    }

    /// Current SIPEND value, computed from all interrupt sources.
    pub fn sipend(&self) -> u32 {
        let siel = self.load32(SIEL);
        let mut pend = self.irq_edge_latched;
        for n in 0..8 {
            let edge_mode = siel & Self::irq_bit(n) != 0;
            if !edge_mode && self.irq_pins & (1 << n) != 0 {
                pend |= Self::irq_bit(n);
            }
        }
        // PIT: PS && PIE raise the level programmed in PIRQ.
        let piscr = self.load16(PISCR);
        if piscr & 0x0080 != 0 && piscr & 0x0004 != 0 {
            pend |= Self::level_field_to_sipend((piscr >> 8) as u8);
        }
        // FEC: (IEVENT & IMASK) raises the level in IVEC[ILEVEL] (bits 0-2).
        if self.fec.ievent & self.fec.imask != 0 {
            pend |= Self::lvl_bit(self.fec.ivec >> 29);
        }
        // CPM interrupt controller: any unmasked, not-in-service source raises
        // the SIU level programmed in CICR[IRL] (bits 16-18), if CICR[IEN].
        let cicr = self.load32(CICR);
        if cicr & CICR_IEN != 0 && self.cipr() & self.load32(CIMR) & !self.load32(CISR) != 0 {
            pend |= Self::lvl_bit((cicr >> 13) & 7);
        }
        pend
    }

    /// CPM interrupt pending register, computed from the sources we model.
    fn cipr(&self) -> u32 {
        let spi_events = self.mem[SPIE as usize] & self.mem[SPIM as usize];
        let mut cipr = if spi_events != 0 { CIPR_SPI } else { 0 };
        for ch in 0..2 {
            if self.load8(IDSR[ch]) & self.load8(IDMR[ch]) != 0 {
                cipr |= CIPR_IDMA[ch];
            }
        }
        cipr
    }

    /// CIVR: 5-bit code of the highest-priority unmasked CPM source in bits
    /// 0-4 (code = 31 - bit position of its CIPR bit, MSB first).
    fn civr(&self) -> u16 {
        let active = self.cipr() & self.load32(CIMR);
        let code = if active == 0 { 0 } else { 31 - active.leading_zeros() };
        (code as u16) << 11
    }

    /// The board polls this after IMMR writes: was an SPI transfer started?
    pub fn take_spi_start(&mut self) -> bool {
        std::mem::take(&mut self.spi_start)
    }

    /// The board polls this after IMMR writes: a CPCR command to execute.
    pub fn take_cpm_command(&mut self) -> Option<(u8, u8)> {
        self.cpm_command.take()
    }

    /// Set bits in SPIE (the SPI engine reports events here).
    pub fn spi_event(&mut self, bits: u8) {
        self.mem[SPIE as usize] |= bits;
    }

    /// Offset (in the IMMR window) of the SPI parameter RAM, honouring the
    /// SPI_BASE relocation pointer (MPC860UM §18.6.3).
    pub fn spi_param_base(&self) -> u32 {
        match self.load16(SPI_BASE_PTR) {
            0 => SPI_PARAM_DEFAULT,
            reloc => DPRAM + reloc as u32,
        }
    }

    /// Interrupt line into the core: any pending source that is not masked.
    pub fn external_interrupt(&self) -> bool {
        self.sipend() & self.load32(SIMASK) != 0
    }

    /// SIVEC: interrupt code (= number * 4) of the highest-priority unmasked
    /// pending source in bits 0-7. IRQ0 = number 0, LVL0 = 1, IRQ1 = 2, ...
    fn sivec(&self) -> u32 {
        let active = self.sipend() & self.load32(SIMASK);
        let number = if active == 0 { 15 } else { active.leading_zeros() };
        (number * 4) << 24
    }

    /// Board-level IRQ pin change. Edge-triggered pins latch on assertion.
    pub fn set_irq_pin(&mut self, n: u32, asserted: bool) {
        let was = self.irq_pins & (1 << n) != 0;
        if asserted {
            self.irq_pins |= 1 << n;
        } else {
            self.irq_pins &= !(1 << n);
        }
        let edge_mode = self.load32(SIEL) & Self::irq_bit(n) != 0;
        if edge_mode && asserted && !was {
            self.irq_edge_latched |= Self::irq_bit(n);
        }
    }

    // ------------------------------------------------------------------
    // Time
    // ------------------------------------------------------------------

    /// CPU cycles per PITRTCLK period. ROMMON programs SCCR = 0x03800000
    /// (RTSEL = EXTCLK, RTDIV = /512) and PLPRCR[MF] = 0 (EXTCLK = SYSCLK =
    /// 48 MHz), so the PIT counts at 93.75 kHz: one count every 512 cycles.
    /// (My first model assumed a 32 kHz crystal and ran 11x too slow, which
    /// stretched every ROMMON network timeout by the same factor.)
    pub fn cycles_per_pit_tick(&self) -> u64 {
        let sccr = self.load32(SCCR);
        let source = if sccr & SCCR_RTSEL != 0 { 1 } else { CYCLES_PER_OSC32K };
        source * if sccr & SCCR_RTDIV != 0 { 512 } else { 4 }
    }

    /// CPU cycles until the PIT next requests an interrupt (PISCR[PS] set
    /// with PIE enabled): `None` if it can't, `Some(0)` if one is pending.
    /// Used to skip idle time without jumping over a timer interrupt.
    pub fn cycles_until_pit_interrupt(&self) -> Option<u64> {
        let piscr = self.load16(PISCR);
        if piscr & 0x0001 == 0 || piscr & 0x0004 == 0 {
            return None; // PTE or PIE clear
        }
        // Masked in the SIU (ROMMON polls PS instead): expiries can't reach
        // the CPU, and PS is sticky, so merging several is what the hardware
        // does too. Only an unmasked PIT bounds a skip.
        if self.load32(SIMASK) & Self::level_field_to_sipend((piscr >> 8) as u8) == 0 {
            return None;
        }
        if piscr & 0x0080 != 0 {
            return Some(0); // PS already set
        }
        let period = self.cycles_per_pit_tick();
        let ticks_left = self.pit.count.max(1) as u64; // count reaches zero after this many ticks
        Some((ticks_left - 1) * period + (period - self.pit.acc.min(period)))
    }

    /// CPU cycles until the PIT next reaches zero (sets PISCR[PS]), if it
    /// runs, whether or not that interrupts: software may poll PS.
    pub fn cycles_until_pit_expiry(&self) -> Option<u64> {
        if self.load16(PISCR) & 0x0001 == 0 {
            return None; // PTE clear
        }
        let period = self.cycles_per_pit_tick();
        let ticks_left = self.pit.count.max(1) as u64;
        Some((ticks_left - 1) * period + (period - self.pit.acc.min(period)))
    }

    /// Advance on-chip timers by `cycles` CPU cycles.
    pub fn advance(&mut self, cycles: u64) {
        let piscr = self.load16(PISCR);
        if piscr & 0x0001 != 0 {
            // PTE: timer enabled, counts down at PITRTCLK.
            let period = self.cycles_per_pit_tick();
            self.pit.acc += cycles;
            while self.pit.acc >= period {
                self.pit.acc -= period;
                if self.pit.count <= 1 {
                    // Reached zero: set PS and reload from PITC.
                    let piscr = self.load16(PISCR) | 0x0080;
                    self.store16(PISCR, piscr);
                    self.pit.count = (self.load32(PITC) >> 16) as u16;
                } else {
                    self.pit.count -= 1;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Bus interface (offset = address - ISB)
    // ------------------------------------------------------------------

    #[inline]
    fn count(&mut self, off: u32, write: bool) {
        if let Some(st) = &mut self.stats {
            let i = (off as usize / 4).min(st.reads.len() - 1);
            if write { st.writes[i] += 1 } else { st.reads[i] += 1 }
        }
    }

    pub fn read8(&mut self, off: u32) -> u8 {
        self.count(off, false);
        self.refresh(off & !3);
        self.mem.get(off as usize).copied().unwrap_or(0)
    }
    pub fn read16(&mut self, off: u32) -> u16 {
        self.count(off, false);
        self.refresh(off & !3);
        self.load16(off)
    }
    pub fn read32(&mut self, off: u32) -> u32 {
        self.count(off, false);
        self.refresh(off);
        self.load32(off)
    }

    pub fn write8(&mut self, off: u32, value: u8) {
        self.count(off, true);
        if !self.write_hook(off, 1, value as u32) {
            if let Some(b) = self.mem.get_mut(off as usize) {
                *b = value;
            }
        }
        self.after_write(off & !3);
    }
    pub fn write16(&mut self, off: u32, value: u16) {
        self.count(off, true);
        if !self.write_hook(off, 2, value as u32) {
            self.store16(off, value);
        }
        self.after_write(off & !3);
    }
    pub fn write32(&mut self, off: u32, value: u32) {
        self.count(off, true);
        if !self.write_hook(off, 4, value) {
            self.store32(off, value);
        }
        self.after_write(off);
    }

    /// Bring computed registers up to date before a read.
    fn refresh(&mut self, off: u32) {
        match off {
            SIPEND => self.store32(SIPEND, self.sipend()),
            SIVEC => self.store32(SIVEC, self.sivec()),
            CIPR => self.store32(CIPR, self.cipr()),
            CIVR => {
                let v = self.civr();
                self.store16(CIVR, v);
            }
            PITR => self.store32(PITR, (self.pit.count as u32) << 16),
            x if (FEC_BASE..FEC_END).contains(&x) => {
                let v = self.fec.read(x - FEC_BASE, self.load32(x));
                self.store32(x, v);
            }
            _ => {}
        }
    }

    /// Registers whose write does something other than "store the bytes".
    /// Returns `true` if the write was fully handled here.
    fn write_hook(&mut self, off: u32, size: u8, value: u32) -> bool {
        match (off, size) {
            // write-1-to-clear edge latches
            (SIPEND, 4) => {
                self.irq_edge_latched &= !value;
                true
            }
            (SIVEC, _) => true, // read-only
            // PISCR: PS (0x0080) is write-1-to-clear, the rest is control.
            (PISCR, 2) => {
                let old = self.load16(PISCR);
                let ps = if value & 0x0080 != 0 { 0 } else { old & 0x0080 };
                self.store16(PISCR, (value as u16 & !0x0080) | ps);
                true
            }
            // PITC: writing the modulus also (re)loads the counter.
            (PITC, 4) => {
                self.store32(PITC, value);
                self.pit.count = (value >> 16) as u16;
                self.pit.acc = 0;
                true
            }
            (PITR, _) => true, // read-only
            // RSR bits are write-1-to-clear.
            (RSR, 4) => {
                let v = self.load32(RSR) & !value;
                self.store32(RSR, v);
                true
            }
            // CPM command register: the CP executes instantly, so the FLG
            // (command busy, bit 15) and RST bits read back as 0.
            // OPCODE = bits 4-7, CH_NUM = bits 8-11 (MPC860UM §18.5.3).
            (CPCR, 2) => {
                if value & 0x8000 != 0 {
                    log::debug!("CPM reset");
                } else if value & 0x0001 != 0 {
                    self.cpm_command = Some((((value >> 8) & 0xF) as u8, ((value >> 4) & 0xF) as u8));
                }
                self.store16(CPCR, value as u16 & !0x8001);
                true
            }
            // SPI event register: write-1-to-clear (8-bit register; also
            // reached by 16/32-bit accesses that cover it).
            (SPIE, 1) => {
                self.mem[SPIE as usize] &= !(value as u8);
                true
            }
            // IDMA status registers: write-1-to-clear (8-bit).
            (x, 1) if x == IDSR[0] || x == IDSR[1] => {
                self.mem[x as usize] &= !(value as u8);
                true
            }
            (SPIE, 2) => {
                self.mem[SPIE as usize] &= !((value >> 8) as u8);
                true
            }
            (x, 4) if x == SPIE & !3 => {
                self.mem[SPIE as usize] &= !((value >> 8) as u8);
                self.store16(x, (value >> 16) as u16);
                true
            }
            // CPM interrupt in-service register: write-1-to-clear.
            (CISR, 4) => {
                let v = self.load32(CISR) & !value;
                self.store32(CISR, v);
                true
            }
            (CIPR, _) => true, // computed from the sources
            (x, 4) if (FEC_BASE..FEC_END).contains(&x) => {
                let keep = self.fec.write(x - FEC_BASE, value);
                self.store32(x, keep);
                true
            }
            _ => false,
        }
    }

    /// Side effects that only need the stored value.
    fn after_write(&mut self, off: u32) {
        match off {
            BR0..=OR7 => self.map_changed(),
            MCR => self.upm_command(),
            // SPCOM[STR] (start transfer) clears itself after one clock.
            x if x == SPCOM & !3 => {
                if self.mem[SPCOM as usize] & 0x80 != 0 {
                    self.mem[SPCOM as usize] &= 0x7F;
                    self.spi_start = true;
                }
            }
            _ => {}
        }
    }

    /// Memory Command Register (MPC860UM §15.4.5):
    /// OP[0:1] 00=write UPM RAM from MDR, 01=read UPM RAM into MDR, 10=run
    /// pattern; UM[8] selects UPMA/UPMB; MAD[26:31] is the RAM address.
    fn upm_command(&mut self) {
        let mcr = self.load32(MCR);
        let op = mcr >> 30;
        let upm = ((mcr >> 23) & 1) as usize;
        let mad = (mcr & 0x3F) as usize;
        match op {
            0 => self.upm[upm][mad] = self.load32(MDR),
            1 => self.store32(MDR, self.upm[upm][mad]),
            2 => log::trace!(
                "UPM{} run pattern at {mad:#x} on CS{} x{}",
                if upm == 0 { 'A' } else { 'B' },
                (mcr >> 13) & 7,
                ((mcr >> 8) & 0xF).max(1)
            ),
            _ => {}
        }
    }

    /// Human-readable summary of which on-chip registers software used.
    pub fn stats_report(&self) -> String {
        let Some(st) = &self.stats else { return String::new() };
        let mut out = String::from("IMMR registers accessed (reads/writes):\r\n");
        for i in 0..0x2000 / 4 {
            let (r, w) = (st.reads[i], st.writes[i]);
            if r + w > 0 {
                out += &format!("  {:#06x} {:<16} r={r:<10} w={w}\r\n", i * 4, names::immr_name(i as u32 * 4));
            }
        }
        let dpram: (u64, u64) = (0x2000 / 4..0x3C00 / 4).fold((0, 0), |a, i| (a.0 + st.reads[i], a.1 + st.writes[i]));
        out += &format!("  DPRAM 0x2000-0x3BFF (buffer descriptors/data): r={} w={}\r\n", dpram.0, dpram.1);
        let mut hot: Vec<usize> = (0x2000 / 4..0x4000 / 4).filter(|&i| st.reads[i] > 0).collect();
        hot.sort_by_key(|&i| std::cmp::Reverse(st.reads[i]));
        for &i in hot.iter().take(5) {
            out += &format!("    most read: IMMR+{:#06x} r={}\r\n", i * 4, st.reads[i]);
        }
        for (k, (base, name)) in PRAM_BLOCKS.iter().enumerate() {
            let end = PRAM_BLOCKS.get(k + 1).map_or(0x4000, |b| b.0);
            let (r, w) = (*base as usize / 4..end as usize / 4).fold((0, 0), |a, i| (a.0 + st.reads[i], a.1 + st.writes[i]));
            if r + w > 0 {
                out += &format!("  parameter RAM {base:#06x} {name:<22} r={r} w={w}\r\n");
            }
        }
        out
    }

    // ------------------------------------------------------------------
    // Raw big-endian access to the register file
    // ------------------------------------------------------------------

    pub fn load8(&self, off: u32) -> u8 {
        self.mem.get(off as usize).copied().unwrap_or(0)
    }
    pub fn store8(&mut self, off: u32, v: u8) {
        if let Some(b) = self.mem.get_mut(off as usize) {
            *b = v;
        }
    }
    pub fn load16(&self, off: u32) -> u16 {
        let o = off as usize;
        match self.mem.get(o..o + 2) {
            Some(b) => u16::from_be_bytes([b[0], b[1]]),
            None => 0,
        }
    }
    pub fn load32(&self, off: u32) -> u32 {
        let o = off as usize;
        match self.mem.get(o..o + 4) {
            Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            None => 0,
        }
    }
    pub fn store16(&mut self, off: u32, v: u16) {
        let o = off as usize;
        if let Some(b) = self.mem.get_mut(o..o + 2) {
            b.copy_from_slice(&v.to_be_bytes());
        }
    }
    pub fn store32(&mut self, off: u32, v: u32) {
        let o = off as usize;
        if let Some(b) = self.mem.get_mut(o..o + 4) {
            b.copy_from_slice(&v.to_be_bytes());
        }
    }
}

impl Default for Mpc860 {
    fn default() -> Self {
        Self::new()
    }
}
