//! The Cisco C1700 mainboard: what is wired to the MPC860's chip selects.
//!
//! Chip-select assignment, recovered from `siu_init` (0xFFF06D7C) and
//! `dram_init_and_size` (0xFFF0453C) — see docs/04-hardware-map.md:
//!
//! | CS  | after ROM init                    | device                          |
//! |-----|-----------------------------------|---------------------------------|
//! | CS0 | 0xFFF00000, 1 MB, 8-bit, GPCM     | boot EPROM (512 KB, mirrored)   |
//! | CS1-4 | sized at boot, UPMA/UPMB        | SDRAM banks                     |
//! | CS6 | 0x60000000, 64 MB, 16-bit, GPCM   | boot flash (Intel x16 or AMD)   |
//! | CS7 | 0x68000000, 1 MB, 8-bit, GPCM     | I/O bus: NVRAM, CPLD, 16550 UART|
//!
//! Address decoding follows the MPC860 memory controller: an address belongs
//! to bank *n* when `BRn.V` is set and `(addr & ORn.AM) == (BRn.BA & ORn.AM)`;
//! the on-chip IMMR window has priority over all banks, and the lowest
//! numbered matching bank wins. Accesses nobody claims end in a bus-monitor
//! timeout (TEA), which the CPU turns into a machine check.

pub mod cookie;
pub mod cpld;
mod cpm_spi;
mod fec_dma;
mod fpga_config;
mod idma;
pub mod flash;
pub mod flashfs;
pub mod iobus;
pub mod nvram;
pub mod uart16550;

use crate::mpc860::Mpc860;
use crate::net::{Link, NetConfig, VirtualNetwork};
use flash::Flash;
use iobus::IoBus;
use ppc8xx::jit::CodeWatch;
use ppc8xx::{BusError, Platform, spr};

/// Address decoding granularity: ORx.AM compares address bits 0..16, so no
/// chip select can start or end inside a 32 KB page.
const PAGE_SHIFT: u32 = 15;
const PAGE_SIZE: u32 = 1 << PAGE_SHIFT;
const PAGE_MASK: u32 = PAGE_SIZE - 1;
const NUM_PAGES: usize = 1 << (32 - PAGE_SHIFT);

/// Host page numbers for the JIT's code watch (docs ch. 17): 4 KB page `p`
/// of DRAM bank `b` is `b * DRAM_PAGES + p`, ROM page `p` is
/// `4 * DRAM_PAGES + p`. Aliases of the same memory get the same number.
const DRAM_PAGES: u32 = 1 << 16; // up to 256 MB per bank
const ROM_PAGES_BASE: u32 = 4 * DRAM_PAGES;

/// What a 32 KB page of the physical address space is connected to.
/// `base` is the offset, inside the device, of the first byte of the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// Not decoded yet (the map is rebuilt lazily).
    Stale,
    /// No chip select: bus error.
    Unmapped,
    /// On-chip peripherals (IMMR window).
    Immr,
    Rom { base: u32 },
    Dram { bank: u8, base: u32 },
    /// A UPM bank with no SDRAM fitted: the UPM still terminates the cycle,
    /// reads return a floating bus (all ones), writes vanish. This is what
    /// lets the ROM's sizing code detect empty sockets.
    EmptyBank,
    Flash { base: u32 },
    IoBus { base: u32 },
}

/// Board configuration (from the command line).
#[derive(Clone, Debug)]
pub struct BoardConfig {
    /// SDRAM fitted on CS1..CS4, in bytes (0 = empty). Power of two, >= 2 MB.
    pub dram_banks: [u32; 4],
    /// Boot flash size in MB: 4 (2 x AMD Am29F016), 8, 16 or 32 (Intel 28F640J3 / 28F128J3 x16).
    pub flash_mb: u32,
    /// Value of CPLD register 4 bits 0-2 (board type).
    pub board_id: u8,
    /// Virtual network plugged into the Ethernet port (`None` = no cable).
    pub net: Option<NetConfig>,
}

impl Default for BoardConfig {
    fn default() -> Self {
        BoardConfig { dram_banks: [32 << 20, 0, 0, 0], flash_mb: 16, board_id: 1, net: None }
    }
}

