//! The complete system: CPU core + board, and the loop that drives them.
//!
//! Time is *virtual*: one executed instruction = one CPU cycle. Devices are
//! advanced in batches instead of after every instruction: up to the next
//! timed device event (a PIT expiry, a network poll, FEC DMA work), at most
//! [`MAX_SYNC_INTERVAL`] instructions (docs ch. 17.16). The guest can't tell
//! the difference, and it keeps the inner loop tight.

use crate::board::{Board, BoardConfig, nvram::Nvram};
use crate::fastforward::FastForward;
use crate::idle::{Idle, IdleMode};
use crate::mpc860::rsr;
use powerpc::{Extensions, Ins};
use ppc8xx::jit::Jit;
use ppc8xx::{BatchEnd, Cpu, Platform, StepOutcome, msr};
use std::collections::{BTreeSet, HashMap};

/// Most instructions between two device updates (timers, interrupt line,
/// DMA). A device event due sooner shortens the stretch
/// (`Board::cycles_until_event`). It also bounds how late the devices see a
/// register write that starts something (a transmit, a timer).
pub const MAX_SYNC_INTERVAL: u64 = 4096;

/// PLPRCR[CSR]: checkstop reset enable.
const PLPRCR_CSR: u32 = 0x0000_0080;

/// How the CPU's instructions are executed (`--cpu`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CpuEngine {
    /// The interpreter: the reference implementation.
    Interp,
    /// The JIT: hot code translated to host machine code (docs ch. 17).
    Jit,
    /// The JIT, every region run verified against the interpreter
    /// (lockstep, much slower; docs ch. 17.12).
    JitCheck,
}

/// What [`Machine::run`] does after the per-address hooks.
enum Hook {
    /// Execute the instruction.
    Run,
    /// A hook changed the CPU state (bulk memory clear): look again.
    Again,
    Stop(StopReason),
}

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
    /// Sync the devices when `unsynced` reaches this: the next device event,
    /// as computed at the last sync.
    sync_horizon: u64,
    pub resets: u32,
    /// Optional execution profile: one counter per ROM instruction word.
    pub profile: Option<Vec<u64>>,
    /// Sampled PCs over the whole address space (every `SAMPLE_PERIOD`
    /// instructions), for code outside the ROM such as IOS: the whole run,
    /// and the window since the last `Ctrl-] p` (if any).
    pc_samples: Option<HashMap<u32, u64>>,
    pc_window: Option<(u64, HashMap<u32, u64>)>,
    sample_countdown: u32,
    /// Skips the ROM's timebase busy-wait loops (see `fastforward.rs`).
    pub fast_forward: FastForward,
    /// Skips IOS's idle scheduler loop (see `idle.rs`).
    pub idle: Idle,
    /// The idle anchor is chosen automatically (re-checked after resets).
    pub idle_auto: bool,
    /// Realtime pacing: stop a `run` once `cpu.cycles` passes this.
    pub pace_limit: Option<u64>,
    /// The last `run` stopped at the idle anchor because of `pace_limit`
    /// (the guest is idle and ahead of the wall clock).
    pub paced: bool,
    /// `--cpu jit`: the JIT runs the batches instead of the interpreter.
    /// (Breakpoints, tracing, profiling and `--watch` still single-step
    /// through the interpreter.)
    pub jit: Option<Box<Jit>>,
    /// The breakpoints the JIT's translations were made for: they are hook
    /// addresses to it (regions end before them), so it starts afresh when
    /// the set changes.
    jit_breakpoints: BTreeSet<u32>,
}

