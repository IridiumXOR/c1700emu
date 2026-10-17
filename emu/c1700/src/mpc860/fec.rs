//! Fast Ethernet Controller of the MPC860T (MPC860UM chapter 43).
//!
//! This file holds the FEC's *register-level* state. Moving frames needs
//! access to RAM (the FEC is a bus master that reads/writes buffer
//! descriptors and buffers), so the DMA engine lives on the board:
//! see `board/fec_dma.rs`.
//!
//! What ROMMON uses (`eth_init_fec_based` 0xFFF1AEE0, transmit 0xFFF1A38C,
//! receive poll 0xFFF1A4DC):
//!
//! * ECNTRL: RESET (self-clearing), ETHER_EN, FEC_PINMUX;
//! * the MII management interface to talk to the PHY;
//! * the receive/transmit descriptor rings (R/X_DES_START, R_BUFF_SIZE) and
//!   the two "ring updated" command registers R_DES_ACTIVE/X_DES_ACTIVE.
//!   The driver writes X_DES_ACTIVE **only if it reads 0**, so the model must
//!   report "active" while the ring has work and 0 when it's done — exactly
//!   as the manual says (§43.4.1.11-12);
//! * I_EVENT is updated (TXB/TFINT/RXB/RFINT) but ROMMON polls with IMASK=0.
//!
//! The PHY on the Cisco 1700 mainboard answers PHY ID1 = 0x0013 (the ROM
//! checks for exactly that value and then programs register 20, the LED
//! configuration register of a Level One / Intel **LXT971**).

/// Offsets inside the FEC block (FEC base = IMMR + 0xE00).
pub mod off {
    pub const ADDR_LOW: u32 = 0x00;
    pub const ADDR_HIGH: u32 = 0x04;
    pub const R_DES_START: u32 = 0x10;
    pub const X_DES_START: u32 = 0x14;
    pub const R_BUFF_SIZE: u32 = 0x18;
    pub const ECNTRL: u32 = 0x40;
    pub const IEVENT: u32 = 0x44;
    pub const IMASK: u32 = 0x48;
    pub const IVEC: u32 = 0x4C;
    pub const R_DES_ACTIVE: u32 = 0x50;
    pub const X_DES_ACTIVE: u32 = 0x54;
    pub const MII_DATA: u32 = 0x80;
    pub const R_CNTRL: u32 = 0x144;
    pub const R_HASH: u32 = 0x148;
    pub const X_CNTRL: u32 = 0x184;
}

/// ECNTRL bits.
pub const ECNTRL_RESET: u32 = 0x1;
pub const ECNTRL_ETHER_EN: u32 = 0x2;

/// I_EVENT bits (§43.4.1.9).
pub const IEVENT_GRA: u32 = 0x1000_0000;
pub const IEVENT_TFINT: u32 = 0x0800_0000;
pub const IEVENT_TXB: u32 = 0x0400_0000;
pub const IEVENT_RFINT: u32 = 0x0200_0000;
pub const IEVENT_RXB: u32 = 0x0100_0000;
pub const IEVENT_MII: u32 = 0x0080_0000;
pub const IEVENT_EBERR: u32 = 0x0040_0000;

/// Value read from R_DES_ACTIVE / X_DES_ACTIVE while the ring is being polled (bit 7).
pub const DES_ACTIVE: u32 = 0x0100_0000;

pub struct Fec {
    pub ecntrl: u32,
    pub ievent: u32,
    pub imask: u32,
    pub ivec: u32,
    pub mii_data: u32,
    pub phy: Lxt971,
    /// The driver said "there are empty receive buffers" and we haven't yet
    /// found a descriptor without the E bit.
    pub rx_active: bool,
    /// The driver said "there are frames to send" and we haven't yet found a
    /// descriptor without the R bit.
    pub tx_active: bool,
    /// Address of the next descriptor the DMA engine will look at
    /// (`None` = the start of the ring, i.e. R_DES_START / X_DES_START).
    pub rx_cursor: Option<u32>,
    pub tx_cursor: Option<u32>,
    /// Bytes of a frame spread over several transmit buffers, until the
    /// descriptor with the L (last) bit.
    pub tx_partial: Vec<u8>,
}

