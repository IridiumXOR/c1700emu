//! Translating a region: guest instructions → Cranelift IR → host code.
//!
//! A region (see `scan::scan_region`) is translated to one function
//! `fn(ctx, cpu, budget, entry) -> u32`. It starts at guest block number
//! `entry` (every block of the region is an entry point), runs guest blocks
//! on the state in the [`Cpu`] struct, jumping from block to block inside
//! the host code while the next block fits in `budget` instructions, and
//! returns how many
//! instructions it executed: with [`RAISED`] set if the last one raised an
//! exception (its address is left in `JitCtx::fault_pc`), [`EARLY`] if a
//! helper asked to stop. The count so far lives in a variable, so every
//! exit reports it exactly.
//!
//! **Guest registers** are Cranelift variables, as in QEMU's TCG: within a
//! guest block a register is loaded from the `Cpu` struct the first time it
//! is read and then lives in a host register; a write only changes the
//! variable and marks the register *dirty*. Dirty registers are stored back
//! where the struct must be current: at the end of the guest block, before
//! an interpreter helper call, and in the exit stubs (each stub stores the
//! registers dirty at its own exit point, so exceptions stay precise).
//! The struct is therefore current at every block edge. The registers a
//! region uses are loaded once at its entry and their values travel from
//! block to block in the variables (docs ch. 17.18), so blocks don't reload
//! what the previous one just stored. (Also deferring the stores to the
//! region's exits was tried and is slower on x86-64: too few host
//! registers; docs ch. 17.13.)
//!
//! **Memory accesses** look the page up in a fast table (one load and one
//! compare). On a hit the access is one host load or store; on a miss, a
//! helper runs the interpreter's access function, which handles devices,
//! misaligned accesses and faults exactly like the interpreter.
//!
//! **Chaining.** Every exit to a known address outside the region has a
//! [`Link`]: once the dispatcher has filled it in, the exit jumps straight
//! into the region there (a tail call) while the link is current and the
//! next block fits in the budget; otherwise it returns to the dispatcher.
//! A computed branch (`blr`, `bctr`) has a link too, remembering the last
//! target it was filled in for: it is used when the branch goes there
//! again (a one-entry inline cache: a loop calling a function returns to
//! the same place every time).
//! Regions use Cranelift's `tail` calling convention for that; the
//! dispatcher calls them through a small C-convention [`Trampoline`]
//! (docs ch. 17.14).

use super::runtime::{EXCEPTION, EXIT, FastEntry, Helpers, JitCtx, TABLE_ENTRIES};
use super::scan::{Block as GuestBlock, Kind};
use crate::Cpu;
use crate::execute::mask;
use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::ir::types::{I8, I16, I32, I64};
use cranelift_codegen::ir::{AbiParam, Block, Function, InstBuilder, JumpTableData, MemFlagsData, SigRef, Signature, Type, UserFuncName, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Module, default_libcall_names};
use powerpc::{Ins, Opcode};
use std::mem::offset_of;

/// A translated region's code: `(ctx, cpu, budget, entry block, steps done
/// before) -> steps | flags`, in Cranelift's `tail` calling convention, so
/// only callable from generated code.
pub(super) type RegionCode = *const u8;

/// Calls a region from Rust: `(ctx, cpu, budget, entry block, region) ->
/// steps | flags`, starting from 0 steps.
pub(super) type Trampoline = unsafe extern "C" fn(*mut JitCtx, *mut Cpu, u32, u32, RegionCode) -> u32;

/// The link of one region exit (see the module docs). The generated code
/// reads it; the dispatcher fills it in.
#[repr(C)]
pub(super) struct Link {
    /// The region to jump to, valid while `epoch == JitCtx::chain_epoch`.
    pub func: RegionCode,
    pub epoch: u64,
    /// Its entry block for `target`, and that block's length (it must fit
    /// in the budget).
    pub entry: u32,
    pub len: u32,
    /// The guest address this exit goes to (for a computed branch: went to
    /// when the link was filled in).
    pub target: u32,
    /// A computed branch: the dispatcher may fill the link in for any target.
    pub computed: bool,
}

impl Link {
    /// Never current: `chain_epoch` only counts up from 0.
    pub const UNUSED: Link = Link { func: std::ptr::null(), epoch: u64::MAX, entry: 0, len: 0, target: 0, computed: false };
}

/// Most links one guest block needs: a conditional branch has two exits.
pub(super) const LINKS_PER_BLOCK: usize = 2;

/// Set in a region's result when its last executed instruction raised an
/// exception (stored in `JitCtx::exception`, its address in `fault_pc`).
pub(super) const RAISED: u32 = 1 << 16;
/// Set when a helper made the region stop early (interrupt raised, code
/// written...). Statistics only.
pub(super) const EARLY: u32 = 1 << 17;
/// Mask of the instruction count in a region's result.
pub(super) const STEPS: u32 = 0xFFFF;

/// Guest state slots the translator keeps in variables: r0-r31, then:
const CR: usize = 32;
const XER: usize = 33;
const LR: usize = 34;
const CTR: usize = 35;
const SLOTS: usize = 36;

const XER_CA: u32 = 0x2000_0000;

/// Where a slot lives inside [`Cpu`].
fn slot_offset(slot: usize) -> i32 {
    let offset = match slot {
        0..32 => offset_of!(Cpu, gpr) + 4 * slot,
        CR => offset_of!(Cpu, cr),
        XER => offset_of!(Cpu, xer),
        LR => offset_of!(Cpu, lr),
        CTR => offset_of!(Cpu, ctr),
        _ => unreachable!(),
    };
    offset as i32
}

/// Owns the Cranelift JIT module (the executable memory) and the reusable
/// compilation contexts. Lives on the thread that compiles: the dispatcher's
/// (inline compiling) or the background worker's (`worker.rs`).
pub(super) struct Compiler {
    module: JITModule,
    ctx: Context,
    fctx: FunctionBuilderContext,
    block_sig: Signature,
    load_sig: Signature,
    store_sig: Signature,
    exec_sig: Signature,
    /// Bytes of host code generated since the last [`Compiler::reset`].
    pub code_bytes: usize,
}

fn new_module() -> JITModule {
    // Frame pointers: Cranelift's x86-64 tail calls (chaining) need them.
    let flags = [("opt_level", "speed"), ("enable_verifier", "false"), ("preserve_frame_pointers", "true")];
    let builder = JITBuilder::with_flags(&flags, default_libcall_names())
        .expect("Cranelift cannot generate code for this host");
    let module = JITModule::new(builder);
    assert_eq!(module.target_config().pointer_type(), I64, "the JIT needs a 64-bit host");
    module
}

/// A signature in `module`'s default (C) calling convention.
fn signature(module: &JITModule, params: &[Type], ret: Type) -> Signature {
    let mut s = module.make_signature();
    s.params.extend(params.iter().map(|&t| AbiParam::new(t)));
    s.returns.push(AbiParam::new(ret));
    s
}

/// A region's signature: (ctx, cpu, budget, entry block, steps done) ->
/// steps | flags, in the `tail` convention (chained jumps are tail calls).
fn region_signature(module: &JITModule) -> Signature {
    let mut s = signature(module, &[I64, I64, I32, I32, I32], I32);
    s.call_conv = CallConv::Tail;
    s
}

impl Compiler {
    pub fn new() -> Self {
        let module = new_module();
        Compiler {
            block_sig: region_signature(&module),
            load_sig: signature(&module, &[I64, I32], I64),
            store_sig: signature(&module, &[I64, I32, I32], I32),
            exec_sig: signature(&module, &[I64, I32, I32], I32),
            ctx: module.make_context(),
            fctx: FunctionBuilderContext::new(),
            module,
            code_bytes: 0,
        }
    }

