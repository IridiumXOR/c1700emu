//! The FEC's DMA engine: walks the transmit and receive buffer-descriptor
//! rings in guest RAM (MPC860UM §43.4.3).
//!
//! Buffer descriptor (8 bytes, big-endian):
//!
//! ```text
//!   +0  u16 status    TxBD: R(0x8000) W(0x2000) L(0x0800) TC(0x0400) ...
//!                     RxBD: E(0x8000) W(0x2000) L(0x0800) BC(0x0080) MC(0x0040) ...
//!   +2  u16 length    Tx: bytes to send   Rx: bytes written (frame + 4-byte CRC if L)
//!   +4  u32 buffer    address of the data buffer
//! ```
//!
//! Ownership protocol: software hands a TxBD to the FEC by setting R (then
//! writes X_DES_ACTIVE); the FEC clears R when the buffer is sent. Software
//! hands empty receive buffers over with E set (then writes R_DES_ACTIVE);
//! the FEC clears E when it filled one. A descriptor with W is the last of
//! its ring: the next one is at R_DES_START / X_DES_START again.

use super::Board;
use crate::mpc860::fec::{self, off};
use crate::mpc860::regs::FEC_BASE;
use ppc8xx::{BusError, Platform};

const BD_SIZE: u32 = 8;
const BD_READY_OR_EMPTY: u16 = 0x8000; // TxBD R / RxBD E
const BD_WRAP: u16 = 0x2000;
const BD_LAST: u16 = 0x0800;
const RXBD_BC: u16 = 0x0080;
const RXBD_MC: u16 = 0x0040;
/// TxBD bits the FEC writes when the frame completes (DEF HB LC RL RC UN CSL).
const TXBD_STATUS_BITS: u16 = 0x03FF;
/// R_CNTRL bits.
const R_CNTRL_BC_REJ: u32 = 0x10;
const R_CNTRL_PROM: u32 = 0x08;
/// Safety net against malformed rings (e.g. no W bit, every BD ready).
const MAX_BDS_PER_CALL: usize = 512;
/// Ethernet minimum frame size without FCS: shorter frames are padded by the sender.
const MIN_FRAME: usize = 60;

impl Board {
    fn fec_reg(&self, o: u32) -> u32 {
        self.mpc.load32(FEC_BASE + o)
    }

    /// Run the FEC's DMA: send what the guest queued, deliver what the
    /// network has for the guest. Called by `Board::advance`.
    pub(crate) fn fec_dma(&mut self) {
        if !self.mpc.fec.enabled() {
            return;
        }
        if let Err(BusError) = self.fec_transmit().and_then(|_| self.fec_receive()) {
            log::warn!("FEC: bus error during DMA");
            self.mpc.fec.ievent |= fec::IEVENT_EBERR;
        }
    }

    fn next_bd(cursor: u32, status: u16, ring_start: u32) -> u32 {
        if status & BD_WRAP != 0 { ring_start } else { cursor + BD_SIZE }
    }

    fn fec_transmit(&mut self) -> Result<(), BusError> {
        let ring = self.fec_reg(off::X_DES_START) & !3;
        for _ in 0..MAX_BDS_PER_CALL {
            if !self.mpc.fec.tx_active {
                break;
            }
            let bd = self.mpc.fec.tx_cursor.unwrap_or(ring);
            let status = self.read16(bd)?;
            if status & BD_READY_OR_EMPTY == 0 {
                // Nothing more to send: X_DES_ACTIVE reads 0 again.
                self.mpc.fec.tx_active = false;
                break;
            }
            let len = (self.read16(bd + 2)? & 0x07FF) as u32;
            let buf = self.read32(bd + 4)?;
            for i in 0..len {
                let b = self.read8(buf + i)?;
                self.mpc.fec.tx_partial.push(b);
            }
            // Hand the BD back: clear R; a perfect transmission sets no error bits.
            self.write16(bd, status & !(BD_READY_OR_EMPTY | TXBD_STATUS_BITS))?;
            self.mpc.fec.ievent |= fec::IEVENT_TXB;
            if status & BD_LAST != 0 {
                let frame = std::mem::take(&mut self.mpc.fec.tx_partial);
                self.mpc.fec.ievent |= fec::IEVENT_TFINT;
                match &mut self.net {
                    Some(net) => net.guest_transmit(frame),
                    None => log::trace!("FEC: frame of {} bytes sent into an unplugged cable", frame.len()),
                }
            }
            self.mpc.fec.tx_cursor = Some(Self::next_bd(bd, status, ring));
        }
        Ok(())
    }

