//! A minimal boot ROM built into the emulator (`--rom builtin`, docs ch.
//! 24): boots IOS without Cisco's boot ROM, as Dynamips does, by doing in
//! Rust what ROMMON 12.2(1r)XE1 does for an IOS image, and nothing else.
//!
//! What IOS needs from the ROM (measured on the real one, docs ch. 24):
//!
//! * **The machine set up**: IMMR at 0xFF000000, the memory controller
//!   decoding the boot ROM, SDRAM (also at 0x80000000: the base masks leave
//!   out address bit 0), flash and the I/O bus, the timebase running.
//! * **The image loaded**: the allocated PROGBITS sections of the ELF file
//!   (`e_machine` 0x33) copied to their addresses, then a jump to the entry
//!   with r1 = a stack, r3 = 2, r4 = 0, r5 = a parameter block, LR = back
//!   into the ROM, MSR[IP] = 0 (exception vectors in RAM).
//! * **ROM monitor calls** ("EMT" calls): IOS and its self-decompressor
//!   execute `sc` with the call number in r3 and arguments in r4-r6; the
//!   RAM vector at 0xC00 jumps into the ROM, which answers in r3. About
//!   540 calls per boot, 20 distinct ones (console output, memory size,
//!   configuration register, cookie, NVRAM layout, boot data, ROM monitor
//!   variables, reload).
//!
//! The image is mostly empty: the 8xx exception entry points (0x100-0x1F00)
//! hold `b .`, except the EMT entry and the decrementer, which hold `rfi`.
//! The emulator services those addresses (`Machine::before_step`): the
//! reset vector boots, the EMT entry runs [`FakeRom::emt`] and then lets
//! the `rfi` return to the caller.

use crate::board::{Board, flashfs};
use ppc8xx::{Cpu, Platform, msr};

/// Size of the image (that of the real AT27C040 boot ROM).
pub const ROM_SIZE: usize = 512 << 10;
const ROM_BASE: u32 = 0xFFF0_0000;
/// The reset vector (hard reset with MSR[IP] = 1): boot.
pub const RESET: u32 = ROM_BASE + 0x100;
/// Where the RAM vector 0xC00 sends `sc`: the ROM monitor call entry.
pub const EMT_ENTRY: u32 = ROM_BASE + 0xC00;
const DECREMENTER: u32 = ROM_BASE + 0x900;
/// The image's return address (LR at its entry).
pub const IMAGE_RETURN: u32 = ROM_BASE + 0x80;
/// The version string (EMT 10: `show version` prints "ROM: " and its first
/// line, from the third character on).
const BANNER: u32 = ROM_BASE + 0x2000;
const BANNER_TEXT: &str = "\r\nc1700emu built-in boot ROM, Version 1.0 (no ROM monitor)\r\n";

const INS_RFI: u32 = 0x4C00_0064;
const INS_SPIN: u32 = 0x4800_0000; // b .

/// The ROM's data in low RAM, below the image (0x80008000), as ROMMON's
/// (IOS never touches this region: it keeps calling the ROM, which uses it).
const DATA: u32 = 0x8000_4000;
/// EMT 13: the `boot_data` words: [0] = 19 (BOOT_COUNT), [1] = why the
/// system last returned to the ROM (0 power-on, 9 reload), [2] PC, [3]
/// address.
const BOOT_DATA: u32 = DATA;
/// EMT 26: boot device type (3 = flash) and the booted file, "flash:NAME".
const BOOT_INFO: u32 = DATA + 0x100;
/// EMT 34/35: a string IOS stores (its banner and text base) and reads back.
const IMAGE_STRING: u32 = DATA + 0x400;
/// EMT 36/37: the version string `show version` compares (set once per boot).
const VERSION_STRING: u32 = DATA + 0x600;
/// r5 at the image entry (a parameter block; IOS doesn't use it).
const PARAMS: u32 = DATA + 0x800;
/// r1 at the image entry (the decompressor sets its own stack).
const STACK: u32 = DATA + 0x3F00;
const STRING_LEN: u32 = 257;

