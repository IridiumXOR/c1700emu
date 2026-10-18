//! Lockstep checking (`--cpu jit-check`, docs ch. 17.12): every translated
//! region run is verified against the interpreter.
//!
//! 1. The region runs on the real platform through a [`Recorder`], with
//!    the fast memory tables off, so every memory access goes through the
//!    helpers and is logged with its value (plus the external interrupt line
//!    after it).
//! 2. The CPU is put back in its state before the region, and the
//!    interpreter executes the same number of steps against a [`Replayer`]:
//!    reads are answered from the log, writes are checked against it. The
//!    devices never see an access twice.
//! 3. Registers, clock and exception counts must be identical and the log
//!    fully consumed; otherwise the run stops with a report.

use crate::{ArchState, BusError, Cpu, Platform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    Read { size: u8, addr: u32, value: u32 },
    Write { size: u8, addr: u32, value: u32 },
}

/// One logged access, and the interrupt line right after it.
#[derive(Clone, Copy, Debug)]
pub(super) struct Event {
    access: Access,
    irq_after: bool,
}

/// Forwards everything to the real platform, logging what the region did.
pub(super) struct Recorder<'a, P: Platform> {
    pub inner: &'a mut P,
    pub log: Vec<Event>,
}

impl<P: Platform> Recorder<'_, P> {
    fn note(&mut self, access: Access) {
        let irq_after = self.inner.external_interrupt();
        self.log.push(Event { access, irq_after });
    }
}

macro_rules! recorded {
    ($read:ident, $write:ident, $ty:ty, $size:expr) => {
        fn $read(&mut self, addr: u32) -> Result<$ty, BusError> {
            let r = self.inner.$read(addr);
            // a bus error is logged with value u32::MAX and size | 0x80
            let (size, value) = match r {
                Ok(v) => ($size, v as u32),
                Err(_) => ($size | 0x80, u32::MAX),
            };
            self.note(Access::Read { size, addr, value });
            r
        }
        fn $write(&mut self, addr: u32, v: $ty) -> Result<(), BusError> {
            let r = self.inner.$write(addr, v);
            let size = if r.is_ok() { $size } else { $size | 0x80 };
            self.note(Access::Write { size, addr, value: v as u32 });
            r
        }
    };
}

impl<P: Platform> Platform for Recorder<'_, P> {
    recorded!(read8, write8, u8, 1);
    recorded!(read16, write16, u16, 2);
    recorded!(read32, write32, u32, 4);
    fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
        self.inner.fetch(addr)
    }
    // `read_spr` asks the platform whether it implements an SPR (only IMMR
    // on this board): a query without side effects, made by the
    // interpreter for every mfspr and by the JIT only for helper ones, so
    // it isn't logged; the replayer asks the real platform too.
    fn read_spr(&mut self, spr: u16) -> Option<u32> {
        self.inner.read_spr(spr)
    }
    fn write_spr(&mut self, spr: u16, value: u32) -> bool {
        self.inner.write_spr(spr, value)
    }
    fn external_interrupt(&self) -> bool {
        self.inner.external_interrupt()
    }
    // No direct memory access: every access must be seen.
    fn host_page(&mut self, _pa: u32, _write: bool) -> Option<*mut u8> {
        None
    }
    fn memory_map_epoch(&self) -> u64 {
        self.inner.memory_map_epoch()
    }
    fn watch_code(&mut self, pa: u32, len: u32) -> Option<u32> {
        self.inner.watch_code(pa, len)
    }
    fn take_code_write(&mut self) -> Option<u32> {
        self.inner.take_code_write()
    }
    fn unwatch_code_pages(&mut self) {
        self.inner.unwatch_code_pages()
    }
}

/// Answers the interpreter from the log; records the first difference.
pub(super) struct Replayer<'a, P: Platform> {
    /// For instruction fetches outside `code`.
    pub inner: &'a mut P,
    /// The region's page as it was before the region ran (a region can
    /// rewrite its own code, and the replay runs afterwards).
    pub code: &'a CodePage,
    pub log: &'a [Event],
    pub pos: usize,
    pub irq: bool,
    pub mismatch: Option<String>,
}

