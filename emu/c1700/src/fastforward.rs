//! Fast-forwarding the ROM's timebase busy-wait loops.
//!
//! `delay_us` (0xFFF12AC4) and `delay_ms` (0xFFF1299C) spin on `mftb` until
//! the timebase reaches a target. A spinning CPU can't observe anything but
//! the passing of time, so instead of executing millions of iterations we
//! advance the virtual clock straight to (one tick before) the target. The
//! loop then runs one more iteration and exits by itself.
//!
//! This is *exact*: virtual time advances by the same amount the loop would
//! have taken. Interrupts are respected: while MSR[EE]=1 we stop one tick
//! before the decrementer's next interrupt, the loop runs across that tick
//! and takes the interrupt at the very instruction the real CPU would, and
//! the next visit skips the rest. The PIT gets the same treatment: skips
//! stop one tick before its next expiry. The first version only looked at
//! the decrementer and stopped *on* the tick; jumping over PIT expiries
//! merged several periodic interrupts into one, and a skip taken while a
//! decrementer interrupt was already *pending* jumped over many periods
//! (10 of 116 interrupts lost during a ROMMON boot); and with the
//! 3-instruction `delay_ms` loop, a skip of K ticks (16·K cycles) is only a
//! whole number of iterations if K is a multiple of 3. The test
//! `rom_accelerators_are_exact` (boot with and without, compare everything
//! at the same instruction) found both.
//!
//! The hooks are tied to this exact ROM: at start-up the instruction words at
//! each loop are compared with what we expect, and the feature turns itself
//! off if they don't match.
//!
//! Measured effect: half of the time spent copying an image to flash, and a
//! quarter of the boot, were these loops.
//!
//! The same idea covers the ROM's **memory clear** (`bzero_bytes`,
//! 0xFFF1682C), which writes all of RAM one byte at a time at power-on:
//! 4 instructions per byte, ~0.5 billion instructions for 128 MB, 13% of an
//! IOS boot (docs ch. 15). The loop is replaced by one bulk fill of plain
//! DRAM, with the registers, CR1, PC and virtual time left exactly as the
//! loop would have left them (4 cycles per byte), and it stops before the
//! next decrementer interrupt like the delay loops do.

use ppc8xx::{Cpu, msr};

/// How a loop decides it is done (register numbers come from the disassembly).
#[derive(Clone, Copy, Debug)]
enum Wait {
    /// `while (TBL - r[start]) < r[ticks]`
    Elapsed { start: usize, ticks: usize },
    /// `while TBL < r[target]`
    Until { target: usize },
    /// `while TBL > 1` — waiting for TBL to wrap past zero
    Wrap,
}

struct WaitLoop {
    /// Address of the loop's `mftb` (the loop head).
    pc: u32,
    /// The loop's instructions, to make sure we are hooking the right ROM.
    code: &'static [u32],
    wait: Wait,
}

const LOOPS: &[WaitLoop] = &[
    // delay_us:  mftb r0 ; sub r0,r0,r11 ; cmplw cr1,r0,r9 ; blt cr1,loop
    WaitLoop { pc: 0xFFF1_2AD4, code: &[0x7C0C_42E6, 0x7C0B_0050, 0x7C80_4840, 0x4184_FFF4], wait: Wait::Elapsed { start: 11, ticks: 9 } },
    // delay_ms:  mftb r0 ; cmplw cr1,r0,r9 ; blt cr1,loop
    WaitLoop { pc: 0xFFF1_29C0, code: &[0x7C0C_42E6, 0x7C80_4840, 0x4184_FFF8], wait: Wait::Until { target: 9 } },
    // delay_ms when the target wrapped: first wait for TBL to wrap ...
    WaitLoop { pc: 0xFFF1_29D0, code: &[0x7C0C_42E6, 0x2880_0001, 0x4185_FFF8], wait: Wait::Wrap },
    // ... then wait for the target
    WaitLoop { pc: 0xFFF1_29DC, code: &[0x7C0C_42E6, 0x7C80_4840, 0x4184_FFF8], wait: Wait::Until { target: 9 } },
];

