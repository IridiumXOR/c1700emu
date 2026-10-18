//! Two byte-wide AMD Am29F016 side by side on the 16-bit bus (4 MB).
//!
//! The ROM's flash driver treats AMD parts as described in docs/04 §4.5:
//! AMD autoselect (unlock `0xAA @ 0x5555`, `0x55 @ 0x2AAA`, command
//! `0x90 @ 0x5555`, chip addresses) gives mandev 0x01AD = "AMD 29F016".
//! Chip 0 sits on the even byte lane (D0-D7), chip 1 on the odd lane; each
//! chip sees address `cpu_offset / 2`. Programming and erasing complete
//! instantly, so DQ7 polling loops see the final value on the first read.

use super::Storage;

const LANES: u32 = 2;

/// AMD command state of one chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChipState {
    Read,
    Unlock1,
    Unlock2,
    Autoselect,
    ProgramArmed,
    EraseSetup,
    EraseUnlock1,
    EraseUnlock2,
}

#[derive(Debug)]
pub struct AmdChipType {
    pub name: &'static str,
    pub manufacturer: u8,
    pub device: u8,
    pub size: u32,
    pub sector_size: u32,
}

pub const AM29F016: AmdChipType =
    AmdChipType { name: "AMD Am29F016", manufacturer: 0x01, device: 0xAD, size: 2 << 20, sector_size: 64 << 10 };

pub struct AmdFlash {
    pub(super) store: Storage,
    pub(super) chip: &'static AmdChipType,
    state: [ChipState; LANES as usize],
}