    /// Free all generated code. Every region handed out becomes invalid.
    pub fn reset(&mut self) {
        let old = std::mem::replace(&mut self.module, new_module());
        // SAFETY: the caller has dropped every pointer into the old code.
        unsafe { old.free_memory() };
        self.code_bytes = 0;
    }
    /// Translate a region (`region[0]` is where it is entered) to host code.
    ///
    /// With the log target `jit_ir` at trace level (`--log-level
    /// warn,jit_ir=trace`) every region is logged three times: guest code,
    /// Cranelift IR, and the host code Cranelift generated.
    ///
    /// `links` has room for [`LINKS_PER_BLOCK`] links per block, at a
    /// stable address; the generated code reads them from there. Returns
    /// the code and the target address of each link used, in order (`None`
    /// for computed branches).
    pub fn compile(&mut self, region: &[GuestBlock], links: *mut Link) -> Result<(RegionCode, Vec<Option<u32>>), String> {
        let dump = log::log_enabled!(target: "jit_ir", log::Level::Trace);
        self.ctx.set_disasm(dump);
        // Pass 1 finds the registers the region uses; pass 2 loads them once
        // at entry and carries them from block to block (docs ch. 17.18).
        let (used, _) = self.build(region, links, 0);
        let (_, link_targets) = self.build(region, links, used);
        let ir = if dump { self.ctx.func.display().to_string() } else { String::new() };
        self.finish_compile(region, dump, ir, link_targets)
    }

    /// Build the region's IR into `self.ctx.func`, carrying the registers in
    /// `carry` across its blocks; returns the registers it used and the
    /// link targets.
    fn build(&mut self, region: &[GuestBlock], links: *mut Link, carry: u64) -> (u64, Vec<Option<u32>>) {
        self.ctx.func = Function::with_name_signature(UserFuncName::default(), self.block_sig.clone());
        {
            let mut b = FunctionBuilder::new(&mut self.ctx.func, &mut self.fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let (ctx, cpu, budget, entry_index, done) = (params[0], params[1], params[2], params[3], params[4]);
            let sig_region = b.import_signature(self.block_sig.clone());
            let sig_load = b.import_signature(self.load_sig.clone());
            let sig_store = b.import_signature(self.store_sig.clone());
            let sig_exec = b.import_signature(self.exec_sig.clone());
            let read_tab = b.ins().load(I64, MemFlagsData::trusted(), ctx, offset_of!(JitCtx, read_table) as i32);
            let write_tab = b.ins().load(I64, MemFlagsData::trusted(), ctx, offset_of!(JitCtx, write_table) as i32);
            let steps = b.declare_var(I32);
            let mut t = Tx {
                b,
                ctx,
                cpu,
                read_tab,
                write_tab,
                budget,
                steps,
                sig_load,
                sig_store,
                sig_exec,
                vars: [None; SLOTS],
                loaded: 0,
                dirty: 0,
                carry,
                used: 0,
                cr_cache: [None; 8],
                labels: Vec::new(),
                stubs: Vec::new(),
                sig_region,
                links,
                link_capacity: LINKS_PER_BLOCK * region.len(),
                link_targets: Vec::new(),
            };
            t.region(region, entry_index, done);
            t.emit_stubs();
            t.b.seal_all_blocks();
            t.b.finalize(self.module.target_config());
            (t.used, t.link_targets)
        }
    }

    fn finish_compile(&mut self, region: &[GuestBlock], dump: bool, ir: String, link_targets: Vec<Option<u32>>) -> Result<(RegionCode, Vec<Option<u32>>), String> {
        let id = self.module.declare_anonymous_function(&self.block_sig).map_err(|e| e.to_string())?;
        self.module.define_function(id, &mut self.ctx).map_err(|e| format!("{e:?}"))?;
        if dump {
            let mut guest = String::new();
            for block in region {
                for (i, insn) in block.insns.iter().enumerate() {
                    let pc = block.start.wrapping_add(4 * i as u32);
                    guest += &format!("  {pc:08x}: {:08x}  {}\n", insn.word, insn.ins.simplified());
                }
                guest += "  --\n";
            }
            let host = self.ctx.compiled_code().and_then(|c| c.vcode.clone()).unwrap_or_default();
            log::trace!(target: "jit_ir", "region at {:#010x}, {} blocks\n{guest}--- Cranelift IR ---\n{ir}--- host code ---\n{host}", region[0].start, region.len());
        }
        let size = self.ctx.compiled_code().map_or(0, |c| c.code_info().total_size as usize);
        self.code_bytes += size;
        self.module.clear_context(&mut self.ctx);
        self.module.finalize_definitions().map_err(|e| e.to_string())?;
        let code = self.module.get_finalized_function(id);
        perf_map(code, size, region[0].start);
        Ok((code, link_targets))
    }
}

/// With `C1700_PERF_MAP` set, name each region for Linux `perf` in
/// `/tmp/perf-PID.map` (the file `perf report` looks for), as
/// `guest_XXXXXXXX` after its guest address (docs ch. 17.18). Cranelift
/// itself appends anonymous names there when `PERF_BUILDID_DIR` is set (as
/// `perf record` does), so the file is appended to, not replaced.
fn perf_map(code: *const u8, size: usize, guest_pc: u32) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static MAP: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        std::env::var_os("C1700_PERF_MAP")?;
        let path = format!("/tmp/perf-{}.map", std::process::id());
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(file) = map {
        let _ = writeln!(file.lock().unwrap(), "{:x} {size:x} guest_{guest_pc:08x}", code as usize);
    }
}

/// The [`Trampoline`], in a small code module of its own that is never
/// freed: it stays valid across [`Compiler::reset`]s and whichever thread
/// compiles the regions.
pub(super) struct Trampolines {
    _module: JITModule,
    pub call: Trampoline,
}

impl Trampolines {
    pub fn new() -> Self {
        let mut module = new_module();
        let sig = signature(&module, &[I64, I64, I32, I32, I64], I32);
        let mut ctx = module.make_context();
        let mut fctx = FunctionBuilderContext::new();
        ctx.func = Function::with_name_signature(UserFuncName::default(), sig.clone());
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let p = b.block_params(entry).to_vec();
            let region_sig = b.import_signature(region_signature(&module));
            let zero = b.ins().iconst(I32, 0);
            let call = b.ins().call_indirect(region_sig, p[4], &[p[0], p[1], p[2], p[3], zero]);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize(module.target_config());
        }
        let id = module.declare_anonymous_function(&sig).expect("declaring the trampoline");
        module.define_function(id, &mut ctx).expect("compiling the trampoline");
        module.finalize_definitions().expect("finalizing the trampoline");
        let code = module.get_finalized_function(id);
        // SAFETY: built with `sig`, the host's C calling convention for
        // (pointer, pointer, u32, u32, pointer) -> u32.
        let call = unsafe { std::mem::transmute::<*const u8, Trampoline>(code) };
        Trampolines { _module: module, call }
    }
}

/// Which clock register `Tx::time_now` reads.
#[derive(Clone, Copy, PartialEq)]
enum Clock {
    Timebase,
    Decrementer,
}

/// Exit paths, emitted after the region body. `executed` counts the
/// instructions of the current guest block up to and including the one
/// that exits; the instructions of earlier blocks are in the `steps`
/// variable.
enum Stub {
    /// The instruction at `pc` raised an exception.
    Raised { executed: u32, pc: u32, dirty: u64 },
    /// A helper asked to stop: continue at `next_pc`.
    Exit { executed: u32, next_pc: u32, dirty: u64 },
}

/// Register tracking along one path (see `Tx::reg_state`).
#[derive(Clone, Copy)]
struct RegState {
    loaded: u64,
    dirty: u64,
    cr_cache: [Option<[Value; 3]>; 8],
}

/// Where a branch goes.
#[derive(Clone, Copy)]
enum Target {
    Known(u32),
    Computed(Value),
}

/// How a loaded value is converted before it reaches the register.
#[derive(Clone, Copy, PartialEq)]
enum Conv {
    None,
    SignHalf,
    SwapHalf,
    SwapWord,
}

