//! IDMA emulation (MPC860UM §19.3): the CPM's two "independent DMA"
//! channels, which IOS uses as a **memory-copy engine**.
//!
//! How it was found: received Ethernet frames reached IOS's driver (a
//! `--watch` on the receive buffers showed it reading the type field and
//! the destination MAC) but were never processed, and the packet IOS held
//! was all zeros. No CPU instruction ever copied the frame. IOS's platform
//! init (`0x80130368`) sets IDMA1 up for memory-to-memory transfers
//! (`DCMR = 0`), unmasks its CPM interrupt and enables DONE interrupts;
//! its completion handler (`0x80130010`) clears IDSR1 and PCSO[DREQ0]. With
//! no IDMA model the first copy never completed and every later frame
//! queued behind it.
//!
//! Requests come from the DREQ pins (there is no internal request
//! mechanism, §19.3.6.1). Setting PCSO[DREQx] makes the pin a request input,
//! so the board must hold DREQ0 asserted and software starts a copy just
//! by enabling it: one PCSO read-modify-write per received frame, as the
//! `--stats` counters showed.
//!
//! While DREQx is enabled, the CP processes valid buffer descriptors from
//! IBPTR on. Each BD (16 bytes in the dual-port RAM) holds status, function
//! codes, length, source and destination. When a BD's count is exhausted
//! the CP clears V (unless CM, continuous mode), sets IDSR[AD] if I was set
//! and IDSR[DONE] if L was set, and moves to the next BD, or back to IBASE
//! after one with W. Finding no valid BD sets IDSR[OB].

use super::Board;
use crate::mpc860::regs::DPRAM;
use ppc8xx::Platform;

/// Parameter RAM of IDMA1 / IDMA2 (offsets in the IMMR window).
const PRAM: [u32; 2] = [0x3CC0, 0x3DC0];
const IBASE: u32 = 0x00;
const DCMR: u32 = 0x02;
const IBPTR: u32 = 0x0C;
/// IDSR1/IDSR2 (8-bit status) and their masks IDMR1/IDMR2.
const IDSR: [u32; 2] = [0x910, 0x918];
/// Port C special options: DREQ0 (PC15) requests IDMA1, DREQ1 (PC14) IDMA2.
const PCSO: u32 = 0x964;
const PCSO_DREQ: [u16; 2] = [0x0001, 0x0002];

// BD status bits (Table 19-7) and IDSR bits (Table 19-6).
const BD_V: u16 = 0x8000;
const BD_W: u16 = 0x2000;
const BD_I: u16 = 0x1000;
const BD_L: u16 = 0x0800;
const BD_CM: u16 = 0x0200;
const IDSR_AD: u8 = 0x04;
const IDSR_DONE: u8 = 0x02;
const IDSR_OB: u8 = 0x01;
const DCMR_SD: u16 = 0x0006; // source/destination: 00 = memory to memory
const BD_SIZE: u16 = 16;
const DPRAM_MASK: u16 = 0x1FFF;

impl Board {
    /// CPCR INIT IDMA: the current BD pointer goes back to the table start.
    pub(super) fn idma_init(&mut self, ch: usize) {
        let pram = PRAM[ch];
        let ibase = self.mpc.load16(pram + IBASE);
        self.mpc.store16(pram + IBPTR, ibase);
        log::debug!(target: "idma", "IDMA{} init: BD table at DPRAM+{ibase:#x}", ch + 1);
    }

    /// CPCR STOP IDMA: terminate; the current BD (if any) is closed.
    pub(super) fn idma_stop(&mut self, ch: usize) {
        self.idma_flag(ch, IDSR_DONE);
    }

    fn idma_flag(&mut self, ch: usize, bits: u8) {
        let v = self.mpc.load8(IDSR[ch]) | bits;
        self.mpc.store8(IDSR[ch], v);
    }

    /// Service both channels whose DREQ input is enabled (called after IMMR
    /// writes, since that is how software starts or feeds a transfer).
    pub(super) fn idma_service(&mut self) {
        if self.idma_busy {
            return; // a transfer wrote into the IMMR window (e.g. DPRAM)
        }
        let pcso = self.mpc.load16(PCSO);
        for ch in 0..2 {
            if pcso & PCSO_DREQ[ch] != 0 {
                self.idma_busy = true;
                self.idma_run(ch);
                self.idma_busy = false;
            }
        }
    }

    fn idma_run(&mut self, ch: usize) {
        let pram = PRAM[ch];
        let ibase = self.mpc.load16(pram + IBASE);
        let dcmr = self.mpc.load16(pram + DCMR);
        let mut ibptr = self.mpc.load16(pram + IBPTR);
        if dcmr & DCMR_SD != 0 {
            log::warn!(target: "idma", "IDMA{}: peripheral transfers (DCMR={dcmr:#06x}) are not emulated", ch + 1);
            return;
        }
        let mut flags = 0u8;
        for _ in 0..256 {
            let bd = DPRAM + (ibptr & DPRAM_MASK) as u32;
            let status = self.mpc.load16(bd);
            if status & BD_V == 0 {
                flags |= IDSR_OB;
                break;
            }
            let len = self.mpc.load32(bd + 4);
            let src = self.mpc.load32(bd + 8);
            let dst = self.mpc.load32(bd + 12);
            self.idma_copy(ch, src, dst, len);
            if status & BD_CM == 0 {
                self.mpc.store16(bd, status & !BD_V);
            }
            if status & BD_I != 0 {
                flags |= IDSR_AD;
            }
            if status & BD_L != 0 {
                flags |= IDSR_DONE;
            }
            ibptr = if status & BD_W != 0 { ibase } else { ibptr.wrapping_add(BD_SIZE) };
        }
        self.mpc.store16(pram + IBPTR, ibptr);
        self.idma_flag(ch, flags);
    }

    /// Dual-address memory-to-memory transfer (physical addresses: DMA
    /// bypasses the core's MMU).
    fn idma_copy(&mut self, ch: usize, src: u32, dst: u32, len: u32) {
        log::debug!(target: "idma", "IDMA{}: copy {len} bytes {src:#010x} -> {dst:#010x}", ch + 1);
        if len > 1 << 20 {
            log::warn!(target: "idma", "IDMA{}: refusing a {len}-byte transfer", ch + 1);
            return;
        }
        for i in 0..len {
            let b = self.read8(src.wrapping_add(i)).unwrap_or(0xFF);
            let _ = self.write8(dst.wrapping_add(i), b);
        }
    }
}