impl AmdFlash {
    /// Snapshot state (docs ch. 20): array contents and each chip's
    /// command state.
    pub(super) fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        const STATES: [ChipState; 8] = [
            ChipState::Read,
            ChipState::Unlock1,
            ChipState::Unlock2,
            ChipState::Autoselect,
            ChipState::ProgramArmed,
            ChipState::EraseSetup,
            ChipState::EraseUnlock1,
            ChipState::EraseUnlock2,
        ];
        v.bytes_fixed(&mut self.store.data);
        for state in self.state.iter_mut() {
            let mut tag = STATES.iter().position(|s| s == state).unwrap_or(0) as u8;
            v.u8(&mut tag);
            *state = STATES[tag as usize % STATES.len()];
        }
        if v.loading() {
            self.store.dirty = true;
        }
    }

    pub fn new(chip: &'static AmdChipType) -> Self {
        AmdFlash { store: Storage::new((chip.size * LANES) as usize), chip, state: [ChipState::Read; LANES as usize] }
    }

    fn size(&self) -> u32 {
        self.store.data.len() as u32
    }

    pub fn read8(&self, off: u32) -> u8 {
        let off = off % self.size();
        let lane = (off % LANES) as usize;
        let chip_addr = off / LANES;
        match self.state[lane] {
            ChipState::Autoselect => match chip_addr & 0xFF {
                0 => self.chip.manufacturer,
                1 => self.chip.device,
                _ => 0, // sector protection status: unprotected
            },
            _ => self.store.data[off as usize],
        }
    }

    pub fn read16(&self, off: u32) -> u16 {
        u16::from_be_bytes([self.read8(off), self.read8(off + 1)])
    }

    pub fn write16(&mut self, off: u32, value: u16) {
        let [hi, lo] = value.to_be_bytes();
        self.write8(off & !1, hi);
        self.write8(off | 1, lo);
    }

    /// A byte write reaches exactly one chip (the one on that byte lane).
    pub fn write8(&mut self, off: u32, value: u8) {
        let off = off % self.size();
        let lane = (off % LANES) as usize;
        let chip_addr = off / LANES;
        let cmd_addr = chip_addr & 0x7FF; // the Am29F016 decodes A0-A10 for commands
        use ChipState::*;
        let next = match (self.state[lane], cmd_addr, value) {
            (_, _, 0xF0) => Read, // "reset" works from any state
            (Read | Autoselect, 0x555, 0xAA) => Unlock1,
            (Unlock1, 0x2AA, 0x55) => Unlock2,
            (Unlock2, 0x555, 0x90) => Autoselect,
            (Unlock2, 0x555, 0xA0) => ProgramArmed,
            (Unlock2, 0x555, 0x80) => EraseSetup,
            (ProgramArmed, _, data) => {
                self.store.data[off as usize] &= data; // programming only clears bits
                self.store.dirty = true;
                Read
            }
            (EraseSetup, 0x555, 0xAA) => EraseUnlock1,
            (EraseUnlock1, 0x2AA, 0x55) => EraseUnlock2,
            (EraseUnlock2, 0x555, 0x10) => {
                log::info!("flash chip {lane}: chip erase");
                self.erase(lane, 0, self.chip.size);
                Read
            }
            (EraseUnlock2, _, 0x30) => {
                let sector = chip_addr / self.chip.sector_size * self.chip.sector_size;
                log::debug!("flash chip {lane}: erase sector at chip address {sector:#x}");
                self.erase(lane, sector, self.chip.sector_size);
                Read
            }
            (Autoselect, _, _) => Autoselect,
            (state, _, _) => {
                if state != Read || value != 0xFF {
                    log::debug!("flash chip {lane}: unexpected write {value:#04x} @ chip {chip_addr:#x} in {state:?}");
                }
                Read
            }
        };
        self.state[lane] = next;
    }

    fn erase(&mut self, lane: usize, start: u32, len: u32) {
        for chip_addr in start..start + len {
            let off = (chip_addr * LANES) as usize + lane;
            if let Some(b) = self.store.data.get_mut(off) {
                *b = 0xFF;
            }
        }
        self.store.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Flash;
    use super::*;

    /// Issue an AMD command to both chips, the way the ROM does on a 16-bit
    /// bus: unlock at chip addresses 0x5555/0x2AAA (CPU offsets x2).
    fn command(f: &mut Flash, cmd: u8) {
        f.write16(0x5555 * 2, 0xAAAA);
        f.write16(0x2AAA * 2, 0x5555);
        f.write16(0x5555 * 2, u16::from_be_bytes([cmd, cmd]));
    }

    #[test]
    fn autoselect_reports_two_am29f016() {
        let mut f = Flash::Amd(AmdFlash::new(&AM29F016));
        command(&mut f, 0x90);
        assert_eq!(f.read16(0), 0x0101); // manufacturer AMD on both lanes
        assert_eq!(f.read16(2), 0xADAD); // device Am29F016 on both lanes
        f.write16(0, 0xF0F0); // reset
        assert_eq!(f.read16(0), 0xFFFF);
    }

    #[test]
    fn program_only_clears_bits_and_erase_restores() {
        let mut f = Flash::Amd(AmdFlash::new(&AM29F016));
        command(&mut f, 0xA0);
        f.write16(0x100, 0x1234);
        assert_eq!(f.read16(0x100), 0x1234);
        command(&mut f, 0xA0);
        f.write16(0x100, 0xFF00);
        assert_eq!(f.read16(0x100), 0x1200);
        command(&mut f, 0x80);
        f.write16(0x5555 * 2, 0xAAAA);
        f.write16(0x2AAA * 2, 0x5555);
        f.write16(0x100, 0x3030);
        assert_eq!(f.read16(0x100), 0xFFFF);
        assert!(f.is_dirty());
    }

    #[test]
    fn content_persists_through_the_host_file() {
        let path = std::env::temp_dir().join(format!("c1700emu-flash-test-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut f = Flash::Amd(AmdFlash::new(&AM29F016));
            f.attach_file(path.clone()).unwrap();
            command(&mut f, 0xA0);
            f.write16(0x40, 0xBAD0);
        } // dropped here -> saved
        let mut g = Flash::Amd(AmdFlash::new(&AM29F016));
        g.attach_file(path.clone()).unwrap();
        assert_eq!(g.read16(0x40), 0xBAD0);
        std::fs::remove_file(&path).unwrap();
    }
}