/// NVRAM layout shared with the real ROM (docs ch. 4).
const NV_COOKIE_DATA: usize = 2; // 128 bytes after the cookie's magic
const NV_CONFREG: usize = 0x90; // u16, then its complement, then 0xFEEDFACE
const NV_IOMEM: usize = 0x9F; // IOMEM percentage
const NV_EMT71: usize = 0xA0; // a word IOS reads through EMT 71
/// IOS's NVRAM area starts after the ROM's first 2 KB (EMT 47).
const NV_ROM_AREA: u32 = 0x800;
/// Room for ROM monitor variables, as in the real ROM.
const VARS_SPACE: usize = 768;

/// Reload reasons in `boot_data` (IOS's `sub_8061A190`).
const REASON_POWER_ON: u32 = 0;
const REASON_RELOAD: u32 = 9;

/// The built-in ROM's image: entry points at the 8xx exception vector
/// offsets and its version string.
pub fn image() -> Vec<u8> {
    let mut rom = vec![0u8; ROM_SIZE];
    let mut put = |addr: u32, word: u32| {
        let o = (addr - ROM_BASE) as usize;
        rom[o..o + 4].copy_from_slice(&word.to_be_bytes());
    };
    for v in (0x100..0x2000).step_by(0x100) {
        put(ROM_BASE + v, INS_SPIN); // unexpected exception (reported by the hook)
    }
    put(EMT_ENTRY, INS_RFI); // the hook has answered: back to the caller
    put(DECREMENTER, INS_RFI); // nothing to do before IOS takes over
    put(IMAGE_RETURN, INS_SPIN);
    let o = (BANNER - ROM_BASE) as usize;
    rom[o..o + BANNER_TEXT.len()].copy_from_slice(BANNER_TEXT.as_bytes());
    rom
}

/// Is `pc` an address of the built-in ROM that the emulator services?
#[inline]
pub fn is_hook(pc: u32) -> bool {
    pc >> 13 == ROM_BASE >> 13 && (pc & 0xFF == 0 || pc == IMAGE_RETURN)
}

/// What a ROM entry point asks the machine to do.
pub enum Action {
    /// Execute the instruction there (`rfi` after an EMT call).
    Run,
    /// The CPU state changed (the image was started): look again.
    Again,
    /// Reset the machine and boot again (EMT 8, IOS's `reload`).
    Reload,
    /// Stop the machine; the message was printed on the console.
    Halt,
}

/// The built-in ROM's state outside guest memory.
#[derive(Default)]
pub struct FakeRom {
    /// Reason the system returns to the ROM, for the next boot's `boot_data`.
    next_reason: u32,
    /// The configuration register the image was booted with (EMT 28).
    boot_confreg: u16,
    /// EMT 37 stores its string once per boot.
    version_set: bool,
    /// ROM monitor variables (EMT 45/46), in setting order. Kept across
    /// reloads, not in NVRAM.
    vars: Vec<(String, String)>,
    /// EMT numbers already reported as not implemented.
    warned: Vec<u32>,
}

/// The image's loadable pieces, as ROMMON's `loadprog` takes them: the
/// allocated PROGBITS sections with data, as (address, bytes), and the
/// entry point.
pub fn elf_sections(image: &[u8]) -> Result<(u32, Vec<(u32, &[u8])>), String> {
    let half = |o: usize| image.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize);
    let word = |o: usize| image.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    if image.get(0..4) != Some(b"\x7fELF".as_slice()) || image.get(4) != Some(&1) || image.get(5) != Some(&2) {
        return Err("not a 32-bit big-endian ELF file".into());
    }
    let short = || "truncated ELF header".to_string();
    let machine = half(18).ok_or_else(short)?;
    if machine != 0x33 {
        return Err(format!("e_machine {machine:#x} is not a C1700 image (0x33)"));
    }
    let entry = word(24).ok_or_else(short)?;
    let (shoff, shentsize, shnum) = (word(32).ok_or_else(short)? as usize, half(46).ok_or_else(short)?, half(48).ok_or_else(short)?);
    let mut sections = Vec::new();
    for i in 0..shnum {
        let h = shoff + i * shentsize;
        let field = |n: usize| word(h + 4 * n).ok_or_else(|| format!("truncated section header {i}"));
        let (kind, flags, addr, offset, size) = (field(1)?, field(2)?, field(3)?, field(4)? as usize, field(5)? as usize);
        if kind == 1 && flags & 2 != 0 && size != 0 {
            let data = image.get(offset..offset + size).ok_or_else(|| format!("section {i} lies outside the file"))?;
            sections.push((addr, data));
        }
    }
    if sections.is_empty() {
        return Err("no loadable sections".into());
    }
    Ok((entry, sections))
}