/// `bzero_bytes` inner loop:
/// `stb r9,0(r3) ; addi r3,r3,1 ; cmplw cr1,r3,r0 ; blt cr1,loop`, then `blr`.
const BZERO_LOOP: u32 = 0xFFF1_683C;
const BZERO_CODE: [u32; 4] = [0x9923_0000, 0x3863_0001, 0x7C83_0040, 0x4184_FFF4];
const BZERO_EXIT: u32 = BZERO_LOOP + 16;
const CYCLES_PER_BYTE: u64 = 4;

/// One bulk fill the machine should perform.
pub struct Fill {
    pub addr: u32,
    pub len: u32,
    pub value: u8,
}

pub struct FastForward {
    enabled: bool,
    /// `bzero_bytes` matched the expected code.
    bzero: bool,
    /// Total timebase ticks skipped (for statistics).
    pub skipped_ticks: u64,
    /// Bytes cleared in bulk instead of by the ROM's loop.
    pub bulk_cleared: u64,
}

impl FastForward {
    /// Snapshot state (docs ch. 20): only statistics; what is enabled
    /// comes from the command line.
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        v.u64(&mut self.skipped_ticks);
        v.u64(&mut self.bulk_cleared);
    }

    /// Enable the hooks if `peek` shows the expected instructions at every loop.
    pub fn new(mut peek: impl FnMut(u32) -> Option<u32>) -> Self {
        let matches = LOOPS.iter().all(|l| {
            l.code.iter().enumerate().all(|(i, &w)| peek(l.pc + 4 * i as u32) == Some(w))
        });
        if !matches {
            log::info!("fast-forward of delay loops disabled: ROM code does not match");
        }
        let bzero = BZERO_CODE.iter().enumerate().all(|(i, &w)| peek(BZERO_LOOP + 4 * i as u32) == Some(w));
        FastForward { enabled: matches, bzero, skipped_ticks: 0, bulk_cleared: 0 }
    }

    pub fn disabled() -> Self {
        FastForward { enabled: false, bzero: false, skipped_ticks: 0, bulk_cleared: 0 }
    }

    /// Turn the delay-loop and memory-clear accelerators on/off separately
    /// (they are only on if the ROM code matched).
    pub fn restrict(&mut self, delay_loops: bool, memory_clear: bool) {
        self.enabled &= delay_loops;
        self.bzero &= memory_clear;
    }

    /// Cheap test done before every instruction: the memory-clear loop head?
    #[inline]
    pub fn is_bzero(&self, pc: u32) -> bool {
        self.bzero && pc == BZERO_LOOP
    }

    /// What the loop at its head is about to do, bounded by the next
    /// decrementer interrupt. `None` = let it run normally.
    pub fn bzero_plan(&self, cpu: &Cpu, pit_cycles: Option<u64>) -> Option<Fill> {
        if cpu.msr & msr::DR != 0 {
            return None; // only with translation off, as the ROM runs
        }
        if cpu.msr & msr::EE != 0 && cpu.dec_pending {
            return None; // an interrupt is about to be taken
        }
        let (start, end) = (cpu.gpr[3], cpu.gpr[0]);
        let mut len = end.checked_sub(start)? as u64;
        if cpu.msr & msr::EE != 0 && cpu.tb_enabled {
            let ticks = cpu.ticks_until_decrementer()?.saturating_sub(1);
            len = len.min(ticks * cpu.tb_divider as u64 / CYCLES_PER_BYTE);
        }
        if let Some(pit) = pit_cycles {
            let ticks = (pit / cpu.tb_divider as u64).saturating_sub(1);
            len = len.min(ticks * cpu.tb_divider as u64 / CYCLES_PER_BYTE);
        }
        // whole timebase ticks only: 4 cycles/byte, 16 cycles/tick
        let len = len / 4 * 4;
        (len >= 64).then(|| Fill { addr: start, len: len as u32, value: cpu.gpr[9] as u8 })
    }

    /// The fill was done: leave the CPU as the loop would have, and return
    /// the cycles it would have taken.
    pub fn bzero_done(&mut self, cpu: &mut Cpu, fill: &Fill) -> u64 {
        let cycles = fill.len as u64 * CYCLES_PER_BYTE;
        cpu.gpr[3] = fill.addr + fill.len;
        let so = (cpu.xer >> 31) & 1;
        if cpu.gpr[3] == cpu.gpr[0] {
            cpu.set_cr_field(1, 0b0010 | so); // cmplw: equal, blt not taken
            cpu.pc = BZERO_EXIT;
        } else {
            cpu.set_cr_field(1, 0b1000 | so); // less: back at the loop head
        }
        if cpu.tb_enabled {
            cpu.skip_timebase(cycles / cpu.tb_divider as u64);
        } else {
            cpu.cycles += cycles;
        }
        self.bulk_cleared += fill.len as u64;
        cycles
    }

    /// Is `pc` an address where [`FastForward::try_skip`] or the memory
    /// clear can act? (The machine single-steps exactly these.)
    #[inline]
    pub fn is_hook(&self, pc: u32) -> bool {
        self.is_bzero(pc) || (self.enabled && LOOPS.iter().any(|l| l.pc == pc))
    }

    /// Cheap test done before every instruction.
    #[inline]
    pub fn is_candidate(&self, pc: u32) -> bool {
        self.enabled && (pc & 0xFFFF_F000) == 0xFFF1_2000
    }

    /// If the CPU is at the head of a known wait loop, let time pass.
    /// Returns the number of CPU cycles skipped.
    /// `pit_cycles`: CPU cycles until the PIT's next interrupt, if armed.
    pub fn try_skip(&mut self, cpu: &mut Cpu, pit_cycles: Option<u64>) -> u64 {
        let Some(l) = LOOPS.iter().find(|l| l.pc == cpu.pc) else { return 0 };
        if !cpu.tb_enabled {
            return 0; // the real loop would spin forever; don't pretend otherwise
        }
        if cpu.msr & msr::EE != 0 && cpu.dec_pending {
            return 0; // the interrupt is taken at the next instruction: don't jump past it
        }
        let tbl = cpu.tb as u32;
        let r = &cpu.gpr;
        let remaining = match l.wait {
            Wait::Elapsed { start, ticks } => r[ticks].saturating_sub(tbl.wrapping_sub(r[start])),
            Wait::Until { target } => r[target].saturating_sub(tbl),
            Wait::Wrap => {
                if tbl > 1 {
                    0u32.wrapping_sub(tbl)
                } else {
                    0
                }
            }
        } as u64;
        // Leave the last tick to the loop itself so it exits through its own code.
        let mut ticks = remaining.saturating_sub(1);
        if cpu.msr & msr::EE != 0 {
            if let Some(dec) = cpu.ticks_until_decrementer() {
                // Stop one tick *before* the decrementer underflows: the tick
                // itself happens at a precise instruction (it depends on the
                // timebase prescaler phase), so let the loop run across it and
                // take the interrupt exactly where the real CPU would.
                ticks = ticks.min(dec.saturating_sub(1));
            }
        }
        // Same for the PIT: a skip over its expiry would merge several
        // periodic interrupts into one (the test found 1 227 missing cycles).
        if let Some(pit) = pit_cycles {
            ticks = ticks.min((pit / cpu.tb_divider as u64).saturating_sub(1));
        }
        // The skip must cover whole loop iterations *and* whole timebase ticks,
        // or the loop's phase against the timebase shifts: 16·ticks cycles must
        // be a multiple of the loop length (3 instructions for delay_ms).
        let len = l.code.len() as u64;
        let step = len / gcd(len, cpu.tb_divider as u64);
        let ticks = ticks / step * step;
        if ticks < 8 {
            return 0; // not worth it
        }
        cpu.skip_timebase(ticks);
        self.skipped_ticks += ticks;
        ticks * cpu.tb_divider as u64
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}
