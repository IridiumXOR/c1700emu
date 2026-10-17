//! The complete system: CPU core + board, and the loop that drives them.
//!
//! Time is *virtual*: one executed instruction = one CPU cycle. Devices are
//! advanced in small batches (every [`DEVICE_SYNC_INTERVAL`] instructions)
//! instead of after every instruction — the ROM can't tell the difference,
//! and it keeps the inner loop tight.

use crate::board::{Board, BoardConfig, nvram::Nvram};
use crate::fastforward::FastForward;
use crate::mpc860::rsr;
use powerpc::{Extensions, Ins};
use ppc8xx::{Cpu, StepOutcome};
use std::collections::BTreeSet;

/// Instructions between two device updates (timers, interrupt line).
pub const DEVICE_SYNC_INTERVAL: u64 = 256;

/// PLPRCR[CSR]: checkstop reset enable.
const PLPRCR_CSR: u32 = 0x0000_0080;

/// Why [`Machine::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The instruction budget was used up.
    Budget,
    /// The next instruction is at a breakpoint (it has not executed yet).
    Breakpoint(u32),
    /// The core entered checkstop and the SIU was not told to reset:
    /// the system is dead until an external reset.
    Checkstop,
}

pub struct Machine {
    pub cpu: Cpu,
    pub board: Board,
    pub breakpoints: BTreeSet<u32>,
    /// Log every instruction (target `insn`, level TRACE).
    pub trace: bool,
    /// Cycles executed since the last device sync.
    unsynced: u64,
    pub resets: u32,
    /// Optional execution profile: one counter per ROM instruction word.
    pub profile: Option<Vec<u64>>,
    /// Skips the ROM's timebase busy-wait loops (see `fastforward.rs`).
    pub fast_forward: FastForward,
}

/// Address range covered by the execution profile (the boot ROM).
const PROFILE_BASE: u32 = 0xFFF0_0000;
const PROFILE_WORDS: usize = 0x8_0000 / 4;

impl Machine {
    pub fn new(rom: Vec<u8>, cfg: &BoardConfig, nvram: Nvram) -> Self {
        let mut m = Machine {
            cpu: Cpu::new(),
            board: Board::new(rom, cfg, nvram),
            breakpoints: BTreeSet::new(),
            trace: false,
            unsynced: 0,
            resets: 0,
            profile: None,
            fast_forward: FastForward::disabled(),
        };
        m.power_on();
        m.fast_forward = FastForward::new(|a| m.board.peek_word(a));
        m
    }

    /// Cold start: like plugging the power cord in.
    pub fn power_on(&mut self) {
        self.board.power_on_reset();
        self.cpu = Cpu::new();
        self.cpu.hard_reset(true); // hard reset config word: IP=1 → boot at 0xFFF00100
        self.sync_devices();
    }

    /// Internal hard reset (checkstop reset, watchdog...). RAM survives.
    pub fn hard_reset(&mut self, cause: u32) {
        self.resets += 1;
        self.board.hard_reset(cause);
        let tb = self.cpu.tb;
        let cycles = self.cpu.cycles;
        self.cpu = Cpu::new();
        self.cpu.hard_reset(true);
        self.cpu.tb = tb;
        self.cpu.cycles = cycles;
        self.sync_devices();
    }

    /// Execute up to `budget` instructions.
    ///
    /// When `ignore_breakpoint_at_start` is true, a breakpoint on the current
    /// PC does not stop us before the first instruction (needed to continue
    /// from a breakpoint).
    pub fn run(&mut self, budget: u64, ignore_breakpoint_at_start: bool) -> StopReason {
        let check_bp = !self.breakpoints.is_empty();
        for i in 0..budget {
            let pc = self.cpu.pc;
            if check_bp && self.breakpoints.contains(&pc) && !(i == 0 && ignore_breakpoint_at_start) {
                self.sync_devices();
                return StopReason::Breakpoint(pc);
            }
            if self.trace {
                self.trace_instruction(pc);
            }
            if self.fast_forward.is_candidate(pc) {
                // The guest is about to spin on the timebase: let the time pass.
                self.unsynced += self.fast_forward.try_skip(&mut self.cpu);
            }
            if let Some(prof) = &mut self.profile {
                let idx = (pc.wrapping_sub(PROFILE_BASE) / 4) as usize;
                if idx < PROFILE_WORDS {
                    prof[idx] += 1;
                }
            }
            if self.cpu.step(&mut self.board) == StepOutcome::Checkstop {
                if let Some(reason) = self.on_checkstop() {
                    return reason;
                }
            }
            self.unsynced += 1;
            if self.unsynced >= DEVICE_SYNC_INTERVAL {
                self.sync_devices();
            }
        }
        StopReason::Budget
    }

    /// The core stopped on a machine check with MSR[ME]=0. If PLPRCR[CSR] is
    /// set the SIU converts that into an internal hard reset — this is how the
    /// ROMMON `reset` command works (see `system_reset_via_checkstop`).
    fn on_checkstop(&mut self) -> Option<StopReason> {
        if self.board.mpc.plprcr() & PLPRCR_CSR != 0 {
            log::info!("checkstop at pc={:#010x} → checkstop reset", self.cpu.pc);
            self.hard_reset(rsr::CSRS);
            None
        } else {
            log::error!("checkstop at pc={:#010x} (PLPRCR[CSR]=0): system halted", self.cpu.pc);
            Some(StopReason::Checkstop)
        }
    }

    /// Bring devices up to date with the CPU's virtual clock.
    pub fn sync_devices(&mut self) {
        self.board.advance(self.unsynced);
        self.unsynced = 0;
        self.cpu.tb_enabled = self.board.mpc.timebase_enabled();
    }