/// The configuration register in NVRAM, if valid (complement and magic).
fn nvram_confreg(nv: &[u8]) -> Option<u16> {
    let confreg = u16::from_be_bytes([nv[NV_CONFREG], nv[NV_CONFREG + 1]]);
    let inverse = u16::from_be_bytes([nv[NV_CONFREG + 2], nv[NV_CONFREG + 3]]);
    let magic = u32::from_be_bytes(nv[NV_CONFREG + 4..NV_CONFREG + 8].try_into().unwrap());
    (magic == 0xFEED_FACE && confreg == !inverse).then_some(confreg)
}

/// Console speed selected by configuration register bits 5, 11 and 12.
fn console_baud(confreg: u16) -> u32 {
    let speed = (confreg >> 11) & 3;
    if confreg & 0x0020 != 0 { [19200, 38400, 57600, 115200][speed as usize] } else { [9600, 4800, 1200, 2400][speed as usize] }
}

/// Print on the console like the ROM's `putc` (LF becomes CR LF).
fn console(board: &mut Board, text: &str) {
    for b in text.bytes() {
        if b == b'\n' {
            board.io.uart.tx.push(b'\r');
        }
        board.io.uart.tx.push(b);
    }
}

/// A NUL-terminated string in guest memory (at most `max` bytes).
fn read_cstr(board: &mut Board, addr: u32, max: u32) -> String {
    let bytes: Vec<u8> = (0..max).map_while(|i| board.peek8(addr.wrapping_add(i)).filter(|&b| b != 0)).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Store `text` at `addr` as a NUL-terminated string padded with NULs to
/// `len` bytes (truncated if longer, like the ROM's `strncpy`).
fn write_cstr(board: &mut Board, addr: u32, text: &[u8], len: u32) {
    for i in 0..len {
        board.poke8(addr + i, text.get(i as usize).copied().unwrap_or(0));
    }
}

fn write_words(board: &mut Board, addr: u32, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        let _ = board.write32(addr + 4 * i as u32, *w);
    }
}

impl FakeRom {
    /// What ROMMON does from reset to the image's first instruction.
    pub fn boot(&mut self, cpu: &mut Cpu, board: &mut Board) -> Action {
        match self.try_boot(cpu, board) {
            Ok(()) => Action::Again,
            Err(e) => {
                console(board, &format!("\nbuilt-in boot ROM: {e}\nsystem halted\n"));
                log::error!("built-in boot ROM: {e}");
                Action::Halt
            }
        }
    }

    fn try_boot(&mut self, cpu: &mut Cpu, board: &mut Board) -> Result<(), String> {
        configure_hardware(board);
        let memsize = memory_size(board);
        console(board, BANNER_TEXT.trim_start_matches("\r\n").trim_end());
        console(board, &format!("\nC1700 platform with {} Kbytes of main memory\n", memsize >> 10));

        let confreg = nvram_confreg(board.io.nvram.bytes()).unwrap_or(0x2102);
        self.boot_confreg = confreg;
        if confreg & 0xF == 0 {
            return Err(format!(
                "configuration register {confreg:#06x} asks for the ROM monitor (boot field 0), which this ROM doesn't have; use --confreg 0x2102"
            ));
        }
        // Like ROMMON with no BOOT variable: the first file in flash.
        let files = flashfs::list(board.flash.bytes());
        let file = files.first().ok_or("no image in flash (put one there with --flash-file)")?;
        let start = file.offset + flashfs::HEADER_LEN;
        let data = board.flash.bytes().get(start..start + file.length).ok_or("flash file extends past the flash")?.to_vec();
        let (entry, sections) = elf_sections(&data).map_err(|e| format!("flash:{}: {e}", file.name))?;

        // Clear the RAM above the ROM's data (a reload leaves the old system there).
        if !board.fill_dram(0x8000, memsize - 0x8000, 0) {
            return Err("main memory is not where the memory controller put it".into());
        }
        let mut size = 0;
        for (addr, bytes) in &sections {
            if !board.load_dram(*addr, bytes) {
                return Err(format!("flash:{}: section at {addr:#x} is not in main memory", file.name));
            }
            size += bytes.len();
        }

        // Exception vectors in RAM (MSR[IP] = 0): each jumps to the ROM's
        // entry point for it, with an absolute branch (`ba`), so it still
        // works wherever IOS copies it.
        for v in (0x100..0x2000).step_by(0x100) {
            let _ = board.write32(v, 0x4800_0002 | ((ROM_BASE + v) & 0x03FF_FFFC));
        }
        let mut boot_data = [0u32; 20];
        boot_data[0] = 19;
        boot_data[1] = std::mem::replace(&mut self.next_reason, REASON_POWER_ON);
        write_words(board, BOOT_DATA, &boot_data);
        write_words(board, BOOT_INFO, &[3]);
        write_cstr(board, BOOT_INFO + 4, format!("flash:{}", file.name).as_bytes(), 0x1FC);
        write_cstr(board, IMAGE_STRING, b"", STRING_LEN);
        write_cstr(board, VERSION_STRING, b"", STRING_LEN);
        write_words(board, PARAMS, &[0; 16]);
        self.version_set = false;

        console(board, &format!("program load complete, entry point: {entry:#x}, size: {size:#x}\n"));
        log::info!("built-in boot ROM: booting flash:{} ({} sections, {size} bytes), entry {entry:#x}", file.name, sections.len());
        cpu.gpr[1] = STACK;
        cpu.gpr[3] = 2;
        cpu.gpr[4] = 0;
        cpu.gpr[5] = PARAMS;
        cpu.gpr[6] = entry;
        cpu.lr = IMAGE_RETURN;
        cpu.set_msr(msr::ME | msr::RI);
        cpu.pc = entry;
        Ok(())
    }