    /// Would the address filter accept a frame for this destination MAC?
    fn fec_accepts(&self, dst: &[u8]) -> bool {
        let r_cntrl = self.fec_reg(off::R_CNTRL);
        if r_cntrl & R_CNTRL_PROM != 0 {
            return true;
        }
        let lo = self.fec_reg(off::ADDR_LOW).to_be_bytes();
        let hi = self.fec_reg(off::ADDR_HIGH).to_be_bytes();
        let own = [lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]];
        if dst == own {
            return true;
        }
        if dst == [0xFF; 6] {
            return r_cntrl & R_CNTRL_BC_REJ == 0;
        }
        // Multicast goes through a 64-bit hash table; ROMMON leaves it at 0,
        // so "any bit set" is a good enough approximation.
        dst[0] & 1 != 0 && (self.fec_reg(0x08) | self.fec_reg(0x0C)) != 0
    }

    fn fec_receive(&mut self) -> Result<(), BusError> {
        let ring = self.fec_reg(off::R_DES_START) & !3;
        let buf_size = (self.fec_reg(off::R_BUFF_SIZE) & 0x07F0).max(16);
        loop {
            if !self.mpc.fec.rx_active {
                return Ok(());
            }
            let Some(net) = &self.net else { return Ok(()) };
            let Some(next) = net.peek_for_guest() else { return Ok(()) };
            if next.len() < 14 || !self.fec_accepts(&next[0..6]) {
                self.net.as_mut().unwrap().pop_for_guest(); // filtered out
                continue;
            }
            // Is the next receive buffer free? If not, keep the frame queued
            // (like the FEC's FIFO) and stop polling until R_DES_ACTIVE.
            let first_bd = self.mpc.fec.rx_cursor.unwrap_or(ring);
            if self.read16(first_bd)? & BD_READY_OR_EMPTY == 0 {
                self.mpc.fec.rx_active = false;
                return Ok(());
            }
            let mut frame = self.net.as_mut().unwrap().pop_for_guest().unwrap();
            if frame.len() < MIN_FRAME {
                frame.resize(MIN_FRAME, 0);
            }
            let flags = if frame[0..6] == [0xFF; 6] {
                RXBD_BC
            } else if frame[0] & 1 != 0 {
                RXBD_MC
            } else {
                0
            };
            // The FEC stores the frame check sequence after the data.
            let fcs = crc32fast::hash(&frame);
            frame.extend_from_slice(&fcs.to_le_bytes());
            self.fec_store_frame(&frame, ring, buf_size, flags)?;
        }
    }

    /// Spread one received frame over as many receive buffers as needed.
    fn fec_store_frame(&mut self, frame: &[u8], ring: u32, buf_size: u32, flags: u16) -> Result<(), BusError> {
        let mut done = 0usize;
        for _ in 0..MAX_BDS_PER_CALL {
            let bd = self.mpc.fec.rx_cursor.unwrap_or(ring);
            let status = self.read16(bd)?;
            if status & BD_READY_OR_EMPTY == 0 {
                // Ran out of buffers in the middle of a frame: overrun, drop the rest.
                log::debug!("FEC: receive ring full mid-frame, dropping {} bytes", frame.len() - done);
                self.mpc.fec.rx_active = false;
                return Ok(());
            }
            let buf = self.read32(bd + 4)?;
            let chunk = &frame[done..frame.len().min(done + buf_size as usize)];
            for (i, &b) in chunk.iter().enumerate() {
                self.write8(buf + i as u32, b)?;
            }
            done += chunk.len();
            let last = done == frame.len();
            // Keep W and the software-owned bits RO1/RO2, clear E and the error bits.
            let mut new_status = status & (BD_WRAP | 0x5000);
            let length = if last {
                new_status |= BD_LAST | flags;
                frame.len() as u16 // whole frame including FCS
            } else {
                buf_size as u16
            };
            self.write16(bd + 2, length)?;
            self.write16(bd, new_status)?;
            self.mpc.fec.ievent |= fec::IEVENT_RXB;
            self.mpc.fec.rx_cursor = Some(Self::next_bd(bd, status, ring));
            if last {
                self.mpc.fec.ievent |= fec::IEVENT_RFINT;
                return Ok(());
            }
        }
        Ok(())
    }
}
