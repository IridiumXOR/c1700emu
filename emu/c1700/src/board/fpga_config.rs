//! The FPGA configuration port behind IO FPGA register 0x4C.
//!
//! Found by sampling a hung IOS 12.4(8) boot on a 1760 with the GDB stub:
//! the PC sat in `0x80517d10: lbz r0,0x4C(r28)` / `andi. r9,r0,8` / `beq`
//! with r28 = 0x68020000. The routine around it (`0x80517c68`, arguments:
//! buffer, length; used on the 1760, never on the 1720) loads a **Xilinx
//! bitstream bit-serially** ("slave serial" mode), using one CPLD register
//! as the FPGA's configuration pins:
//!
//! | bit  | dir | Xilinx pin | how IOS uses it                                   |
//! |------|-----|------------|---------------------------------------------------|
//! | 0x40 | out | PROG_B     | pulsed low, `delay(250)`, high: clears the FPGA    |
//! | 0x20 | out | CCLK       | low, data, high: one rising edge per bit          |
//! | 0x10 | out | DIN        | the bit, MSB of each byte first                   |
//! | 0x08 | in  | INIT_B     | waited for (no timeout!) before sending data      |
//! | 0x02 | in  | DONE       | checked after the stream; low = error, retry (×4) |
//!
//! The 166 980-byte stream IOS sends (at 0x83426FD0 in 12.4(8)) is a
//! Spartan-II configuration: frame length register 0x11 (18 words = 576-bit
//! frames) and a total length that match an XC2S200.
//!
//! A real FPGA asserts DONE during its startup sequence, which begins with
//! the START command near the end of the stream; the filler words after it
//! provide the clocks. So the model finds the sync word, walks the packets
//! (so a START-looking word inside frame data can't fool it) and raises
//! DONE on `CMD = START`. CRCs aren't checked: there is no FPGA logic behind
//! it to break. Dynamips returns 0xFF for this register (INIT_B and DONE
//! permanently high), which also unblocks IOS but hides what happens.

pub const PROG_B: u8 = 0x40;
pub const CCLK: u8 = 0x20;
pub const DIN: u8 = 0x10;
pub const INIT_B: u8 = 0x08;
pub const DONE: u8 = 0x02;

const SYNC_WORD: u32 = 0xAA99_5566;
/// Configuration registers and commands (Xilinx XAPP151, Virtex/Spartan-II).
const REG_CMD: u32 = 4;
const REG_FLR: u32 = 11;
const REG_FDRI: u32 = 2;
const CMD_START: u32 = 5;
const CMD_DESYNCH: u32 = 0xD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packet {
    Header,
    Data { reg: u32, left: u32 },
}

pub struct FpgaConfigPort {
    /// Last value written: the CPU-driven pins (PROG_B, CCLK, DIN).
    latch: u8,
    done: bool,
    synced: bool,
    shift: u32,
    /// Bits of the current 32-bit word received since the sync word.
    word_bits: u32,
    packet: Packet,
    last_reg: u32,
    bits_since_prog: u64,
    frame_words: u64,
    flr: Option<u32>,
}

impl FpgaConfigPort {
    pub fn new() -> Self {
        FpgaConfigPort {
            latch: 0,
            done: false,
            synced: false,
            shift: 0,
            word_bits: 0,
            packet: Packet::Header,
            last_reg: 0,
            bits_since_prog: 0,
            frame_words: 0,
            flr: None,
        }
    }

    /// INIT_B is held low while PROG_B is low; clearing the configuration
    /// memory afterwards is instantaneous here.
    pub fn read(&self) -> u8 {
        let mut v = self.latch & !(INIT_B | DONE);
        if self.latch & PROG_B != 0 {
            v |= INIT_B;
        }
        if self.done {
            v |= DONE;
        }
        v
    }

    pub fn write(&mut self, value: u8) {
        let old = std::mem::replace(&mut self.latch, value);
        if value & PROG_B == 0 {
            if old & PROG_B != 0 || self.done || self.bits_since_prog != 0 {
                log::debug!(target: "fpga", "PROG_B low: FPGA configuration cleared");
            }
            *self = FpgaConfigPort { latch: value, ..FpgaConfigPort::new() };
            return;
        }
        if old & CCLK == 0 && value & CCLK != 0 {
            self.clock_in(value & DIN != 0);
        }
    }