    /// A ROM monitor call: `sc` with the number in r3, arguments in r4-r6;
    /// the result goes in r3 (-1: not supported, as the real ROM answers).
    pub fn emt(&mut self, cpu: &mut Cpu, board: &mut Board) -> Action {
        let (n, a, b, c) = (cpu.gpr[3], cpu.gpr[4], cpu.gpr[5], cpu.gpr[6]);
        let nv_len = board.io.nvram.bytes().len();
        let result: u32 = match n {
            // putc
            1 => {
                let ch = a as u8;
                if ch == b'\n' {
                    board.io.uart.tx.push(b'\r');
                }
                board.io.uart.tx.push(ch);
                0
            }
            2 => 0xC00, // ROM monitor level: above 0x1FF, IOS takes the version string from EMT 10
            4 => memory_size(board),
            // the NVRAM configuration register, complemented (-1 if invalid)
            6 => nvram_confreg(board.io.nvram.bytes()).map_or(u32::MAX, |c| !c as u32),
            7 => 0x33, // platform: C1700
            8 => {
                self.next_reason = REASON_RELOAD;
                return Action::Reload;
            }
            10 => BANNER,
            13 => BOOT_DATA,
            24 => nv_len as u32, // NVRAM size
            26 => BOOT_INFO,
            28 => self.boot_confreg as u32,
            34 => IMAGE_STRING,
            35 => {
                let text = read_cstr(board, a, STRING_LEN);
                write_cstr(board, IMAGE_STRING, text.as_bytes(), STRING_LEN);
                1
            }
            36 => VERSION_STRING,
            37 => {
                if !self.version_set {
                    self.version_set = true;
                    let text = read_cstr(board, a, STRING_LEN);
                    write_cstr(board, VERSION_STRING, text.as_bytes(), STRING_LEN);
                }
                1
            }
            // the cookie (board identity) from NVRAM
            44 => {
                let cookie = board.io.nvram.bytes()[NV_COOKIE_DATA..NV_COOKIE_DATA + 128].to_vec();
                write_cstr(board, a, &cookie, 128);
                0
            }
            45 => self.set_var(board, a),
            46 => self.get_var(board, a, b, c),
            47 => NV_ROM_AREA,
            // IOMEM percentage (25 unless NVRAM holds a plausible one)
            60 => match board.io.nvram.bytes()[NV_IOMEM] {
                p @ 1..=99 => p as u32,
                _ => 25,
            },
            62 => console_baud(self.boot_confreg),
            71 => match u32::from_be_bytes(board.io.nvram.bytes()[NV_EMT71..NV_EMT71 + 4].try_into().unwrap()) {
                u32::MAX => 0, // erased NVRAM
                w => w,
            },
            _ => {
                if !self.warned.contains(&n) {
                    self.warned.push(n);
                    log::warn!(target: "syscall", "built-in boot ROM: EMT call {n} (r4={a:#x}) not implemented, returning -1");
                }
                u32::MAX
            }
        };
        cpu.gpr[3] = result;
        Action::Run
    }