pub struct Board {
    pub mpc: Mpc860,
    pub rom: Vec<u8>,
    pub dram: [Vec<u8>; 4],
    pub flash: Flash,
    pub io: IoBus,
    map: Vec<Region>,
    /// Cached level of the CPU's external interrupt input.
    irq_line: bool,
    /// What the Ethernet port is plugged into (`None`: unplugged).
    pub net: Option<Link>,
    /// Virtual time in CPU cycles (drives the network's timers).
    now_cycles: u64,
    last_net_poll: u64,
    /// `--watch`: log every access to this physical range (debugging aid).
    pub watch: Option<Watch>,
    /// An IDMA transfer is in progress (its writes may re-enter `run_cpm`).
    idma_busy: bool,
    /// Pages holding JIT-translated code; every write to DRAM (CPU, DMA,
    /// debugger, accelerators) is checked against it.
    code: CodeWatch,
}

/// A physical address range whose accesses are logged, like a watchpoint.
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    pub start: u32,
    pub end: u32,
    /// PC of the instruction being executed, `u32::MAX` outside the CPU (DMA).
    pub pc: u32,
}

/// CPU cycles per microsecond of virtual time (48 MHz core).
pub const CYCLES_PER_US: u64 = 48;
/// How often the virtual network runs (in CPU cycles).
const NET_POLL_INTERVAL: u64 = 4096;

impl Board {
    fn watch_access(&self, addr: u32, size: u32, value: u32, write: bool) {
        let Some(w) = self.watch else { return };
        if addr < w.end && addr + size > w.start {
            let who = if w.pc == u32::MAX { "DMA".to_string() } else { format!("pc={:#010x}", w.pc) };
            log::info!(target: "watch", "{} {addr:#010x} = {value:#0width$x} ({size} bytes) {who}",
                if write { "write" } else { "read " }, width = 2 + 2 * size as usize);
        }
    }

    pub fn new(rom: Vec<u8>, cfg: &BoardConfig, nvram: nvram::Nvram) -> Self {
        let flash = flash::for_size_mb(cfg.flash_mb).unwrap_or_else(|e| panic!("{e}"));
        assert!(rom.len().is_power_of_two(), "ROM size must be a power of two");
        let dram = cfg.dram_banks.map(|size| {
            assert!(size == 0 || (size.is_power_of_two() && size >= 2 << 20), "bad DRAM bank size {size:#x}");
            vec![0u8; size as usize]
        });
        Board {
            mpc: Mpc860::new(),
            rom,
            dram,
            flash,
            io: IoBus::new(nvram, cfg.board_id),
            map: vec![Region::Stale; NUM_PAGES],
            irq_line: false,
            net: cfg.net.clone().map(|c| Link::Virtual(Box::new(VirtualNetwork::new(c)))),
            now_cycles: 0,
            last_net_poll: 0,
            watch: None,
            idma_busy: false,
            code: CodeWatch::new(),
        }
    }

    /// Power-on reset of the whole board (RAM content is lost).
    pub fn power_on_reset(&mut self) {
        self.mpc.power_on_reset();
        self.io.power_on_reset();
        self.invalidate_map();
    }

    /// Hard reset (e.g. checkstop reset): RAM, NVRAM and RSR survive.
    pub fn hard_reset(&mut self, reset_cause: u32) {
        self.mpc.hard_reset();
        self.mpc.add_reset_cause(reset_cause);
        self.io.hard_reset();
        self.invalidate_map();
    }

    /// Advance devices by `cycles` CPU cycles and refresh the interrupt line.
    pub fn advance(&mut self, cycles: u64) {
        self.now_cycles += cycles;
        self.mpc.advance(cycles);
        self.fec_dma();
        if self.net.is_some() && self.now_cycles - self.last_net_poll >= NET_POLL_INTERVAL {
            self.last_net_poll = self.now_cycles;
            let now_us = self.now_cycles / CYCLES_PER_US;
            if let Some(net) = self.net.as_mut().filter(|n| n.wants_poll(now_us)) {
                net.poll(now_us);
                self.fec_dma(); // deliver whatever the network answered
            }
        }
        self.update_irq();
    }

    /// Let the CPM (communication processor) act on commands software just
    /// gave it: CPCR commands, SPI transfers, IDMA requests.
    fn run_cpm(&mut self) {
        if let Some((op, ch)) = self.mpc.take_cpm_command() {
            self.cpm_command(op, ch);
        }
        if self.mpc.take_spi_start() {
            self.spi_transfer();
        }
        self.idma_service();
    }

