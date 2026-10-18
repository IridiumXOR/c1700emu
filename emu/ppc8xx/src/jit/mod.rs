//! A just-in-time compiler: the second way to run the CPU, next to the
//! interpreter (`Cpu::run_batch`). Same `Cpu` state, same [`Platform`], same
//! results instruction for instruction; only faster on hot code. Chapter 17
//! of the docs explains the design step by step.
//!
//! * A **block** is straight-line guest code starting at one address: up to
//!   a branch, an instruction the translator leaves to the interpreter, the
//!   end of the page or [`scan::MAX_BLOCK`] instructions.
//! * The dispatcher ([`Jit::run_batch`]) interprets code until a block's
//!   start address has been reached `threshold` times, then translates the
//!   block to host code with Cranelift and from then on runs the
//!   translation.
//! * **Exactness.** Interrupts are recognised at block starts, exactly where
//!   `Cpu::step` would check them, and a block only runs if the decrementer
//!   can't fire inside it. A device access that raises an interrupt ends
//!   the block after that instruction. Exceptions inside a block are precise
//!   (see `codegen.rs`). Time advances by the block's length when it ends.
//! * **Self-modifying code.** The platform watches the instruction words
//!   that were translated and reports writes to them
//!   ([`Platform::watch_code`]); the translations covering a written word
//!   are dropped and retranslated when needed.
//! * **Chaining.** A region exit to a known address jumps straight into
//!   the region there once the dispatcher has linked the two (see
//!   `codegen.rs`). Nothing the dispatcher checks between regions can
//!   change on such a path: interrupts can only appear through a helper
//!   (which ends the region), the decrementer is covered by the budget, the
//!   MSR and the MMU only change in the interpreter. Links die together
//!   (`JitCtx::chain_epoch`) when a region is dropped or the instruction
//!   translation may have changed.

mod check;
mod codegen;
mod runtime;
mod scan;
mod watch;

pub use watch::{CodeWatch, WORDS_PER_PAGE, word_id};

use crate::mmu::{Access, Fault};
use crate::{BatchEnd, Cpu, Exception, Platform, StepOutcome, msr};
use codegen::{Compiler, EARLY, LINKS_PER_BLOCK, Link, RAISED, RegionCode, STEPS};
use powerpc::{Extensions, Ins};
use runtime::{Helpers, JitCtx, mode_index};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::{Duration, Instant};

/// Times a block's start is reached before the block is translated.
/// Translating costs ~50-100 µs, a few thousand interpreted instructions;
/// code that runs fewer times than this is cheaper to interpret.
pub const DEFAULT_THRESHOLD: u32 = 50;

/// Generated code is thrown away (and regenerated when needed) beyond this.
const CODE_LIMIT: usize = 64 << 20;

/// The JIT indexes its translations by 32-byte lines of the platform's
/// pages: a written word drops the translations registered on its line.
const INDEX_LINE: u32 = 32;

fn index_line(page: u32, offset_in_page: u32) -> u32 {
    word_id(page, offset_in_page) / (INDEX_LINE / 4)
}

/// MSR bits a translation depends on: they are part of the block's key.
const MODE_BITS: u32 = msr::IR | msr::DR | msr::PR;

/// Slots of the jump cache: a direct-mapped table (by PC) of recently run
/// translated blocks, in front of the hash table.
const JUMP_ENTRIES: usize = 4096;

#[derive(Clone, Copy)]
struct JumpEntry {
    key: u64,
    pa: u32,
    block: Option<Translated>,
}

const NO_JUMP: JumpEntry = JumpEntry { key: u64::MAX, pa: 0, block: None };

fn jump_slot(pc: u32) -> usize {
    (pc >> 2) as usize % JUMP_ENTRIES
}

/// A ready entry point: block number `entry` of a translated region.
#[derive(Clone, Copy)]
struct Translated {
    func: RegionCode,
    entry: u32,
    /// Length of that block: it must fit before the region is entered.
    len: u32,
}