    /// EMT 45: set a ROM monitor variable from "NAME=VALUE" at `addr` (an
    /// empty value deletes it); 0 just asks. Returns the space left.
    fn set_var(&mut self, board: &mut Board, addr: u32) -> u32 {
        if addr != 0 {
            let text = read_cstr(board, addr, VARS_SPACE as u32);
            let Some((name, value)) = text.split_once('=') else { return u32::MAX };
            self.vars.retain(|(n, _)| n != name);
            if !value.is_empty() {
                self.vars.push((name.to_string(), value.to_string()));
            }
            log::debug!(target: "syscall", "built-in boot ROM: variable {name}={value}");
        }
        let used: usize = self.vars.iter().map(|(n, v)| n.len() + v.len() + 2).sum();
        VARS_SPACE.saturating_sub(used) as u32
    }

    /// EMT 46: copy the value of variable `name` into `buf` (`size` bytes);
    /// returns its length, -1 if unset, -2 if it doesn't fit.
    fn get_var(&mut self, board: &mut Board, name: u32, buf: u32, size: u32) -> u32 {
        let name = read_cstr(board, name, 64);
        match self.vars.iter().find(|(n, _)| *n == name) {
            None => u32::MAX,
            Some((_, v)) if v.len() as u32 > size => (-2i32) as u32,
            Some((_, v)) => {
                let value = v.clone();
                write_cstr(board, buf, value.as_bytes(), (value.len() as u32 + 1).min(size.max(1)));
                value.len() as u32
            }
        }
    }

    /// Snapshot state (docs ch. 20).
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        v.u32(&mut self.next_reason);
        v.u16(&mut self.boot_confreg);
        v.bool(&mut self.version_set);
        let mut blob: Vec<u8> = self.vars.iter().flat_map(|(n, val)| format!("{n}={val}\0").into_bytes()).collect();
        v.bytes(&mut blob);
        if v.loading() {
            self.vars = blob
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| String::from_utf8_lossy(s).split_once('=').map(|(n, val)| (n.to_string(), val.to_string())))
                .collect();
        }
    }
}

/// Main memory in bytes (the SDRAM banks fitted).
fn memory_size(board: &Board) -> u32 {
    board.dram.iter().map(|b| b.len() as u32).sum()
}

