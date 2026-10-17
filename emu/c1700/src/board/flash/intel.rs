//! Intel StrataFlash (28F320J3 / 28F640J3 / 28F128J3) in x16 mode, one chip
//! per bank, banks stacked from 0x60000000.
//!
//! Why this part: Dynamips models the C1700 flash as "1x28F320 in word
//! mode" (ID 0x89/0x14); the ROM confirms the layout — its board hook says
//! "width 2" and rotates 32-bit ID reads by 16, which turns the x16 ID read
//! `0x0089_00DD` into mandev `0x89DD`, an entry of its chip table; and IOS
//! 12.4 knows "INTEL 28F128J3" (largest part it lists: 16 MB → two banks
//! for a 32 MB 1751-V/1760-V).
//!
//! Command set (Intel "Basic/Scalable Command Set", per chip, command code
//! in the low byte of a 16-bit write):
//!
//! | code | command | follow-up |
//! |------|---------|-----------|
//! | 0xFF | read array | |
//! | 0x90 | read identifier | word 0 = 0x0089, word 1 = device, block+2 = lock status |
//! | 0x98 | CFI query | "QRY" table |
//! | 0x70 | read status | SR bit 7 = ready |
//! | 0x50 | clear status | |
//! | 0x40/0x10 | word program | next write = data |
//! | 0xE8 | write to buffer | count-1, data words, 0xD0 confirm |
//! | 0x20 | block erase | 0xD0 at the block address |
//! | 0x60 | lock bits | 0x01 set / 0xD0 clear (accepted, ignored) |
//!
//! Program/erase finish instantly and leave the chip in read-status mode
//! (status = 0x80, ready, no errors), as on the real part after completion.

use super::Storage;

#[derive(Debug)]
pub struct IntelChipType {
    pub name: &'static str,
    pub device: u16,
    pub size: u32,
    pub block_size: u32,
}

pub const I28F320J3: IntelChipType = IntelChipType { name: "Intel 28F320J3", device: 0x0016, size: 4 << 20, block_size: 128 << 10 };
pub const I28F640J3: IntelChipType = IntelChipType { name: "Intel 28F640J3", device: 0x0017, size: 8 << 20, block_size: 128 << 10 };
pub const I28F128J3: IntelChipType = IntelChipType { name: "Intel 28F128J3", device: 0x0018, size: 16 << 20, block_size: 128 << 10 };

const MANUFACTURER_INTEL: u16 = 0x0089;
const SR_READY: u8 = 0x80;
const SR_ERASE_ERROR: u8 = 0x20;
const SR_PROGRAM_ERROR: u8 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    ReadArray,
    ReadId,
    ReadQuery,
    ReadStatus,
    /// After 0x40/0x10: the next write is the data word.
    Program,
    /// After 0xE8: reads return the extended status (buffer available), the
    /// next write is the word count minus one.
    BufferCount,
    BufferData { remaining: u16 },
    BufferConfirm,
    /// After 0x20: the next write must be 0xD0 at the block address.
    Erase,
    /// After 0x60: lock-bit command (0x01 / 0xD0 / 0x2F).
    Lock,
}

#[derive(Clone)]
pub(super) struct Bank {
    mode: Mode,
    status: u8,
    /// Pending write-buffer content: (byte offset in the array, word).
    buffer: Vec<(u32, u16)>,
}

pub struct IntelFlash {
    pub(super) store: Storage,
    pub(super) chip: &'static IntelChipType,
    pub(super) banks: Vec<Bank>,
}

impl IntelFlash {
    pub fn new(chip: &'static IntelChipType, banks: usize) -> Self {
        let bank = Bank { mode: Mode::ReadArray, status: SR_READY, buffer: Vec::new() };
        IntelFlash { store: Storage::new(chip.size as usize * banks), chip, banks: vec![bank; banks] }
    }

    fn size(&self) -> u32 {
        self.store.data.len() as u32
    }

