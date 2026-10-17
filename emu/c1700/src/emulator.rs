//! An emulator session: the [`Machine`] plus the user's console, and the
//! loop that moves characters between them.

use crate::console::{Console, ConsoleEvent};
use crate::machine::{Machine, StopReason};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Instructions executed between two console/housekeeping rounds.
pub const SLICE: u64 = 100_000;

pub struct Emulator {
    pub machine: Machine,
    pub console: Console,
    /// Typed characters waiting for room in the UART receive FIFO.
    pending_input: VecDeque<u8>,
    pub quit: bool,
    last_flush: Instant,
}

impl Emulator {
    pub fn new(machine: Machine, console: Console) -> Self {
        Emulator { machine, console, pending_input: VecDeque::new(), quit: false, last_flush: Instant::now() }
    }

    /// Move output to the console and input to the UART; persist NVRAM now and then.
    pub fn pump_console(&mut self) {
        let out = self.machine.take_console_output();
        self.console.write(&out);
        // AUX port (DUART channel B): not wired to a terminal yet, show it in the log.
        let aux = std::mem::take(&mut self.machine.board.io.aux.tx);
        if !aux.is_empty() {
            log::info!(target: "aux", "{}", String::from_utf8_lossy(&aux).trim_end());
        }

        while let Some(ev) = self.console.poll() {
            match ev {
                ConsoleEvent::Input(b) => self.pending_input.push_back(b),
                ConsoleEvent::Break => self.machine.console_break(),
                ConsoleEvent::Quit => self.quit = true,
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

    /// Run until the user quits, the machine dies, or `max_instructions` pass.
    pub fn run_interactive(&mut self, max_instructions: Option<u64>) -> StopReason {
        let start = self.machine.cpu.cycles;
        loop {
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
        }
    }
}
