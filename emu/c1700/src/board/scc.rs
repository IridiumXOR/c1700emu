//! The CPM's serial communication controllers in HDLC mode (MPC860UM
//! chapters 21 and 23; docs ch. 23): what the WICs' serial ports run on.
//! WIC slot 0's first port is SCC1, slot 1's SCC2; a 2T's second ports
//! are SCC4 (slot 0) and SCC3 (slot 1) (IOS's `wic_slot_scc_channels`).
//!
//! The registers (GSMR, PSMR, TODR, SCCE, SCCM) live in the MPC860 model;
//! this is the engine, on the board because it needs memory access, like
//! the SPI (`cpm_spi.rs`) and the FEC (`fec_dma.rs`):
//!
//! * **Transmit**: the SCC walks its TxBD ring from TBPTR and sends every
//!   frame whose buffers are all ready (up to the one with L), closes the
//!   buffers and sets SCCE[TXB] for those with I. It runs when software
//!   writes TODR[TOD] (transmit on demand), enables the transmitter or
//!   issues RESTART TRANSMIT, and at every device sync (the hardware polls
//!   every 64 clocks). Frames go to the port's link without flags or CRC.
//! * **Receive**: a frame from the link is written to the RxBDs from
//!   RBPTR, MRBLR bytes per buffer, followed by its CRC (2 or 4 bytes, as
//!   PSMR[CRC] selects), which the length of the last buffer includes, as
//!   on the hardware. SCCE[RXF] is set every RFTHR frames, SCCE[BSY] when
//!   no buffer is free.
//! * **Local loopback** (GSMR_L[DIAG] = 01): sent frames come back to the
//!   same SCC's receiver.

use super::Board;
use super::cpm_spi::bd_addr;
use crate::net::host::HostLink;
use ppc8xx::Platform;

const BD_READY_OR_EMPTY: u16 = 0x8000; // TxBD R / RxBD E
const BD_WRAP: u16 = 0x2000;
const BD_INTERRUPT: u16 = 0x1000;
const BD_LAST: u16 = 0x0800;
const BD_FIRST: u16 = 0x0400; // RxBD F (TxBD TC: append the CRC)
const BD_CONTINUOUS: u16 = 0x0200;
/// TxBD error bits (underrun, CTS lost) and RxBD error/status bits.
const TXBD_ERRORS: u16 = 0x0003;
const RXBD_STATUS: u16 = 0x00FF;
const RXBD_LG: u16 = 0x0020;

/// SCCE/SCCM bits in HDLC mode (MPC860UM Figure 23-7).
pub const SCCE_GRA: u16 = 0x0080;
const SCCE_RXF: u16 = 0x0008;
const SCCE_BSY: u16 = 0x0004;
const SCCE_TXB: u16 = 0x0002;
const SCCE_RXB: u16 = 0x0001;

/// GSMR_L fields: MODE (0 = HDLC), ENT, ENR, DIAG.
const GSMR_L_MODE: u32 = 0x0000_000F;
const GSMR_L_ENT: u32 = 0x0000_0010;
const GSMR_L_ENR: u32 = 0x0000_0020;
const GSMR_L_DIAG: u32 = 0x0000_00C0;
const DIAG_LOCAL_LOOPBACK: u32 = 0x0000_0040;
/// PSMR[CRC] = 10: 32-bit CRC.
const PSMR_CRC32: u16 = 0x0800;

/// SCC register block n (0-3) in the IMMR window, and its parameter RAM.
pub fn regs(n: usize) -> u32 {
    0xA00 + 0x20 * n as u32
}
fn param(n: usize) -> u32 {
    0x3C00 + 0x100 * n as u32
}

// Parameter RAM offsets (common part, MPC860UM Table 21-?; HDLC part, Table 23-1).
const RBASE: u32 = 0x00;
const TBASE: u32 = 0x02;
const MRBLR: u32 = 0x06;
const RBPTR: u32 = 0x10;
const TBPTR: u32 = 0x20;
const DISFC: u32 = 0x3C;
const NMARC: u32 = 0x42;
const MFLR: u32 = 0x46;
const RFTHR: u32 = 0x4A;
const RFCNT: u32 = 0x4C;
const HMASK: u32 = 0x4E;
const HADDR1: u32 = 0x50;

/// What the board keeps per SCC besides its registers.
#[derive(Default)]
pub struct SccState {
    /// STOP TRANSMIT or GRACEFUL STOP TRANSMIT was issued (until RESTART).
    pub tx_stopped: bool,
    /// Frames sent and received, for `--stats`.
    pub frames_out: u64,
    pub frames_in: u64,
}

