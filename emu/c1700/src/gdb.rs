//! GDB remote debugging, implemented with the `gdbstub` crate.
//!
//! `c1700emu --gdb 1234` starts with the CPU halted at the reset vector and
//! waits for:
//!
//! ```text
//! $ gdb
//! (gdb) set architecture powerpc:common
//! (gdb) target remote :1234
//! (gdb) break *0xfff060d8        # rom_init_main
//! (gdb) continue
//! ```
//!
//! The register layout is gdbstub_arch's `PowerPcAltivec32` (r0-r31, pc, msr,
//! cr, lr, ctr, xer; the FPU/AltiVec registers it also declares read as 0 —
//! the MPC860 has neither). The console keeps working while GDB is attached.

use crate::emulator::{Emulator, SLICE};
use crate::machine::StopReason;
use gdbstub::common::Signal;
use gdbstub::conn::{Connection, ConnectionExt as _};
use gdbstub::stub::{DisconnectReason, GdbStub, SingleThreadStopReason, run_blocking};
use gdbstub::target::ext::base::BaseOps;
use gdbstub::target::ext::base::singlethread::{
    SingleThreadBase, SingleThreadResume, SingleThreadResumeOps, SingleThreadSingleStep,
    SingleThreadSingleStepOps,
};
use gdbstub::target::ext::breakpoints::{Breakpoints, BreakpointsOps, SwBreakpoint, SwBreakpointOps};
use gdbstub::target::{Target, TargetError, TargetResult};
use gdbstub_arch::ppc::PowerPcAltivec32;
use gdbstub_arch::ppc::reg::PowerPcCommonRegs;
use std::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExecMode {
    Continue,
    Step,
}

/// The emulator as seen by gdbstub. It owns the [`Emulator`] for the
/// duration of the debug session (gdbstub's event loop wants a `'static`
/// target type, and ownership is the simplest way to give it one).
pub struct GdbTarget {
    pub emu: Emulator,
    mode: ExecMode,
    /// We are resuming from a breakpoint: execute one instruction before
    /// checking breakpoints again.
    leaving_breakpoint: bool,
}

impl Target for GdbTarget {
    type Arch = PowerPcAltivec32<()>;
    type Error = String;

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn support_breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadBase for GdbTarget {
    fn read_registers(&mut self, regs: &mut PowerPcCommonRegs) -> TargetResult<(), Self> {
        let cpu = &self.emu.machine.cpu;
        regs.r = cpu.gpr;
        regs.pc = cpu.pc;
        regs.msr = cpu.msr;
        regs.cr = cpu.cr;
        regs.lr = cpu.lr;
        regs.ctr = cpu.ctr;
        regs.xer = cpu.xer;
        Ok(())
    }

    fn write_registers(&mut self, regs: &PowerPcCommonRegs) -> TargetResult<(), Self> {
        let cpu = &mut self.emu.machine.cpu;
        cpu.gpr = regs.r;
        cpu.pc = regs.pc;
        cpu.msr = regs.msr;
        cpu.cr = regs.cr;
        cpu.lr = regs.lr;
        cpu.ctr = regs.ctr;
        cpu.xer = regs.xer;
        Ok(())
    }

    fn read_addrs(&mut self, start_addr: u32, data: &mut [u8]) -> TargetResult<usize, Self> {
        let n = self.emu.machine.read_mem(start_addr, data);
        if n == 0 && !data.is_empty() {
            return Err(TargetError::NonFatal);
        }
        Ok(n)
    }

    fn write_addrs(&mut self, start_addr: u32, data: &[u8]) -> TargetResult<(), Self> {
        if self.emu.machine.write_mem(start_addr, data) { Ok(()) } else { Err(TargetError::NonFatal) }
    }

    fn support_resume(&mut self) -> Option<SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for GdbTarget {
    fn resume(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.mode = ExecMode::Continue;
        self.leaving_breakpoint = true;
        Ok(())
    }

    fn support_single_step(&mut self) -> Option<SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for GdbTarget {
    fn step(&mut self, _signal: Option<Signal>) -> Result<(), Self::Error> {
        self.mode = ExecMode::Step;
        Ok(())
    }
}

impl Breakpoints for GdbTarget {
    fn support_sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}

// "Software" breakpoints are implemented in the emulator (no code patching):
// the machine checks the PC against this set before every instruction.
impl SwBreakpoint for GdbTarget {
    fn add_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.emu.machine.breakpoints.insert(addr))
    }

    fn remove_sw_breakpoint(&mut self, addr: u32, _kind: usize) -> TargetResult<bool, Self> {
        Ok(self.emu.machine.breakpoints.remove(&addr))
    }
}

enum EventLoop {}

impl run_blocking::BlockingEventLoop for EventLoop {
    type Target = GdbTarget;
    type Connection = TcpStream;
    type StopReason = SingleThreadStopReason<u32>;

