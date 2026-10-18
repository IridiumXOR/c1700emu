//! The board CPLD at 0x68020000 (8-bit registers).
//!
//! Only partially understood. From the ROM (R) and IOS 12.4(8) on a 1760
//! (I, docs ch. 19):
//!
//! | reg  | known use                                                     |
//! |------|---------------------------------------------------------------|
//! | 0x00 | bit 7: system LED (blinked N times by `fatal_error_blink`) (R) |
//! | 0x04 | bits 0-2: board type (ROM special-cases 2, 3 and 4) (R)       |
//! | 0x0A | slot 3 (1760): bit 0 reset pulse, device select field (I)     |
//! | 0x0C | written once at init: 0 on 1751/1760 (I), meaning unknown     |
//! | 0x10 | card present (inputs): bit 1 slot 0, bit 2 slot 1, bit 0 slot 2, bit 5 slot 3 (I, `sub_806293D8`); bit 3: VPN module fitted (IOS reserves its buffers) (I) |
//! | 0x14 | bit 0: NVRAM write protect, bit 2: flag set at every boot (R); bits 6/7: voice card in slot 0/1 (I) |
//! | 0x18 | bit 0: set right before the CPM is reset (R); slots 1/2 reset pulse (bits 0/1) and slot 0-2 device selects, incl. the WIC ID EEPROM selects 0x08/0x20 (I) |
//! | 0x1C | bit 5: slot 0 reset pulse (I)                                 |
//! | 0x20 | bits 6/7: read when choosing the FPGA image size (I)          |
//! | 0x24 | initialised to 0xFF (R)                                       |
//! | 0x48 | bit 3: second LED on board type 2; initialised to 0xD8 (R); voice TDM: TSA routing nibbles (I) |
//! | 0x4C | FPGA configuration port (IOS on the 1760), see `fpga_config`  |
//! | 0x5C | voice TDM control: port bits 0x08/0x10, 0x20 enable, 0x80 on the 1760 (I) |
//! | 0x6C | DSP time-slot programming strobe (slot \| 0x40), 0x80 at init (I) |
//! | 0x70, 0x74 | DSP time-slot masks (I)                                |
//!
//! Everything else behaves as a read/write latch, which is all the slot
//! and voice registers need without cards or DSPs fitted.

use super::fpga_config::FpgaConfigPort;

pub struct Cpld {
    regs: [u8; 256],
    board_id: u8,
    /// Card-present inputs of register 0x10 (`CARD_PRESENT` bits).
    cards_present: u8,
    fpga: FpgaConfigPort,
    /// (reads, writes) per register, for `--stats`.
    pub access_counts: Box<[(u64, u64); 256]>,
}

const REG_LED: usize = 0x00;
const REG_BOARD_ID: usize = 0x04;
const REG_FPGA_CONFIG: usize = 0x4C;
const REG_CARDS: usize = 0x10;

/// Register 0x10 bit that reads 1 when WIC/VIC slot 0..3 holds a card.
pub const CARD_PRESENT: [u8; 4] = [0x02, 0x04, 0x01, 0x20];
const CARD_PRESENT_MASK: u8 = 0x27;

impl Cpld {
    /// Snapshot state (docs ch. 20). Access counters aren't state.
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        v.bytes_fixed(&mut self.regs);
        v.u8(&mut self.board_id);
        self.fpga.visit_state(v);
    }

    pub fn new(board_id: u8) -> Self {
        let mut c = Cpld {
            regs: [0; 256],
            board_id: board_id & 7,
            cards_present: 0,
            fpga: FpgaConfigPort::new(),
            access_counts: Box::new([(0, 0); 256]),
        };
        c.power_on_reset();
        c
    }

    pub fn power_on_reset(&mut self) {
        self.regs = [0; 256];
        self.fpga = FpgaConfigPort::new();
        self.hard_reset();
    }

    /// Hard reset keeps the latches (bit 2 of reg 0x14 is the ROM's way of
    /// telling a warm restart from a power-on).
    pub fn hard_reset(&mut self) {
        self.regs[REG_BOARD_ID] = self.board_id;
    }

    pub fn read8(&mut self, reg: u32) -> u8 {
        self.access_counts[reg as usize & 0xFF].0 += 1;
        self.peek8(reg)
    }

    /// The cards fitted in the slots (a `CARD_PRESENT` bit each).
    pub fn set_cards_present(&mut self, bits: u8) {
        self.cards_present = bits & CARD_PRESENT_MASK;
    }

    /// Read without counting (debugger).
    pub fn peek8(&self, reg: u32) -> u8 {
        match reg as usize & 0xFF {
            REG_FPGA_CONFIG => self.fpga.read(),
            REG_CARDS => self.regs[REG_CARDS] & !CARD_PRESENT_MASK | self.cards_present,
            r => self.regs[r],
        }
    }

    /// Registers that were accessed: (reg, reads, writes, current value).
    pub fn used_registers(&self) -> Vec<(usize, u64, u64, u8)> {
        (0..256)
            .filter(|&r| self.access_counts[r] != (0, 0))
            .map(|r| (r, self.access_counts[r].0, self.access_counts[r].1, self.peek8(r as u32)))
            .collect()
    }

    pub fn write8(&mut self, reg: u32, value: u8) {
        let reg = reg as usize & 0xFF;
        self.access_counts[reg].1 += 1;
        match reg {
            REG_BOARD_ID => {} // read-only straps
            REG_FPGA_CONFIG => {
                self.regs[reg] = value;
                self.fpga.write(value);
            }
            REG_LED => {
                if (self.regs[REG_LED] ^ value) & 0x80 != 0 {
                    log::trace!("system LED {}", if value & 0x80 != 0 { "off" } else { "on" });
                }
                self.regs[reg] = value;
            }
            _ => {
                log::trace!("CPLD[{reg:#04x}] = {value:#04x}");
                self.regs[reg] = value;
            }
        }
    }
}
