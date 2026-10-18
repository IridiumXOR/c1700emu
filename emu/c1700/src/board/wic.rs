//! WAN interface cards in the WIC slots (docs ch. 23).
//!
//! A data card identifies itself through a serial ID EEPROM on the
//! board's SPI bus. IO FPGA register 0x18 (0x0A for the 1760's slot 3)
//! holds a 2-bit device select per slot, from IOS's table at 0x8268A74C:
//!
//! | slot | register | field mask | EEPROM selected | idle |
//! |---|---|---|---|---|
//! | 0 | 0x18 | 0x60 | 0x60 | 0x40 |
//! | 1 | 0x18 | 0x18 | 0x18 | 0x10 |
//! | 2 | 0x18 | 0x84 | 0x84 | 0x80 |
//! | 3 | 0x0A | 0x30 | 0x30 | 0x20 |
//!
//! IOS's smart init (`c1700_smart_init.c`, 0x80140E2C) reads 64 words with
//! the EEPROM's READ command: `03 <byte address> 00 00`, the last two bytes
//! clocking out one big-endian word. The contents use Cisco's old ID PROM
//! layout (format version 1), as `show diag` prints it (0x80516F04). A card
//! is only looked at if IO FPGA register 0x10 says the slot is occupied
//! (`cpld::CARD_PRESENT`).
//!
//! A serial card also has registers in its slot window (IOS's
//! `sub_8050E868`; a second port, on a 2T, at +0x14/+0x18):
//!
//! * **+4**: bits 0-1 select one of four cable-ID pins, read back in bit
//!   0x20 (`sub_8050E33C` reads them MSB first); bits 2-4 strobe the line
//!   transceivers' modes.
//! * **+8**: bit 0 = "cable changed" latch, bits 1-2 control.
//!
//! The cable ID (`sub_8050E4A4`): bit 0 set = DCE cable; the rest is the
//! interface: 2 V.24 (RS-232), 4 V.35, 6 V.11 (RS-449), 8 V.11 (X.21),
//! 14 EIA-530, 16 530A; 0x01 = "No serial cable attached". The modem
//! signals go to MPC860 port pins (`Board::update_wic_pins`).

/// The card types the emulator can fit (`--wic0 wic-1t`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum WicKind {
    /// One synchronous serial port (card type 0x02): "WAN Interface Card -
    /// Serial (1T)", served by the CPM's SCC1 (slot 0) or SCC2 (slot 1).
    #[value(name = "wic-1t")]
    Wic1T,
}

impl WicKind {
    /// The card type byte of the ID EEPROM (IOS's switch at 0x80502C88).
    pub fn card_type(self) -> u8 {
        match self {
            WicKind::Wic1T => 0x02,
        }
    }
}

/// What a serial card's port is cabled to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cable {
    None,
    /// A V.35 DTE cable (the other end provides the clock).
    V35Dte,
}

impl Cable {
    /// The 4-bit cable ID the card reports.
    fn id(self) -> u8 {
        match self {
            Cable::None => 0x01,
            Cable::V35Dte => 0x04,
        }
    }
}

/// A card in a slot.
pub struct Wic {
    pub kind: WicKind,
    eeprom: [u8; 128],
    pub cable: Cable,
    /// The window registers software writes (latches).
    regs: [u8; 0x20],
}

impl Wic {
    pub fn new(kind: WicKind, slot: usize) -> Self {
        let mut e = [0xFF; 128];
        e[0] = 0x01; // format version 1
        e[1] = kind.card_type();
        e[2..4].copy_from_slice(&[1, 0]); // hardware revision 1.0
        e[4..8].copy_from_slice(&(20_041_000 + slot as u32).to_be_bytes()); // serial number
        e[8] = 73; // part number 73-2834-07, the WIC-1T assembly
        e[9..11].copy_from_slice(&2834u16.to_be_bytes());
        e[11] = 7;
        e[12] = 0x00; // test history
        e[13..16].copy_from_slice(&[0, 0, 0]); // RMA number
        e[16] = 0x50; // board revision A0
        e[23] = 0x01; // connector type: WAN module
        Wic { kind, eeprom: e, cable: Cable::None, regs: [0; 0x20] }
    }