    fn wait_for_stop_reason(
        target: &mut Self::Target,
        conn: &mut Self::Connection,
    ) -> Result<
        run_blocking::Event<Self::StopReason>,
        run_blocking::WaitForStopReasonError<<Self::Target as Target>::Error, <Self::Connection as Connection>::Error>,
    > {
        if target.mode == ExecMode::Step {
            let stop = target.emu.machine.run(1, true);
            target.emu.pump_console();
            return Ok(run_blocking::Event::TargetStopped(match stop {
                StopReason::Checkstop => SingleThreadStopReason::Terminated(Signal::SIGSEGV),
                _ => SingleThreadStopReason::DoneStep,
            }));
        }
        loop {
            let ignore_bp = std::mem::take(&mut target.leaving_breakpoint);
            let stop = target.emu.machine.run(SLICE / 10, ignore_bp);
            target.emu.pump_console();
            match stop {
                StopReason::Breakpoint(_) => {
                    return Ok(run_blocking::Event::TargetStopped(SingleThreadStopReason::SwBreak(())));
                }
                StopReason::Checkstop => {
                    return Ok(run_blocking::Event::TargetStopped(SingleThreadStopReason::Terminated(
                        Signal::SIGSEGV,
                    )));
                }
                StopReason::Budget => {}
            }
            if target.emu.quit {
                return Ok(run_blocking::Event::TargetStopped(SingleThreadStopReason::Exited(0)));
            }
            // Ctrl-C (or any packet) from GDB interrupts the run.
            if conn.peek().map(|b| b.is_some()).unwrap_or(true) {
                let byte = conn.read().map_err(run_blocking::WaitForStopReasonError::Connection)?;
                return Ok(run_blocking::Event::IncomingData(byte));
            }
        }
    }

    fn on_interrupt(_target: &mut Self::Target) -> Result<Option<Self::StopReason>, <Self::Target as Target>::Error> {
        Ok(Some(SingleThreadStopReason::Signal(Signal::SIGINT)))
    }
}

/// Wait for GDB on `port`, then debug until it disconnects. Returns the
/// emulator so the caller can keep running it normally afterwards.
pub fn serve(emu: Emulator, port: u16) -> Result<Emulator, String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    eprint!("[c1700emu] waiting for GDB on 127.0.0.1:{port} (CPU halted at {:#010x})\r\n", emu.machine.cpu.pc);
    let (stream, peer) = listener.accept().map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    eprint!("[c1700emu] GDB connected from {peer}\r\n");

    let mut target = GdbTarget { emu, mode: ExecMode::Continue, leaving_breakpoint: false };
    let gdb = GdbStub::<GdbTarget, TcpStream>::new(stream);
    match gdb.run_blocking::<EventLoop>(&mut target) {
        Ok(DisconnectReason::Disconnect) => eprint!("[c1700emu] GDB disconnected, continuing\r\n"),
        Ok(reason) => eprint!("[c1700emu] GDB session ended: {reason:?}\r\n"),
        Err(e) => return Err(format!("gdbstub error: {e}")),
    }
    Ok(target.emu)
}
