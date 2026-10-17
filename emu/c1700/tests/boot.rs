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

/// The ROM accelerators (delay-loop fast-forward, bulk memory clear) must be
/// invisible to the guest: stopped at the same instruction (ROMMON's first
/// wait for a key), virtual time, timebase, registers, console output and
/// all of RAM are identical with and without them.
#[test]
fn rom_accelerators_are_exact() {
    if !rom_available() {
        return;
    }
    use c1700emu::machine::StopReason;
    const FIRST_KEY_POLL: u32 = 0xFFF0_BCAC; // uart_getc_nowait
    let run = |delay_loops: bool, memory_clear: bool| {
        let cfg = BoardConfig { dram_banks: [16 << 20, 0, 0, 0], net: None, ..BoardConfig::default() };
        let mut m = new_machine(&cfg, 0x2100);
        m.fast_forward.restrict(delay_loops, memory_clear);
        m.breakpoints.insert(FIRST_KEY_POLL);
        let mut out = Vec::new();
        loop {
            match m.run(1_000_000, false) {
                StopReason::Breakpoint(_) => break,
                StopReason::Budget => assert!(m.cpu.cycles < 3_000_000_000, "never reached ROMMON's key poll"),
                other => panic!("{other:?}"),
            }
            out.extend(m.take_console_output());
        }
        out.extend(m.take_console_output());
        let ram = m.board.dram[0].clone();
        let exc: Vec<(usize, u64)> = m.cpu.exception_counts.iter().copied().enumerate().filter(|(_, n)| *n > 0).collect();
        eprintln!("delay_loops={delay_loops} memory_clear={memory_clear}: exceptions {exc:x?}, skipped ticks {}", m.fast_forward.skipped_ticks);
        (m.cpu.cycles, m.cpu.tb, m.cpu.gpr, m.cpu.cr, String::from_utf8_lossy(&out).into_owned(), ram, m.fast_forward.bulk_cleared)
    };
    let slow = run(false, false);
    let only_delay = run(true, false);
    let only_clear = run(false, true);
    eprintln!("cycles: none {} / delay loops only {} / memory clear only {}", slow.0, only_delay.0, only_clear.0);
    let fast = run(true, true);
    assert!(fast.6 > 15 << 20, "the 16 MB memory clear was done in bulk ({} bytes)", fast.6);
    assert_eq!(slow.6, 0);
    assert_eq!(fast.0, slow.0, "same virtual time at the breakpoint");
    assert_eq!((fast.1, fast.2, fast.3), (slow.1, slow.2, slow.3), "same timebase and registers");
    assert_eq!(fast.4, slow.4, "same console output");
    assert!(fast.5 == slow.5, "same RAM contents");
    eprintln!("both reach the first key poll at {} cycles", fast.0);
}
