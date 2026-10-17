//! An emulator session: the [`Machine`] plus the user's console, and the
//! loop that moves characters between them.

use crate::board::CYCLES_PER_US;
use crate::console::{Console, ConsoleEvent};
use crate::idle::IdleMode;
use crate::machine::{Machine, StopReason};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Instructions executed between two console/housekeeping rounds.
pub const SLICE: u64 = 100_000;
/// Realtime idle: how far (in cycles) virtual time may run ahead of the
/// wall clock before the emulator waits (10 ms).
const PACE_SLACK: u64 = 10_000 * CYCLES_PER_US;

pub struct Emulator {
    pub machine: Machine,
    pub console: Console,
    /// Optional telnet console of the AUX port (DUART channel B).
    pub aux: Option<Console>,
    /// Typed characters waiting for room in the UART receive FIFO.
    pending_input: VecDeque<u8>,
    pending_aux_input: VecDeque<u8>,
    pub quit: bool,
    last_flush: Instant,
}

impl Emulator {
    pub fn new(machine: Machine, console: Console, aux: Option<Console>) -> Self {
        Emulator {
            machine,
            console,
            aux,
            pending_input: VecDeque::new(),
            pending_aux_input: VecDeque::new(),
            quit: false,
            last_flush: Instant::now(),
        }
    }

    /// Move output to the console and input to the UART; persist NVRAM now and then.
    pub fn pump_console(&mut self) {
        let out = self.machine.take_console_output();
        self.console.write(&out);
        self.pump_aux();

        while let Some(ev) = self.console.poll() {
            match ev {
                ConsoleEvent::Input(b) => self.pending_input.push_back(b),
                ConsoleEvent::Break => self.machine.console_break(),
                ConsoleEvent::Quit => self.quit = true,
                ConsoleEvent::ProfileReset => self.machine.reset_profile(),
            }
        }
        while let Some(&b) = self.pending_input.front() {
            if !self.machine.console_input(b) {
                break;
            }
            self.pending_input.pop_front();
        }

        if self.last_flush.elapsed() > Duration::from_secs(2) {
            self.last_flush = Instant::now();
            if let Err(e) = self.machine.board.io.nvram.flush() {
                log::error!("NVRAM save failed: {e}");
            }
            if let Err(e) = self.machine.board.flash.flush() {
                log::error!("flash save failed: {e}");
            }
        }
    }

    /// The AUX port: to its telnet console if there is one, else to the log.
    fn pump_aux(&mut self) {
        let out = self.machine.take_aux_output();
        let Some(aux) = &self.aux else {
            if !out.is_empty() {
                log::info!(target: "aux", "{}", String::from_utf8_lossy(&out).trim_end());
            }
            return;
        };
        aux.write(&out);
        while let Some(ev) = aux.poll() {
            match ev {
                ConsoleEvent::Input(b) => self.pending_aux_input.push_back(b),
                ConsoleEvent::Break => self.machine.aux_break(),
                ConsoleEvent::Quit | ConsoleEvent::ProfileReset => {} // main console only
            }
        }
        while let Some(&b) = self.pending_aux_input.front() {
            if !self.machine.aux_input(b) {
                break;
            }
            self.pending_aux_input.pop_front();
        }
    }

    /// Run until the user quits, the machine dies, or `max_instructions` pass.
    pub fn run_interactive(&mut self, max_instructions: Option<u64>) -> StopReason {
        let start = self.machine.cpu.cycles;
        let wall_start = Instant::now();
        // Virtual cycle count that corresponds to "now" on the wall clock.
        let wall_cycles = move || start + wall_start.elapsed().as_micros() as u64 * CYCLES_PER_US;
        loop {
            // Decided on every slice: with `--idle-pc auto` the anchor is only
            // known once IOS runs.
            let realtime = self.machine.idle.mode == IdleMode::Realtime && self.machine.idle.anchor.is_some();
            if realtime {
                // Idle skips may advance virtual time, but not past the wall clock.
                self.machine.pace_limit = Some(wall_cycles() + PACE_SLACK);
            }
            let stop = self.machine.run(SLICE, true);
            self.pump_console();
            if self.quit || stop == StopReason::Checkstop {
                return stop;
            }
            if let StopReason::Breakpoint(pc) = stop {
                log::warn!("breakpoint at {pc:#010x} (attach GDB with --gdb to inspect)");
                return stop;
            }
            if max_instructions.is_some_and(|max| self.machine.cpu.cycles - start >= max) {
                return StopReason::Budget;
            }
            // The guest is spinning in "wait for a key": don't burn a host core.
            if self.machine.guest_is_idle() && self.pending_input.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Realtime idle: virtual time got ahead of the wall clock, so wait
            // (in short steps, to keep the console responsive).
            if realtime {
                let ahead = self.machine.cpu.cycles.saturating_sub(wall_cycles());
                if ahead > 0 {
                    std::thread::sleep(Duration::from_micros((ahead / CYCLES_PER_US).min(20_000)));
                }
            }
        }
    }
}
