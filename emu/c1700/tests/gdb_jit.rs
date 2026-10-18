//! GDB with the JIT (docs ch. 7.3): a synthetic ROM spins in a loop until
//! the JIT runs it as translated code, then a minimal GDB remote-protocol
//! client attaches to the real stub (`gdb::serve`) and checks that:
//!
//! * a breakpoint in the middle of the translated loop stops it, on every
//!   iteration (the JIT treats breakpoints as hooks and starts afresh when
//!   they change);
//! * single steps execute one instruction;
//! * code patched from GDB is what runs next (the write goes through the
//!   code watch, the stale translation is dropped).

use c1700emu::board::{BoardConfig, nvram::Nvram};
use c1700emu::console::Console;
use c1700emu::emulator::Emulator;
use c1700emu::machine::{CpuEngine, Machine};
use c1700emu::wakeup::Wakeup;
use powerpc_asm::{Argument, Arguments, assemble};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn asm(line: &str) -> u32 {
    let (mnemonic, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    let mut args: Arguments = [Argument::None; 5];
    for (n, tok) in rest.split([',', '(', ')']).map(str::trim).filter(|t| !t.is_empty()).enumerate() {
        let tok = tok.strip_prefix('r').unwrap_or(tok);
        let (neg, digits) = tok.strip_prefix('-').map_or((false, tok), |d| (true, d));
        let magnitude = match digits.strip_prefix("0x") {
            Some(hex) => i64::from_str_radix(hex, 16),
            None => digits.parse::<i64>(),
        }
        .unwrap();
        let value = if neg { -magnitude } else { magnitude };
        args[n] = if value < 0 { Argument::Signed(value as i32) } else { Argument::Unsigned(value as u32) };
    }
    assemble(mnemonic, &args).unwrap_or_else(|e| panic!("cannot assemble {line:?}: {e}"))
}

const LOOP: u32 = 0xFFF0_0108; // addi r3, r3, 1
const ADD: u32 = 0xFFF0_010C; // add r4, r4, r3: the breakpoint

const PROGRAM: &[&str] = &[
    "li r3, 0",               // 0x100
    "li r4, 0",               // 0x104
    "addi r3, r3, 1",         // 0x108 loop:
    "add r4, r4, r3",         // 0x10c
    "xor r5, r5, r4",         // 0x110
    "b -12",                  // 0x114
];

/// A GDB remote serial protocol connection (acknowledged mode).
struct Rsp(TcpStream);

impl Rsp {
    fn connect(port: u16) -> Rsp {
        let start = Instant::now();
        loop {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
                return Rsp(s);
            }
            assert!(start.elapsed() < Duration::from_secs(10), "GDB stub never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send a command, return the reply's payload.
    fn cmd(&mut self, data: &str) -> String {
        let sum = data.bytes().fold(0u8, |s, b| s.wrapping_add(b));
        write!(self.0, "${data}#{sum:02x}").unwrap();
        let mut reply = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            self.0.read_exact(&mut byte).unwrap();
            match byte[0] {
                b'+' if reply.is_empty() => continue, // ack of our packet
                b'$' => reply.clear(),
                b'#' => break,
                b => reply.push(b),
            }
        }
        let mut checksum = [0u8; 2];
        self.0.read_exact(&mut checksum).unwrap();
        self.0.write_all(b"+").unwrap();
        // Run-length encoding: "X*n" repeats X another (n - 29) times.
        let mut out = Vec::new();
        let mut bytes = reply.into_iter();
        while let Some(b) = bytes.next() {
            if b == b'*' {
                let last = *out.last().unwrap();
                let count = bytes.next().unwrap() - 29;
                out.extend(std::iter::repeat_n(last, count as usize));
            } else {
                out.push(b);
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// (pc, r3) from a `g` packet: r0-r31, f0-f31 (8 bytes each), pc, ...
    fn pc_r3(&mut self) -> (u32, u32) {
        let g = self.cmd("g");
        let word = |at: usize| u32::from_str_radix(&g[at..at + 8], 16).unwrap();
        (word(2 * (32 * 4 + 32 * 8)), word(2 * 3 * 4))
    }
}

#[test]
fn gdb_breakpoints_steps_and_patches_with_the_jit() {
    let mut rom = vec![0u8; 64 << 10];
    for (i, line) in PROGRAM.iter().enumerate() {
        rom[0x100 + 4 * i..0x104 + 4 * i].copy_from_slice(&asm(line).to_be_bytes());
    }
    let cfg = BoardConfig { net: None, ..BoardConfig::default() };
    let mut machine = Machine::new(rom, &cfg, Nvram::new(32 * 1024, None).unwrap());
    machine.set_engine(CpuEngine::Jit, 1);
    // Spin until the loop runs as translated code (compiled in the background).
    let start = Instant::now();
    while machine.jit.as_ref().unwrap().stats.jit_instructions < 100_000 {
        machine.run(100_000, false);
        assert!(start.elapsed() < Duration::from_secs(20), "the loop never got translated");
    }
    let console = Console::new(false, None, Wakeup::default()).unwrap();
    let emu = Emulator::new(machine, console, None);
    let port = 40_000 + (std::process::id() % 20_000) as u16;
    // The machine stays on this thread (the stub runs it); GDB is a thread.
    let client = std::thread::spawn(move || {
        let mut gdb = Rsp::connect(port);
        gdb.cmd("?");
        assert_eq!(gdb.cmd(&format!("Z0,{ADD:x},4")), "OK");
        let reply = gdb.cmd("c");
        assert!(reply.starts_with('T') || reply.starts_with('S'), "stop reply: {reply}");
        let (pc, r3) = gdb.pc_r3();
        assert_eq!(pc, ADD, "stopped at the breakpoint inside the translated loop");
        gdb.cmd("c");
        assert_eq!(gdb.pc_r3(), (ADD, r3 + 1), "stops again after exactly one iteration");

        // Single step: one instruction.
        assert_eq!(gdb.cmd(&format!("z0,{ADD:x},4")), "OK");
        gdb.cmd("s");
        assert_eq!(gdb.pc_r3().0, ADD + 4);

        // Patch the loop's increment to 2 from GDB: the next iterations use it.
        let addi_2 = asm("addi r3, r3, 2");
        assert_eq!(gdb.cmd(&format!("M{LOOP:x},4:{addi_2:08x}")), "OK");
        assert_eq!(gdb.cmd(&format!("Z0,{ADD:x},4")), "OK");
        gdb.cmd("c");
        let (_, a) = gdb.pc_r3();
        gdb.cmd("c");
        let (pc, b) = gdb.pc_r3();
        assert_eq!((pc, b), (ADD, a + 2), "the patched instruction runs, not the stale translation");
        assert_eq!(gdb.cmd("D"), "OK");
    });
    let emu = c1700emu::gdb::serve(emu, port).unwrap();
    client.join().unwrap();
    let stats = &emu.machine.jit.as_ref().unwrap().stats;
    assert!(stats.flushes >= 2, "breakpoint changes restarted the JIT: {} flushes", stats.flushes);
}