impl Fec {
    pub fn new() -> Self {
        Fec {
            ecntrl: 0,
            ievent: 0,
            imask: 0,
            ivec: 0,
            mii_data: 0,
            phy: Lxt971::new(),
            rx_active: false,
            tx_active: false,
            rx_cursor: None,
            tx_cursor: None,
            tx_partial: Vec::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.ecntrl & ECNTRL_ETHER_EN != 0
    }

    /// Value to return for a 32-bit read at `off` (`stored` = what the
    /// register file holds, used for registers we don't model).
    pub fn read(&mut self, off: u32, stored: u32) -> u32 {
        match off {
            off::ECNTRL => self.ecntrl,
            off::IEVENT => self.ievent,
            off::IMASK => self.imask,
            off::IVEC => self.ivec,
            off::MII_DATA => self.mii_data,
            off::R_DES_ACTIVE => if self.rx_active { DES_ACTIVE } else { 0 },
            off::X_DES_ACTIVE => if self.tx_active { DES_ACTIVE } else { 0 },
            _ => stored,
        }
    }

    /// Handle a 32-bit write; returns the value the register file should hold.
    pub fn write(&mut self, off: u32, value: u32) -> u32 {
        if off != off::MII_DATA {
            log::debug!(target: "fec", "write {} = {value:#010x}", crate::mpc860::names::immr_name(super::regs::FEC_BASE + off));
        }
        match off {
            off::ECNTRL => {
                if value & ECNTRL_RESET != 0 {
                    // Like a hardware reset of the FEC alone; the PHY is a separate chip.
                    *self = Fec { phy: self.phy.clone(), ..Fec::new() };
                }
                let was_enabled = self.enabled();
                self.ecntrl = value & !ECNTRL_RESET; // RESET self-clears
                if self.enabled() != was_enabled {
                    // Setting or clearing ETHER_EN resets the DMA/BD logic:
                    // descriptor pointers go back to the start of the rings.
                    self.rx_active = false;
                    self.tx_active = false;
                    self.rx_cursor = None;
                    self.tx_cursor = None;
                    self.tx_partial.clear();
                }
                self.ecntrl
            }
            off::IEVENT => {
                self.ievent &= !value; // write-1-to-clear
                self.ievent
            }
            off::IMASK => {
                self.imask = value;
                value
            }
            off::IVEC => {
                self.ivec = value;
                value
            }
            // "Whenever the register is written, the bit is set, regardless of
            // the data written" — and only meaningful with ETHER_EN set.
            off::R_DES_ACTIVE => {
                self.rx_active = self.enabled();
                value
            }
            off::X_DES_ACTIVE => {
                self.tx_active = self.enabled();
                value
            }
            // X_CNTRL[GTS] (graceful transmit stop): we never have a frame
            // half-sent, so the stop completes at once (I_EVENT[GRA]).
            off::X_CNTRL => {
                if value & 1 != 0 {
                    self.ievent |= IEVENT_GRA;
                }
                value
            }
            off::MII_DATA => {
                self.mii_data = self.mii_frame(value);
                self.ievent |= IEVENT_MII; // the transfer completes "instantly"
                self.mii_data
            }
            _ => value,
        }
    }

    /// Execute an IEEE 802.3 clause-22 management frame:
    /// `ST(2)=01 OP(2) PA(5) RA(5) TA(2)=10 DATA(16)`, OP 10 = read, 01 = write.
    fn mii_frame(&mut self, frame: u32) -> u32 {
        let op = (frame >> 28) & 3;
        let phy_addr = (frame >> 23) & 0x1F;
        let reg = ((frame >> 18) & 0x1F) as usize;
        if phy_addr != 0 {
            // nobody there: the data lines float high
            return (frame & 0xFFFF_0000) | 0xFFFF;
        }
        match op {
            0b10 => (frame & 0xFFFF_0000) | self.phy.read(reg) as u32,
            0b01 => {
                self.phy.write(reg, frame as u16);
                frame
            }
            _ => frame,
        }
    }
}

impl Default for Fec {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal LXT971 PHY register model.
#[derive(Clone)]
pub struct Lxt971 {
    regs: [u16; 32],
}

impl Lxt971 {
    pub fn new() -> Self {
        let mut regs = [0u16; 32];
        regs[0] = 0x1100; // control: autoneg enable, full duplex
        regs[1] = 0x782D; // status: 100FD/100HD/10FD/10HD capable, autoneg done, link up
        regs[2] = 0x0013; // PHY ID1  (checked by the ROM)
        regs[3] = 0x78E2; // PHY ID2  (LXT971A)
        regs[4] = 0x01E1; // autoneg advertisement
        regs[5] = 0x45E1; // link partner ability
        // status #2: 100 Mbit (0x4000), link (0x0400), full duplex (0x0200),
        // autoneg enabled (0x0100) and complete (0x0080). The old value 0x4C00
        // set collision (0x0800) instead of duplex: IOS said "Half-duplex".
        regs[17] = 0x4780;
        regs[20] = 0x0422; // LED configuration
        Lxt971 { regs }
    }
    pub fn read(&self, reg: usize) -> u16 {
        self.regs[reg]
    }
    pub fn write(&mut self, reg: usize, value: u16) {
        match reg {
            0 => self.regs[0] = value & !0x8000, // reset bit self-clears
            1 | 2 | 3 | 5 | 17 => {}            // read-only
            _ => self.regs[reg] = value,
        }
    }
}

impl Default for Lxt971 {
    fn default() -> Self {
        Self::new()
    }
}
