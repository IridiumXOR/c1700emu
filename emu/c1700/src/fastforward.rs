//! Fast-forwarding the ROM's timebase busy-wait loops.
//!
//! `delay_us` (0xFFF12AC4) and `delay_ms` (0xFFF1299C) spin on `mftb` until
//! the timebase reaches a target. A spinning CPU can't observe anything but
//! the passing of time, so instead of executing millions of iterations we
//! advance the virtual clock straight to (one tick before) the target. The
//! loop then runs one more iteration and exits by itself.
//!
//! This is *exact*: virtual time advances by the same amount the loop would
//! have taken. Interrupts are respected: we never jump past the decrementer's
//! next interrupt while MSR[EE]=1 — we stop right there, the ROM takes the
//! interrupt, returns into the loop, and the next visit skips the rest.
//!
//! The hooks are tied to this exact ROM: at start-up the instruction words at
//! each loop are compared with what we expect, and the feature turns itself
//! off if they don't match.
//!
//! Measured effect: half of the time spent copying an image to flash, and a
//! quarter of the boot, were these loops.

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

pub struct FastForward {
    enabled: bool,
    /// Total timebase ticks skipped (for statistics).
    pub skipped_ticks: u64,
}

impl FastForward {
    /// Enable the hooks if `peek` shows the expected instructions at every loop.
    pub fn new(mut peek: impl FnMut(u32) -> Option<u32>) -> Self {
        let matches = LOOPS.iter().all(|l| {
            l.code.iter().enumerate().all(|(i, &w)| peek(l.pc + 4 * i as u32) == Some(w))
        });
        if !matches {
            log::info!("fast-forward of delay loops disabled: ROM code does not match");
        }
        FastForward { enabled: matches, skipped_ticks: 0 }
    }

    pub fn disabled() -> Self {
        FastForward { enabled: false, skipped_ticks: 0 }
    }

    /// Cheap test done before every instruction.
    #[inline]
    pub fn is_candidate(&self, pc: u32) -> bool {
        self.enabled && (pc & 0xFFFF_F000) == 0xFFF1_2000
    }

    /// If the CPU is at the head of a known wait loop, let time pass.
    /// Returns the number of CPU cycles skipped.
    pub fn try_skip(&mut self, cpu: &mut Cpu) -> u64 {
        let Some(l) = LOOPS.iter().find(|l| l.pc == cpu.pc) else { return 0 };
        if !cpu.tb_enabled {
            return 0; // the real loop would spin forever; don't pretend otherwise
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
                ticks = ticks.min(dec);
            }
        }
        if ticks < 8 {
            return 0; // not worth it
        }
        cpu.skip_timebase(ticks);
        self.skipped_ticks += ticks;
        ticks * cpu.tb_divider as u64
    }
}