    /// Split a CPU byte offset into (bank, word index inside the bank).
    fn locate(&self, off: u32) -> (usize, u32) {
        let off = off % self.size();
        ((off / self.chip.size) as usize, (off % self.chip.size) / 2)
    }

    fn array_word(&self, off: u32) -> u16 {
        let o = (off % self.size() & !1) as usize;
        u16::from_be_bytes([self.store.data[o], self.store.data[o + 1]])
    }

    pub fn read16(&self, off: u32) -> u16 {
        let (bank, word) = self.locate(off);
        let b = &self.banks[bank];
        match b.mode {
            Mode::ReadArray => self.array_word(off),
            Mode::ReadId => {
                let words_per_block = self.chip.block_size / 2;
                match word % words_per_block {
                    0 if word < words_per_block => MANUFACTURER_INTEL,
                    1 if word < words_per_block => self.chip.device,
                    _ => 0x0000, // block lock configuration: unlocked
                }
            }
            Mode::ReadQuery => self.cfi_word(word),
            Mode::BufferCount => SR_READY as u16, // XSR: write buffer available
            _ => b.status as u16,
        }
    }

    pub fn read8(&self, off: u32) -> u8 {
        let w = self.read16(off & !1);
        if off & 1 == 0 { (w >> 8) as u8 } else { w as u8 }
    }

    /// A byte write on an x16 part is only meaningful as a command.
    pub fn write8(&mut self, off: u32, value: u8) {
        self.write16(off & !1, value as u16);
    }

    pub fn write16(&mut self, off: u32, value: u16) {
        let (bank, _) = self.locate(off);
        let cmd = value as u8;
        let mode = self.banks[bank].mode;
        let next = match mode {
            Mode::Program => {
                self.program(off, value);
                Mode::ReadStatus
            }
            Mode::BufferCount => {
                let count = (value & 0x1F) + 1; // words to follow
                self.banks[bank].buffer.clear();
                Mode::BufferData { remaining: count }
            }
            Mode::BufferData { remaining } => {
                self.banks[bank].buffer.push((off, value));
                if remaining <= 1 { Mode::BufferConfirm } else { Mode::BufferData { remaining: remaining - 1 } }
            }
            Mode::BufferConfirm => {
                if cmd == 0xD0 {
                    let words = std::mem::take(&mut self.banks[bank].buffer);
                    for (o, w) in words {
                        self.program(o, w);
                    }
                    self.banks[bank].status = SR_READY;
                } else {
                    self.banks[bank].status = SR_READY | SR_ERASE_ERROR | SR_PROGRAM_ERROR; // sequence error
                }
                Mode::ReadStatus
            }
            Mode::Erase => {
                if cmd == 0xD0 {
                    self.erase_block(off);
                    self.banks[bank].status = SR_READY;
                } else {
                    self.banks[bank].status = SR_READY | SR_ERASE_ERROR | SR_PROGRAM_ERROR;
                }
                Mode::ReadStatus
            }
            Mode::Lock => Mode::ReadStatus,
            _ => self.command(bank, cmd, mode),
        };
        self.banks[bank].mode = next;
    }

    fn command(&mut self, bank: usize, cmd: u8, mode: Mode) -> Mode {
        match cmd {
            0xFF => Mode::ReadArray,
            0x90 => Mode::ReadId,
            0x98 => Mode::ReadQuery,
            0x70 => Mode::ReadStatus,
            0x50 => {
                self.banks[bank].status = SR_READY;
                mode
            }
            0x40 | 0x10 => Mode::Program,
            0xE8 => Mode::BufferCount,
            0x20 => Mode::Erase,
            0x60 => Mode::Lock,
            0xB0 | 0xD0 => mode, // suspend/resume: nothing is ever in progress
            other => {
                log::debug!("intel flash bank {bank}: unknown command {other:#04x}");
                mode
            }
        }
    }

