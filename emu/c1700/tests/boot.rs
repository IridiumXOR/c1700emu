//! End-to-end test: boot the real ROM to the ROMMON prompt and run commands.
//! Skipped (with a message) when the ROM image is not present.

mod common;
use c1700emu::board::BoardConfig;
use common::*;

#[test]
fn boots_to_rommon_and_answers_commands() {
    if !rom_available() {
        return;
    }
    let mut m = new_machine(&BoardConfig::default(), 0x2100); // boot field 0: stay in ROMMON
    let text = expect(&mut m, "rommon 1 >", 500_000_000);
    eprintln!("prompt after {} cycles", m.cpu.cycles);
    assert!(text.contains("System Bootstrap, Version 12.2(1r)XE1"));
    assert!(text.contains("C1700 platform with 32768 Kbytes of main memory"));
    assert!(!text.contains("Cookie information is corrupt"), "default cookie not accepted:\n{text}");

    type_line(&mut m, "meminfo");
    let text = expect(&mut m, "rommon 2 >", 50_000_000);
    assert!(text.contains("Main memory size: 32 MB"), "{text}");
    assert!(text.contains("NVRAM size: 32KB"), "{text}");

    type_line(&mut m, "dev");
    let text = expect(&mut m, "rommon 3 >", 50_000_000);
    assert!(text.contains("flash:"));
}

/// "Boot directly from the internal flash": the image is put into the flash
/// file system before power-on (what `--flash-file` does) and ROMMON, with
/// boot field 2, finds and boots it — no TFTP involved.
#[test]
fn boots_image_preloaded_in_internal_flash() {
    if !rom_available() {
        return;
    }
    let hello = include_bytes!("../src/board/testdata/hello.bin").to_vec();
    let mut m = new_machine(&BoardConfig { net: None, ..BoardConfig::default() }, 0x2102);
    let image = c1700emu::board::flashfs::build(&[("hello.bin".into(), hello)], m.board.flash.size() as usize).unwrap();
    m.board.flash.load(&image);
    let text = expect(&mut m, "Hello from a TFTP-booted image!", 1_000_000_000);
    assert!(text.contains("program load complete, entry point: 0x80008000"), "{text}");
}
