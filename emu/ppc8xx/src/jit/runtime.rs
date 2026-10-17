//! What translated code runs against: the context block it receives, the
//! fast memory tables it looks addresses up in, and the helpers (plain Rust)
//! it calls for everything else.
//!
//! The generated code only knows *offsets*: of the guest registers inside
//! [`Cpu`], and of the fields below inside [`JitCtx`]. Both structs are
//! ordinary Rust structs; `offset_of!` tells the code generator where things
//! are, so nothing has to be `repr(C)` except what the code reads directly.

use crate::mmu::Access;
use crate::{BusError, Cpu, Exception, Platform, msr};

/// Entries in each fast memory table (direct-mapped by guest page number).
pub(super) const TABLE_ENTRIES: usize = 1024;

/// A tag no lookup key can match. Keys are `ea & (0xFFFF_F000 | (size - 1))`,
/// so bits 2-11 of a key are always zero; this tag has them set.
pub(super) const INVALID_TAG: u32 = 0xFFF;

/// One fast memory table entry: the guest effective page `tag` (low 12
/// bits zero) is plain host memory at `ea + addend`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct FastEntry {
    pub tag: u32,
    _pad: u32,
    pub addend: u64,
}

const EMPTY: FastEntry = FastEntry { tag: INVALID_TAG, _pad: 0, addend: 0 };

/// The translation regimes a block can run in. Data accesses translate
/// differently in each, so each has its own tables.
pub(super) fn mode_index(msr: u32) -> usize {
    if msr & msr::DR == 0 {
        0 // real mode: effective = physical
    } else if msr & msr::PR == 0 {
        1 // translated, supervisor
    } else {
        2 // translated, user
    }
}

/// Fast tables for loads and for stores, one pair per mode. A store entry
/// is only ever made for a page with no translated code, so stores to code
/// always take the helper path, where the platform notices them.
pub(super) struct FastTables {
    pub read: [Box<[FastEntry]>; 3],
    pub write: [Box<[FastEntry]>; 3],
}

impl FastTables {
    fn new() -> Self {
        let table = || vec![EMPTY; TABLE_ENTRIES].into_boxed_slice();
        FastTables { read: [table(), table(), table()], write: [table(), table(), table()] }
    }

    pub fn flush_all(&mut self) {
        for t in self.read.iter_mut().chain(self.write.iter_mut()) {
            t.fill(EMPTY);
        }
    }

    /// The MMU changed: forget the translated modes (real mode stays valid).
    pub fn flush_translated(&mut self) {
        for mode in 1..3 {
            self.read[mode].fill(EMPTY);
            self.write[mode].fill(EMPTY);
        }
    }

    /// A page started holding code: no store entry may point at it.
    pub fn flush_writes(&mut self) {
        for t in self.write.iter_mut() {
            t.fill(EMPTY);
        }
    }
}

pub(super) type LoadFn = unsafe extern "C" fn(*mut JitCtx, u32) -> u64;
pub(super) type StoreFn = unsafe extern "C" fn(*mut JitCtx, u32, u32) -> u32;
pub(super) type ExecFn = unsafe extern "C" fn(*mut JitCtx, u32, u32) -> u32;

/// Helper status codes (for loads: in bits 32-63 of the result).
pub(super) const OK: u32 = 0;
/// The access raised an exception; it is in [`JitCtx::exception`].
pub(super) const EXCEPTION: u32 = 1;
/// The access succeeded, but the block must stop after this instruction
/// (an interrupt became pending, code was overwritten, the memory map or the
/// MMU changed).
pub(super) const EXIT: u32 = 2;

/// The helpers, instantiated for one platform type. The generated code calls
/// them through this table, so the same code works with any platform.
#[repr(C)]
pub(super) struct Helpers {
    /// 1, 2 and 4-byte loads (big-endian value, zero-extended).
    pub load: [LoadFn; 3],
    pub store: [StoreFn; 3],
    /// Run one instruction (`word` at `pc`) with the interpreter.
    pub exec: ExecFn,
}

impl Helpers {
    pub fn new<P: Platform>() -> Self {
        Helpers { load: [load8::<P>, load16::<P>, load32::<P>], store: [store8::<P>, store16::<P>, store32::<P>], exec: exec::<P> }
    }
}

/// The context block passed to every translated block.
#[repr(C)]
pub(super) struct JitCtx {
    /// The current mode's fast tables (set before each block runs).
    pub read_table: *mut FastEntry,
    pub write_table: *mut FastEntry,
    pub helpers: Helpers,
    pub cpu: *mut Cpu,
    /// The platform, as a `*mut P` of the type the helpers were made for.
    pub sys: *mut (),
    /// The exception a helper raised.
    pub exception: Option<Exception>,
    /// Fill the fast tables from the helpers (off in the lockstep checker,
    /// which must see every access).
    pub fill: bool,
    pub tables: FastTables,
    /// Platform memory map and MMU state the tables were filled under.
    pub map_epoch: u64,
    pub mmu_generation: u64,
    /// Watched pages reported written, not yet invalidated.
    pub code_writes: Vec<u32>,
    /// A helper ran since the dispatcher last looked: platform, MMU or
    /// memory map may have changed. (Translated code alone can't change
    /// them, so after most blocks there is nothing to check.)
    pub touched: bool,
}