enum State {
    /// Interpreted; reached this many times.
    Cold(u32),
    Translated(Translated),
    /// Its first instruction (this word) is one the translator leaves to the
    /// interpreter.
    FirstIsStop(u32),
    /// The platform can't watch this memory for writes (not plain RAM/ROM).
    Unwatched,
}

struct Entry {
    /// Physical address the block was found at (the key is the effective
    /// address; if the MMU now maps it elsewhere, the entry is stale).
    pa: u32,
    state: State,
}

/// What the dispatcher does with the code at the current PC.
enum Action {
    Run(Translated),
    Translate,
    /// Interpret up to the next change of control flow.
    Stretch(Interpreted),
    /// Interpret one instruction.
    One(u32),
}

/// Why instructions were interpreted, for the statistics.
#[derive(Clone, Copy)]
enum Interpreted {
    Cold,
    /// The translated block didn't fit before the batch end or the next
    /// decrementer interrupt.
    Tail,
    Unwatched,
}

#[derive(Clone, Default, Debug)]
pub struct JitStats {
    /// Regions translated, and entry points (blocks) in them.
    pub blocks_translated: u64,
    pub entries_translated: u64,
    pub translate_time: Duration,
    /// Guest instructions in all translated blocks (for the average size).
    pub translated_instructions: u64,
    pub block_runs: u64,
    /// Regions entered straight from another one (chaining).
    pub chained_runs: u64,
    /// Links filled in by the dispatcher.
    pub links_made: u64,
    /// Instructions executed by translated code.
    pub jit_instructions: u64,
    /// Instructions executed by the interpreter, by reason.
    pub cold: u64,
    pub tail: u64,
    pub unwatched: u64,
    pub stops: u64,
    /// Interrupts and ITLB exceptions entered by the dispatcher.
    pub entries: u64,
    /// Exceptions raised inside translated blocks.
    pub exceptions: u64,
    /// Blocks that stopped early (interrupt raised, code written...).
    pub early_exits: u64,
    /// Region runs verified against the interpreter (`--cpu jit-check`).
    pub checked_runs: u64,
    pub invalidated_blocks: u64,
    pub flushes: u64,
    /// Untranslatable first instructions, by instruction word.
    pub stop_words: HashMap<u32, u64>,
}