/// Addressing modes (see `execute.rs`).
#[derive(Clone, Copy, PartialEq)]
enum Addr {
    D,
    DUpdate,
    X,
    XUpdate,
}

/// The translator for one region.
struct Tx<'a> {
    b: FunctionBuilder<'a>,
    ctx: Value,
    cpu: Value,
    read_tab: Value,
    write_tab: Value,
    /// Most instructions this call may execute (batch end, decrementer).
    budget: Value,
    /// Instructions executed in the guest blocks before the current one.
    steps: Variable,
    sig_load: SigRef,
    sig_store: SigRef,
    sig_exec: SigRef,
    vars: [Option<Variable>; SLOTS],
    /// Slots whose variable holds the current value (else read the struct).
    loaded: u64,
    /// Slots written since they were last stored to the struct.
    dirty: u64,
    /// Slots loaded once at the region's entry and carried from block to
    /// block (their values stay in variables across internal jumps; the
    /// struct is still updated at block ends).
    carry: u64,
    /// Slots read or written (pass 1 of `Compiler::compile`).
    used: u64,
    /// Per CR field: the (LT, GT, EQ) comparison results that produced it,
    /// while still valid, so a following branch tests them directly.
    cr_cache: [Option<[Value; 3]>; 8],
    /// Guest block start → (its Cranelift block, its length).
    labels: Vec<(u32, Block, u32)>,
    stubs: Vec<(Block, Stub)>,
    /// Regions' own signature, for chained jumps.
    sig_region: SigRef,
    /// This region's links (`link_capacity` of them), and the target of
    /// each one used so far.
    links: *mut Link,
    link_capacity: usize,
    link_targets: Vec<Option<u32>>,
}