impl JitCtx {
    pub fn new() -> Box<Self> {
        Box::new(JitCtx {
            read_table: std::ptr::null_mut(),
            write_table: std::ptr::null_mut(),
            helpers: Helpers::new::<NoPlatform>(), // replaced before each run
            cpu: std::ptr::null_mut(),
            sys: std::ptr::null_mut(),
            exception: None,
            fill: true,
            tables: FastTables::new(),
            map_epoch: 0,
            mmu_generation: 0,
            code_writes: Vec::new(),
            touched: false,
        })
    }
}

/// Placeholder platform for the helper table of a `JitCtx` that hasn't run yet.
struct NoPlatform;

impl Platform for NoPlatform {
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
}

/// After a successful access through a helper: must the block stop?
fn after_access<P: Platform>(ctx: &mut JitCtx, cpu: &Cpu, sys: &mut P) -> u32 {
    while let Some(page) = sys.take_code_write() {
        ctx.code_writes.push(page);
    }
    let stop = !ctx.code_writes.is_empty()
        || sys.memory_map_epoch() != ctx.map_epoch
        || cpu.mmu.data_generation() != ctx.mmu_generation
        || (cpu.msr & msr::EE != 0 && sys.external_interrupt());
    if stop { EXIT } else { OK }
}

/// After a successful access at `ea`: remember where its page is in host
/// memory, if it is plain memory and translates uniformly.
fn fill<P: Platform>(ctx: &mut JitCtx, cpu: &Cpu, sys: &mut P, ea: u32, write: bool) {
    if !ctx.fill {
        return;
    }
    let pa = if cpu.msr & msr::DR == 0 {
        ea
    } else {
        let access = if write { Access::Store } else { Access::Load };
        match cpu.mmu.cached(ea, access, cpu.msr & msr::PR != 0) {
            Some(pa) => pa,
            None => return,
        }
    };
    let Some(host) = sys.host_page(pa & !0xFFF, write) else { return };
    let page = ea & !0xFFF;
    let mode = mode_index(cpu.msr);
    let table = if write { &mut ctx.tables.write[mode] } else { &mut ctx.tables.read[mode] };
    table[(ea >> 12) as usize % TABLE_ENTRIES] =
        FastEntry { tag: page, _pad: 0, addend: (host as u64).wrapping_sub(page as u64) };
}

// The helpers. They run the interpreter's own access functions, so every
// slow case (devices, misaligned accesses, MMU faults, bus errors) behaves
// exactly as in the interpreter.
//
// Safety: called only by translated code, with the `JitCtx` set up by the
// dispatcher for the platform type `P`.

/// The context, CPU and platform behind `ctx` (three distinct objects).
///
/// # Safety
/// `ctx` points to a live `JitCtx` whose `cpu` and `sys` point to a live
/// `Cpu` and `P`, none of them otherwise borrowed during the call.
unsafe fn parts<'a, P>(ctx: *mut JitCtx) -> (&'a mut JitCtx, &'a mut Cpu, &'a mut P) {
    // SAFETY: as documented; read the pointers before borrowing the context.
    unsafe {
        let (cpu, sys) = ((*ctx).cpu, (*ctx).sys as *mut P);
        (*ctx).touched = true;
        (&mut *ctx, &mut *cpu, &mut *sys)
    }
}

macro_rules! load_helper {
    ($name:ident, $method:ident) => {
        unsafe extern "C" fn $name<P: Platform>(ctx: *mut JitCtx, ea: u32) -> u64 {
            // SAFETY: see `parts`.
            let (ctx, cpu, sys) = unsafe { parts::<P>(ctx) };
            match cpu.$method(sys, ea) {
                Ok(v) => {
                    fill(ctx, cpu, sys, ea, false);
                    v as u64 | (after_access(ctx, cpu, sys) as u64) << 32
                }
                Err(e) => {
                    ctx.exception = Some(e);
                    (EXCEPTION as u64) << 32
                }
            }
        }
    };
}

macro_rules! store_helper {
    ($name:ident, $method:ident, $ty:ty) => {
        unsafe extern "C" fn $name<P: Platform>(ctx: *mut JitCtx, ea: u32, value: u32) -> u32 {
            // SAFETY: see `parts`.
            let (ctx, cpu, sys) = unsafe { parts::<P>(ctx) };
            match cpu.$method(sys, ea, value as $ty) {
                Ok(()) => {
                    fill(ctx, cpu, sys, ea, true);
                    after_access(ctx, cpu, sys)
                }
                Err(e) => {
                    ctx.exception = Some(e);
                    EXCEPTION
                }
            }
        }
    };
}

load_helper!(load8, load8);
load_helper!(load16, load16);
load_helper!(load32, load32);
store_helper!(store8, store8, u8);
store_helper!(store16, store16, u16);
store_helper!(store32, store32, u32);

unsafe extern "C" fn exec<P: Platform>(ctx: *mut JitCtx, word: u32, pc: u32) -> u32 {
    // SAFETY: see `parts`.
    let (ctx, cpu, sys) = unsafe { parts::<P>(ctx) };
    cpu.pc = pc;
    let ins = cpu.decoder.decode(word, pc);
    match cpu.execute(ins, sys) {
        Ok(()) => after_access(ctx, cpu, sys),
        Err(e) => {
            ctx.exception = Some(e);
            EXCEPTION
        }
    }
}
