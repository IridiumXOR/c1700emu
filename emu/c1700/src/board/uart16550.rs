//! 16550-compatible console UART at 0x68050000 (registers at stride 1).
//!
//! The ROM drives it the textbook way (see `uart_init` 0xFFF04474):
//! LCR=0x80 (DLAB) → divisor latch = 3 686 400 / (16 × baud) → LCR=3 (8N1)
//! → MCR=3 (DTR|RTS) → FCR=1 (FIFO on). Output polls LSR.THRE (bit 5),
//! input polls LSR.DR (bit 0). IER=4 (line-status interrupt) is used to
//! catch a BREAK from the terminal; the interrupt pin goes to IRQ1.
//!
//! Characters leave immediately (THRE is always set): the host side is
//! faster than any baud rate.

use std::collections::VecDeque;

const RX_FIFO_DEPTH: usize = 16;

// Register offsets (DLAB=0)
const RBR_THR: u32 = 0;
const IER: u32 = 1;
const IIR_FCR: u32 = 2;
const LCR: u32 = 3;
const MCR: u32 = 4;
const LSR: u32 = 5;
const MSR: u32 = 6;
const SCR: u32 = 7;

// LSR bits
const LSR_DR: u8 = 0x01;
const LSR_OE: u8 = 0x02;
const LSR_BI: u8 = 0x10;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;

// IER bits
const IER_RX_DATA: u8 = 0x01;
const IER_THRE: u8 = 0x02;
const IER_LINE_STATUS: u8 = 0x04;

pub struct Uart16550 {
    rx: VecDeque<u8>,
    /// Bytes written by the guest, waiting to be shown to the user.
    pub tx: Vec<u8>,
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    /// Sticky line-status error bits (OE, BI), cleared by reading LSR.
    lsr_errors: u8,
    /// THRE interrupt pending (cleared by reading IIR or writing THR).
    thre_irq: bool,
    /// Consecutive LSR reads that found no received data: lets the machine
    /// notice that the guest is idling in a polling loop.
    pub idle_polls: u32,
}

impl Uart16550 {
    pub fn new() -> Self {
        Uart16550 {
            rx: VecDeque::new(),
            tx: Vec::new(),
            ier: 0,
            fcr: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            lsr_errors: 0,
            thre_irq: false,
            idle_polls: 0,
        }
    }

    pub fn reset(&mut self) {
        let tx = std::mem::take(&mut self.tx);
        *self = Uart16550::new();
        self.tx = tx; // don't lose output still waiting for the console
    }

    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    /// Baud rate currently programmed (3.6864 MHz clock / 16 / divisor).
    pub fn baud(&self) -> u32 {
        let div = u16::from_be_bytes([self.dlm, self.dll]) as u32;
        if div == 0 { 0 } else { 3_686_400 / (16 * div) }
    }

    /// Room for more host input?
    pub fn rx_has_room(&self) -> bool {
        self.rx.len() < RX_FIFO_DEPTH
    }

    /// A character arrives on the serial line.
    pub fn receive(&mut self, byte: u8) {
        if self.rx.len() >= RX_FIFO_DEPTH {
            self.lsr_errors |= LSR_OE;
        } else {
            self.rx.push_back(byte);
        }
    }

    /// The line is held low for longer than a character: a BREAK. The 16550
    /// sets LSR.BI and loads a NUL character into the FIFO.
    pub fn receive_break(&mut self) {
        self.lsr_errors |= LSR_BI;
        if self.rx.len() < RX_FIFO_DEPTH {
            self.rx.push_back(0);
        }
    }

    fn lsr(&self) -> u8 {
        let dr = if self.rx.is_empty() { 0 } else { LSR_DR };
        dr | self.lsr_errors | LSR_THRE | LSR_TEMT
    }

    /// Interrupt identification: highest-priority pending source.
    fn iir(&self) -> u8 {
        let fifo_bits = if self.fcr & 1 != 0 { 0xC0 } else { 0 };
        let id = if self.ier & IER_LINE_STATUS != 0 && self.lsr_errors != 0 {
            0x06
        } else if self.ier & IER_RX_DATA != 0 && !self.rx.is_empty() {
            0x04
        } else if self.ier & IER_THRE != 0 && self.thre_irq {
            0x02
        } else {
            0x01 // no interrupt pending
        };
        fifo_bits | id
    }

    /// Level of the UART's interrupt output.
    pub fn interrupt_pending(&self) -> bool {
        self.iir() & 0x01 == 0
    }

    pub fn read(&mut self, reg: u32) -> u8 {
        match reg {
            RBR_THR if self.dlab() => self.dll,
            RBR_THR => self.rx.pop_front().unwrap_or(0),
            IER if self.dlab() => self.dlm,
            IER => self.ier,
            IIR_FCR => {
                let v = self.iir();
                if v & 0x0F == 0x02 {
                    self.thre_irq = false;
                }
                v
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => {
                let v = self.lsr();
                self.lsr_errors = 0;
                self.idle_polls = if v & LSR_DR == 0 { self.idle_polls.saturating_add(1) } else { 0 };
                v
            }
            MSR => 0xB0, // DCD, DSR, CTS asserted: a terminal is connected
            SCR => self.scr,
            _ => 0xFF,
        }
    }

    /// Read without side effects (debugger).
    pub fn peek(&self, reg: u32) -> u8 {
        match reg {
            RBR_THR if self.dlab() => self.dll,
            RBR_THR => self.rx.front().copied().unwrap_or(0),
            IER if self.dlab() => self.dlm,
            IER => self.ier,
            IIR_FCR => self.iir(),
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => self.lsr(),
            MSR => 0xB0,
            _ => self.scr,
        }
    }

    pub fn write(&mut self, reg: u32, value: u8) {
        match reg {
            RBR_THR if self.dlab() => self.dll = value,
            RBR_THR => {
                self.tx.push(value);
                self.thre_irq = true; // transmitter is empty again right away
            }
            IER if self.dlab() => self.dlm = value,
            IER => {
                self.ier = value & 0x0F;
                if value & IER_THRE != 0 {
                    self.thre_irq = true;
                }
            }
            IIR_FCR => {
                self.fcr = value;
                if value & 0x02 != 0 {
                    self.rx.clear(); // reset receive FIFO
                }
            }
            LCR => {
                let was = self.lcr;
                self.lcr = value;
                if was & 0x80 != 0 && value & 0x80 == 0 {
                    log::debug!("console UART: {} baud, LCR={value:#04x}", self.baud());
                }
            }
            MCR => self.mcr = value,
            SCR => self.scr = value,
            _ => {}
        }
    }
}

impl Default for Uart16550 {
    fn default() -> Self {
        Self::new()
    }
}