    /// A read in the card's slot window.
    pub fn read8(&self, off: u32) -> u8 {
        let off = off as usize & 0xFFFF;
        match off {
            // Cable-ID mux: the pin selected by bits 0-1, in bit 0x20.
            4 => {
                let latch = self.regs[4];
                let pin = (self.cable.id() >> (latch & 3)) & 1;
                latch & !0x20 | pin << 5
            }
            8 => self.regs[8] & !1, // no cable change to report
            // Offset 0 is the voice-card presence test (0x05 -> 0x0A): a data
            // card has no logic there and the bus floats.
            _ => 0xFF,
        }
    }

    pub fn write8(&mut self, off: u32, value: u8) {
        if let Some(r) = self.regs.get_mut(off as usize & 0xFFFF) {
            *r = value;
        }
    }

    /// Snapshot state: the window latches (the cable is configuration).
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        v.bytes_fixed(&mut self.regs);
    }

    /// The bytes clocked back while this card's EEPROM is selected and
    /// the board sends `tx`: after a READ command (0x03) and a byte address,
    /// the EEPROM shifts out its contents from that address on.
    pub fn eeprom_transfer(&self, tx: &[u8]) -> Vec<u8> {
        let mut rx = vec![0xFF; tx.len()];
        if tx.len() >= 2 && tx[0] == 0x03 {
            let addr = tx[1] as usize;
            for (i, b) in rx.iter_mut().enumerate().skip(2) {
                *b = self.eeprom[(addr + i - 2) % self.eeprom.len()];
            }
        }
        rx
    }
}

/// Which slot's EEPROM the IO FPGA select registers (0x18, 0x0A) address.
pub fn selected_eeprom(reg18: u8, reg0a: u8) -> Option<usize> {
    [(reg18, 0x60), (reg18, 0x18), (reg18, 0x84), (reg0a, 0x30)]
        .iter()
        .position(|&(reg, mask)| reg & mask == mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IOS's read sequence for word 0 and word 8 of a WIC-1T in slot 0.
    #[test]
    fn eeprom_answers_ios_read_commands() {
        let wic = Wic::new(WicKind::Wic1T, 0);
        assert_eq!(wic.eeprom_transfer(&[0x03, 0x00, 0, 0]), vec![0xFF, 0xFF, 0x01, 0x02]);
        assert_eq!(wic.eeprom_transfer(&[0x03, 0x08, 0, 0])[2..], [73, 0x0B]);
        assert_eq!(wic.eeprom_transfer(&[0x05, 0x00, 0, 0]), vec![0xFF; 4], "not a READ");
    }

    /// IOS's cable sensing (`sub_8050E33C`): select pins 3..0, read bit 0x20.
    #[test]
    fn cable_id_reads_through_the_mux() {
        let mut wic = Wic::new(WicKind::Wic1T, 0);
        let read_id = |wic: &mut Wic| {
            (0..4).rev().fold(0, |id, i| {
                wic.write8(4, i);
                id << 1 | (wic.read8(4) >> 5 & 1)
            })
        };
        assert_eq!(read_id(&mut wic), 0x01, "no cable");
        wic.cable = Cable::V35Dte;
        assert_eq!(read_id(&mut wic), 0x04, "V.35 DTE");
        assert_eq!(wic.read8(0), 0xFF, "no voice logic at offset 0");
    }

    #[test]
    fn select_registers_pick_the_slot() {
        assert_eq!(selected_eeprom(0x60 | 0x10, 0), Some(0));
        assert_eq!(selected_eeprom(0x40 | 0x18, 0), Some(1));
        assert_eq!(selected_eeprom(0xD4, 0), Some(2));
        assert_eq!(selected_eeprom(0x40 | 0x10 | 0x80, 0x30), Some(3));
        assert_eq!(selected_eeprom(0x40 | 0x10 | 0x80, 0x20), None);
    }
}
