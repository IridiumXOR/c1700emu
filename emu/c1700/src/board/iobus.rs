//! The 8-bit "I/O bus" behind chip select 7 (0x68000000, 1 MB window).
//!
//! Sub-decoding by address bits 16-19, recovered from the ROM's accesses:
//!
//! | offset            | device                                   |
//! |-------------------|------------------------------------------|
//! | 0x00000 - 0x1FFFF | NVRAM (mirrored every NVRAM size)        |
//! | 0x20000 - 0x2FFFF | CPLD / IO FPGA registers (256 bytes, mirrored) |
//! | 0x30000, 0x40000, 0x80000, 0xB0000 | WIC/VIC slots 0, 1, 2, 3: empty |
//! | 0x50000 - 0x5FFFF | NS16552 dual UART: channel A = console at +0..7, channel B = AUX at +8..15 |
//!
//! **Slots** (docs ch. 19, from IOS 12.4(8)): a 1720/1721 has 2, a
//! 1750/1751 3, a 1760 4 (slot 2 at 0x80000, slot 3 at 0xB0000). IOS tests
//! each slot for a card with its own logic (a voice card) by writing 0x05 to
//! offset 0 and expecting 0x0A back, and polls a status register at offset
//! 5. Cards are identified by their ID EEPROM over SPI (`cpm_spi.rs`). No
//! card is fitted: an empty slot drives nothing, so reads float to 0xFF
//! (the presence test fails, as on a real router) and writes vanish.
//!
//! Anything else is logged once per 64 KB slot and reads as 0xFF, which is
//! what an undriven 8-bit bus returns on this kind of board.

use super::cpld::Cpld;
use super::nvram::Nvram;
use super::uart16550::Uart16550;
use std::collections::{BTreeMap, HashSet};

pub struct IoBus {
    pub nvram: Nvram,
    pub cpld: Cpld,
    /// DUART channel A: the console port.
    pub uart: Uart16550,
    /// DUART channel B: the AUX port (Dynamips: channel B at +8; IOS
    /// configures it — before it existed, those writes landed on the console).
    pub aux: Uart16550,
    /// (reads, writes) per DUART channel, for `--stats`.
    pub duart_access: [(u64, u64); 2],
    /// (reads, writes, last value written) per WIC/VIC slot, for `--stats`.
    pub slot_access: [(u64, u64, u8); 4],
    warned: HashSet<u32>,
    /// Accesses to devices we don't model: offset -> (reads, writes, last value written).
    pub unknown: BTreeMap<u32, (u64, u64, u8)>,
}

impl IoBus {
    /// Snapshot state (docs ch. 20).
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        self.nvram.visit_state(v);
        self.cpld.visit_state(v);
        self.uart.visit_state(v);
        self.aux.visit_state(v);
    }

    pub fn new(nvram: Nvram, board_id: u8) -> Self {
        IoBus {
            nvram,
            cpld: Cpld::new(board_id),
            uart: Uart16550::new(),
            aux: Uart16550::new(),
            duart_access: [(0, 0); 2],
            slot_access: [(0, 0, 0); 4],
            warned: HashSet::new(),
            unknown: BTreeMap::new(),
        }
    }

    pub fn power_on_reset(&mut self) {
        self.cpld.power_on_reset();
        self.uart.reset();
        self.aux.reset();
    }

    pub fn hard_reset(&mut self) {
        self.cpld.hard_reset();
        self.uart.reset();
        self.aux.reset();
    }

    pub fn read8(&mut self, off: u32) -> u8 {
        match off >> 16 {
            0x0 | 0x1 => self.nvram.read8(off),
            0x2 => self.cpld.read8(off & 0xFF),
            0x5 if off & 8 == 0 => {
                self.duart_access[0].0 += 1;
                self.uart.read(off & 7)
            }
            0x5 => {
                self.duart_access[1].0 += 1;
                self.aux.read(off & 7)
            }
            window if let Some(slot) = wic_slot(window) => {
                self.slot_access[slot].0 += 1;
                0xFF // empty slot: the bus floats
            }
            slot => {
                self.warn(slot, off, "read");
                self.unknown.entry(off).or_default().0 += 1;
                0xFF
            }
        }
    }

    pub fn write8(&mut self, off: u32, value: u8) {
        match off >> 16 {
            0x0 | 0x1 => self.nvram.write8(off, value),
            0x2 => self.cpld.write8(off & 0xFF, value),
            0x5 if off & 8 == 0 => {
                self.duart_access[0].1 += 1;
                self.uart.write(off & 7, value)
            }
            0x5 => {
                self.duart_access[1].1 += 1;
                self.aux.write(off & 7, value)
            }
            window if let Some(slot) = wic_slot(window) => {
                let e = &mut self.slot_access[slot];
                e.1 += 1;
                e.2 = value; // empty slot: nobody latches it
            }
            slot => {
                self.warn(slot, off, "write");
                let e = self.unknown.entry(off).or_default();
                e.1 += 1;
                e.2 = value;
            }
        }
    }

    /// Side-effect-free read for the debugger.
    pub fn peek8(&self, off: u32) -> u8 {
        match off >> 16 {
            0x0 | 0x1 => self.nvram.read8(off),
            0x2 => self.cpld.peek8(off & 0xFF),
            0x5 if off & 8 == 0 => self.uart.peek(off & 7),
            0x5 => self.aux.peek(off & 7),
            _ => 0xFF,
        }
    }

    fn warn(&mut self, slot: u32, off: u32, what: &str) {
        debug_assert!(wic_slot(slot).is_none());
        if self.warned.insert(slot) {
            log::warn!("I/O bus: {what} of unknown device at {:#010x}", 0x6800_0000 + off);
        }
    }
}

/// The WIC/VIC slot whose 64 KB window is `window` (address bits 16-19).
fn wic_slot(window: u32) -> Option<usize> {
    match window {
        0x3 => Some(0),
        0x4 => Some(1),
        0x8 => Some(2),
        0xB => Some(3),
        _ => None,
    }
}
