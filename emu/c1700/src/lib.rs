//! Emulator for the Cisco C1700 "System Bootstrap" ROM (ROMMON), MPC860T based.
//!
//! The code is split along the same lines as the real hardware:
//!
//! * `ppc8xx` (separate crate) – the PowerPC core of the MPC860. It only knows
//!   registers, instructions and exceptions and talks to the outside world
//!   through the [`ppc8xx::Platform`] trait.
//! * [`mpc860`]  – the on-chip peripherals in the 64 KB IMMR window: SIU
//!   (interrupts, timers, clocks/reset), memory controller, CPM, FEC.
//! * [`board`]   – what Cisco soldered around the chip: boot ROM, SDRAM,
//!   flash, NVRAM, a CPLD and a 16550 UART, plus the chip-select decoder.
//! * [`machine`] – CPU + board + the virtual clock.
//! * [`net`]     – a virtual network (smoltcp) with a TFTP server, plugged
//!   into the FEC so ROMMON's `tftpdnld` works without root privileges.
//! * [`console`], [`emulator`], [`gdb`] – the user's side: terminal/telnet
//!   console and the GDB remote stub.
//!
//! See `docs/` in the repository for the reverse-engineering notes behind
//! every device model.

pub mod board;
pub mod console;
pub mod coredump;
pub mod emulator;
pub mod fakerom;
pub mod fastforward;
pub mod gdb;
pub mod idle;
pub mod machine;
pub mod mpc860;
pub mod net;
pub mod snapshot;
pub mod wakeup;