impl SccState {
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        v.bool(&mut self.tx_stopped);
    }
}

/// CRC-16/X.25 (HDLC's FCS-16), as sent: complemented, low byte first.
fn fcs16(data: &[u8]) -> [u8; 2] {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    (!crc).to_le_bytes()
}

impl Board {
    /// SCC frame counts for `--stats` (SCCs that moved frames only).
    pub fn scc_report(&self) -> String {
        let mut out = String::new();
        for (n, s) in self.scc.iter().enumerate() {
            if s.frames_out + s.frames_in > 0 {
                let link = self.serial.get(n).and_then(|l| l.as_ref()).map_or("no cable".into(), |l| l.what.clone());
                out += &format!("SCC{} (HDLC, {link}): {} frames sent, {} received\r\n", n + 1, s.frames_out, s.frames_in);
            }
        }
        out
    }

    /// The link of SCC n's serial port.
    fn scc_link(&mut self, n: usize) -> Option<&mut HostLink> {
        self.serial.get_mut(n).and_then(|l| l.as_mut())
    }

    /// Run the SCCs: send what their TxBD rings hold, deliver frames that
    /// arrived on their links.
    pub(super) fn scc_service(&mut self) {
        for n in 0..self.scc.len() {
            self.scc_transmit(n);
            if self.scc_link(n).is_some() {
                let link = self.scc_link(n).unwrap();
                link.poll();
                while let Some(frame) = self.scc_link(n).unwrap().pop_for_guest() {
                    self.scc_receive(n, &frame);
                }
            }
        }
    }

    /// CPCR transmit commands for SCC n (MPC860UM Table 23-2). Frames go
    /// out instantly, so a graceful stop never waits for one.
    pub(super) fn scc_command(&mut self, n: usize, opcode: u8) {
        match opcode {
            4 => self.scc[n].tx_stopped = true, // STOP TRANSMIT
            5 => {
                // GRACEFUL STOP TRANSMIT
                self.scc[n].tx_stopped = true;
                self.mpc.scc_event(n, SCCE_GRA);
            }
            6 => {
                // RESTART TRANSMIT
                self.scc[n].tx_stopped = false;
                self.scc_transmit(n);
            }
            _ => {}
        }
    }

    fn scc_transmit(&mut self, n: usize) {
        let gsmr_l = self.mpc.load32(regs(n));
        if gsmr_l & GSMR_L_MODE != 0 || gsmr_l & GSMR_L_ENT == 0 || self.scc[n].tx_stopped {
            return;
        }
        let p = param(n);
        let tbase = self.mpc.load16(p + TBASE);
        let mut tbptr = self.mpc.load16(p + TBPTR);
        // At most one ring's worth of frames per call.
        for _ in 0..64 {
            // Only a frame whose buffers are all ready is sent.
            let mut bds = Vec::new();
            let mut cursor = tbptr;
            loop {
                let status = self.mpc.load16(bd_addr(cursor));
                if status & BD_READY_OR_EMPTY == 0 || bds.len() >= 64 {
                    bds.clear();
                    break;
                }
                bds.push((cursor, status));
                cursor = if status & BD_WRAP != 0 { tbase } else { cursor.wrapping_add(8) };
                if status & BD_LAST != 0 {
                    break;
                }
            }
            if bds.is_empty() {
                break;
            }
            let mut frame = Vec::new();
            for &(bd, status) in &bds {
                let a = bd_addr(bd);
                let len = self.mpc.load16(a + 2) as u32;
                let ptr = self.mpc.load32(a + 4);
                frame.extend((0..len).map(|i| self.read8(ptr.wrapping_add(i)).unwrap_or(0xFF)));
                let keep = if status & BD_CONTINUOUS != 0 { BD_READY_OR_EMPTY } else { 0 };
                self.mpc.store16(a, (status & !(BD_READY_OR_EMPTY | TXBD_ERRORS)) | keep);
                if status & BD_INTERRUPT != 0 {
                    self.mpc.scc_event(n, SCCE_TXB);
                }
            }
            tbptr = cursor;
            self.mpc.store16(p + TBPTR, tbptr);
            self.scc[n].frames_out += 1;
            log::debug!(target: "scc", "SCC{} sends {} bytes: {:02x?}", n + 1, frame.len(), &frame[..frame.len().min(24)]);
            if gsmr_l & GSMR_L_DIAG == DIAG_LOCAL_LOOPBACK {
                self.scc_receive(n, &frame);
            } else if let Some(link) = self.scc_link(n) {
                link.guest_transmit(frame);
            }
        }
    }

