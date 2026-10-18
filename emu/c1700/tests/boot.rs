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

/// The JIT (docs ch. 17) boots the ROM to the ROMMON prompt in exactly the
/// same way as the interpreter: same instructions, same virtual time, same
/// registers, console output and RAM after the same number of steps.
#[test]
fn jit_boots_rommon_exactly_like_the_interpreter() {
    if !rom_available() {
        return;
    }
    use c1700emu::machine::CpuEngine;
    let cfg = BoardConfig { dram_banks: [16 << 20, 0, 0, 0], net: None, ..BoardConfig::default() };
    let boot = |engine: CpuEngine, slices: Option<usize>| {
        let mut m = new_machine(&cfg, 0x2100);
        m.set_engine(engine, 2);
        let mut out = Vec::new();
        let mut n = 0;
        while slices.map_or(!String::from_utf8_lossy(&out).contains("rommon 1 >"), |s| n < s) {
            m.run(1_000_000, false);
            out.extend(m.take_console_output());
            n += 1;
            assert!(n < 2000, "never reached the ROMMON prompt");
        }
        let report = m.jit.as_ref().map(|j| j.report()).unwrap_or_default();
        (n, m.cpu.cycles, m.cpu.tb, m.cpu.gpr, m.cpu.cr, m.cpu.msr, String::from_utf8_lossy(&out).into_owned(), m.board.dram[0].clone(), report)
    };
    let interp = boot(CpuEngine::Interp, None);
    let jit = boot(CpuEngine::Jit, Some(interp.0));
    eprintln!("{}", jit.8);
    assert_eq!(jit.1, interp.1, "virtual time");
    assert_eq!((jit.2, jit.3, jit.4, jit.5), (interp.2, interp.3, interp.4, interp.5), "timebase and registers");
    assert_eq!(jit.6, interp.6, "console output");
    assert!(jit.7 == interp.7, "RAM contents");
    assert!(jit.8.contains("regions translated"));
}

/// A snapshot taken in the middle of the boot and restored into a fresh
/// machine continues exactly like the original: same instructions, same
/// virtual time, same console output and RAM, also after typing a command.
/// Then a core dump of the result is a well-formed ELF file.
#[test]
fn snapshot_restore_continues_identically() {
    if !rom_available() {
        return;
    }
    let cfg = BoardConfig { dram_banks: [16 << 20, 0, 0, 0], net: None, ..BoardConfig::default() };
    let dir = std::env::temp_dir().join(format!("c1700-snap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let snap = dir.join("mid-boot.snap");

    // Snapshot right after the banner: mid-boot, before memory sizing and
    // the prompt (the accelerated ROMMON boot is only ~100k steps).
    let mut original = new_machine(&cfg, 0x2100);
    let mut early_output = Vec::new();
    while !String::from_utf8_lossy(&early_output).contains("System Bootstrap") {
        original.run(1_000, false);
        early_output.extend(original.take_console_output());
    }
    assert!(!String::from_utf8_lossy(&early_output).contains("rommon"), "snapshot taken before the prompt");
    original.save_snapshot(&snap).unwrap();

    // Boot on to the prompt, type a command, let it answer.
    let finish = |m: &mut c1700emu::machine::Machine| {
        let mut out = Vec::new();
        for i in 0..80 {
            if i == 60 {
                type_line(m, "confreg");
            }
            m.run(1_000_000, false);
            out.extend(m.take_console_output());
        }
        (String::from_utf8_lossy(&out).into_owned(), m.cpu.cycles, m.cpu.tb, m.cpu.gpr, m.cpu.pc, m.board.dram[0].clone())
    };
    let a = finish(&mut original);

    let mut restored = new_machine(&cfg, 0x2100);
    restored.load_snapshot(&snap).unwrap();
    let b = finish(&mut restored);
    assert!(!early_output.is_empty(), "the snapshot was taken after some output");
    assert!(a.0.contains("rommon 1 >") && a.0.contains("Configuration Summary"), "{}", a.0);
    assert_eq!(a.0, b.0, "console output after the restore");
    assert_eq!((a.1, a.2, a.3, a.4), (b.1, b.2, b.3, b.4), "virtual time and registers");
    assert!(a.5 == b.5, "RAM contents");

    // Snapshots of different configurations are refused.
    let other = BoardConfig { dram_banks: [32 << 20, 0, 0, 0], net: None, ..BoardConfig::default() };
    let err = new_machine(&other, 0x2100).load_snapshot(&snap).unwrap_err();
    assert!(err.contains("RAM"), "{err}");

    // Core dump: ELF32 big-endian PowerPC core with the registers' note.
    let core = dir.join("core.elf");
    let segments = restored.write_core(&core).unwrap();
    let bytes = std::fs::read(&core).unwrap();
    assert_eq!(&bytes[0..6], &[0x7F, b'E', b'L', b'F', 1, 2]);
    assert_eq!(u16::from_be_bytes([bytes[16], bytes[17]]), 4, "ET_CORE");
    assert_eq!(u16::from_be_bytes([bytes[18], bytes[19]]), 20, "EM_PPC");
    assert_eq!(u16::from_be_bytes([bytes[44], bytes[45]]) as usize, segments + 1, "PT_NOTE + segments");
    assert!(bytes.len() > 16 << 20, "the RAM is in the dump");
    std::fs::remove_dir_all(&dir).ok();
}