    fn program(&mut self, off: u32, value: u16) {
        let o = (off % self.size() & !1) as usize;
        let [hi, lo] = value.to_be_bytes();
        self.store.data[o] &= hi; // programming can only turn 1s into 0s
        self.store.data[o + 1] &= lo;
        self.store.dirty = true;
    }

    fn erase_block(&mut self, off: u32) {
        let off = off % self.size();
        let start = off / self.chip.block_size * self.chip.block_size;
        log::debug!("intel flash: erase block at {:#x}", 0x6000_0000 + start);
        self.store.data[start as usize..(start + self.chip.block_size) as usize].fill(0xFF);
        self.store.dirty = true;
    }

    /// Minimal CFI table (JEDEC JESD68): enough to identify the part.
    fn cfi_word(&self, word: u32) -> u16 {
        let size_log2 = 31 - self.chip.size.leading_zeros();
        let blocks = self.chip.size / self.chip.block_size;
        match word {
            0x10 => b'Q' as u16,
            0x11 => b'R' as u16,
            0x12 => b'Y' as u16,
            0x13 => 0x0001, // primary command set: Intel/Sharp extended
            0x15 => 0x0031,
            0x27 => size_log2 as u16,
            0x28 => 0x0002, // x8/x16 interface
            0x2A => 0x0005, // max buffer write = 2^5 = 32 bytes
            0x2C => 0x0001, // one erase block region
            0x2D => ((blocks - 1) & 0xFF) as u16,
            0x2E => ((blocks - 1) >> 8) as u16,
            0x2F => ((self.chip.block_size >> 8) & 0xFF) as u16,
            0x30 => (self.chip.block_size >> 16) as u16,
            _ => 0x0000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Flash;
    use super::*;

    /// The ROM's ID read on this board: one 32-bit read at the bank base,
    /// rotated by 16, gives mandev = (low byte << 8) | bits 16-23.
    #[test]
    fn rom_style_id_read_gives_its_table_code() {
        let mut f = Flash::Intel(IntelFlash::new(&I28F128J3, 2));
        f.write16(0, 0x9090);
        let v4 = ((f.read16(0) as u32) << 16) | f.read16(2) as u32;
        let v6 = v4.rotate_left(16);
        let mandev = ((v6 & 0xFF) << 8) | ((v6 >> 16) & 0xFF);
        assert_eq!(mandev, 0x8918, "INTEL 28F128J3A in the ROM's table");
        f.write16(0, 0x00FF);
        assert_eq!(f.read16(0), 0xFFFF);
    }

    #[test]
    fn word_program_buffer_program_and_block_erase() {
        let mut f = Flash::Intel(IntelFlash::new(&I28F128J3, 2));
        let bank1 = 16 << 20;
        f.write16(bank1 + 0x10, 0x0040);
        f.write16(bank1 + 0x10, 0x1234);
        assert_eq!(f.read16(bank1), 0x0080, "status: ready");
        f.write16(bank1, 0x00FF);
        assert_eq!(f.read16(bank1 + 0x10), 0x1234);
        // write-to-buffer: 2 words
        f.write16(bank1 + 0x20, 0x00E8);
        assert_eq!(f.read16(bank1 + 0x20) & 0x80, 0x80, "buffer available");
        f.write16(bank1 + 0x20, 0x0001);
        f.write16(bank1 + 0x20, 0xCAFE);
        f.write16(bank1 + 0x22, 0xBABE);
        f.write16(bank1 + 0x20, 0x00D0);
        f.write16(bank1, 0x00FF);
        assert_eq!((f.read16(bank1 + 0x20), f.read16(bank1 + 0x22)), (0xCAFE, 0xBABE));
        // bank 0 untouched by bank 1's commands
        assert_eq!(f.read16(0x10), 0xFFFF);
        // erase the 128 KB block
        f.write16(bank1 + 0x20, 0x0020);
        f.write16(bank1 + 0x20, 0x00D0);
        f.write16(bank1, 0x00FF);
        assert_eq!(f.read16(bank1 + 0x10), 0xFFFF);
    }
}