/// The on-chip setup ROMMON leaves for the image (values read from the real
/// ROM's boot, at the image entry): internal registers at 0xFF000000; chip
/// selects for the boot ROM (CS0), the SDRAM banks (CS1-CS4, their masks
/// leaving out address bit 0 so RAM also answers at 0x80000000), the flash
/// (CS6) and the I/O bus (CS7); the timebase running.
fn configure_hardware(board: &mut Board) {
    const IMMR: u32 = 0xFF00_0000;
    board.write_spr(ppc8xx::spr::IMMR, IMMR);
    let mut reg = |off: u32, value: u32| {
        let _ = board.write32(IMMR + off, value);
    };
    reg(0x000, 0x0023_0440); // SIUMCR
    reg(0x004, 0xFFFF_FF88); // SYPCR: software watchdog off
    reg(0x014, 0x2000_0000); // SIMASK: IRQ1 (the console UART)
    reg(0x100, 0xFFF0_0401); // BR0/OR0: boot ROM, 1 MB, 8-bit
    reg(0x104, 0xFFF0_05A6);
    reg(0x170, 0x0C80_4111); // MAMR, MBMR
    reg(0x174, 0x0C80_4111);
    reg(0x280, 0x0380_0000); // SCCR
    reg(0x284, 0x0000_50C0); // PLPRCR
    let banks: Vec<u32> = board.dram.iter().map(|b| b.len() as u32).collect();
    let mut base = 0u32;
    for (i, &size) in banks.iter().enumerate() {
        let cs = 1 + i as u32;
        if size == 0 {
            continue;
        }
        let mut reg = |off: u32, value: u32| {
            let _ = board.write32(IMMR + off, value);
        };
        reg(0x100 + 8 * cs, base | 0x81); // valid, UPM
        reg(0x104 + 8 * cs, (!(size - 1) & 0x7FFF_8000) | 0x600);
        base += size;
    }
    let flash = board.flash.size().max(1 << 20).next_power_of_two();
    let mut reg = |off: u32, value: u32| {
        let _ = board.write32(IMMR + off, value);
    };
    reg(0x130, 0x6000_0901); // BR6/OR6: flash, 16-bit
    reg(0x134, (!(flash - 1) & 0xFFFF_8000) | 0x190);
    reg(0x138, 0x6800_0401); // BR7/OR7: I/O bus (NVRAM, CPLD, UART, slots), 8-bit
    reg(0x13C, 0xFFF0_01A8);
    let _ = board.write16(IMMR + 0x200, 0x0001); // TBSCR: timebase enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny ELF like an IOS image: one text section, one data section and
    /// a NOTE (not loaded).
    fn elf() -> Vec<u8> {
        let mut f = vec![0u8; 0x200];
        f[0..4].copy_from_slice(b"\x7fELF");
        f[4] = 1; // 32-bit
        f[5] = 2; // big-endian
        f[18..20].copy_from_slice(&0x33u16.to_be_bytes());
        f[24..28].copy_from_slice(&0x8000_8000u32.to_be_bytes());
        f[32..36].copy_from_slice(&0x40u32.to_be_bytes()); // e_shoff
        f[46..48].copy_from_slice(&40u16.to_be_bytes());
        f[48..50].copy_from_slice(&4u16.to_be_bytes());
        let mut sh = |i: usize, kind: u32, flags: u32, addr: u32, off: u32, size: u32| {
            let h = 0x40 + 40 * i;
            for (n, v) in [(1, kind), (2, flags), (3, addr), (4, off), (5, size)] {
                f[h + 4 * n..h + 4 * n + 4].copy_from_slice(&v.to_be_bytes());
            }
        };
        sh(1, 1, 7, 0x8000_8000, 0x100, 8); // text
        sh(2, 7, 0, 0x8100_0000, 0x110, 4); // NOTE: skipped
        sh(3, 1, 3, 0x8000_9000, 0x120, 4); // data
        f[0x100..0x108].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        f[0x120..0x124].copy_from_slice(&[9, 9, 9, 9]);
        f
    }

    #[test]
    fn elf_sections_are_the_allocated_progbits_ones() {
        let image = elf();
        let (entry, sections) = elf_sections(&image).unwrap();
        assert_eq!(entry, 0x8000_8000);
        assert_eq!(sections, vec![(0x8000_8000, &[1u8, 2, 3, 4, 5, 6, 7, 8][..]), (0x8000_9000, &[9u8, 9, 9, 9][..])]);
        let mut other = image.clone();
        other[19] = 0x14;
        assert!(elf_sections(&other).unwrap_err().contains("e_machine"));
        assert!(elf_sections(b"MZ...").is_err());
    }

    #[test]
    fn confreg_and_console_speed() {
        let mut nv = vec![0xFFu8; 0x100];
        assert_eq!(nvram_confreg(&nv), None);
        nv[0x90..0x92].copy_from_slice(&0x2102u16.to_be_bytes());
        nv[0x92..0x94].copy_from_slice(&(!0x2102u16).to_be_bytes());
        nv[0x94..0x98].copy_from_slice(&0xFEED_FACEu32.to_be_bytes());
        assert_eq!(nvram_confreg(&nv), Some(0x2102));
        assert_eq!(console_baud(0x2102), 9600);
        assert_eq!(console_baud(0x2102 | 0x0800), 4800);
        assert_eq!(console_baud(0x2102 | 0x1820), 115200);
    }

    #[test]
    fn image_has_the_entry_points() {
        let rom = image();
        let word = |a: u32| u32::from_be_bytes(rom[(a - ROM_BASE) as usize..][..4].try_into().unwrap());
        assert_eq!(word(EMT_ENTRY), INS_RFI);
        assert_eq!(word(DECREMENTER), INS_RFI);
        assert_eq!(word(RESET), INS_SPIN);
        assert!(is_hook(RESET) && is_hook(EMT_ENTRY) && is_hook(IMAGE_RETURN));
        assert!(!is_hook(0xFFF0_0C04) && !is_hook(0x8000_0C00) && !is_hook(0xFFF1_0000));
        assert_eq!(rom.len(), ROM_SIZE);
    }
}
