//! The built-in boot ROM (`--rom builtin`, docs ch. 24) boots a tiny image
//! from flash, which talks to it through EMT calls the way IOS does: it
//! prints "OK", looks at why it was booted (`boot_data`), reloads once and
//! then stores the memory size where the test can read it.

use c1700emu::board::{BoardConfig, flashfs, nvram::Nvram};
use c1700emu::fakerom;
use c1700emu::machine::Machine;

const ENTRY: u32 = 0x8000_8000;

/// The test image's code, at ENTRY.
const PROGRAM: [u32; 18] = [
    0x3860_0001, // li r3,1          EMT 1: putc 'O'
    0x3880_004F, // li r4,'O'
    0x4400_0002, // sc
    0x3860_0001, // li r3,1          putc 'K'
    0x3880_004B, // li r4,'K'
    0x4400_0002, // sc
    0x3860_000D, // li r3,13         EMT 13: boot_data
    0x4400_0002, // sc
    0x80A3_0004, // lwz r5,4(r3)     the reason for this boot
    0x2C05_0009, // cmpwi r5,9       9: after a reload
    0x4182_000C, // beq done
    0x3860_0008, // li r3,8          EMT 8: reload
    0x4400_0002, // sc
    0x3860_0004, // done: li r3,4    EMT 4: memory size
    0x4400_0002, // sc
    0x3CC0_8001, // lis r6,0x8001
    0x9066_0000, // stw r3,0(r6)
    0x4800_0000, // b .
];
const DONE: u32 = ENTRY + 4 * 17;

/// An ELF file as IOS images are: `e_machine` 0x33, one text section.
fn elf() -> Vec<u8> {
    let code: Vec<u8> = PROGRAM.iter().flat_map(|w| w.to_be_bytes()).collect();
    let mut f = vec![0u8; 0x100];
    f[0..6].copy_from_slice(b"\x7fELF\x01\x02");
    f[18..20].copy_from_slice(&0x33u16.to_be_bytes());
    f[24..28].copy_from_slice(&ENTRY.to_be_bytes());
    f[32..36].copy_from_slice(&0x34u32.to_be_bytes()); // e_shoff
    f[46..48].copy_from_slice(&40u16.to_be_bytes()); // e_shentsize
    f[48..50].copy_from_slice(&2u16.to_be_bytes()); // e_shnum: null + .text
    let text = 0x34 + 40;
    for (n, v) in [(1, 1), (2, 6), (3, ENTRY), (4, 0x100), (5, code.len() as u32)] {
        f[text + 4 * n..text + 4 * n + 4].copy_from_slice(&v.to_be_bytes());
    }
    f.extend_from_slice(&code);
    f
}

#[test]
fn builtin_rom_boots_an_image_and_answers_its_emt_calls() {
    let cfg = BoardConfig { net: None, ..BoardConfig::default() };
    let mut m = Machine::new(fakerom::image(), &cfg, Nvram::new(32 * 1024, None).unwrap());
    m.use_builtin_rom();
    let flash = flashfs::build(&[("test.elf".into(), elf())], m.board.flash.size() as usize).unwrap();
    m.board.flash.load(&flash);

    let mut out = Vec::new();
    assert!(m.run_until_output("OK", 1_000_000, &mut out));
    for _ in 0..100 {
        if m.cpu.pc == DONE {
            break;
        }
        m.run(10_000, false);
    }
    out.extend(m.board.io.uart.tx.drain(..));
    let text = String::from_utf8_lossy(&out);
    assert_eq!(m.cpu.pc, DONE, "console:\n{text}");
    assert!(text.contains("built-in boot ROM") && text.contains("entry point: 0x80008000"), "{text}");
    assert_eq!(text.matches("OK").count(), 2, "booted twice (power-on, reload):\n{text}");
    assert_eq!(m.resets, 1);
    assert_eq!(m.board.peek_word(0x8001_0000), Some(32 << 20));
}

#[test]
fn builtin_rom_reports_a_missing_image() {
    let cfg = BoardConfig { net: None, ..BoardConfig::default() };
    let mut m = Machine::new(fakerom::image(), &cfg, Nvram::new(32 * 1024, None).unwrap());
    m.use_builtin_rom();
    let mut out = Vec::new();
    assert!(m.run_until_output("system halted", 100_000, &mut out), "{}", String::from_utf8_lossy(&out));
    assert!(String::from_utf8_lossy(&out).contains("no image in flash"));
}