    /// A frame (without CRC) arrived for SCC n.
    fn scc_receive(&mut self, n: usize, frame: &[u8]) {
        let gsmr_l = self.mpc.load32(regs(n));
        if gsmr_l & GSMR_L_MODE != 0 || gsmr_l & GSMR_L_ENR == 0 || frame.is_empty() {
            return; // receiver off: the frame is lost on the line
        }
        let p = param(n);
        // Address recognition: the first two bytes against HADDR1-4 under HMASK.
        let hmask = self.mpc.load16(p + HMASK);
        if hmask != 0 {
            let addr = frame[0] as u16 | (*frame.get(1).unwrap_or(&0) as u16) << 8;
            let matches = (0..4).any(|i| (self.mpc.load16(p + HADDR1 + 2 * i) ^ addr) & hmask == 0);
            if !matches {
                let c = self.mpc.load16(p + NMARC).wrapping_add(1);
                self.mpc.store16(p + NMARC, c);
                return;
            }
        }
        let crc32 = self.mpc.load16(regs(n) + 8) & PSMR_CRC32 != 0;
        let mut data = frame.to_vec();
        if crc32 {
            // The 32-bit FCS is Ethernet's CRC-32, also sent low byte first.
            data.extend_from_slice(&crc32fast::hash(frame).to_le_bytes());
        } else {
            data.extend_from_slice(&fcs16(frame));
        }
        let mflr = self.mpc.load16(p + MFLR) as usize;
        let too_long = mflr != 0 && frame.len() > mflr;
        let mrblr = (self.mpc.load16(p + MRBLR) as usize).max(1);
        let rbase = self.mpc.load16(p + RBASE);
        let mut rbptr = self.mpc.load16(p + RBPTR);
        let mut offset = 0;
        let stored = if too_long { mflr } else { data.len() };
        let mut first = true;
        loop {
            let a = bd_addr(rbptr);
            let status = self.mpc.load16(a);
            if status & BD_READY_OR_EMPTY == 0 {
                // No free buffer: the rest of the frame is discarded.
                self.mpc.scc_event(n, SCCE_BSY);
                let c = self.mpc.load16(p + DISFC).wrapping_add(1);
                self.mpc.store16(p + DISFC, c);
                break;
            }
            let ptr = self.mpc.load32(a + 4);
            let chunk = mrblr.min(stored - offset);
            for (i, &b) in data[offset..offset + chunk].iter().enumerate() {
                let _ = self.write8(ptr.wrapping_add(i as u32), b);
            }
            offset += chunk;
            let last = offset >= stored;
            let keep = if status & BD_CONTINUOUS != 0 { BD_READY_OR_EMPTY } else { 0 };
            let mut new = (status & !(BD_READY_OR_EMPTY | RXBD_STATUS | BD_LAST | BD_FIRST)) | keep;
            if first {
                new |= BD_FIRST;
            }
            if last {
                new |= BD_LAST;
                if too_long {
                    new |= RXBD_LG;
                }
                // The last buffer's length is the whole frame's, CRC included.
                self.mpc.store16(a + 2, data.len() as u16);
            } else {
                self.mpc.store16(a + 2, chunk as u16);
                if status & BD_INTERRUPT != 0 {
                    self.mpc.scc_event(n, SCCE_RXB);
                }
            }
            self.mpc.store16(a, new);
            rbptr = if status & BD_WRAP != 0 { rbase } else { rbptr.wrapping_add(8) };
            first = false;
            if last {
                // RXF every RFTHR frames (not maskable through RxBD[I]).
                let count = self.mpc.load16(p + RFCNT).wrapping_sub(1);
                if count == 0 || count > self.mpc.load16(p + RFTHR) {
                    self.mpc.scc_event(n, SCCE_RXF);
                    let thr = self.mpc.load16(p + RFTHR).max(1);
                    self.mpc.store16(p + RFCNT, thr);
                } else {
                    self.mpc.store16(p + RFCNT, count);
                }
                self.scc[n].frames_in += 1;
                break;
            }
        }
        self.mpc.store16(p + RBPTR, rbptr);
        log::debug!(target: "scc", "SCC{} received {} bytes", n + 1, frame.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The X.25 check value: the FCS of "123456789" is 0x906E.
    #[test]
    fn fcs16_matches_the_x25_check_value() {
        assert_eq!(u16::from_le_bytes(fcs16(b"123456789")), 0x906E);
    }
}