    /// The UART interrupt output is wired to IRQ1 (the ROM unmasks only IRQ1
    /// in SIMASK and enables the 16550 line-status interrupt to catch BREAK).
    pub fn update_irq(&mut self) {
        // Both channels of the NS16552 DUART (console + AUX) share IRQ1.
        let uart_irq = self.io.uart.interrupt_pending() || self.io.aux.interrupt_pending();
        self.mpc.set_irq_pin(1, uart_irq);
        self.irq_line = self.mpc.external_interrupt();
    }

    // ------------------------------------------------------------------
    // Address decoding
    // ------------------------------------------------------------------

    fn invalidate_map(&mut self) {
        self.map.fill(Region::Stale);
        self.mpc.map_dirty = false;
    }

    /// Look up (and lazily decode) the region an address belongs to. The
    /// common case is one table read; decoding is kept out of line so this
    /// inlines into every access (it was 11% of host time as a call).
    #[inline(always)]
    fn region(&mut self, addr: u32) -> Region {
        if !self.mpc.map_dirty {
            let r = self.map[(addr >> PAGE_SHIFT) as usize];
            if r != Region::Stale {
                return r;
            }
        }
        self.region_slow(addr)
    }

    #[cold]
    #[inline(never)]
    fn region_slow(&mut self, addr: u32) -> Region {
        if self.mpc.map_dirty {
            self.invalidate_map();
        }
        let page = (addr >> PAGE_SHIFT) as usize;
        let r = self.decode(addr & !PAGE_MASK);
        self.map[page] = r;
        r
    }

    /// The memory controller's decision for the page starting at `page_addr`.
    fn decode(&self, page_addr: u32) -> Region {
        if page_addr & 0xFFFF_0000 == self.mpc.isb() {
            return Region::Immr;
        }
        for cs in 0..8 {
            let (br, or) = self.mpc.chip_select(cs);
            if br & 1 == 0 {
                continue; // BRx.V: bank not valid
            }
            let am = or & 0xFFFF_8000;
            if page_addr & am != br & am {
                continue;
            }
            let offset = page_addr & !am; // offset of the page inside the bank window
            return match cs {
                0 => Region::Rom { base: offset % self.rom.len() as u32 },
                1..=4 => {
                    let bank = &self.dram[cs - 1];
                    if bank.is_empty() {
                        Region::EmptyBank
                    } else {
                        // SDRAM ignores address lines above its size: aliasing.
                        Region::Dram { bank: (cs - 1) as u8, base: offset % bank.len() as u32 }
                    }
                }
                // The flash does NOT mirror inside the 64 MB CS6 window: above the
                // fitted size nothing drives the bus. The ROM relies on this to stop
                // probing for more flash banks (an AMD ID read there must fail).
                6 if offset < self.flash.size() => Region::Flash { base: offset },
                6 => Region::EmptyBank,
                7 => Region::IoBus { base: offset },
                _ => Region::EmptyBank,
            };
        }
        Region::Unmapped
    }

    fn bus_error(&self, addr: u32, what: &str) -> BusError {
        log::debug!("bus error: {what} at {addr:#010x} (no chip select)");
        BusError
    }

    // ------------------------------------------------------------------
    // Debugger access: like normal reads/writes but without side effects
    // on devices (reading the UART must not eat a received character).
    // ------------------------------------------------------------------

    /// A DRAM byte at `offset` in `bank` is about to change: tell the JIT
    /// if it holds translated code (one test when nothing is translated).
    #[inline]
    fn note_dram_write(&mut self, bank: u8, offset: u32) {
        self.code.note_write(bank as u32 * DRAM_PAGES + (offset >> 12), offset);
    }

    pub fn peek8(&mut self, addr: u32) -> Option<u8> {
        match self.region(addr) {
            Region::Rom { base } => Some(self.rom[(base + (addr & PAGE_MASK)) as usize]),
            Region::Dram { bank, base } => Some(self.dram[bank as usize][(base + (addr & PAGE_MASK)) as usize]),
            Region::Flash { base } => Some(self.flash.peek8(base + (addr & PAGE_MASK))),
            Region::Immr => Some(self.mpc.load32((addr - self.mpc.isb()) & !3).to_be_bytes()[(addr & 3) as usize]),
            Region::IoBus { base } => Some(self.io.peek8(base + (addr & PAGE_MASK))),
            Region::EmptyBank => Some(0xFF),
            Region::Unmapped | Region::Stale => None,
        }
    }