/// One PC sample every this many instructions. A prime, so the sampling
/// can't lock onto the period of a loop.
const SAMPLE_PERIOD: u32 = 997;

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
            sync_horizon: 1,
            resets: 0,
            profile: None,
            pc_samples: None,
            pc_window: None,
            sample_countdown: SAMPLE_PERIOD,
            fast_forward: FastForward::disabled(),
            idle: Idle::new(IdleMode::Off, None),
            idle_auto: false,
            pace_limit: None,
            paced: false,
            jit: None,
            jit_breakpoints: BTreeSet::new(),
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
        self.idle.reset(self.idle_auto);
        self.flush_jit();
        self.sync_devices();
    }

    /// The whole machine's state, for snapshots (docs ch. 20): the CPU,
    /// the board, and the emulator's own bookkeeping that affects what
    /// happens next (cycles not yet given to the devices, the idle
    /// detector). Breakpoints, profiles and the engine choice stay as
    /// configured; the JIT starts afresh.
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        self.cpu.visit_state(v);
        self.board.visit_state(v);
        v.u64(&mut self.unsynced);
        v.u64(&mut self.sync_horizon);
        v.u32(&mut self.resets);
        self.idle.visit_state(v);
        self.fast_forward.visit_state(v);
        if v.loading() {
            self.flush_jit();
        }
    }

    /// Select the CPU engine.
    pub fn set_engine(&mut self, engine: CpuEngine, jit_threshold: u32) {
        self.jit = (engine != CpuEngine::Interp).then(|| {
            let mut jit = Box::new(Jit::with_threshold(jit_threshold));
            jit.set_check(engine == CpuEngine::JitCheck);
            // Compile in a background thread, except when checking, which
            // verifies region runs one at a time (docs ch. 17.17).
            jit.set_background(engine == CpuEngine::Jit, &mut self.board);
            jit
        });
    }

    /// The CPU state was replaced: translations made for the old one go.
    fn flush_jit(&mut self) {
        if let Some(jit) = &mut self.jit {
            jit.flush(&mut self.board);
        }
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
        self.idle.reset(self.idle_auto);
        self.flush_jit();
        self.sync_devices();
    }

    /// Execute up to `budget` instructions.
    ///
    /// When `ignore_breakpoint_at_start` is true, a breakpoint on the current
    /// PC does not stop us before the first instruction (needed to continue
    /// from a breakpoint).
    pub fn run(&mut self, budget: u64, ignore_breakpoint_at_start: bool) -> StopReason {
        self.paced = false;
        let check_bp = !self.breakpoints.is_empty();
        // With the JIT, breakpoints are hooks: translated code stops before
        // them (`jit_batch`), so debugging keeps the JIT's speed (docs ch. 7.3).
        let bp_hooks = check_bp && self.jit.is_some();
        if self.jit.is_some() && self.breakpoints != self.jit_breakpoints {
            self.flush_jit();
            self.jit_breakpoints = self.breakpoints.clone();
        }
        // Breakpoints (interpreter), trace, profile and watch need a look at
        // every instruction; the other hooks live at a few known addresses,
        // so an ordinary instruction only pays for a page test (`is_hook`).
        let slow = (check_bp && !bp_hooks) || self.trace || self.profile.is_some() || self.board.watch.is_some();
        let mut done = 0;
        while done < budget {
            let pc = self.cpu.pc;
            if slow || is_hook(&self.fast_forward, &self.idle, pc) || (bp_hooks && self.breakpoints.contains(&pc)) {
                // One instruction at a time, with the per-address hooks.
                match self.before_step(pc, check_bp && !(done == 0 && ignore_breakpoint_at_start)) {
                    Hook::Run => {}
                    Hook::Again => continue,
                    Hook::Stop(reason) => return reason,
                }
                if let Some(w) = &mut self.board.watch {
                    w.pc = pc;
                }
                let outcome = self.cpu.step(&mut self.board);
                if let Some(w) = &mut self.board.watch {
                    w.pc = u32::MAX;
                }
                done += 1;
                self.unsynced += 1;
                if outcome == StepOutcome::Checkstop {
                    if let Some(reason) = self.on_checkstop() {
                        return reason;
                    }
                }
            } else {
                // Fast path: a batch until the next device sync, a hook page
                // or a checkstop, by the interpreter (step inlined) or the JIT.
                let max = (self.sync_horizon.saturating_sub(self.unsynced)).max(1).min(budget - done);
                let (n, end) = if self.jit.is_some() {
                    self.jit_batch(max)
                } else {
                    let (ff, idle) = (&self.fast_forward, &self.idle);
                    self.cpu.run_batch(&mut self.board, max, |pc| is_hook(ff, idle, pc))
                };
                done += n;
                self.unsynced += n;
                if end == BatchEnd::Checkstop {
                    if let Some(reason) = self.on_checkstop() {
                        return reason;
                    }
                }
            }
            if self.unsynced >= self.sync_horizon {
                self.sync_devices();
            }
        }
        StopReason::Budget
    }

    /// [`Machine::run`]'s batch with the JIT. A separate function, kept out
    /// of `run`: next to the interpreter's inlined loop it made the
    /// interpreter 4% slower even when not used (measured).
    #[inline(never)]
    fn jit_batch(&mut self, max: u64) -> (u64, BatchEnd) {
        let (ff, idle, bps) = (&self.fast_forward, &self.idle, &self.breakpoints);
        let jit = self.jit.as_mut().expect("JIT engine selected");
        jit.run_batch(&mut self.cpu, &mut self.board, max, |pc| is_hook(ff, idle, pc) || (!bps.is_empty() && bps.contains(&pc)))
    }

    /// The per-instruction hooks, run before executing the instruction at `pc`.
    fn before_step(&mut self, pc: u32, check_bp: bool) -> Hook {
        if check_bp && self.breakpoints.contains(&pc) {
            self.sync_devices();
            return Hook::Stop(StopReason::Breakpoint(pc));
        }
        if self.trace {
            self.trace_instruction(pc);
        }
        if self.idle.is_candidate(pc) {
            let code = [0, 4, 8, 12].map(|o| self.peek_word(pc + o).unwrap_or(0));
            self.idle.check_candidate(pc, code);
        }
        if self.idle.anchor == Some(pc) && self.idle.at_anchor(self.cpu.cycles, self.cpu.msr & msr::EE != 0) {
            self.idle_skip();
            // Ahead of the wall clock: stop and let the caller wait, unless
            // the sync in `idle_skip` brought an interrupt (a frame from the
            // network, a key): that is work, do it now (docs ch. 18.5).
            if self.pace_limit.is_some_and(|limit| self.cpu.cycles >= limit) && !self.interrupt_pending() {
                self.paced = true;
                return Hook::Stop(StopReason::Budget);
            }
        }
        if self.fast_forward.is_bzero(pc) && self.board.watch.is_none() && !self.interrupt_pending() {
            if let Some(fill) = self.fast_forward.bzero_plan(&self.cpu, self.pit_horizon()) {
                if self.board.fill_dram(fill.addr, fill.len, fill.value) {
                    self.unsynced += self.fast_forward.bzero_done(&mut self.cpu, &fill);
                    return Hook::Again; // the CPU state changed: re-check from the top
                }
            }
        }
        if self.fast_forward.is_candidate(pc) && !self.interrupt_pending() {
            // The guest is about to spin on the timebase: let the time pass.
            let pit = self.pit_horizon();
            self.unsynced += self.fast_forward.try_skip(&mut self.cpu, pit);
        }
        if let Some(prof) = &mut self.profile {
            let idx = (pc.wrapping_sub(PROFILE_BASE) / 4) as usize;
            if idx < PROFILE_WORDS {
                prof[idx] += 1;
            }
            self.sample_countdown -= 1;
            if self.sample_countdown == 0 {
                self.sample_countdown = SAMPLE_PERIOD;
                if let Some(samples) = &mut self.pc_samples {
                    *samples.entry(pc).or_insert(0) += 1;
                }
                if let Some((_, window)) = &mut self.pc_window {
                    *window.entry(pc).or_insert(0) += 1;
                }
            }
        }
        Hook::Run
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
    /// Will the CPU take an interrupt at the next instruction? Time must not
    /// be skipped then, or the skipped periods would be lost.
    fn interrupt_pending(&self) -> bool {
        self.cpu.msr & msr::EE != 0 && (self.cpu.dec_pending || self.board.external_interrupt())
    }

    /// CPU cycles until the PIT's next interrupt, counting the cycles not yet
    /// given to the devices (they advance in batches).
    fn pit_horizon(&self) -> Option<u64> {
        self.board.mpc.cycles_until_pit_interrupt().map(|c| c.saturating_sub(self.unsynced))
    }

    /// The scheduler is idle: let virtual time pass up to the next timer
    /// interrupt (one timebase tick short), never past it.
    fn idle_skip(&mut self) {
        self.sync_devices();
        if self.interrupt_pending() {
            return; // something is already pending: let it be taken
        }
        let div = self.cpu.tb_divider as u64;
        let until_dec = if self.cpu.tb_enabled { self.cpu.ticks_until_decrementer().map(|t| t * div) } else { None };
        let until_pit = self.board.mpc.cycles_until_pit_interrupt();
        let Some(skip) = self.idle.skip_cycles(until_dec, until_pit, div) else { return };
        if self.cpu.tb_enabled {
            self.cpu.skip_timebase(skip / div);
        } else {
            self.cpu.cycles += skip;
        }
        self.unsynced += skip;
        self.sync_devices();
    }

    pub fn sync_devices(&mut self) {
        self.board.advance(self.unsynced);
        self.unsynced = 0;
        self.cpu.tb_enabled = self.board.mpc.timebase_enabled();
        self.sync_horizon = self.board.cycles_until_event().clamp(1, MAX_SYNC_INTERVAL);
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
        self.pc_samples = Some(HashMap::new());
    }

    /// All PC samples as text lines `address whole_run window`, for
    /// `--profile-dump` (offline analysis, e.g. against IDA functions).
    pub fn profile_dump(&self) -> String {
        let Some(samples) = &self.pc_samples else { return String::new() };
        let window = self.pc_window.as_ref().map(|(_, w)| w);
        let mut addrs: Vec<u32> = samples.keys().copied().collect();
        addrs.sort_unstable();
        let mut out = String::from("# address samples_whole_run samples_window\n");
        for a in addrs {
            let w = window.and_then(|w| w.get(&a)).copied().unwrap_or(0);
            out += &format!("{a:#010x} {} {w}\n", samples[&a]);
        }
        out
    }

    /// Start a profile window (console `Ctrl-] p`), e.g. once IOS is idle at
    /// its prompt. The whole-run histogram keeps counting.
    pub fn reset_profile(&mut self) {
        if self.profile.is_some() {
            self.pc_window = Some((self.cpu.cycles, HashMap::new()));
            log::info!("profile window started at {} instructions", self.cpu.cycles);
        }
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
        if let Some(samples) = &self.pc_samples {
            out += &sample_report(&format!("whole run ({} instructions)", self.cpu.cycles), samples);
        }
        if let Some((start, window)) = &self.pc_window {
            let title = format!("window since Ctrl-] p ({} instructions)", self.cpu.cycles - start);
            out += &sample_report(&title, window);
        }
        out
    }

    /// `--stats`: what the guest used (exceptions, on-chip registers, unknown devices).
    pub fn stats_report(&mut self) -> String {
        let names = [
            (0x1, "system reset"), (0x2, "machine check"), (0x3, "DSI"), (0x4, "ISI"), (0x5, "external"),
            (0x6, "alignment"), (0x7, "program"), (0x9, "decrementer"), (0xC, "system call"), (0xD, "trace"),
            (0x10, "software emulation"), (0x11, "ITLB miss"), (0x12, "DTLB miss"),
            (0x13, "ITLB error"), (0x14, "DTLB error"),
        ];
        let mut out = String::from("exceptions taken:\r\n");
        for (i, n) in names {
            let c = self.cpu.exception_counts[i];
            if c > 0 {
                out += &format!("  {:#06x} {n:<20} {c}\r\n", i << 8);
            }
        }
        let mmu = &self.cpu.mmu;
        if mmu.itlb_misses + mmu.dtlb_misses + mmu.tlb_errors > 0 {
            let (i, d) = mmu.occupancy();
            out += &format!(
                "MMU: {} ITLB misses, {} DTLB misses, {} TLB errors; valid entries now {i} ITLB / {d} DTLB\r\n",
                mmu.itlb_misses, mmu.dtlb_misses, mmu.tlb_errors
            );
        }
        if self.idle.anchor.is_some() {
            out += &self.idle.report();
        }
        if let Some(jit) = &self.jit {
            out += &jit.report();
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
        let slots = self.board.io.slot_access;
        if slots.iter().any(|&(r, w, _)| r + w > 0) {
            out += "WIC/VIC slots (window accesses):";
            for (i, (r, w, v)) in slots.iter().enumerate() {
                let card = self.board.io.wics[i].as_ref().map_or("empty".to_string(), |w| format!("{:?}", w.kind));
                out += &format!(" slot {i} ({card}) r={r} w={w} last={v:#04x};");
            }
            out += "\r\n";
        }
        out += &self.board.scc_report();
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
        self.board.peek_word(self.debugger_address(addr))
    }

    /// Physical address a debugger means by `ea`: when the guest runs with
    /// translation on, look it up in the TLBs (without side effects);
    /// otherwise, or if no entry maps it, use it as a physical address.
    pub fn debugger_address(&self, ea: u32) -> u32 {
        let msr = self.cpu.msr;
        if msr & (ppc8xx::msr::IR | ppc8xx::msr::DR) == 0 {
            return ea;
        }
        self.cpu.mmu.probe(ea, false).or_else(|| self.cpu.mmu.probe(ea, true)).unwrap_or(ea)
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

    /// Bytes the guest sent out of the AUX port (DUART channel B).
    pub fn take_aux_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.board.io.aux.tx)
    }

    /// Deliver one byte to the AUX port, if its receive FIFO has room.
    pub fn aux_input(&mut self, byte: u8) -> bool {
        if !self.board.io.aux.rx_has_room() {
            return false;
        }
        self.board.io.aux.receive(byte);
        self.board.update_irq();
        true
    }

    pub fn aux_break(&mut self) {
        self.board.io.aux.receive_break();
        self.board.update_irq();
    }

    /// Is the guest just spinning on "is there a character?"
    pub fn guest_is_idle(&self) -> bool {
        self.board.io.uart.idle_polls > 20_000
    }

    /// Read memory for debugging without device side effects.
    pub fn read_mem(&mut self, addr: u32, buf: &mut [u8]) -> usize {
        for (i, b) in buf.iter_mut().enumerate() {
            match self.board.peek8(self.debugger_address(addr.wrapping_add(i as u32))) {
                Some(v) => *b = v,
                None => return i,
            }
        }
        buf.len()
    }

    pub fn write_mem(&mut self, addr: u32, data: &[u8]) -> bool {
        data.iter().enumerate().all(|(i, &b)| {
            let pa = self.debugger_address(addr.wrapping_add(i as u32));
            self.board.poke8(pa, b)
        })
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

/// Hottest sampled PCs, and the same samples grouped in 4 KB pages (enough
/// to tell phases such as decompression, initialisation and idle apart).
fn sample_report(title: &str, samples: &HashMap<u32, u64>) -> String {
    let total: u64 = samples.values().sum::<u64>().max(1);
    let pct = |n: u64| n as f64 * 100.0 / total as f64;
    let mut hot: Vec<(u32, u64)> = samples.iter().map(|(&a, &n)| (a, n)).collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1));
    let mut pages: HashMap<u32, u64> = HashMap::new();
    for (&a, &n) in samples {
        *pages.entry(a & !0xFFF).or_insert(0) += n;
    }
    let mut pages: Vec<(u32, u64)> = pages.into_iter().collect();
    pages.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = format!("sampled PCs, {title}: {total} samples (1 per {SAMPLE_PERIOD} instructions)\r\n");
    out += "  hottest 4 KB pages:\r\n";
    for (page, n) in pages.iter().take(15) {
        out += &format!("    {page:#010x}  {n:10}  {:5.1}%\r\n", pct(*n));
    }
    out += "  hottest addresses:\r\n";
    for (addr, n) in hot.iter().take(30) {
        out += &format!("    {addr:#010x}  {n:10}  {:5.1}%\r\n", pct(*n));
    }
    out
}

/// Is there a per-address hook at `pc`: one of the ROM's delay loops or its
/// memory clear (when the accelerators are on), the idle anchor or an
/// auto-mode candidate? Ordinary code pays for the page tests only; the
/// exact address is compared on the few pages that have hooks (docs ch. 16.4, 17.6).
#[inline(always)]
fn is_hook(ff: &FastForward, idle: &Idle, pc: u32) -> bool {
    let page = pc >> 12;
    if page == 0xFFF12 || page == 0xFFF16 {
        return ff.is_hook(pc);
    }
    idle.hook_pages.contains(&page) && (idle.anchor == Some(pc) || idle.is_candidate(pc))
}
