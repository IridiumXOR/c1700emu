//! An emulator session: the [`Machine`] plus the user's console, and the
//! loop that moves characters between them.

use crate::board::CYCLES_PER_US;
use crate::console::{Console, ConsoleEvent};
use crate::idle::IdleMode;
use crate::machine::{Machine, StopReason};
use crate::wakeup::Wakeup;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Instructions executed between two console/housekeeping rounds.
pub const SLICE: u64 = 100_000;
/// Realtime idle: how far (in cycles) virtual time may run ahead of the
/// wall clock before the emulator waits (10 ms).
const PACE_SLACK: u64 = 10_000 * CYCLES_PER_US;
/// How far ahead a *busy* guest may run (50 ms). More than the idle slack:
/// work that starts while virtual time is ahead after idling (a frame
/// arrives, a key is pressed) must not wait for the wall clock to catch up
/// (docs ch. 18.5).
const BUSY_SLACK: u64 = 50_000 * CYCLES_PER_US;

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
    /// Where `Ctrl-] s` saves a snapshot and `Ctrl-] d` a core dump.
    pub snapshot_path: std::path::PathBuf,
    pub core_path: std::path::PathBuf,
    /// Notified by the input threads (console, network): ends idle sleeps.
    pub wakeup: Wakeup,
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
            snapshot_path: "c1700.snap".into(),
            core_path: "c1700-core.elf".into(),
            wakeup: Wakeup::default(),
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
                ConsoleEvent::Snapshot => self.save_snapshot(),
                ConsoleEvent::CoreDump => self.write_core(),
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
                ConsoleEvent::Quit | ConsoleEvent::ProfileReset | ConsoleEvent::Snapshot | ConsoleEvent::CoreDump => {} // main console only
            }
        }
        while let Some(&b) = self.pending_aux_input.front() {
            if !self.machine.aux_input(b) {
                break;
            }
            self.pending_aux_input.pop_front();
        }
    }

    /// `Ctrl-] s`: save the machine to `snapshot_path`.
    fn save_snapshot(&mut self) {
        let start = Instant::now();
        match self.machine.save_snapshot(&self.snapshot_path) {
            Ok(()) => {
                let size = std::fs::metadata(&self.snapshot_path).map_or(0, |m| m.len());
                eprint!(
                    "\r\n[c1700emu] snapshot saved to {} ({} MB, {:.1} s); restore with --restore\r\n",
                    self.snapshot_path.display(),
                    size >> 20,
                    start.elapsed().as_secs_f64()
                );
            }
            Err(e) => eprint!("\r\n[c1700emu] snapshot failed: {e}\r\n"),
        }
    }

    /// `Ctrl-] d`: write an ELF core dump to `core_path`.
    fn write_core(&mut self) {
        match self.machine.write_core(&self.core_path) {
            Ok(n) => eprint!("\r\n[c1700emu] core dump ({n} memory segments) written to {}\r\n", self.core_path.display()),
            Err(e) => eprint!("\r\n[c1700emu] core dump failed: {e}\r\n"),
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
            // (Not with realtime idle pacing, which waits by itself: IOS polls
            // the console all the time, and this sleep delayed everything,
            // network replies included, by 5 ms; docs ch. 18.5.)
            if !realtime && self.machine.guest_is_idle() && self.pending_input.is_empty() {
                self.wakeup.sleep(Duration::from_millis(5));
            }
            // Realtime idle: virtual time got ahead of the wall clock, so wait
            // (in short steps, to keep the console responsive), unless input
            // or a network frame arrives first.
            if realtime {
                let slack = if self.machine.paced { 0 } else { BUSY_SLACK };
                let ahead = self.machine.cpu.cycles.saturating_sub(wall_cycles() + slack);
                if ahead > 0 {
                    log::trace!(target: "pace", "ahead of the wall clock by {} us: wait", ahead / CYCLES_PER_US);
                    self.wakeup.sleep(Duration::from_micros((ahead / CYCLES_PER_US).min(20_000)));
                }
            }
        }
    }
}