    pub fn peek_word(&mut self, addr: u32) -> Option<u32> {
        let mut b = [0u8; 4];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = self.peek8(addr.wrapping_add(i as u32))?;
        }
        Some(u32::from_be_bytes(b))
    }

    /// Fill `len` bytes at `addr` with `value`, but only if the whole range
    /// is plain DRAM (no devices, no side effects); otherwise change nothing
    /// and return false. Used by the ROM memory-clear accelerator.
    pub fn fill_dram(&mut self, addr: u32, len: u32, value: u8) -> bool {
        let Some(end) = addr.checked_add(len) else { return false };
        let mut chunks = Vec::new();
        let mut a = addr;
        while a < end {
            let n = (PAGE_SIZE - (a & PAGE_MASK)).min(end - a);
            match self.region(a) {
                Region::Dram { bank, base } => chunks.push((bank as usize, (base + (a & PAGE_MASK)) as usize, n as usize)),
                _ => return false,
            }
            a += n;
        }
        for (bank, off, n) in chunks {
            let first = bank as u32 * DRAM_PAGES + (off >> 12) as u32;
            let last = bank as u32 * DRAM_PAGES + ((off + n - 1) >> 12) as u32;
            self.code.note_pages(first, last);
            self.dram[bank][off..off + n].fill(value);
        }
        true
    }

    pub fn poke8(&mut self, addr: u32, value: u8) -> bool {
        match self.region(addr) {
            Region::Dram { bank, base } => {
                self.note_dram_write(bank, base + (addr & PAGE_MASK));
                self.dram[bank as usize][(base + (addr & PAGE_MASK)) as usize] = value;
                true
            }
            // Let debuggers patch the ROM image in memory (handy for experiments).
            Region::Rom { base } => {
                let offset = base + (addr & PAGE_MASK);
                self.code.note_write(ROM_PAGES_BASE + (offset >> 12), offset);
                self.rom[(base + (addr & PAGE_MASK)) as usize] = value;
                true
            }
            _ => self.write8(addr, value).is_ok(),
        }
    }
}

// ----------------------------------------------------------------------
// Typed big-endian helpers for byte slices
// ----------------------------------------------------------------------