impl<P: Platform> Replayer<'_, P> {
    fn next(&mut self, want: Access) -> Option<Access> {
        let Some(ev) = self.log.get(self.pos).copied() else {
            self.mismatch.get_or_insert(format!("the interpreter made an extra access {want:x?}"));
            return None;
        };
        self.pos += 1;
        self.irq = ev.irq_after;
        let same = match (want, ev.access) {
            (Access::Read { size, addr, .. }, Access::Read { size: s2, addr: a2, .. }) => size == s2 & 0x7F && addr == a2,
            (Access::Write { size, addr, value }, Access::Write { size: s2, addr: a2, value: v2 }) => {
                size == s2 & 0x7F && addr == a2 && value == v2
            }
            _ => false,
        };
        if !same {
            self.mismatch.get_or_insert(format!("access #{}: interpreter {want:x?}, JIT {:x?}", self.pos - 1, ev.access));
        }
        Some(ev.access)
    }
}

macro_rules! replayed {
    ($read:ident, $write:ident, $ty:ty, $size:expr) => {
        fn $read(&mut self, addr: u32) -> Result<$ty, BusError> {
            match self.next(Access::Read { size: $size, addr, value: 0 }) {
                Some(Access::Read { size, value, .. }) if size & 0x80 == 0 => Ok(value as $ty),
                _ => Err(BusError),
            }
        }
        fn $write(&mut self, addr: u32, v: $ty) -> Result<(), BusError> {
            match self.next(Access::Write { size: $size, addr, value: v as u32 }) {
                Some(Access::Write { size, .. }) if size & 0x80 == 0 => Ok(()),
                _ => Err(BusError),
            }
        }
    };
}

impl<P: Platform> Platform for Replayer<'_, P> {
    replayed!(read8, write8, u8, 1);
    replayed!(read16, write16, u16, 2);
    replayed!(read32, write32, u32, 4);
    fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
        match self.code.word(addr) {
            Some(w) => Ok(w),
            None => self.inner.fetch(addr),
        }
    }
    fn read_spr(&mut self, spr: u16) -> Option<u32> {
        self.inner.read_spr(spr) // side-effect free, see the Recorder
    }
    fn external_interrupt(&self) -> bool {
        self.irq
    }
}

/// The 4 KB physical page holding a region, captured before it runs.
pub(super) struct CodePage {
    base: u32,
    words: Vec<u32>,
}

impl CodePage {
    /// Capture the page containing `pa` (plain RAM or ROM: fetching it has
    /// no side effects).
    pub fn capture<P: Platform>(sys: &mut P, pa: u32) -> Self {
        let base = pa & !0xFFF;
        let words = (0..1024).map(|i| sys.fetch(base + 4 * i).unwrap_or(0)).collect();
        CodePage { base, words }
    }

    fn word(&self, addr: u32) -> Option<u32> {
        (addr & !0xFFF == self.base).then(|| self.words[(addr & 0xFFF) as usize / 4])
    }
}

/// Describe how two states differ (empty if they don't).
pub(super) fn diff(interp: &ArchState, jit: &ArchState) -> String {
    let mut out = String::new();
    for r in 0..32 {
        if interp.gpr[r] != jit.gpr[r] {
            out += &format!(" r{r}: {:#010x} vs {:#010x};", interp.gpr[r], jit.gpr[r]);
        }
    }
    let pairs = [
        ("pc", interp.pc, jit.pc),
        ("cr", interp.cr, jit.cr),
        ("xer", interp.xer, jit.xer),
        ("lr", interp.lr, jit.lr),
        ("ctr", interp.ctr, jit.ctr),
        ("msr", interp.msr, jit.msr),
        ("srr0", interp.srr0, jit.srr0),
        ("srr1", interp.srr1, jit.srr1),
        ("dar", interp.dar, jit.dar),
        ("dsisr", interp.dsisr, jit.dsisr),
        ("dec", interp.dec, jit.dec),
    ];
    for (name, a, b) in pairs {
        if a != b {
            out += &format!(" {name}: {a:#010x} vs {b:#010x};");
        }
    }
    if interp.cycles != jit.cycles || interp.tb != jit.tb || interp.tb_prescaler != jit.tb_prescaler {
        out += &format!(" clock: cycles {} tb {} vs cycles {} tb {};", interp.cycles, interp.tb, jit.cycles, jit.tb);
    }
    if interp.sprg != jit.sprg || interp.reservation != jit.reservation || interp.dec_pending != jit.dec_pending {
        out += " sprg/reservation/dec_pending differ;";
    }
    if interp.exception_counts != jit.exception_counts {
        out += " exceptions taken differ;";
    }
    out
}

