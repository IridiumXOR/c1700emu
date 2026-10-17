//! CPM pieces that need bus-master access: CPCR commands and the SPI
//! controller (MPC860UM chapter 30).
//!
//! On the C1700 the SPI bus reaches the ID EEPROMs of the two WIC slots;
//! IO FPGA register 0x18 selects which one (Dynamips: bit 0x20 → slot 0,
//! bit 0x08 → slot 1). IOS reads them at boot: in the first IOS run it
//! started 5 416 SPI transfers and spun 88 million times on a descriptor that
//! never completed. With no WIC fitted the EEPROM is absent and the data
//! line floats high: every received byte is 0xFF.
//!
//! An SPI transfer (master mode) walks the TxBD ring from TBPTR; every byte
//! sent clocks one byte in, which the CPM stores in the RxBD at RBPTR. BD
//! tables and pointers are offsets from the start of the dual-port RAM.
//!
//! IOS 12.4 writes RBASE/TBASE = 0x2420/0x2428 (and 0x3020/0x3028): offsets
//! from IMMR, not from the DPRAM. The DPRAM is 8 KB, so the CPM can only
//! use the low 13 bits of an offset, and 0x2428 lands on DPRAM offset
//! 0x428 = IMMR+0x2428, the very descriptor IOS polls. Hence `bd_addr`.

use super::Board;
use crate::mpc860::regs::DPRAM;
use ppc8xx::Platform;

const BD_READY_OR_EMPTY: u16 = 0x8000; // TxBD R / RxBD E
const BD_WRAP: u16 = 0x2000;
const BD_INTERRUPT: u16 = 0x1000;
const BD_LAST: u16 = 0x0800;
const BD_CONTINUOUS: u16 = 0x0200;
const SPIE_RXB: u8 = 0x01;
const SPIE_TXB: u8 = 0x02;
const SPIE_BSY: u8 = 0x04;

/// The dual-port RAM is 8 KB: the CPM decodes 13 bits of a BD offset.
const DPRAM_MASK: u16 = 0x1FFF;

fn bd_addr(offset: u16) -> u32 {
    DPRAM + (offset & DPRAM_MASK) as u32
}

/// CPCR channel numbers (MPC860UM Table 18-?).
const CH_SPI: u8 = 5;

/// Parameter-RAM base of a CPM channel (offset in the IMMR window).
fn channel_param_base(board: &Board, ch: u8) -> Option<u32> {
    Some(match ch {
        0 => 0x3C00,                     // SCC1
        4 => 0x3D00,                     // SCC2
        CH_SPI => board.mpc.spi_param_base(),
        8 => 0x3E00,                     // SCC3
        9 => 0x3E80,                     // SMC1
        12 => 0x3F00,                    // SCC4
        13 => 0x3F80,                    // SMC2
        _ => return None,
    })
}

impl Board {
    /// Execute a CPCR command. Only the INIT commands have an effect we need:
    /// they reset the channel's BD pointers to the start of its tables
    /// (RBPTR = RBASE, TBPTR = TBASE; same offsets for SCC, SMC and SPI).
    pub(super) fn cpm_command(&mut self, opcode: u8, ch: u8) {
        log::debug!(target: "cpm", "CPCR opcode {opcode} channel {ch}");
        // IDMA1 shares channel 1 with I2C, IDMA2 channel 5 with SPI; their
        // commands are INIT IDMA (5) and STOP IDMA (11) (MPC860UM §19.3.5).
        let idma = match ch {
            1 => Some(0),
            CH_SPI => Some(1),
            _ => None,
        };
        match (idma, opcode) {
            (Some(i), 5) => return self.idma_init(i),
            (Some(i), 11) => return self.idma_stop(i),
            _ => {}
        }
        let Some(base) = channel_param_base(self, ch) else { return };
        let (rbase, tbase) = (self.mpc.load16(base), self.mpc.load16(base + 2));
        match opcode {
            0 => {
                self.mpc.store16(base + 0x10, rbase);
                self.mpc.store16(base + 0x20, tbase);
            }
            1 => self.mpc.store16(base + 0x10, rbase),
            2 => self.mpc.store16(base + 0x20, tbase),
            _ => {}
        }
    }

    /// The device on the SPI bus: the ID EEPROM of the selected WIC.
    fn spi_device(&self, tx: &[u8]) -> Vec<u8> {
        let select = self.io.cpld.peek8(0x18);
        log::debug!(target: "spi", "SPI transfer select={select:#04x} tx={tx:02x?}");
        // No WIC models yet: an empty slot answers with a floating (high) line.
        vec![0xFF; tx.len()]
    }

    /// SPCOM[STR] was set: send every ready TxBD, receive into RxBDs.
    pub(super) fn spi_transfer(&mut self) {
        let base = self.mpc.spi_param_base();
        let (rbase, tbase) = (self.mpc.load16(base), self.mpc.load16(base + 2));
        let mut rbptr = self.mpc.load16(base + 0x10);
        let mut tbptr = self.mpc.load16(base + 0x20);
        log::debug!(target: "spi", "SPI start: param RAM at IMMR+{base:#06x} rbase={rbase:#x} tbase={tbase:#x}");
        for _ in 0..64 {
            let txbd = bd_addr(tbptr);
            let tstat = self.mpc.load16(txbd);
            if tstat & BD_READY_OR_EMPTY == 0 {
                break;
            }
            let len = self.mpc.load16(txbd + 2) as u32;
            let ptr = self.mpc.load32(txbd + 4);
            let tx: Vec<u8> = (0..len).map(|i| self.read8(ptr.wrapping_add(i)).unwrap_or(0xFF)).collect();
            let rx = self.spi_device(&tx);

            let rxbd = bd_addr(rbptr);
            let rstat = self.mpc.load16(rxbd);
            if rstat & BD_READY_OR_EMPTY != 0 {
                let rptr = self.mpc.load32(rxbd + 4);
                for (i, &b) in rx.iter().enumerate() {
                    let _ = self.write8(rptr.wrapping_add(i as u32), b);
                }
                self.mpc.store16(rxbd + 2, rx.len() as u16);
                // Close the buffer: clear E (unless continuous mode), mark last.
                let keep_empty = rstat & BD_CONTINUOUS != 0;
                let new = (rstat & !BD_READY_OR_EMPTY) | BD_LAST | if keep_empty { BD_READY_OR_EMPTY } else { 0 };
                self.mpc.store16(rxbd, new);
                if rstat & BD_INTERRUPT != 0 {
                    self.mpc.spi_event(SPIE_RXB);
                }
                rbptr = if rstat & BD_WRAP != 0 { rbase } else { rbptr + 8 };
            } else {
                self.mpc.spi_event(SPIE_BSY); // nowhere to put the received bytes
            }

            let new_t = if tstat & BD_CONTINUOUS != 0 { tstat } else { tstat & !BD_READY_OR_EMPTY };
            self.mpc.store16(txbd, new_t);
            if tstat & BD_INTERRUPT != 0 {
                self.mpc.spi_event(SPIE_TXB);
            }
            tbptr = if tstat & BD_WRAP != 0 { tbase } else { tbptr + 8 };
            if tstat & BD_LAST != 0 {
                break;
            }
        }
        self.mpc.store16(base + 0x10, rbptr);
        self.mpc.store16(base + 0x20, tbptr);
    }
}