#[inline]
fn be16(mem: &[u8], off: u32) -> u16 {
    let o = off as usize;
    u16::from_be_bytes(mem[o..o + 2].try_into().unwrap())
}
#[inline]
fn be32(mem: &[u8], off: u32) -> u32 {
    let o = off as usize;
    u32::from_be_bytes(mem[o..o + 4].try_into().unwrap())
}
#[inline]
fn put16(mem: &mut [u8], off: u32, v: u16) {
    let o = off as usize;
    mem[o..o + 2].copy_from_slice(&v.to_be_bytes());
}
#[inline]
fn put32(mem: &mut [u8], off: u32, v: u32) {
    let o = off as usize;
    mem[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

impl Board {
    #[inline(never)]
    fn read8_slow(&mut self, addr: u32) -> Result<u8, BusError> {
        let low = addr & PAGE_MASK;
        let v = match self.region(addr) {
            Region::Dram { bank, base } => self.dram[bank as usize][(base + low) as usize],
            Region::Rom { base } => self.rom[(base + low) as usize],
            Region::Immr => self.mpc.read8(addr - self.mpc.isb()),
            Region::Flash { base } => self.flash.read8(base + low),
            Region::IoBus { base } => {
                let v = self.io.read8(base + low);
                self.update_irq();
                v
            }
            Region::EmptyBank => 0xFF,
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "read8")),
        };
        if self.watch.is_some() {
            self.watch_access(addr, 1, v as u32, false);
        }
        Ok(v)
    }

    #[inline(never)]
    fn read16_slow(&mut self, addr: u32) -> Result<u16, BusError> {
        let low = addr & PAGE_MASK;
        let v = match self.region(addr) {
            Region::Dram { bank, base } => be16(&self.dram[bank as usize], base + low),
            Region::Rom { base } => be16(&self.rom, base + low),
            Region::Immr => self.mpc.read16(addr - self.mpc.isb()),
            Region::Flash { base } => self.flash.read16(base + low),
            // 8-bit port: the memory controller splits the access into byte cycles.
            Region::IoBus { .. } => u16::from_be_bytes([self.read8(addr)?, self.read8(addr + 1)?]),
            Region::EmptyBank => 0xFFFF,
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "read16")),
        };
        if self.watch.is_some() {
            self.watch_access(addr, 2, v as u32, false);
        }
        Ok(v)
    }

    #[inline(never)]
    fn read32_slow(&mut self, addr: u32) -> Result<u32, BusError> {
        let low = addr & PAGE_MASK;
        let v = match self.region(addr) {
            Region::Dram { bank, base } => be32(&self.dram[bank as usize], base + low),
            Region::Rom { base } => be32(&self.rom, base + low),
            Region::Immr => self.mpc.read32(addr - self.mpc.isb()),
            Region::Flash { base } => {
                let hi = self.flash.read16(base + low) as u32;
                let lo = self.flash.read16(base + low + 2) as u32;
                (hi << 16) | lo
            }
            Region::IoBus { .. } => u32::from_be_bytes([
                self.read8(addr)?,
                self.read8(addr + 1)?,
                self.read8(addr + 2)?,
                self.read8(addr + 3)?,
            ]),
            Region::EmptyBank => 0xFFFF_FFFF,
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "read32")),
        };
        if self.watch.is_some() {
            self.watch_access(addr, 4, v as u32, false);
        }
        Ok(v)
    }

    #[inline(never)]
    fn write8_slow(&mut self, addr: u32, value: u8) -> Result<(), BusError> {
        if self.watch.is_some() {
            self.watch_access(addr, 1, value as u32, true);
        }
        let low = addr & PAGE_MASK;
        match self.region(addr) {
            Region::Dram { bank, base } => {
                self.note_dram_write(bank, base + low);
                self.dram[bank as usize][(base + low) as usize] = value;
            }
            Region::Rom { .. } | Region::EmptyBank => {}
            Region::Immr => {
                self.mpc.write8(addr - self.mpc.isb(), value);
                self.run_cpm();
                self.update_irq();
            }
            Region::Flash { base } => self.flash.write8(base + low, value),
            Region::IoBus { base } => {
                self.io.write8(base + low, value);
                self.update_irq();
            }
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "write8")),
        }
        Ok(())
    }

    #[inline(never)]
    fn write16_slow(&mut self, addr: u32, value: u16) -> Result<(), BusError> {
        if self.watch.is_some() {
            self.watch_access(addr, 2, value as u32, true);
        }
        let low = addr & PAGE_MASK;
        match self.region(addr) {
            Region::Dram { bank, base } => {
                self.note_dram_write(bank, base + low);
                put16(&mut self.dram[bank as usize], base + low, value);
            }
            Region::Rom { .. } | Region::EmptyBank => {}
            Region::Immr => {
                self.mpc.write16(addr - self.mpc.isb(), value);
                self.run_cpm();
                self.update_irq();
            }
            Region::Flash { base } => self.flash.write16(base + low, value),
            Region::IoBus { .. } => {
                let [a, b] = value.to_be_bytes();
                self.write8(addr, a)?;
                self.write8(addr + 1, b)?;
            }
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "write16")),
        }
        Ok(())
    }

    #[inline(never)]
    fn write32_slow(&mut self, addr: u32, value: u32) -> Result<(), BusError> {
        if self.watch.is_some() {
            self.watch_access(addr, 4, value as u32, true);
        }
        let low = addr & PAGE_MASK;
        match self.region(addr) {
            Region::Dram { bank, base } => {
                self.note_dram_write(bank, base + low);
                put32(&mut self.dram[bank as usize], base + low, value);
            }
            Region::Rom { .. } | Region::EmptyBank => {}
            Region::Immr => {
                self.mpc.write32(addr - self.mpc.isb(), value);
                self.run_cpm();
                self.update_irq();
            }
            Region::Flash { base } => {
                self.flash.write16(base + low, (value >> 16) as u16);
                self.flash.write16(base + low + 2, value as u16);
            }
            Region::IoBus { .. } => {
                for (i, b) in value.to_be_bytes().into_iter().enumerate() {
                    self.write8(addr + i as u32, b)?;
                }
            }
            Region::Unmapped | Region::Stale => return Err(self.bus_error(addr, "write32")),
        }
        Ok(())
    }
}

impl Platform for Board {
    // Fast paths: plain DRAM (and ROM for reads) with no --watch active is a
    // table lookup and a slice access, inlined into the CPU loop; everything
    // else (devices, flash, IMMR, watch logging) goes to the `*_slow` bodies.

    #[inline]
    fn read8(&mut self, addr: u32) -> Result<u8, BusError> {
        if self.watch.is_none() {
            if let Region::Dram { bank, base } = self.region(addr) {
                return Ok(self.dram[bank as usize][(base + (addr & PAGE_MASK)) as usize]);
            }
        }
        self.read8_slow(addr)
    }