/// The dispatcher's block table is keyed by `pc | mode << 32`.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0 ^ b as u64);
        }
    }
    fn write_u64(&mut self, k: u64) {
        let h = k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type BlockMap = HashMap<u64, Entry, BuildHasherDefault<KeyHasher>>;

pub struct Jit {
    ctx: Box<JitCtx>,
    compiler: Compiler,
    blocks: BlockMap,
    jump: Box<[JumpEntry]>,
    /// 32-byte line (`index_line`) → regions with code on it.
    lines: HashMap<u32, Vec<usize>>,
    /// Translated regions: the keys of their entry points (`None` once
    /// dropped). A region is one function, so it is dropped as a whole.
    regions: Vec<Option<Vec<u64>>>,
    threshold: u32,
    /// Lockstep checking: verify every region run with the interpreter.
    check: bool,
    /// The links of all regions translated since the last flush (the
    /// generated code holds pointers into them).
    links: Vec<Box<[Link]>>,
    /// The link of the exit the last region run left by, to fill in if the
    /// next thing the dispatcher does is running a region at its target.
    pending_link: *mut Link,
    /// `Mmu::instruction_generation` the links were made under.
    itlb_generation: u64,
    pub stats: JitStats,
}

impl Default for Jit {
    fn default() -> Self {
        Self::new()
    }
}

impl Jit {
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_THRESHOLD)
    }

    /// `threshold` = 1 translates every block the first time it runs (tests).
    pub fn with_threshold(threshold: u32) -> Self {
        Jit {
            ctx: JitCtx::new(),
            compiler: Compiler::new(),
            blocks: BlockMap::default(),
            jump: vec![NO_JUMP; JUMP_ENTRIES].into_boxed_slice(),
            lines: HashMap::new(),
            regions: Vec::new(),
            threshold: threshold.max(1),
            check: false,
            links: Vec::new(),
            pending_link: std::ptr::null_mut(),
            itlb_generation: 0,
            stats: JitStats::default(),
        }
    }

    /// Run up to `max` instructions, like [`Cpu::run_batch`] and with the
    /// same results. `stop_before` is asked at every block start and, when
    /// a block is translated, for each of its instructions: a block ends
    /// before an address where it is true. Translations assume its answers
    /// don't change; call [`Jit::flush`] if they do (an address that stops
    /// being a hook is harmless: blocks just stay a bit shorter).
    ///
    /// Never inlined: inlined into the caller's loop next to the
    /// interpreter's `run_batch`, it made the interpreter 4% slower.
    #[inline(never)]
    pub fn run_batch<P: Platform>(&mut self, cpu: &mut Cpu, sys: &mut P, max: u64, mut stop_before: impl FnMut(u32) -> bool) -> (u64, BatchEnd) {
        self.ctx.helpers = Helpers::new::<P>();
        self.sync(cpu, sys);
        self.pending_link = std::ptr::null_mut();
        let mut n = 0;
        while n < max {
            let pc = cpu.pc;
            // Anything but running a region right here forgets the link.
            let link = std::mem::replace(&mut self.pending_link, std::ptr::null_mut());
            if stop_before(pc) {
                return (n, BatchEnd::Hook);
            }
            // What `Cpu::step` does first: take a pending interrupt (as one
            // step, without advancing the clock). The interpreter does it.
            if cpu.msr & msr::EE != 0 && (cpu.dec_pending || sys.external_interrupt()) {
                n += 1;
                self.stats.entries += 1;
                if cpu.step(sys) == StepOutcome::Checkstop {
                    return (n, BatchEnd::Checkstop);
                }
                continue;
            }
            // Then translate the fetch address; a fault is an exception
            // entry, again one step without a clock tick.
            let pa = if cpu.msr & msr::IR != 0 {
                match cpu.mmu.translate(pc, Access::Fetch, cpu.msr & msr::PR != 0) {
                    Ok(pa) => pa,
                    Err(fault) => {
                        let exc = match fault {
                            Fault::Miss => Exception::ItlbMiss,
                            Fault::Error(status) => Exception::ItlbError { status },
                        };
                        cpu.enter_exception(exc, pc);
                        n += 1;
                        self.stats.entries += 1;
                        continue;
                    }
                }
            } else {
                pc
            };

            let key = pc as u64 | ((cpu.msr & MODE_BITS) as u64) << 32;
            let room = max - n;
            let fits = |t: &Translated| {
                let len = t.len as u64;
                len <= room && (cpu.msr & msr::EE == 0 || cpu.instructions_until_decrementer_request() >= len)
            };
            let slot = jump_slot(pc);
            let jump = self.jump[slot];
            let action = if let Some(t) = jump.block.filter(|_| jump.key == key && jump.pa == pa) {
                if fits(&t) { Action::Run(t) } else { Action::Stretch(Interpreted::Tail) }
            } else {
                let entry = self.blocks.entry(key).or_insert(Entry { pa, state: State::Cold(0) });
                if entry.pa != pa {
                    *entry = Entry { pa, state: State::Cold(0) };
                }
                match &mut entry.state {
                    State::Translated(t) => {
                        self.jump[slot] = JumpEntry { key, pa, block: Some(*t) };
                        if fits(t) { Action::Run(*t) } else { Action::Stretch(Interpreted::Tail) }
                    }
                    State::Cold(hits) => {
                        *hits += 1;
                        if *hits >= self.threshold { Action::Translate } else { Action::Stretch(Interpreted::Cold) }
                    }
                    State::FirstIsStop(word) => Action::One(*word),
                    State::Unwatched => Action::Stretch(Interpreted::Unwatched),
                }
            };

            let (k, end) = match action {
                Action::Run(t) => {
                    if !link.is_null() {
                        // SAFETY: links live until the next flush, and a
                        // flush clears `pending_link`.
                        let link = unsafe { &mut *link };
                        if link.target == pc || link.computed {
                            *link = Link { func: t.func, epoch: self.ctx.chain_epoch, entry: t.entry, len: t.len, target: pc, computed: link.computed };
                            self.stats.links_made += 1;
                        }
                    }
                    let (k, end) = if self.check { self.run_checked(cpu, sys, t, room, pc, pa) } else { self.run_translated(cpu, sys, t, room) };
                    n += k;
                    if end == BatchEnd::Checkstop {
                        return (n, end);
                    }
                    if std::mem::take(&mut self.ctx.touched) {
                        self.sync(cpu, sys);
                    }
                    continue;
                }
                Action::Translate => {
                    if let Some(state) = self.translate(cpu, sys, pc, pa, key, &mut stop_before) {
                        self.blocks.insert(key, Entry { pa, state });
                    }
                    continue; // dispatch again: now it runs (or is interpreted)
                }
                Action::Stretch(why) => self.interpret(cpu, sys, room, false, why, &mut stop_before),
                Action::One(word) => {
                    *self.stats.stop_words.entry(word).or_default() += 1;
                    self.stats.stops += 1;
                    self.interpret(cpu, sys, room, true, Interpreted::Cold, &mut stop_before)
                }
            };
            n += k;
            if end == BatchEnd::Checkstop {
                return (n, end);
            }
            self.sync(cpu, sys);
        }
        (n, BatchEnd::Budget)
    }

    /// Lockstep checking (`--cpu jit-check`, docs ch. 17.12): every region
    /// run is replayed by the interpreter and compared; a difference stops
    /// the emulator with a report. Much slower; for verification.
    pub fn set_check(&mut self, on: bool) {
        self.check = on;
        // Every access must go through the helpers to be recorded.
        self.ctx.fill = !on;
        self.ctx.tables.flush_all();
    }

    /// Throw every translation away (the machine was reset, or memory was
    /// rewritten behind the platform's back).
    pub fn flush<P: Platform>(&mut self, sys: &mut P) {
        self.ctx.chain_epoch += 1;
        self.pending_link = std::ptr::null_mut();
        self.links.clear();
        self.blocks.clear();
        self.jump.fill(NO_JUMP);
        self.lines.clear();
        self.regions.clear();
        sys.unwatch_code_pages();
        self.ctx.code_writes.clear();
        self.ctx.tables.flush_all();
        self.ctx.map_epoch = sys.memory_map_epoch();
        self.ctx.host_epoch = sys.host_page_epoch();
        self.ctx.mmu_generation = u64::MAX; // re-read at the next sync
        self.compiler.reset();
        self.stats.flushes += 1;
    }

    /// Run a translated region with at most `room` instructions (fewer if
    /// the decrementer would fire sooner).
    fn run_translated<P: Platform>(&mut self, cpu: &mut Cpu, sys: &mut P, t: Translated, room: u64) -> (u64, BatchEnd) {
        let mut budget = room;
        if cpu.msr & msr::EE != 0 {
            budget = budget.min(cpu.instructions_until_decrementer_request());
        }
        let budget = budget.min(STEPS as u64) as u32;
        let mode = mode_index(cpu.msr);
        let ctx = &mut *self.ctx;
        ctx.read_table = ctx.tables.read[mode].as_mut_ptr();
        ctx.write_table = ctx.tables.write[mode].as_mut_ptr();
        // One raw pointer each, used by the block and its helpers; `cpu` and
        // `sys` are not touched until the block returns.
        let (cpu_ptr, sys_ptr): (*mut Cpu, *mut P) = (cpu, sys);
        ctx.cpu = cpu_ptr;
        ctx.sys = sys_ptr as *mut ();
        ctx.exit_link = std::ptr::null_mut();
        let chains = ctx.chains;
        let ctx_ptr: *mut JitCtx = ctx;
        // SAFETY: the region was generated for this JitCtx/Cpu layout; the
        // pointers are live and exclusively used by the code for the call.
        let r = unsafe { (self.compiler.trampoline)(ctx_ptr, cpu_ptr, budget, t.entry, t.func) };

        let steps = (r & STEPS) as u64;
        cpu.advance_clock(steps);
        self.stats.block_runs += 1;
        self.stats.chained_runs += self.ctx.chains - chains;
        // Left by a linkable exit: the dispatcher links it to what runs next
        // (not when checking, which verifies one region run at a time).
        if r & (RAISED | EARLY) == 0 && !self.check {
            self.pending_link = self.ctx.exit_link;
        }
        self.stats.jit_instructions += steps;
        if r & RAISED != 0 {
            // Precise exception at the region's `steps`-th instruction; like
            // the interpreter, that instruction's clock tick counts.
            let fault_pc = self.ctx.fault_pc;
            cpu.pc = fault_pc;
            let exc = self.ctx.exception.take().expect("a block reported an exception without one");
            self.stats.exceptions += 1;
            if cpu.enter_exception(exc, fault_pc) == StepOutcome::Checkstop {
                return (steps, BatchEnd::Checkstop);
            }
        } else if r & EARLY != 0 {
            self.stats.early_exits += 1;
        }
        (steps, BatchEnd::Budget)
    }

    /// [`Jit::run_translated`] verified against the interpreter (see `check`).
    fn run_checked<P: Platform>(&mut self, cpu: &mut Cpu, sys: &mut P, t: Translated, room: u64, pc: u32, pa: u32) -> (u64, BatchEnd) {
        let before = cpu.arch_state();
        let code = check::CodePage::capture(sys, pa);
        let irq0 = sys.external_interrupt();
        let mut rec = check::Recorder { inner: sys, log: Vec::new() };
        self.ctx.helpers = Helpers::new::<check::Recorder<P>>();
        let (steps, end) = self.run_translated(cpu, &mut rec, t, room);
        let log = std::mem::take(&mut rec.log);
        self.ctx.helpers = Helpers::new::<P>();
        let jit_state = cpu.arch_state();
        match check::replay(cpu, sys, &code, &before, &log, irq0, steps) {
            Ok(interp_state) if interp_state == jit_state => {}
            Ok(interp_state) => panic!(
                "JIT check failed: region at {pc:#010x}, {steps} instructions: interpreter vs JIT:{}",
                check::diff(&interp_state, &jit_state)
            ),
            Err(e) => panic!("JIT check failed: region at {pc:#010x}, {steps} instructions: {e}"),
        }
        // Keep the JIT's result (equal to the interpreter's, but the MMU and
        // devices already saw the JIT's run).
        cpu.set_arch_state(&jit_state);
        self.stats.checked_runs += 1;
        (steps, end)
    }

    /// Interpret one instruction, or (`one` = false) up to the next change
    /// of control flow, the next 1 KB boundary, a hook or `room` steps.
    fn interpret<P: Platform>(
        &mut self,
        cpu: &mut Cpu,
        sys: &mut P,
        room: u64,
        one: bool,
        why: Interpreted,
        stop_before: &mut impl FnMut(u32) -> bool,
    ) -> (u64, BatchEnd) {
        let mut k = 0;
        let mut end = BatchEnd::Budget;
        loop {
            let pc = cpu.pc;
            let outcome = cpu.step(sys);
            k += 1;
            if outcome == StepOutcome::Checkstop {
                end = BatchEnd::Checkstop;
                break;
            }
            let next = cpu.pc;
            if one || k >= room || next != pc.wrapping_add(4) || next & 0x3FF == 0 || stop_before(next) {
                break;
            }
        }
        match why {
            Interpreted::Cold => self.stats.cold += k,
            Interpreted::Tail => self.stats.tail += k,
            Interpreted::Unwatched => self.stats.unwatched += k,
        }
        (k, end)
    }

    /// Translate the region starting at `pc` (physical `pa`). All its block
    /// starts become ready entries in the block table. Returns the state to
    /// record for `pc` if it couldn't be translated.
    fn translate<P: Platform>(
        &mut self,
        cpu: &Cpu,
        sys: &mut P,
        pc: u32,
        pa: u32,
        key: u64,
        stop_before: &mut impl FnMut(u32) -> bool,
    ) -> Option<State> {
        if self.compiler.code_bytes > CODE_LIMIT {
            self.flush(sys);
        }
        let mode = key & !0xFFFF_FFFF;
        // With translation on, 1 KB sub-pages can have different
        // permissions: a block never crosses one. Real mode: the 4 KB page.
        let boundary = if cpu.msr & msr::IR != 0 { 1024 } else { 4096 };
        // A region also stops where another one already has an entry: each
        // piece of code is translated once, and execution passes between
        // regions through the dispatcher.
        let blocks = &self.blocks;
        let translated = |a: u32| matches!(blocks.get(&(a as u64 | mode)), Some(Entry { state: State::Translated(_), .. }));
        let mut stop = |a: u32| stop_before(a) || (a != pc && translated(a));
        let region = scan::scan_region(sys, pc, pa, boundary, cpu.msr & msr::PR != 0, &mut stop);
        if region.is_empty() {
            return Some(State::FirstIsStop(sys.fetch(pa).unwrap_or(0)));
        }
        // Watch the code (nothing runs between reading and watching it).
        // The region is inside one page, so all its blocks have one page id.
        let mut watched = Vec::new();
        for block in &region {
            let block_pa = pa.wrapping_add(block.start.wrapping_sub(pc));
            let bytes = 4 * block.insns.len() as u32;
            let Some(page) = sys.watch_code(block_pa, bytes) else { return Some(State::Unwatched) };
            let offset = block_pa & 0xFFF;
            for o in (offset & !(INDEX_LINE - 1)..offset + bytes).step_by(INDEX_LINE as usize) {
                watched.push(index_line(page, o));
            }
        }
        // Store entries for watched pages must go: stores to code have to
        // reach the platform, which checks them.
        self.ctx.tables.flush_writes();
        let start = Instant::now();
        let mut links: Box<[Link]> = (0..LINKS_PER_BLOCK * region.len()).map(|_| Link::UNUSED).collect();
        match self.compiler.compile(&region, links.as_mut_ptr()) {
            Ok((func, targets)) => {
                for (link, target) in links.iter_mut().zip(targets) {
                    match target {
                        Some(pc) => link.target = pc,
                        None => link.computed = true,
                    }
                }
                self.links.push(links);
                self.stats.blocks_translated += 1;
                self.stats.entries_translated += region.len() as u64;
                self.stats.translated_instructions += region.iter().map(|b| b.insns.len() as u64).sum::<u64>();
                self.stats.translate_time += start.elapsed();
                let id = self.regions.len();
                let mut keys = Vec::new();
                for (i, block) in region.iter().enumerate() {
                    let k = block.start as u64 | mode;
                    let t = Translated { func, entry: i as u32, len: block.insns.len() as u32 };
                    self.blocks.insert(k, Entry { pa: pa.wrapping_add(block.start.wrapping_sub(pc)), state: State::Translated(t) });
                    keys.push(k);
                }
                self.regions.push(Some(keys));
                watched.sort_unstable();
                watched.dedup();
                for line in watched {
                    self.lines.entry(line).or_default().push(id);
                }
                None
            }
            Err(e) => {
                log::error!(target: "jit", "cannot translate the region at {pc:#010x}: {e}");
                Some(State::Unwatched)
            }
        }
    }

    /// Bring the JIT's caches in line with the platform and the MMU.
    fn sync<P: Platform>(&mut self, cpu: &Cpu, sys: &mut P) {
        while let Some(line) = sys.take_code_write() {
            self.ctx.code_writes.push(line);
        }
        while let Some(word) = self.ctx.code_writes.pop() {
            let line = word / (INDEX_LINE / 4);
            for id in self.lines.remove(&line).unwrap_or_default() {
                // (A region is listed under each of its lines: maybe gone already.)
                let Some(keys) = self.regions[id].take() else { continue };
                log::debug!(target: "jit", "code word {word:#x} written: region at {:#010x} dropped", keys[0] as u32);
                self.stats.invalidated_blocks += 1;
                self.ctx.chain_epoch += 1; // links may lead into it
                for k in keys {
                    self.blocks.remove(&k);
                    let slot = jump_slot(k as u32);
                    if self.jump[slot].key == k {
                        self.jump[slot] = NO_JUMP;
                    }
                }
            }
        }
        let epoch = sys.memory_map_epoch();
        if epoch != self.ctx.map_epoch {
            // The memory controller was reprogrammed: what is behind an
            // address may have changed, code included.
            log::debug!(target: "jit", "memory map changed (epoch {} -> {epoch}) at pc={:#010x}: flushing all translations", self.ctx.map_epoch, cpu.pc);
            self.flush(sys);
        }
        let epoch = sys.host_page_epoch();
        if epoch != self.ctx.host_epoch {
            // Some memory read directly stopped being plain (flash command).
            self.ctx.host_epoch = epoch;
            self.ctx.tables.flush_all();
        }
        let generation = cpu.mmu.data_generation();
        if generation != self.ctx.mmu_generation {
            self.ctx.mmu_generation = generation;
            self.ctx.tables.flush_translated();
        }
        // A linked exit doesn't translate its target address again.
        let generation = cpu.mmu.instruction_generation();
        if generation != self.itlb_generation {
            self.itlb_generation = generation;
            self.ctx.chain_epoch += 1;
        }
    }

    /// Statistics for `--stats`.
    pub fn report(&self) -> String {
        let s = &self.stats;
        let interpreted = s.cold + s.tail + s.unwatched + s.stops;
        let total = (s.jit_instructions + interpreted).max(1);
        let pct = |n: u64| n as f64 * 100.0 / total as f64;
        let mut out = format!(
            "JIT: {} regions translated ({} blocks, avg {:.1} instructions per region, {:.2} s, {} KB of code), {} flushes, {} regions invalidated\r\n",
            s.blocks_translated,
            s.entries_translated,
            s.translated_instructions as f64 / s.blocks_translated.max(1) as f64,
            s.translate_time.as_secs_f64(),
            self.compiler.code_bytes / 1024,
            s.flushes,
            s.invalidated_blocks,
        );
        out += &format!(
            "  instructions: {:.1}% translated code ({} region runs, {} of them chained, avg {:.1}), interpreted: {:.1}% cold, {:.1}% untranslatable, {:.1}% tails, {:.1}% unwatched memory\r\n",
            pct(s.jit_instructions),
            s.block_runs + s.chained_runs,
            s.chained_runs,
            s.jit_instructions as f64 / (s.block_runs + s.chained_runs).max(1) as f64,
            pct(s.cold),
            pct(s.stops),
            pct(s.tail),
            pct(s.unwatched),
        );
        out += &format!("  {} exceptions inside blocks, {} early exits, {} interrupt/ITLB entries\r\n", s.exceptions, s.early_exits, s.entries);
        if s.checked_runs > 0 {
            out += &format!("  lockstep check: {} region runs verified against the interpreter, all identical\r\n", s.checked_runs);
        }
        let mut by_op: HashMap<String, u64> = HashMap::new();
        for (&word, &n) in &s.stop_words {
            let name = Ins::new(word, Extensions::none()).simplified().to_string();
            let name = name.split_whitespace().next().unwrap_or("?").to_string();
            *by_op.entry(name).or_default() += n;
        }
        let mut by_op: Vec<_> = by_op.into_iter().collect();
        by_op.sort_by(|a, b| b.1.cmp(&a.1));
        if !by_op.is_empty() {
            out += "  untranslatable instructions run by the interpreter:";
            for (name, n) in by_op.iter().take(12) {
                let _ = write!(out, " {name} {n}");
            }
            out += "\r\n";
        }
        out
    }
}
