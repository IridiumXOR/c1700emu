//! End-to-end network + flash test:
//!   1. `tftpdnld -r`: download a test image over the virtual network and run it from RAM;
//!   2. `tftpdnld`: write it to flash;
//!   3. new machine with the same flash content and confreg 0x2102: autoboots it.
//!
//! The test image is built by `tools/mkimage.py` (needs python3 + llvm-mc);
//! the test is skipped if that is not possible.

mod common;
use c1700emu::board::BoardConfig;
use c1700emu::net::NetConfig;
use common::*;
use std::path::PathBuf;
use std::process::Command;

const MESSAGE: &str = "Hello from the c1700emu test suite";

fn build_test_image() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("c1700emu-tftp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/mkimage.py");
    let ok = Command::new("python3")
        .args([script, dir.join("hello.bin").to_str()?, MESSAGE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: could not build the test image (python3 + llvm-mc needed)");
        return None;
    }
    Some(dir)
}

fn set_network_variables(m: &mut c1700emu::machine::Machine, first_prompt: u32) {
    let vars = [
        "IP_ADDRESS=10.0.2.15",
        "IP_SUBNET_MASK=255.255.255.0",
        "DEFAULT_GATEWAY=10.0.2.1",
        "TFTP_SERVER=10.0.2.2",
        "TFTP_FILE=hello.bin",
    ];
    for (i, v) in vars.iter().enumerate() {
        type_line(m, v);
        expect(m, &format!("rommon {} >", first_prompt + 1 + i as u32), 50_000_000);
    }
}

#[test]
fn tftp_boot_flash_write_and_autoboot_from_flash() {
    if !rom_available() {
        return;
    }
    let Some(tftp_root) = build_test_image() else { return };
    let cfg = BoardConfig {
        net: Some(NetConfig { tftp_root: Some(tftp_root.clone()), ..NetConfig::default() }),
        ..BoardConfig::default()
    };

    // 1. download and run from RAM
    let mut m = new_machine(&cfg, 0x2100);
    expect(&mut m, "rommon 1 >", 500_000_000);
    set_network_variables(&mut m, 1);
    type_line(&mut m, "tftpdnld -r");
    let text = expect(&mut m, MESSAGE, 1_000_000_000);
    assert!(text.contains("File reception completed."), "{text}");
    assert!(text.contains("program load complete, entry point: 0x80008000"), "{text}");

    // 2. download into flash (fresh machine, the RAM-booted image is still spinning)
    let mut m = new_machine(&cfg, 0x2100);
    expect(&mut m, "rommon 1 >", 500_000_000);
    set_network_variables(&mut m, 1);
    type_line(&mut m, "tftpdnld");
    expect(&mut m, "[n]:", 100_000_000);
    type_line(&mut m, "y");
    let text = expect(&mut m, "rommon 7 >", 3_000_000_000);
    assert!(text.contains("Copying file hello.bin to flash."), "{text}");
    type_line(&mut m, "dir flash:");
    let text = expect(&mut m, "rommon 8 >", 50_000_000);
    assert!(text.contains("hello.bin"), "{text}");
    let flash = m.board.flash.bytes().to_vec();
    assert_eq!(&flash[0..4], &0xBAD0_0B1Eu32.to_be_bytes(), "Cisco flash file header magic");

    // 3. "power cycle" with the same flash content and autoboot
    let mut m = new_machine(&cfg, 0x2102);
    m.board.flash.load(&flash);
    expect(&mut m, MESSAGE, 1_000_000_000);

    let _ = std::fs::remove_dir_all(tftp_root);
}
