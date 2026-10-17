//! `c1700emu` — run the Cisco C1700 System Bootstrap ROM.
//!
//! Examples:
//!
//! ```text
//! c1700emu                              # boot, console on this terminal
//! c1700emu --confreg 0x8000             # verbose diagnostic boot messages
//! c1700emu --tcp 2323                   # also: telnet localhost 2323
//! c1700emu --gdb 1234                   # wait for GDB before the first instruction
//! c1700emu --model 1760 --ram 128 --flash-file roms/c1700-adventerprisek9-mz.124-8.bin --confreg 0x2102
//!                                        # IOS in the internal flash, ROMMON boots it
//! c1700emu --trace --log trace.txt -n 2000000   # instruction trace of the first 2M instructions
//! ```

use c1700emu::board::{BoardConfig, cookie, flashfs, nvram::Nvram};
use c1700emu::console::Console;
use c1700emu::emulator::Emulator;
use c1700emu::gdb;
use c1700emu::machine::Machine;
use c1700emu::net::NetConfig;
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(version, about = "Emulator for the Cisco C1700 (MPC860T) System Bootstrap ROM")]
struct Args {
    /// Boot ROM image (512 KB AT27C040 dump).
    #[arg(long, default_value = "roms/CISCO_bootrom_AT27C040.bin")]
    rom: PathBuf,

    /// NVRAM image file (created if missing, saved on exit).
    #[arg(long, default_value = "c1700-nvram.bin")]
    nvram: PathBuf,

    /// Flash image file. Loaded at start if it exists; whatever the guest
    /// programs or erases is saved back to it.
    #[arg(long, default_value = "c1700-flash.bin")]
    flash: PathBuf,

    /// Chassis model: 1720, 1721, 1750, 1751 or 1760 (sets the cookie's
    /// mainboard PM ID; voice images need 1750/1751/1760).
    #[arg(long, default_value = "1720")]
    model: String,

    /// Processor board serial (`show version`: "Processor board ID"), Cisco
    /// format LLLYYWWSSSS, e.g. FTX0945W0MY. Default for a new NVRAM: EMU1234C170.
    #[arg(long)]
    serial: Option<String>,

    /// Internal flash size in MB: 4 (2 x AMD Am29F016), 8, 16 or 32 (Intel
    /// StrataFlash x16). Default: 32 for 1751/1760, 16 otherwise.
    #[arg(long)]
    flash_size: Option<u32>,

    /// Put this file (e.g. an IOS image) into the flash file system; repeat
    /// for more files. Replaces the flash content (which is then saved to
    /// --flash). With confreg 0x2102 ROMMON boots the first file.
    #[arg(long = "flash-file")]
    flash_files: Vec<PathBuf>,

    /// Main memory in MB (split into power-of-two SDRAM banks, max 4 x 64 MB).
    #[arg(long, default_value_t = 32)]
    ram: u32,

    /// Board type strapped into CPLD register 4 (bits 0-2).
    #[arg(long, default_value_t = 1)]
    board_id: u8,

    /// Write this configuration register value into NVRAM before booting
    /// (0x8000 = diagnostic messages, 0x2102 = typical IOS default).
    #[arg(long, value_parser = parse_u16)]
    confreg: Option<u16>,

    /// Serve files from this directory over TFTP on the virtual network
    /// (for ROMMON's `tftpdnld`; set TFTP_SERVER to --net-server-ip).
    #[arg(long)]
    tftp_root: Option<PathBuf>,

    /// IP address of the virtual host / TFTP server.
    #[arg(long, default_value = "10.0.2.2")]
    net_server_ip: std::net::Ipv4Addr,

    /// Extra IP address answered by the virtual host, for DEFAULT_GATEWAY.
    #[arg(long, default_value = "10.0.2.1")]
    net_gateway: std::net::Ipv4Addr,

    /// Prefix length of the virtual network (24 = 255.255.255.0).
    #[arg(long, default_value_t = 24)]
    net_prefix: u8,

    /// Leave the Ethernet port unplugged (no virtual network).
    #[arg(long)]
    no_net: bool,

    /// Also serve the console over telnet on this TCP port.
    #[arg(long)]
    tcp: Option<u16>,

    /// Idle detection anchor: `auto` (known images, code-checked) or the
    /// hex address of IOS's scheduler loop head. See docs ch. 15.
    #[arg(long, default_value = "auto")]
    idle_pc: c1700emu::idle::AnchorChoice,