/// Replay `steps` interpreter steps from `before` against `log`; returns
/// the interpreter's final state, or a description of the first problem.
pub(super) fn replay<P: Platform>(
    cpu: &mut Cpu,
    sys: &mut P,
    code: &CodePage,
    before: &ArchState,
    log: &[Event],
    irq0: bool,
    steps: u64,
) -> Result<ArchState, String> {
    cpu.set_arch_state(before);
    let mut rep = Replayer { inner: sys, code, log, pos: 0, irq: irq0, mismatch: None };
    for _ in 0..steps {
        cpu.step(&mut rep);
    }
    if rep.pos != log.len() {
        rep.mismatch.get_or_insert(format!("the interpreter made {} accesses, the JIT {}", rep.pos, log.len()));
    }
    match rep.mismatch {
        Some(m) => Err(m),
        None => Ok(cpu.arch_state()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A platform that should never be used for data (fetches only).
    struct Code(Vec<u32>);

    impl Platform for Code {
        fn read8(&mut self, _: u32) -> Result<u8, BusError> {
            Err(BusError)
        }
        fn read16(&mut self, _: u32) -> Result<u16, BusError> {
            Err(BusError)
        }
        fn read32(&mut self, _: u32) -> Result<u32, BusError> {
            Err(BusError)
        }
        fn write8(&mut self, _: u32, _: u8) -> Result<(), BusError> {
            Err(BusError)
        }
        fn write16(&mut self, _: u32, _: u16) -> Result<(), BusError> {
            Err(BusError)
        }
        fn write32(&mut self, _: u32, _: u32) -> Result<(), BusError> {
            Err(BusError)
        }
        fn fetch(&mut self, addr: u32) -> Result<u32, BusError> {
            self.0.get(addr as usize / 4).copied().ok_or(BusError)
        }
    }

    fn cpu_at_zero() -> Cpu {
        let mut cpu = Cpu::new();
        cpu.hard_reset(false);
        cpu.msr = crate::msr::ME;
        cpu.pc = 0;
        cpu.gpr[4] = 0x100;
        cpu
    }

    /// `lwz r3, 0(r4)`: the replay serves the logged value; a log made at
    /// another address, or a missing access, is reported.
    #[test]
    fn replay_follows_the_log_and_reports_differences() {
        let mut code = Code(vec![0x8064_0000]); // lwz r3, 0(r4)
        let read = |addr| Event { access: Access::Read { size: 4, addr, value: 0x1234 }, irq_after: false };
        let mut cpu = cpu_at_zero();
        let before = cpu.arch_state();
        let page = CodePage::capture(&mut code, 0);
        let state = replay(&mut cpu, &mut code, &page, &before, &[read(0x100)], false, 1).unwrap();
        assert_eq!(state.gpr[3], 0x1234);
        assert_eq!(state.pc, 4);

        let err = replay(&mut cpu, &mut code, &page, &before, &[read(0x104)], false, 1).unwrap_err();
        assert!(err.contains("access #0"), "{err}");
        let err = replay(&mut cpu, &mut code, &page, &before, &[], false, 1).unwrap_err();
        assert!(err.contains("extra access"), "{err}");
        let err = replay(&mut cpu, &mut code, &page, &before, &[read(0x100), read(0x100)], false, 1).unwrap_err();
        assert!(err.contains("accesses"), "{err}");
    }
}