    fn clock_in(&mut self, bit: bool) {
        self.bits_since_prog += 1;
        self.shift = (self.shift << 1) | bit as u32;
        if !self.synced {
            if self.shift == SYNC_WORD {
                self.synced = true;
                self.word_bits = 0;
                self.packet = Packet::Header;
            }
            return;
        }
        self.word_bits += 1;
        if self.word_bits == 32 {
            self.word_bits = 0;
            self.word(self.shift);
        }
    }

    /// One 32-bit word of the configuration stream after the sync word.
    fn word(&mut self, w: u32) {
        match self.packet {
            Packet::Header => match w >> 29 {
                1 => {
                    // type 1: register address in bits 26-13, word count in 10-0
                    let (reg, count) = ((w >> 13) & 0x3FFF, w & 0x7FF);
                    self.last_reg = reg;
                    if count > 0 {
                        self.packet = Packet::Data { reg, left: count };
                    }
                }
                2 => {
                    // type 2: long write to the register of the previous type 1
                    let count = w & 0x07FF_FFFF;
                    if count > 0 {
                        self.packet = Packet::Data { reg: self.last_reg, left: count };
                    }
                }
                _ => {} // dummy/filler words
            },
            Packet::Data { reg, left } => {
                match reg {
                    REG_FDRI => self.frame_words += 1,
                    REG_FLR => self.flr = Some(w),
                    REG_CMD if w == CMD_START => self.start_up(),
                    REG_CMD if w == CMD_DESYNCH => self.synced = false,
                    _ => {}
                }
                self.packet = if left > 1 { Packet::Data { reg, left: left - 1 } } else { Packet::Header };
            }
        }
    }

    fn start_up(&mut self) {
        self.done = true;
        log::info!(
            target: "fpga",
            "FPGA configured over IO FPGA reg 0x4C: {} bytes clocked in, frame length {} words, {} frame-data words, START -> DONE",
            self.bits_since_prog / 8,
            self.flr.map_or(0, |f| f + 1),
            self.frame_words
        );
    }
}

impl Default for FpgaConfigPort {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clock words in exactly like IOS's `0x80517c38`: CCLK low, set DIN, CCLK high.
    fn send(port: &mut FpgaConfigPort, words: &[u32]) {
        for &w in words {
            for i in (0..32).rev() {
                let base = port.read() & !(CCLK | DIN | INIT_B | DONE);
                port.write(base);
                let d = if (w >> i) & 1 != 0 { DIN } else { 0 };
                port.write(base | d);
                port.write(base | d | CCLK);
            }
        }
    }

    fn pulse_prog(port: &mut FpgaConfigPort) {
        port.write(PROG_B);
        port.write(0);
        assert_eq!(port.read() & INIT_B, 0, "INIT_B low while PROG_B is low");
        port.write(PROG_B);
        assert_ne!(port.read() & INIT_B, 0, "INIT_B high: ready for data");
    }

    #[test]
    fn done_rises_on_start_command_not_on_lookalike_data() {
        let mut p = FpgaConfigPort::new();
        pulse_prog(&mut p);
        send(
            &mut p,
            &[
                0xFFFF_FFFF, SYNC_WORD,
                0x3000_8001, 0x0000_0007, // CMD = RCRC
                0x3001_6001, 0x0000_0011, // FLR = 0x11
                0x3000_4000, 0x5000_0003, // FDRI, then type 2: 3 data words...
                0x3000_8001, 0x0000_0005, 0x0000_0000, // ...that look like CMD = START
            ],
        );
        assert_eq!(p.read() & DONE, 0, "a START pattern inside frame data is just data");
        send(&mut p, &[0x3000_8001, 0x0000_0005]);
        assert_ne!(p.read() & DONE, 0, "real CMD = START -> DONE");
        assert_eq!((p.flr, p.frame_words), (Some(0x11), 3));

        pulse_prog(&mut p);
        assert_eq!(p.read() & DONE, 0, "PROG_B clears the configuration");
    }

    #[test]
    fn no_done_without_sync_word() {
        let mut p = FpgaConfigPort::new();
        pulse_prog(&mut p);
        send(&mut p, &[0x3000_8001, 0x0000_0005, 0, 0]);
        assert_eq!(p.read() & DONE, 0);
    }
}