impl Tx<'_> {
    // ------------------------------------------------------------------
    // Guest state
    // ------------------------------------------------------------------

    fn var(&mut self, slot: usize) -> Variable {
        match self.vars[slot] {
            Some(v) => v,
            None => {
                let v = self.b.declare_var(I32);
                self.vars[slot] = Some(v);
                v
            }
        }
    }

    fn read(&mut self, slot: usize) -> Value {
        self.used |= 1 << slot;
        let var = self.var(slot);
        if self.loaded & (1 << slot) == 0 {
            let v = self.b.ins().load(I32, MemFlagsData::trusted(), self.cpu, slot_offset(slot));
            self.b.def_var(var, v);
            self.loaded |= 1 << slot;
        }
        self.b.use_var(var)
    }

    fn write(&mut self, slot: usize, v: Value) {
        self.used |= 1 << slot;
        let var = self.var(slot);
        self.b.def_var(var, v);
        self.loaded |= 1 << slot;
        self.dirty |= 1 << slot;
        if slot == CR {
            self.cr_cache = [None; 8];
        }
    }

    /// Load the registers in `slots` from the struct into their variables.
    fn load_slots(&mut self, slots: u64) {
        for slot in (0..SLOTS).filter(|s| slots & (1 << s) != 0) {
            let var = self.var(slot);
            let v = self.b.ins().load(I32, MemFlagsData::trusted(), self.cpu, slot_offset(slot));
            self.b.def_var(var, v);
        }
    }

    /// Store the registers in `slots` from their variables to the struct.
    fn store_slots(&mut self, slots: u64) {
        for slot in (0..SLOTS).filter(|s| slots & (1 << s) != 0) {
            let var = self.var(slot);
            let v = self.b.use_var(var);
            self.b.ins().store(MemFlagsData::trusted(), v, self.cpu, slot_offset(slot));
        }
    }

    /// Bring the struct up to date with every dirty register.
    fn flush(&mut self) {
        self.store_slots(self.dirty);
        self.dirty = 0;
    }

    // `loaded`, `dirty` and `cr_cache` describe one control-flow path. Where
    // code splits in two (the fast and slow paths of a memory access), each
    // side starts from the state at the split, and at the join a register
    // counts as loaded only if both sides loaded it, as dirty if either
    // side dirtied it, and a comparison result only if both sides agree.

    fn reg_state(&self) -> RegState {
        RegState { loaded: self.loaded, dirty: self.dirty, cr_cache: self.cr_cache }
    }

    fn set_reg_state(&mut self, state: RegState) {
        (self.loaded, self.dirty, self.cr_cache) = (state.loaded, state.dirty, state.cr_cache);
    }

    fn merge_reg_state(&mut self, other: RegState) {
        self.loaded &= other.loaded;
        self.dirty |= other.dirty;
        for (mine, theirs) in self.cr_cache.iter_mut().zip(other.cr_cache) {
            if *mine != theirs {
                *mine = None;
            }
        }
    }

    /// `(rA|0)`.
    fn ra0(&mut self, ra: usize) -> Value {
        if ra == 0 { self.c(0) } else { self.read(ra) }
    }

    fn c(&mut self, v: u32) -> Value {
        self.b.ins().iconst(I32, v as i64)
    }

    /// An `icmp` result (I8, 0/1) as an I32.
    fn bit(&mut self, b: Value) -> Value {
        self.b.ins().uextend(I32, b)
    }

    // ------------------------------------------------------------------
    // CR and XER
    // ------------------------------------------------------------------

    /// CR field `field` := `bits` (4-bit value).
    fn set_cr_field(&mut self, field: u8, bits: Value) {
        let shift = (7 - field as u32) * 4;
        let cr = self.read(CR);
        let keep = self.c(!(0xF << shift));
        let kept = self.b.ins().band(cr, keep);
        let sh = self.c(shift);
        let moved = self.b.ins().ishl(bits, sh);
        let v = self.b.ins().bor(kept, moved);
        self.write(CR, v);
    }

    /// LT/GT/EQ of `a` vs `b`, plus a copy of XER[SO], into CR field `crf`.
    fn compare(&mut self, crf: u8, a: Value, b: Value, signed: bool) {
        let (lt_cc, gt_cc) = if signed {
            (IntCC::SignedLessThan, IntCC::SignedGreaterThan)
        } else {
            (IntCC::UnsignedLessThan, IntCC::UnsignedGreaterThan)
        };
        let lt = self.b.ins().icmp(lt_cc, a, b);
        let gt = self.b.ins().icmp(gt_cc, a, b);
        let eq = self.b.ins().icmp(IntCC::Equal, a, b);
        let flags = [lt, gt, eq];
        let (lt, gt, eq) = (self.bit(lt), self.bit(gt), self.bit(eq));
        let lt = self.b.ins().ishl_imm_u(lt, 3);
        let gt = self.b.ins().ishl_imm_u(gt, 2);
        let eq = self.b.ins().ishl_imm_u(eq, 1);
        let xer = self.read(XER);
        let so = self.b.ins().ushr_imm_u(xer, 31);
        let bits = self.b.ins().bor(lt, gt);
        let bits = self.b.ins().bor(bits, eq);
        let bits = self.b.ins().bor(bits, so);
        self.set_cr_field(crf, bits);
        // (after the CR write, which forgets every cached field)
        self.cr_cache[crf as usize] = Some(flags);
    }

    /// Record form: CR0 = signed comparison of `v` with 0, plus SO.
    fn record_cr0(&mut self, v: Value) {
        let zero = self.c(0);
        self.compare(0, v, zero, true);
    }

    /// XER[CA] as 0 or 1.
    fn ca(&mut self) -> Value {
        let xer = self.read(XER);
        let v = self.b.ins().ushr_imm_u(xer, 29);
        self.b.ins().band_imm_u(v, 1)
    }

    /// XER[CA] := `ca` (an I32 0 or 1).
    fn set_ca(&mut self, ca: Value) {
        let xer = self.read(XER);
        let keep = self.c(!XER_CA);
        let kept = self.b.ins().band(xer, keep);
        let moved = self.b.ins().ishl_imm_u(ca, 29);
        let v = self.b.ins().bor(kept, moved);
        self.write(XER, v);
    }

    /// `a + b + cin` and its carry out (I32 0 or 1).
    fn add3(&mut self, a: Value, b: Value, cin: Value) -> (Value, Value) {
        let a = self.b.ins().uextend(I64, a);
        let b = self.b.ins().uextend(I64, b);
        let cin = self.b.ins().uextend(I64, cin);
        let sum = self.b.ins().iadd(a, b);
        let wide = self.b.ins().iadd(sum, cin);
        let result = self.b.ins().ireduce(I32, wide);
        let carry = self.b.ins().ushr_imm_u(wide, 32);
        let carry = self.b.ins().ireduce(I32, carry);
        (result, carry)
    }

    /// CR bit `n` (MSB first) as an I32 0 or 1.
    fn cr_bit(&mut self, n: u8) -> Value {
        let cr = self.read(CR);
        let v = self.b.ins().ushr_imm_u(cr, 31 - n as i64);
        self.b.ins().band_imm_u(v, 1)
    }

    /// The timebase (64 bits) or the decrementer (low 32 bits of the
    /// result) as the interpreter would see it at instruction number
    /// `index` (0-based) of the current guest block: the clock ticked once
    /// per instruction executed since the region was entered, and the
    /// struct holds the value at the region entry. Each tick adds one to
    /// the prescaler; every `tb_divider` of them make one timebase tick
    /// (see `Cpu::tick`).
    fn time_now(&mut self, index: u32, clock: Clock) -> Value {
        let enabled = self.b.ins().uload8(I32, MemFlagsData::trusted(), self.cpu, offset_of!(Cpu, tb_enabled) as i32);
        let prescaler = self.b.ins().load(I32, MemFlagsData::trusted(), self.cpu, offset_of!(Cpu, tb_prescaler) as i32);
        let divider = self.b.ins().load(I32, MemFlagsData::trusted(), self.cpu, offset_of!(Cpu, tb_divider) as i32);
        let before = self.b.use_var(self.steps);
        let before = self.b.ins().iadd_imm_u(before, index as i64);
        let elapsed = self.b.ins().iadd(prescaler, before);
        let ticks = self.b.ins().udiv(elapsed, divider);
        let zero = self.c(0);
        let ticks = self.b.ins().select(enabled, ticks, zero);
        let ticks = self.b.ins().uextend(I64, ticks);
        match clock {
            Clock::Timebase => {
                let tb = self.b.ins().load(I64, MemFlagsData::trusted(), self.cpu, offset_of!(Cpu, tb) as i32);
                self.b.ins().iadd(tb, ticks)
            }
            Clock::Decrementer => {
                let dec = self.b.ins().load(I32, MemFlagsData::trusted(), self.cpu, offset_of!(Cpu, dec) as i32);
                let dec = self.b.ins().uextend(I64, dec);
                self.b.ins().isub(dec, ticks)
            }
        }
    }

    /// Tail of XO-form arithmetic with OE=0: rD, then CR0 if Rc=1.
    fn finish_xo(&mut self, ins: Ins, v: Value) {
        self.write(ins.field_rd() as usize, v);
        if ins.field_rc() {
            self.record_cr0(v);
        }
    }

    /// Tail of logical/rotate/shift operations: rA, then CR0 if Rc=1.
    fn finish_logical(&mut self, ins: Ins, v: Value) {
        self.write(ins.field_ra() as usize, v);
        if ins.field_rc() {
            self.record_cr0(v);
        }
    }

    // ------------------------------------------------------------------
    // The region
    // ------------------------------------------------------------------

    fn region(&mut self, region: &[GuestBlock], entry_index: Value, done: Value) {
        for block in region {
            let label = self.b.create_block();
            self.labels.push((block.start, label, block.insns.len() as u32));
        }
        // Steps count from those done by the regions chained before this one.
        self.b.def_var(self.steps, done);
        self.load_slots(self.carry);
        // Jump to the entry block: a jump table indexed by `entry`. Every
        // edge needs its own BlockCall (the SSA builder appends variable
        // arguments to each), so the default target gets a separate one.
        let calls: Vec<_> = self.labels.iter().map(|&(_, label, _)| self.b.func.dfg.block_call(label, &[])).collect();
        let default = self.b.func.dfg.block_call(self.labels[0].1, &[]);
        let table = self.b.create_jump_table(JumpTableData::new(default, &calls));
        self.b.ins().br_table(entry_index, table);
        let labels = self.labels.clone();
        for (block, &(_, label, _)) in region.iter().zip(&labels) {
            self.b.switch_to_block(label);
            // A guest block can be entered from several places: carried
            // registers arrive in variables, the others are re-read (the
            // struct is up to date at block edges).
            self.loaded = self.carry;
            self.dirty = 0;
            self.cr_cache = [None; 8];
            self.guest_block(block);
        }
    }

    fn guest_block(&mut self, block: &GuestBlock) {
        let len = block.insns.len() as u32;
        for (i, insn) in block.insns.iter().enumerate() {
            let pc = block.start.wrapping_add(4 * i as u32);
            let executed = i as u32 + 1;
            match insn.kind {
                Kind::Native => self.native(insn.ins, executed, pc),
                Kind::Helper => self.helper(insn.word, executed, pc),
                Kind::Branch => return self.branch(insn.ins, len, pc),
                Kind::Stop => unreachable!("the scanner never includes Stop instructions"),
            }
        }
        // No branch at the end: continue with the next instruction.
        let total = self.total(len);
        self.goto(Target::Known(block.start.wrapping_add(4 * len)), total);
    }

    /// Instructions executed once the current guest block's first
    /// `executed` instructions are done.
    fn total(&mut self, executed: u32) -> Value {
        let before = self.b.use_var(self.steps);
        self.b.ins().iadd_imm_u(before, executed as i64)
    }

    /// Continue at `target` with `total` instructions executed: jump to that
    /// guest block if it is in the region and still fits in the budget,
    /// otherwise return to the dispatcher.
    fn goto(&mut self, target: Target, total: Value) {
        self.flush();
        if let Target::Known(pc) = target {
            if let Some(&(_, label, len)) = self.labels.iter().find(|l| l.0 == pc) {
                let after = self.b.ins().iadd_imm_u(total, len as i64);
                let fits = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, after, self.budget);
                let (stay, leave) = (self.b.create_block(), self.b.create_block());
                self.b.ins().brif(fits, stay, &[], leave, &[]);
                self.b.switch_to_block(stay);
                self.b.def_var(self.steps, total);
                self.b.ins().jump(label, &[]);
                self.b.switch_to_block(leave);
                let next = self.c(pc);
                return self.finish(next, total);
            }
        }
        self.chain(target, total);
    }

    /// Leave the region for `target` (outside it) through the exit's
    /// [`Link`]: straight into the region there if the link is current (and
    /// for a computed target, made for this address) and that block fits in
    /// the budget, else back to the dispatcher, telling it which link to
    /// fill in.
    fn chain(&mut self, target: Target, total: Value) {
        let index = self.link_targets.len();
        assert!(index < self.link_capacity, "more region exits than links");
        self.link_targets.push(match target {
            Target::Known(pc) => Some(pc),
            Target::Computed(_) => None,
        });
        // SAFETY: inside the `link_capacity` links the caller provided.
        let link = unsafe { self.links.add(index) };
        let link = self.b.ins().iconst(I64, link as i64);
        let flags = MemFlagsData::trusted();
        let epoch = self.b.ins().load(I64, flags, link, offset_of!(Link, epoch) as i32);
        let current = self.b.ins().load(I64, flags, self.ctx, offset_of!(JitCtx, chain_epoch) as i32);
        let mut valid = self.b.ins().icmp(IntCC::Equal, epoch, current);
        let next = match target {
            Target::Known(pc) => self.c(pc),
            Target::Computed(next) => {
                let cached = self.b.ins().load(I32, flags, link, offset_of!(Link, target) as i32);
                let same = self.b.ins().icmp(IntCC::Equal, cached, next);
                valid = self.b.ins().band(valid, same);
                next
            }
        };
        let len = self.b.ins().load(I32, flags, link, offset_of!(Link, len) as i32);
        let after = self.b.ins().iadd(total, len);
        let fits = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, after, self.budget);
        let go = self.b.ins().band(valid, fits);
        let (jump, leave) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(go, jump, &[], leave, &[]);

        self.b.switch_to_block(jump);
        let chains = self.b.ins().load(I64, flags, self.ctx, offset_of!(JitCtx, chains) as i32);
        let chains = self.b.ins().iadd_imm_u(chains, 1);
        self.b.ins().store(flags, chains, self.ctx, offset_of!(JitCtx, chains) as i32);
        let func = self.b.ins().load(I64, flags, link, offset_of!(Link, func) as i32);
        let entry = self.b.ins().load(I32, flags, link, offset_of!(Link, entry) as i32);
        self.b.ins().return_call_indirect(self.sig_region, func, &[self.ctx, self.cpu, self.budget, entry, total]);

        self.b.switch_to_block(leave);
        self.b.ins().store(flags, link, self.ctx, offset_of!(JitCtx, exit_link) as i32);
        self.finish(next, total);
    }

    /// Return to the dispatcher: `total` instructions done, continue at `next`.
    fn finish(&mut self, next: Value, total: Value) {
        self.b.ins().store(MemFlagsData::trusted(), next, self.cpu, offset_of!(Cpu, pc) as i32);
        self.b.ins().return_(&[total]);
    }

    /// A block leaving the region (filled in by `emit_stubs`). It stores
    /// the registers dirty at this point.
    fn stub(&mut self, stub: Stub) -> Block {
        let block = self.b.create_block();
        self.stubs.push((block, stub));
        block
    }

    fn emit_stubs(&mut self) {
        for (block, stub) in std::mem::take(&mut self.stubs) {
            self.b.switch_to_block(block);
            match stub {
                Stub::Raised { executed, pc, dirty } => {
                    self.store_slots(dirty);
                    let total = self.total(executed);
                    let fault = self.c(pc);
                    self.b.ins().store(MemFlagsData::trusted(), fault, self.ctx, offset_of!(JitCtx, fault_pc) as i32);
                    let r = self.b.ins().bor_imm_u(total, RAISED as i64);
                    self.b.ins().return_(&[r]);
                }
                Stub::Exit { executed, next_pc, dirty } => {
                    self.store_slots(dirty);
                    let total = self.total(executed);
                    let next = self.c(next_pc);
                    self.b.ins().store(MemFlagsData::trusted(), next, self.cpu, offset_of!(Cpu, pc) as i32);
                    let r = self.b.ins().bor_imm_u(total, EARLY as i64);
                    self.b.ins().return_(&[r]);
                }
            }
        }
    }

    /// After a helper returned `status`: on EXCEPTION leave through a
    /// `Raised` stub; otherwise run `then` (the instruction's remaining
    /// effects), and on EXIT leave through an `Exit` stub.
    fn after_helper(&mut self, status: Value, executed: u32, pc: u32, then: impl FnOnce(&mut Self), join: Block) {
        let raised = self.stub(Stub::Raised { executed, pc, dirty: self.dirty });
        let ok = self.b.create_block();
        let is_exc = self.b.ins().icmp_imm_u(IntCC::Equal, status, EXCEPTION as i64);
        self.b.ins().brif(is_exc, raised, &[], ok, &[]);
        self.b.switch_to_block(ok);
        then(self);
        let exit = self.stub(Stub::Exit { executed, next_pc: pc.wrapping_add(4), dirty: self.dirty });
        let is_exit = self.b.ins().icmp_imm_u(IntCC::Equal, status, EXIT as i64);
        self.b.ins().brif(is_exit, exit, &[], join, &[]);
    }

    /// One instruction through the interpreter (`Kind::Helper`).
    fn helper(&mut self, word: u32, steps: u32, pc: u32) {
        // The interpreter works on the struct: bring it up to date, and
        // afterwards re-read what is used (it may have changed anything).
        self.flush();
        let f = self.b.ins().load(I64, MemFlagsData::trusted(), self.ctx, (offset_of!(JitCtx, helpers) + offset_of!(Helpers, exec)) as i32);
        let (w, p) = (self.c(word), self.c(pc));
        let call = self.b.ins().call_indirect(self.sig_exec, f, &[self.ctx, w, p]);
        let status = self.b.inst_results(call)[0];
        // Carried registers must be fresh on every path out of here.
        self.load_slots(self.carry);
        self.loaded = self.carry;
        self.cr_cache = [None; 8];
        let join = self.b.create_block();
        self.after_helper(status, steps, pc, |_| {}, join);
        self.b.switch_to_block(join);
    }

    // ------------------------------------------------------------------
    // Memory
    // ------------------------------------------------------------------

    /// Address of the fast table entry for `ea`, and the lookup key.
    fn table_entry(&mut self, table: Value, ea: Value, size: u32) -> (Value, Value) {
        let page = self.b.ins().ushr_imm_u(ea, 12);
        let index = self.b.ins().band_imm_u(page, TABLE_ENTRIES as i64 - 1);
        let index = self.b.ins().uextend(I64, index);
        let offset = self.b.ins().imul_imm_u(index, size_of::<FastEntry>() as i64);
        let entry = self.b.ins().iadd(table, offset);
        // The low bits of the key make misaligned accesses miss.
        let key_mask = self.c(0xFFFF_F000 | (size - 1));
        let key = self.b.ins().band(ea, key_mask);
        (entry, key)
    }

    /// Host address of `ea` through a table entry that matched.
    fn host_address(&mut self, entry: Value, ea: Value) -> Value {
        let addend = self.b.ins().load(I64, MemFlagsData::trusted(), entry, offset_of!(FastEntry, addend) as i32);
        let ea = self.b.ins().uextend(I64, ea);
        self.b.ins().iadd(ea, addend)
    }

    fn convert(&mut self, raw: Value, conv: Conv) -> Value {
        match conv {
            Conv::None => raw,
            Conv::SignHalf => {
                let h = self.b.ins().ireduce(I16, raw);
                self.b.ins().sextend(I32, h)
            }
            Conv::SwapHalf => {
                let h = self.b.ins().ireduce(I16, raw);
                let h = self.b.ins().bswap(h);
                self.b.ins().uextend(I32, h)
            }
            Conv::SwapWord => self.b.ins().bswap(raw),
        }
    }

    /// A load of `size` bytes at `ea` into rD (and `ea` into rA for update forms).
    fn load(&mut self, ea: Value, size: u32, conv: Conv, rd: usize, update: Option<usize>, steps: u32, pc: u32) {
        let writeback = move |t: &mut Self, raw: Value| {
            let v = t.convert(raw, conv);
            if let Some(ra) = update {
                t.write(ra, ea);
            }
            t.write(rd, v); // last: wins if rD == rA, as in the interpreter
        };
        let (entry, key) = self.table_entry(self.read_tab, ea, size);
        let tag = self.b.ins().load(I32, MemFlagsData::trusted(), entry, offset_of!(FastEntry, tag) as i32);
        let hit = self.b.ins().icmp(IntCC::Equal, tag, key);
        let (fast, slow, join) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(hit, fast, &[], slow, &[]);

        let before = self.reg_state();
        self.b.switch_to_block(fast);
        let host = self.host_address(entry, ea);
        let flags = MemFlagsData::new().with_notrap();
        let raw = match size {
            1 => self.b.ins().uload8(I32, flags, host, 0),
            2 => {
                let h = self.b.ins().load(I16, flags, host, 0);
                let h = self.b.ins().bswap(h);
                self.b.ins().uextend(I32, h)
            }
            _ => {
                let w = self.b.ins().load(I32, flags, host, 0);
                self.b.ins().bswap(w)
            }
        };
        writeback(self, raw);
        self.b.ins().jump(join, &[]);
        let fast_state = self.reg_state();

        self.set_reg_state(before);
        self.b.switch_to_block(slow);
        let k = [0, 0, 1, 1, 2][size as usize];
        let f = self.b.ins().load(I64, MemFlagsData::trusted(), self.ctx, (offset_of!(JitCtx, helpers) + offset_of!(Helpers, load) + 8 * k) as i32);
        let call = self.b.ins().call_indirect(self.sig_load, f, &[self.ctx, ea]);
        let r = self.b.inst_results(call)[0];
        let status = self.b.ins().ushr_imm_u(r, 32);
        let status = self.b.ins().ireduce(I32, status);
        let raw = self.b.ins().ireduce(I32, r);
        self.after_helper(status, steps, pc, |t| writeback(t, raw), join);

        self.b.switch_to_block(join);
        self.merge_reg_state(fast_state);
    }

    /// A store of the low `size` bytes of `value` at `ea` (and `ea` into rA
    /// for update forms).
    fn store(&mut self, ea: Value, value: Value, size: u32, update: Option<usize>, steps: u32, pc: u32) {
        let writeback = move |t: &mut Self| {
            if let Some(ra) = update {
                t.write(ra, ea);
            }
        };
        let (entry, key) = self.table_entry(self.write_tab, ea, size);
        let tag = self.b.ins().load(I32, MemFlagsData::trusted(), entry, offset_of!(FastEntry, tag) as i32);
        let hit = self.b.ins().icmp(IntCC::Equal, tag, key);
        let (fast, slow, join) = (self.b.create_block(), self.b.create_block(), self.b.create_block());
        self.b.ins().brif(hit, fast, &[], slow, &[]);

        let before = self.reg_state();
        self.b.switch_to_block(fast);
        let host = self.host_address(entry, ea);
        let flags = MemFlagsData::new().with_notrap();
        match size {
            1 => {
                self.b.ins().istore8(flags, value, host, 0);
            }
            2 => {
                let h = self.b.ins().ireduce(I16, value);
                let h = self.b.ins().bswap(h);
                self.b.ins().store(flags, h, host, 0);
            }
            _ => {
                let w = self.b.ins().bswap(value);
                self.b.ins().store(flags, w, host, 0);
            }
        }
        writeback(self);
        self.b.ins().jump(join, &[]);
        let fast_state = self.reg_state();

        self.set_reg_state(before);
        self.b.switch_to_block(slow);
        let k = [0, 0, 1, 1, 2][size as usize];
        let f = self.b.ins().load(I64, MemFlagsData::trusted(), self.ctx, (offset_of!(JitCtx, helpers) + offset_of!(Helpers, store) + 8 * k) as i32);
        let call = self.b.ins().call_indirect(self.sig_store, f, &[self.ctx, ea, value]);
        let status = self.b.inst_results(call)[0];
        self.after_helper(status, steps, pc, writeback, join);

        self.b.switch_to_block(join);
        self.merge_reg_state(fast_state);
    }

    fn effective_address(&mut self, ins: Ins, mode: Addr) -> Value {
        let ra = ins.field_ra() as usize;
        let base = match mode {
            Addr::D | Addr::X => self.ra0(ra),
            Addr::DUpdate | Addr::XUpdate => self.read(ra),
        };
        let offset = match mode {
            Addr::D | Addr::DUpdate => self.c(ins.field_simm() as i32 as u32),
            Addr::X | Addr::XUpdate => self.read(ins.field_rb() as usize),
        };
        self.b.ins().iadd(base, offset)
    }

    fn load_insn(&mut self, ins: Ins, size: u32, conv: Conv, mode: Addr, steps: u32, pc: u32) {
        let ea = self.effective_address(ins, mode);
        let update = matches!(mode, Addr::DUpdate | Addr::XUpdate).then_some(ins.field_ra() as usize);
        self.load(ea, size, conv, ins.field_rd() as usize, update, steps, pc);
    }

    fn store_insn(&mut self, ins: Ins, size: u32, swap: bool, mode: Addr, steps: u32, pc: u32) {
        let ea = self.effective_address(ins, mode);
        let mut value = self.read(ins.field_rs() as usize);
        if swap {
            value = self.convert(value, if size == 2 { Conv::SwapHalf } else { Conv::SwapWord });
        }
        let update = matches!(mode, Addr::DUpdate | Addr::XUpdate).then_some(ins.field_ra() as usize);
        self.store(ea, value, size, update, steps, pc);
    }

    // ------------------------------------------------------------------
    // Branches
    // ------------------------------------------------------------------

    /// The BO/BI condition (see `branch_condition` in `execute.rs`), or
    /// `None` for "always".
    fn branch_condition(&mut self, bo: u8, bi: u8, can_decrement_ctr: bool) -> Option<Value> {
        let mut cond = None;
        if bo & 0x04 == 0 && can_decrement_ctr {
            let ctr = self.read(CTR);
            let ctr = self.b.ins().iadd_imm_s(ctr, -1);
            self.write(CTR, ctr);
            let cc = if bo & 0x02 != 0 { IntCC::Equal } else { IntCC::NotEqual };
            cond = Some(self.b.ins().icmp_imm_u(cc, ctr, 0));
        }
        if bo & 0x10 == 0 {
            let want = (bo >> 3) as i64 & 1;
            // A bit set by a comparison earlier in this guest block: test
            // the comparison's result instead of extracting it from CR.
            let ok = match self.cr_cache[bi as usize / 4] {
                Some(flags) if bi % 4 < 3 => {
                    let flag = flags[bi as usize % 4];
                    if want == 1 { flag } else { self.b.ins().bxor_imm_u(flag, 1) }
                }
                _ => {
                    let bit = self.cr_bit(bi);
                    self.b.ins().icmp_imm_u(IntCC::Equal, bit, want)
                }
            };
            cond = Some(match cond {
                Some(c) => self.b.ins().band(c, ok),
                None => ok,
            });
        }
        cond
    }

    /// The branch ending a guest block of `len` instructions, at `pc`.
    fn branch(&mut self, ins: Ins, len: u32, pc: u32) {
        let (cond, target) = match ins.op {
            Opcode::B => {
                let offset = ins.field_li() as u32;
                (None, Target::Known(if ins.field_aa() { offset } else { pc.wrapping_add(offset) }))
            }
            Opcode::Bc => {
                let cond = self.branch_condition(ins.field_bo(), ins.field_bi(), true);
                let offset = ins.field_bd() as i32 as u32;
                (cond, Target::Known(if ins.field_aa() { offset } else { pc.wrapping_add(offset) }))
            }
            Opcode::Bclr => {
                let cond = self.branch_condition(ins.field_bo(), ins.field_bi(), true);
                let lr = self.read(LR); // before LK overwrites it
                let word = self.c(!3);
                (cond, Target::Computed(self.b.ins().band(lr, word)))
            }
            Opcode::Bcctr => {
                let cond = self.branch_condition(ins.field_bo(), ins.field_bi(), false);
                let ctr = self.read(CTR);
                let word = self.c(!3);
                (cond, Target::Computed(self.b.ins().band(ctr, word)))
            }
            _ => unreachable!("not a branch: {:?}", ins.op),
        };
        if ins.field_lk() {
            let ret = self.c(pc.wrapping_add(4));
            self.write(LR, ret);
        }
        let total = self.total(len);
        self.flush(); // before the split: both paths leave the guest block
        match cond {
            None => self.goto(target, total),
            Some(c) => {
                let (taken, not_taken) = (self.b.create_block(), self.b.create_block());
                self.b.ins().brif(c, taken, &[], not_taken, &[]);
                self.b.switch_to_block(taken);
                self.goto(target, total);
                self.b.switch_to_block(not_taken);
                self.goto(Target::Known(pc.wrapping_add(4)), total);
            }
        }
    }

    // ------------------------------------------------------------------
    // Everything else translated natively (`Kind::Native`)
    // ------------------------------------------------------------------

    fn native(&mut self, ins: Ins, steps: u32, pc: u32) {
        use Opcode::*;
        let (rd, ra, rb, rs) = (ins.field_rd() as usize, ins.field_ra() as usize, ins.field_rb() as usize, ins.field_rs() as usize);
        let simm = ins.field_simm() as i32 as u32;
        let uimm = ins.field_uimm() as u32;
        match ins.op {
            // ---- arithmetic, immediate ----
            Addi | Addis => {
                let a = self.ra0(ra);
                let k = self.c(if ins.op == Addis { simm << 16 } else { simm });
                let v = self.b.ins().iadd(a, k);
                self.write(rd, v);
            }
            Addic | Addic_ => {
                let (a, k, zero) = (self.read(ra), self.c(simm), self.c(0));
                let (v, ca) = self.add3(a, k, zero);
                self.write(rd, v);
                self.set_ca(ca);
                if ins.op == Addic_ {
                    self.record_cr0(v);
                }
            }
            Subfic => {
                let a = self.read(ra);
                let na = self.b.ins().bnot(a);
                let (k, one) = (self.c(simm), self.c(1));
                let (v, ca) = self.add3(na, k, one);
                self.write(rd, v);
                self.set_ca(ca);
            }
            Mulli => {
                let (a, k) = (self.read(ra), self.c(simm));
                let v = self.b.ins().imul(a, k);
                self.write(rd, v);
            }

            // ---- arithmetic, register (OE=0) ----
            Add => {
                let (a, b) = (self.read(ra), self.read(rb));
                let v = self.b.ins().iadd(a, b);
                self.finish_xo(ins, v);
            }
            Subf => {
                let (a, b) = (self.read(ra), self.read(rb));
                let v = self.b.ins().isub(b, a);
                self.finish_xo(ins, v);
            }
            Neg => {
                let a = self.read(ra);
                let v = self.b.ins().ineg(a);
                self.finish_xo(ins, v);
            }
            Addc | Adde | Addme | Addze | Subfc | Subfe | Subfme | Subfze => {
                // a + b + carry_in, exactly as in execute.rs
                let a = self.read(ra);
                let a = if matches!(ins.op, Subfc | Subfe | Subfme | Subfze) { self.b.ins().bnot(a) } else { a };
                let b = match ins.op {
                    Addc | Adde | Subfc | Subfe => self.read(rb),
                    Addme | Subfme => self.c(u32::MAX),
                    _ => self.c(0),
                };
                let cin = match ins.op {
                    Addc => self.c(0),
                    Subfc => self.c(1),
                    _ => self.ca(),
                };
                let (v, ca) = self.add3(a, b, cin);
                self.set_ca(ca);
                self.finish_xo(ins, v);
            }
            Mullw | Mulhw | Mulhwu => {
                let (a, b) = (self.read(ra), self.read(rb));
                let v = match ins.op {
                    Mullw => self.b.ins().imul(a, b),
                    Mulhw => self.b.ins().smulhi(a, b),
                    _ => self.b.ins().umulhi(a, b),
                };
                self.finish_xo(ins, v);
            }
            Divw | Divwu => {
                // The undefined cases give 0 (as in the interpreter); the
                // host division must never see them, it would trap.
                let (a, b) = (self.read(ra), self.read(rb));
                let zero = self.b.ins().icmp_imm_u(IntCC::Equal, b, 0);
                let bad = if ins.op == Divw {
                    let min = self.b.ins().icmp_imm_u(IntCC::Equal, a, i32::MIN as u32 as i64);
                    let minus1 = self.b.ins().icmp_imm_u(IntCC::Equal, b, u32::MAX as i64);
                    let overflow = self.b.ins().band(min, minus1);
                    self.b.ins().bor(zero, overflow)
                } else {
                    zero
                };
                let one = self.c(1);
                let safe = self.b.ins().select(bad, one, b);
                let q = if ins.op == Divw { self.b.ins().sdiv(a, safe) } else { self.b.ins().udiv(a, safe) };
                let z = self.c(0);
                let v = self.b.ins().select(bad, z, q);
                self.finish_xo(ins, v);
            }

            // ---- compare ----
            Cmp | Cmpl => {
                let (a, b) = (self.read(ra), self.read(rb));
                self.compare(ins.field_crfd(), a, b, ins.op == Cmp);
            }
            Cmpi => {
                let (a, b) = (self.read(ra), self.c(simm));
                self.compare(ins.field_crfd(), a, b, true);
            }
            Cmpli => {
                let (a, b) = (self.read(ra), self.c(uimm));
                self.compare(ins.field_crfd(), a, b, false);
            }

            // ---- logical ----
            Andi_ | Andis_ | Ori | Oris | Xori | Xoris => {
                let s = self.read(rs);
                let k = self.c(if matches!(ins.op, Andis_ | Oris | Xoris) { uimm << 16 } else { uimm });
                let v = match ins.op {
                    Andi_ | Andis_ => self.b.ins().band(s, k),
                    Ori | Oris => self.b.ins().bor(s, k),
                    _ => self.b.ins().bxor(s, k),
                };
                self.write(ra, v);
                if matches!(ins.op, Andi_ | Andis_) {
                    self.record_cr0(v);
                }
            }
            And | Andc | Or | Orc | Xor | Nand | Nor | Eqv => {
                let (s, b) = (self.read(rs), self.read(rb));
                let v = match ins.op {
                    And => self.b.ins().band(s, b),
                    Andc => self.b.ins().band_not(s, b),
                    Or => self.b.ins().bor(s, b),
                    Orc => self.b.ins().bor_not(s, b),
                    Xor => self.b.ins().bxor(s, b),
                    Nand => {
                        let v = self.b.ins().band(s, b);
                        self.b.ins().bnot(v)
                    }
                    Nor => {
                        let v = self.b.ins().bor(s, b);
                        self.b.ins().bnot(v)
                    }
                    _ => {
                        let v = self.b.ins().bxor(s, b);
                        self.b.ins().bnot(v)
                    }
                };
                self.finish_logical(ins, v);
            }
            Extsb | Extsh => {
                let s = self.read(rs);
                let narrow = self.b.ins().ireduce(if ins.op == Extsb { I8 } else { I16 }, s);
                let v = self.b.ins().sextend(I32, narrow);
                self.finish_logical(ins, v);
            }
            Cntlzw => {
                let s = self.read(rs);
                let v = self.b.ins().clz(s);
                self.finish_logical(ins, v);
            }

            // ---- rotate and shift ----
            Rlwinm | Rlwnm | Rlwimi => {
                let m = mask(ins.field_mb(), ins.field_me());
                let s = self.read(rs);
                let amount = if ins.op == Rlwnm {
                    let b = self.read(rb);
                    self.b.ins().band_imm_u(b, 31)
                } else {
                    self.c(ins.field_sh() as u32)
                };
                let rotated = self.b.ins().rotl(s, amount);
                let km = self.c(m);
                let masked = self.b.ins().band(rotated, km);
                let v = if ins.op == Rlwimi {
                    let a = self.read(ra);
                    let keep = self.c(!m);
                    let kept = self.b.ins().band(a, keep);
                    self.b.ins().bor(masked, kept)
                } else {
                    masked
                };
                self.finish_logical(ins, v);
            }
            Slw | Srw => {
                let (s, b) = (self.read(rs), self.read(rb));
                let n = self.b.ins().band_imm_u(b, 0x3F);
                // Host shifts take the amount modulo 32; 32..63 give 0 here.
                let shifted = if ins.op == Slw { self.b.ins().ishl(s, n) } else { self.b.ins().ushr(s, n) };
                let big = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, n, 32);
                let z = self.c(0);
                let v = self.b.ins().select(big, z, shifted);
                self.finish_logical(ins, v);
            }
            Sraw | Srawi => {
                let s = self.read(rs);
                let (v, out) = if ins.op == Srawi {
                    let n = ins.field_sh() as u32;
                    let v = self.b.ins().sshr_imm_u(s, n as i64);
                    let k = self.c((1u32 << n).wrapping_sub(1));
                    (v, self.b.ins().band(s, k))
                } else {
                    let b = self.read(rb);
                    let n = self.b.ins().band_imm_u(b, 0x3F);
                    let big = self.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, n, 32);
                    // 32..63: the result is all sign bits = a shift by 31,
                    // and every bit counts as shifted out.
                    let k31 = self.c(31);
                    let amount = self.b.ins().select(big, k31, n);
                    let v = self.b.ins().sshr(s, amount);
                    let one = self.c(1);
                    let low = self.b.ins().ishl(one, n);
                    let low = self.b.ins().iadd_imm_s(low, -1);
                    let all = self.c(u32::MAX);
                    let m = self.b.ins().select(big, all, low);
                    (v, self.b.ins().band(s, m))
                };
                // CA = negative and at least one 1-bit shifted out.
                let negative = self.b.ins().icmp_imm_u(IntCC::SignedLessThan, s, 0);
                let lost = self.b.ins().icmp_imm_u(IntCC::NotEqual, out, 0);
                let ca = self.b.ins().band(negative, lost);
                let ca = self.bit(ca);
                self.set_ca(ca);
                self.finish_logical(ins, v);
            }

            // ---- condition register ----
            Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => {
                let (a, b) = (self.cr_bit(ins.field_crba()), self.cr_bit(ins.field_crbb()));
                let r = match ins.op {
                    Crand => self.b.ins().band(a, b),
                    Crandc => self.b.ins().band_not(a, b),
                    Creqv => {
                        let x = self.b.ins().bxor(a, b);
                        self.b.ins().bnot(x)
                    }
                    Crnand => {
                        let x = self.b.ins().band(a, b);
                        self.b.ins().bnot(x)
                    }
                    Crnor => {
                        let x = self.b.ins().bor(a, b);
                        self.b.ins().bnot(x)
                    }
                    Cror => self.b.ins().bor(a, b),
                    Crorc => self.b.ins().bor_not(a, b),
                    _ => self.b.ins().bxor(a, b),
                };
                let r = self.b.ins().band_imm_u(r, 1);
                let shift = 31 - ins.field_crbd() as u32;
                let cr = self.read(CR);
                let keep = self.c(!(1 << shift));
                let kept = self.b.ins().band(cr, keep);
                let moved = self.b.ins().ishl_imm_u(r, shift as i64);
                let v = self.b.ins().bor(kept, moved);
                self.write(CR, v);
            }
            Mcrf => {
                let cr = self.read(CR);
                let f = self.b.ins().ushr_imm_u(cr, 28 - 4 * ins.field_crfs() as i64);
                let f = self.b.ins().band_imm_u(f, 0xF);
                self.set_cr_field(ins.field_crfd(), f);
            }
            Mfcr => {
                let cr = self.read(CR);
                self.write(rd, cr);
            }
            Mtcrf => {
                let crm = ins.field_crm();
                let m = (0..8).filter(|f| crm & (0x80 >> f) != 0).fold(0u32, |m, f| m | 0xF000_0000 >> (4 * f));
                let (cr, s) = (self.read(CR), self.read(rs));
                let keep = self.c(!m);
                let kept = self.b.ins().band(cr, keep);
                let km = self.c(m);
                let new = self.b.ins().band(s, km);
                let v = self.b.ins().bor(kept, new);
                self.write(CR, v);
            }
            Mfspr | Mftb => {
                let n = if ins.op == Mftb { ins.field_tbr() } else { ins.field_spr() };
                let v = match n {
                    crate::spr::TBL_READ => {
                        let tb = self.time_now(steps - 1, Clock::Timebase);
                        self.b.ins().ireduce(I32, tb)
                    }
                    crate::spr::TBU_READ => {
                        let tb = self.time_now(steps - 1, Clock::Timebase);
                        let hi = self.b.ins().ushr_imm_u(tb, 32);
                        self.b.ins().ireduce(I32, hi)
                    }
                    crate::spr::DEC => {
                        let dec = self.time_now(steps - 1, Clock::Decrementer);
                        self.b.ins().ireduce(I32, dec)
                    }
                    other => {
                        let slot = spr_slot(other);
                        self.read(slot)
                    }
                };
                self.write(rd, v);
            }
            Mtspr => {
                let slot = spr_slot(ins.field_spr());
                let mut v = self.read(rs);
                if slot == XER {
                    v = self.b.ins().band_imm_u(v, 0xE000_007F);
                }
                self.write(slot, v);
            }

            // ---- loads and stores ----
            Lbz => self.load_insn(ins, 1, Conv::None, Addr::D, steps, pc),
            Lbzu => self.load_insn(ins, 1, Conv::None, Addr::DUpdate, steps, pc),
            Lbzx => self.load_insn(ins, 1, Conv::None, Addr::X, steps, pc),
            Lbzux => self.load_insn(ins, 1, Conv::None, Addr::XUpdate, steps, pc),
            Lhz => self.load_insn(ins, 2, Conv::None, Addr::D, steps, pc),
            Lhzu => self.load_insn(ins, 2, Conv::None, Addr::DUpdate, steps, pc),
            Lhzx => self.load_insn(ins, 2, Conv::None, Addr::X, steps, pc),
            Lhzux => self.load_insn(ins, 2, Conv::None, Addr::XUpdate, steps, pc),
            Lha => self.load_insn(ins, 2, Conv::SignHalf, Addr::D, steps, pc),
            Lhau => self.load_insn(ins, 2, Conv::SignHalf, Addr::DUpdate, steps, pc),
            Lhax => self.load_insn(ins, 2, Conv::SignHalf, Addr::X, steps, pc),
            Lhaux => self.load_insn(ins, 2, Conv::SignHalf, Addr::XUpdate, steps, pc),
            Lwz => self.load_insn(ins, 4, Conv::None, Addr::D, steps, pc),
            Lwzu => self.load_insn(ins, 4, Conv::None, Addr::DUpdate, steps, pc),
            Lwzx => self.load_insn(ins, 4, Conv::None, Addr::X, steps, pc),
            Lwzux => self.load_insn(ins, 4, Conv::None, Addr::XUpdate, steps, pc),
            Lhbrx => self.load_insn(ins, 2, Conv::SwapHalf, Addr::X, steps, pc),
            Lwbrx => self.load_insn(ins, 4, Conv::SwapWord, Addr::X, steps, pc),
            Stb => self.store_insn(ins, 1, false, Addr::D, steps, pc),
            Stbu => self.store_insn(ins, 1, false, Addr::DUpdate, steps, pc),
            Stbx => self.store_insn(ins, 1, false, Addr::X, steps, pc),
            Stbux => self.store_insn(ins, 1, false, Addr::XUpdate, steps, pc),
            Sth => self.store_insn(ins, 2, false, Addr::D, steps, pc),
            Sthu => self.store_insn(ins, 2, false, Addr::DUpdate, steps, pc),
            Sthx => self.store_insn(ins, 2, false, Addr::X, steps, pc),
            Sthux => self.store_insn(ins, 2, false, Addr::XUpdate, steps, pc),
            Stw => self.store_insn(ins, 4, false, Addr::D, steps, pc),
            Stwu => self.store_insn(ins, 4, false, Addr::DUpdate, steps, pc),
            Stwx => self.store_insn(ins, 4, false, Addr::X, steps, pc),
            Stwux => self.store_insn(ins, 4, false, Addr::XUpdate, steps, pc),
            Sthbrx => self.store_insn(ins, 2, true, Addr::X, steps, pc),
            Stwbrx => self.store_insn(ins, 4, true, Addr::X, steps, pc),

            Sync | Isync | Eieio | Dcbt | Dcbtst => {}

            _ => unreachable!("{:?} is not a native instruction", ins.op),
        }
    }
}

/// The variable slot of an SPR the translator handles natively.
fn spr_slot(n: u16) -> usize {
    match n {
        crate::spr::LR => LR,
        crate::spr::CTR => CTR,
        crate::spr::XER => XER,
        _ => unreachable!("SPR {n} is not native"),
    }
}
