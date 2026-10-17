//! Helpers shared by the integration tests: boot the real ROM headlessly
//! and talk to it through the emulated UART.
#![allow(dead_code)]

use c1700emu::board::{BoardConfig, cookie, nvram::Nvram};
use c1700emu::machine::Machine;
use std::path::Path;

pub const ROM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roms/CISCO_bootrom_AT27C040.bin");

pub fn rom_available() -> bool {
    if Path::new(ROM).exists() {
        true
    } else {
        eprintln!("skipping: {ROM} not found");
        false
    }
}

/// NVRAM with a valid cookie and the given configuration register.
pub fn nvram_with_confreg(confreg: u16) -> Nvram {
    let mut nvram = Nvram::new(32 * 1024, None).unwrap();
    cookie::ensure_in_nvram(nvram.bytes_mut(), &cookie::Cookie::default());
    let b = nvram.bytes_mut();
    b[0x90..0x92].copy_from_slice(&confreg.to_be_bytes());
    b[0x92..0x94].copy_from_slice(&(!confreg).to_be_bytes());
    b[0x94..0x98].copy_from_slice(&0xFEED_FACEu32.to_be_bytes());
    nvram
}

pub fn new_machine(cfg: &BoardConfig, confreg: u16) -> Machine {
    let rom = std::fs::read(ROM).unwrap();
    Machine::new(rom, cfg, nvram_with_confreg(confreg))
}

/// Run until the console shows `needle`; panic with the transcript otherwise.
pub fn expect(m: &mut Machine, needle: &str, budget: u64) -> String {
    let mut out = Vec::new();
    let ok = m.run_until_output(needle, budget, &mut out);
    let text = String::from_utf8_lossy(&out).replace('\r', "");
    assert!(ok, "did not see {needle:?}; console:\n{text}");
    text
}

/// Type a line (plus Enter) on the console, waiting when the FIFO is full.
pub fn type_line(m: &mut Machine, line: &str) {
    for b in line.bytes().chain(std::iter::once(b'\r')) {
        while !m.console_input(b) {
            m.run(10_000, true);
        }
    }
}