    /// Idle detection: realtime (skip idle time, sleep so the router keeps
    /// the wall clock) or off.
    #[arg(long, value_enum, default_value = "realtime")]
    idle: c1700emu::idle::IdleMode,

    /// Log every access to a physical address range, START:LEN in hex
    /// (e.g. 07040000:4000), with the PC of the instruction (or DMA).
    #[arg(long, value_parser = parse_watch)]
    watch: Option<(u32, u32)>,

    /// Serve the AUX port (DUART channel B) over telnet on this TCP port
    /// (without it, AUX output goes to the log).
    #[arg(long)]
    aux_tcp: Option<u16>,

    /// Don't attach the console to this terminal (use with --tcp).
    #[arg(long)]
    no_stdio: bool,

    /// Wait for a GDB connection on this port before executing anything.
    #[arg(long)]
    gdb: Option<u16>,

    /// Stop after this many instructions (useful for scripted tests).
    #[arg(short = 'n', long)]
    max_instructions: Option<u64>,

    /// Log every executed instruction (very slow; use with --log).
    #[arg(long)]
    trace: bool,

    /// Execute the ROM's delay loops instruction by instruction instead of
    /// fast-forwarding the virtual clock through them.
    #[arg(long)]
    no_fast_forward: bool,

    /// Print what the guest used at exit: exceptions, on-chip registers, unknown devices.
    #[arg(long)]
    stats: bool,

    /// Count executed instructions per ROM address and print the hot spots at exit.
    #[arg(long)]
    profile: bool,

    /// With --profile: also write every sampled PC (whole run and window) here.
    #[arg(long)]
    profile_dump: Option<PathBuf>,

    /// Write logs to this file instead of stderr.
    #[arg(long)]
    log: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace (RUST_LOG overrides).
    #[arg(long, default_value = "warn")]
    log_level: String,
}

fn parse_u16(s: &str) -> Result<u16, String> {
    let t = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(t, 16).map_err(|e| format!("expected a hex value like 0x2102: {e}"))
}

/// Split `mb` megabytes into up to four power-of-two banks, largest first,
/// like the ROM's own sizing code lays them out.
fn dram_banks(mb: u32) -> Result<[u32; 4], String> {
    let mut banks = [0u32; 4];
    let mut left = mb;
    for bank in banks.iter_mut() {
        if left == 0 {
            break;
        }
        let size = (1u32 << (31 - left.leading_zeros())).min(64);
        if size < 2 {
            return Err(format!("{mb} MB cannot be built from 2..64 MB banks"));
        }
        *bank = size << 20;
        left -= size;
    }
    if left != 0 {
        return Err(format!("{mb} MB needs more than 4 banks"));
    }
    Ok(banks)
}

fn find_file(p: &Path) -> PathBuf {
    if p.exists() || p.is_absolute() {
        return p.to_path_buf();
    }
    // convenience: running from emu/ or emu/c1700/ finds the ROM in the project root
    for up in ["..", "../.."] {
        let cand = Path::new(up).join(p);
        if cand.exists() {
            return cand;
        }
    }
    p.to_path_buf()
}

fn setup_logging(args: &Args) -> Result<(), String> {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(&args.log_level);
    if let Ok(env) = std::env::var("RUST_LOG") {
        builder.parse_filters(&env);
    }
    if args.trace {
        builder.filter_module("insn", log::LevelFilter::Trace);
    }
    // "\r\n" so log lines stay readable while the terminal is in raw mode.
    builder.format(|buf, rec| write!(buf, "[{:5} {}] {}\r\n", rec.level(), rec.target(), rec.args()));
    if let Some(path) = &args.log {
        let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        builder.target(env_logger::Target::Pipe(Box::new(std::io::BufWriter::new(file))));
    }
    builder.init();
    Ok(())
}