    fn trace_instruction(&mut self, pc: u32) {
        match self.board.peek_word(pc) {
            Some(word) => {
                let ins = Ins::new(word, Extensions::none());
                log::trace!(target: "insn", "{pc:08x}: {word:08x}  {}", ins.simplified());
            }
            None => log::trace!(target: "insn", "{pc:08x}: <unmapped>"),
        }
    }

    pub fn enable_profile(&mut self) {
        self.profile = Some(vec![0; PROFILE_WORDS]);
    }

    /// Hottest ROM addresses and 256-byte regions, for `--profile`.
    pub fn profile_report(&self) -> String {
        let Some(prof) = &self.profile else { return String::new() };
        let total: u64 = prof.iter().sum();
        let mut regions: Vec<(u32, u64)> = prof
            .chunks(64)
            .enumerate()
            .map(|(i, c)| (PROFILE_BASE + i as u32 * 256, c.iter().sum()))
            .filter(|&(_, n)| n > 0)
            .collect();
        regions.sort_by(|a, b| b.1.cmp(&a.1));
        let mut out = format!("execution profile: {total} instructions in ROM\r\n");
        for (addr, n) in regions.iter().take(25) {
            out += &format!("  {addr:#010x}..+0x100  {n:12}  {:5.1}%\r\n", *n as f64 * 100.0 / total.max(1) as f64);
        }
        out
    }

    /// `--stats`: what the guest used (exceptions, on-chip registers, unknown devices).
    pub fn stats_report(&mut self) -> String {
        let names = [
            (0x1, "system reset"), (0x2, "machine check"), (0x3, "DSI"), (0x4, "ISI"), (0x5, "external"),
            (0x6, "alignment"), (0x7, "program"), (0x9, "decrementer"), (0xC, "system call"), (0xD, "trace"),
            (0x10, "software emulation"), (0x11, "ITLB miss"), (0x12, "DTLB miss"),
        ];
        let mut out = String::from("exceptions taken:\r\n");
        for (i, n) in names {
            let c = self.cpu.exception_counts[i];
            if c > 0 {
                out += &format!("  {:#06x} {n:<20} {c}\r\n", i << 8);
            }
        }
        for &pc in &self.cpu.soft_emulation_pcs.clone() {
            let w = self.peek_word(pc).unwrap_or(0);
            let ins = Ins::new(w, Extensions::none());
            out += &format!("  software emulation at {pc:#010x}: {w:08x} {}\r\n", ins.simplified());
        }
        out += &self.board.mpc.stats_report();
        let d = self.board.io.duart_access;
        out += &format!(
            "DUART accesses: channel A (console) r={} w={}, channel B (AUX) r={} w={}\r\n",
            d[0].0, d[0].1, d[1].0, d[1].1
        );
        out += "CPLD registers accessed (0x68020000 + reg: reads/writes/value):\r\n";
        for (r, rd, wr, v) in self.board.io.cpld.used_registers() {
            out += &format!("  {r:#04x} r={rd:<10} w={wr:<8} value={v:#04x}\r\n");
        }
        if !self.board.io.unknown.is_empty() {
            out += "unknown I/O bus devices (offset from 0x68000000: reads/writes/last written):\r\n";
            for (off, (r, w, v)) in &self.board.io.unknown {
                out += &format!("  {:#010x} r={r} w={w} last={v:#04x}\r\n", 0x6800_0000 + off);
            }
        }
        out
    }

    /// Convenience for tests and the debugger: read a word without side effects.
    pub fn peek_word(&mut self, addr: u32) -> Option<u32> {
        self.board.peek_word(addr)
    }

    /// Bytes the guest printed on the console since the last call.
    pub fn take_console_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.board.io.uart.tx)
    }

    /// Deliver one byte typed by the user, if the UART FIFO has room.
    pub fn console_input(&mut self, byte: u8) -> bool {
        if !self.board.io.uart.rx_has_room() {
            return false;
        }
        self.board.io.uart.receive(byte);
        self.board.update_irq();
        true
    }

    /// The user pressed "send BREAK".
    pub fn console_break(&mut self) {
        self.board.io.uart.receive_break();
        self.board.update_irq();
    }

    /// Is the guest just spinning on "is there a character?"
    pub fn guest_is_idle(&self) -> bool {
        self.board.io.uart.idle_polls > 20_000
    }

    /// Read memory for debugging without device side effects.
    pub fn read_mem(&mut self, addr: u32, buf: &mut [u8]) -> usize {
        for (i, b) in buf.iter_mut().enumerate() {
            match self.board.peek8(addr.wrapping_add(i as u32)) {
                Some(v) => *b = v,
                None => return i,
            }
        }
        buf.len()
    }

    pub fn write_mem(&mut self, addr: u32, data: &[u8]) -> bool {
        data.iter().enumerate().all(|(i, &b)| self.board.poke8(addr.wrapping_add(i as u32), b))
    }

    /// Run until the guest prints `needle` or `budget` instructions pass.
    /// Used by the integration tests.
    pub fn run_until_output(&mut self, needle: &str, budget: u64, transcript: &mut Vec<u8>) -> bool {
        let mut done = 0;
        while done < budget {
            let slice = (budget - done).min(100_000);
            let stop = self.run(slice, true);
            done += slice;
            transcript.extend(self.take_console_output());
            if String::from_utf8_lossy(transcript).contains(needle) {
                return true;
            }
            if stop == StopReason::Checkstop {
                return false;
            }
        }
        false
    }
}