    #[inline]
    fn read16(&mut self, addr: u32) -> Result<u16, BusError> {
        if self.watch.is_none() {
            if let Region::Dram { bank, base } = self.region(addr) {
                return Ok(be16(&self.dram[bank as usize], base + (addr & PAGE_MASK)));
            }
        }
        self.read16_slow(addr)
    }

    #[inline]
    fn read32(&mut self, addr: u32) -> Result<u32, BusError> {
        if self.watch.is_none() {
            match self.region(addr) {
                Region::Dram { bank, base } => return Ok(be32(&self.dram[bank as usize], base + (addr & PAGE_MASK))),
                Region::Rom { base } => return Ok(be32(&self.rom, base + (addr & PAGE_MASK))),
                _ => {}
            }
        }
        self.read32_slow(addr)
    }

    #[inline]
    fn write8(&mut self, addr: u32, value: u8) -> Result<(), BusError> {
        if self.watch.is_none() {
            if let Region::Dram { bank, base } = self.region(addr) {
                self.note_dram_write(bank, base + (addr & PAGE_MASK));
                self.dram[bank as usize][(base + (addr & PAGE_MASK)) as usize] = value;
                return Ok(());
            }
        }
        self.write8_slow(addr, value)
    }

    #[inline]
    fn write16(&mut self, addr: u32, value: u16) -> Result<(), BusError> {
        if self.watch.is_none() {
            if let Region::Dram { bank, base } = self.region(addr) {
                self.note_dram_write(bank, base + (addr & PAGE_MASK));
                put16(&mut self.dram[bank as usize], base + (addr & PAGE_MASK), value);
                return Ok(());
            }
        }
        self.write16_slow(addr, value)
    }

    #[inline]
    fn write32(&mut self, addr: u32, value: u32) -> Result<(), BusError> {
        if self.watch.is_none() {
            if let Region::Dram { bank, base } = self.region(addr) {
                self.note_dram_write(bank, base + (addr & PAGE_MASK));
                put32(&mut self.dram[bank as usize], base + (addr & PAGE_MASK), value);
                return Ok(());
            }
        }
        self.write32_slow(addr, value)
    }

    /// Instruction fetch: no watch logging (the watch is for data), so DRAM
    /// and ROM are always the fast path.
    #[inline]
    fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
        match self.region(addr) {
            Region::Dram { bank, base } => Ok(be32(&self.dram[bank as usize], base + (addr & PAGE_MASK))),
            Region::Rom { base } => Ok(be32(&self.rom, base + (addr & PAGE_MASK))),
            _ => self.read32_slow(addr),
        }
    }

    fn read_spr(&mut self, n: u16) -> Option<u32> {
        (n == spr::IMMR).then_some(self.mpc.immr)
    }

    // JIT support (docs ch. 17): plain DRAM and ROM can be accessed
    // directly and hold translated code; everything else goes through the
    // normal accessors.

    fn host_page(&mut self, pa: u32, write: bool) -> Option<*mut u8> {
        if self.watch.is_some() {
            return None; // every access must be seen and logged
        }
        let page_offset = pa & PAGE_MASK & !0xFFF;
        match self.region(pa) {
            Region::Dram { bank, base } => {
                let offset = base + page_offset;
                if write && self.code.is_watched(bank as u32 * DRAM_PAGES + (offset >> 12)) {
                    return None;
                }
                Some(self.dram[bank as usize][offset as usize..].as_mut_ptr())
            }
            Region::Rom { base } if !write => Some(self.rom[(base + page_offset) as usize..].as_mut_ptr()),
            _ => None,
        }
    }

    fn memory_map_epoch(&self) -> u64 {
        self.mpc.map_epoch
    }

    fn watch_code(&mut self, pa: u32, len: u32) -> Option<u32> {
        let page = match self.region(pa) {
            Region::Dram { bank, base } => bank as u32 * DRAM_PAGES + ((base + (pa & PAGE_MASK)) >> 12),
            Region::Rom { base } => ROM_PAGES_BASE + ((base + (pa & PAGE_MASK)) >> 12),
            _ => return None,
        };
        self.code.watch(page, pa & 0xFFF, len);
        Some(page)
    }

    fn take_code_write(&mut self) -> Option<u32> {
        self.code.take()
    }

    fn unwatch_code_pages(&mut self) {
        self.code.clear();
    }

    fn write_spr(&mut self, n: u16, value: u32) -> bool {
        if n == spr::IMMR {
            self.mpc.set_immr(value);
            return true;
        }
        false
    }

    fn external_interrupt(&self) -> bool {
        self.irq_line
    }
}