fn write_confreg(nvram: &mut Nvram, value: u16) {
    let bytes = nvram.bytes_mut();
    bytes[0x90..0x92].copy_from_slice(&value.to_be_bytes());
    bytes[0x92..0x94].copy_from_slice(&(!value).to_be_bytes());
    bytes[0x94..0x98].copy_from_slice(&0xFEED_FACEu32.to_be_bytes());
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    setup_logging(&args)?;

    let rom_path = find_file(&args.rom);
    let rom = std::fs::read(&rom_path).map_err(|e| format!("cannot read ROM {}: {e}", rom_path.display()))?;

    let net = (!args.no_net).then(|| NetConfig {
        server_ip: args.net_server_ip,
        gateway_ip: args.net_gateway,
        prefix_len: args.net_prefix,
        tftp_root: args.tftp_root.clone(),
    });
    let pm_id = cookie::pm_id_for_model(&args.model)
        .ok_or_else(|| format!("unknown model {:?}: use one of 1720, 1721, 1750, 1751, 1760", args.model))?;
    let flash_mb = args.flash_size.unwrap_or(if matches!(args.model.as_str(), "1751" | "1760") { 32 } else { 16 });
    let cfg = BoardConfig { dram_banks: dram_banks(args.ram)?, flash_mb, board_id: args.board_id, net };
    let mut nvram = Nvram::new(32 * 1024, Some(args.nvram.clone())).map_err(|e| e.to_string())?;
    if cookie::ensure_in_nvram(nvram.bytes_mut(), &cookie::Cookie::default()) {
        log::info!("NVRAM had no valid cookie: wrote a default one (MAC 02:17:00:00:00:01)");
    }
    cookie::set_pm_id(nvram.bytes_mut(), pm_id);
    if let Some(serial) = &args.serial {
        cookie::set_serial(nvram.bytes_mut(), serial)?;
    }
    if let Some(cr) = args.confreg {
        write_confreg(&mut nvram, cr);
    }

    let mut machine = Machine::new(rom, &cfg, nvram);
    machine.trace = args.trace;
    if args.no_fast_forward {
        machine.fast_forward = c1700emu::fastforward::FastForward::disabled();
    }
    if args.profile {
        machine.enable_profile();
    }
    if args.stats {
        machine.board.mpc.stats = Some(Box::default());
    }
    machine
        .board
        .flash
        .attach_file(args.flash.clone())
        .map_err(|e| format!("cannot read flash image {}: {e}", args.flash.display()))?;
    if !args.flash_files.is_empty() {
        let mut files = Vec::new();
        for p in &args.flash_files {
            let data = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("image").to_string();
            files.push((name, data));
        }
        let image = flashfs::build(&files, machine.board.flash.size() as usize)?;
        machine.board.flash.load(&image);
        machine.board.flash.mark_dirty();
    }
    let listing: Vec<String> = flashfs::list(machine.board.flash.bytes())
        .iter()
        .map(|e| format!("{} ({} bytes{})", e.name, e.length, if e.checksum_ok { "" } else { ", BAD CHECKSUM" }))
        .collect();
    log::info!(
        "model {} (PM ID {pm_id:#06x}), flash {}: {}",
        args.model,
        machine.board.flash.describe(),
        if listing.is_empty() { "empty".to_string() } else { listing.join(", ") }
    );

    machine.idle_auto = args.idle_pc == c1700emu::idle::AnchorChoice::Auto;
    machine.idle = c1700emu::idle::Idle::new(args.idle, Some(args.idle_pc.clone()));
    if let Some((start, len)) = args.watch {
        machine.board.watch = Some(c1700emu::board::Watch { start, end: start.wrapping_add(len), pc: u32::MAX });
    }
    let console = Console::new(!args.no_stdio, args.tcp).map_err(|e| format!("console: {e}"))?;
    let aux = args.aux_tcp.map(Console::aux).transpose().map_err(|e| format!("AUX port: {e}"))?;
    let mut emu = Emulator::new(machine, console, aux);

    if let Some(port) = args.gdb {
        emu = gdb::serve(emu, port)?;
    }
    if !emu.quit {
        let stop = emu.run_interactive(args.max_instructions);
        log::info!("stopped: {stop:?} after {} instructions", emu.machine.cpu.cycles);
    }
    emu.pump_console();
    if let Some(path) = &args.profile_dump {
        if let Err(e) = std::fs::write(path, emu.machine.profile_dump()) {
            log::error!("profile dump {}: {e}", path.display());
        }
    }
    let mut report = emu.machine.profile_report();
    if args.stats {
        report += &emu.machine.stats_report();
    }
    drop(emu);
    eprint!("{report}"); // restores the terminal and saves NVRAM
    eprintln!();
    Ok(())
}

fn parse_watch(s: &str) -> Result<(u32, u32), String> {
    let (a, l) = s.split_once(':').ok_or("expected START:LEN (hex)")?;
    let hex = |x: &str| u32::from_str_radix(x.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    Ok((hex(a)?, hex(l)?))
}
