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
//! c1700emu --rom builtin --model 1760 --ram 128 --flash-file roms/c1700-adventerprisek9-mz.124-8.bin
//!                                        # the same without Cisco's boot ROM (docs ch. 24)
//! c1700emu --trace --log trace.txt -n 2000000   # instruction trace of the first 2M instructions
//! ```

use c1700emu::board::wic::WicKind;
use c1700emu::board::{BoardConfig, cookie, flashfs, nvram::Nvram, nvram_config};
use c1700emu::console::Console;
use c1700emu::emulator::Emulator;
use c1700emu::fakerom;
use c1700emu::gdb;
use c1700emu::machine::{CpuEngine, Machine};
use c1700emu::net::host::HostLink;
use c1700emu::net::{Link, NetConfig};
use c1700emu::wakeup::Wakeup;
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(version, about = "Emulator for the Cisco C1700 (MPC860T) System Bootstrap ROM")]
// A later occurrence of an option replaces an earlier one: the options from
// --config come first, so the command line wins.
#[command(args_override_self = true)]
struct Args {
    /// Read options from a TOML file: `option = value` for each command-line
    /// option, without the leading dashes (e.g. `model = "1760"`, `ram = 128`,
    /// `cpu = "jit"`, `stats = true`, `flash-file = ["ios.bin"]`). Options
    /// given on the command line override the file. See docs ch. 7.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Boot ROM image (512 KB AT27C040 dump), or `builtin`: the emulator's
    /// own minimal boot ROM, which boots the first image in flash without
    /// a ROM monitor (docs ch. 24).
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

    /// Ethernet MAC address written into the cookie, e.g. 02:17:00:00:00:02
    /// (default 02:17:00:00:00:01; give each router on a link its own).
    #[arg(long, value_parser = parse_mac)]
    mac: Option<[u8; 6]>,

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

    /// WAN interface card in WIC slot 0 (docs ch. 23): wic-1t (one serial
    /// port) or wic-2t (two). IOS numbers the ports Serial0/0, 0/1... in
    /// slot order.
    #[arg(long, value_enum)]
    wic0: Option<WicKind>,

    /// WAN interface card in WIC slot 1.
    #[arg(long, value_enum)]
    wic1: Option<WicKind>,

    /// Cable the first serial port of the card in slot 0 to another
    /// emulator: udp:LPORT:RHOST:RPORT, one HDLC frame per datagram, with a
    /// V.35 DTE cable, or a DCE cable with ",dce" appended (docs ch. 23).
    /// Without it the port has no cable.
    #[arg(long, value_parser = parse_serial)]
    serial0: Option<SerialSpec>,

    /// The same for the second port of a wic-2t in slot 0.
    #[arg(long = "serial0-1", value_parser = parse_serial)]
    serial0_1: Option<SerialSpec>,

    /// The same for the first port of the card in slot 1.
    #[arg(long, value_parser = parse_serial)]
    serial1: Option<SerialSpec>,

    /// The same for the second port of a wic-2t in slot 1.
    #[arg(long = "serial1-1", value_parser = parse_serial)]
    serial1_1: Option<SerialSpec>,

    /// Write this configuration register value into NVRAM before booting
    /// (0x8000 = diagnostic messages, 0x2102 = typical IOS default).
    #[arg(long, value_parser = parse_u16)]
    confreg: Option<u16>,

    /// Set a ROMMON variable in NVRAM before booting, NAME=VALUE (an empty
    /// value removes it); repeat for more. E.g. the tftpdnld variables
    /// IP_ADDRESS, TFTP_SERVER, TFTP_FILE... (docs ch. 7.7)
    #[arg(long = "rommon-var", value_name = "NAME=VALUE")]
    rommon_vars: Vec<String>,

    /// Install this text file as IOS's startup-config in NVRAM before
    /// booting (what `write memory` stores). Replaces the saved one.
    #[arg(long, value_name = "FILE")]
    startup_config: Option<PathBuf>,

    /// Print what the NVRAM file (--nvram) holds: configuration register,
    /// ROMMON variables, IOS startup-config. Then exit; nothing is changed.
    #[arg(long)]
    nvram_info: bool,

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

    /// Leave the Ethernet port unplugged (same as `--net none`).
    #[arg(long)]
    no_net: bool,

    /// Restore the machine from this snapshot before running (taken with
    /// the same --model, --ram and boot ROM). See docs ch. 20.
    #[arg(long)]
    restore: Option<PathBuf>,

    /// Where `Ctrl-] s` saves a snapshot.
    #[arg(long, default_value = "c1700.snap")]
    snapshot: PathBuf,

    /// Where `Ctrl-] d` writes an ELF core dump (memory and registers, for
    /// GDB: `gdb -ex "core-file FILE"`).
    #[arg(long, default_value = "c1700-core.elf")]
    core: PathBuf,

    /// What the Ethernet port is plugged into: `virtual` (the built-in host
    /// 10.0.2.2 with ping and TFTP; default), `none`, `tap:NAME` (an
    /// existing Linux TAP device, e.g. bridged to the LAN) or
    /// `udp:LPORT:RHOST:RPORT` (each frame one UDP datagram, as Dynamips and
    /// GNS3 do). See docs ch. 18.
    #[arg(long, default_value = "virtual", value_parser = parse_net)]
    net: NetChoice,

    /// Also serve the console over telnet on this TCP port.
    #[arg(long)]
    tcp: Option<u16>,

    /// Idle detection anchor: `auto` (known images, code-checked) or the
    /// hex address of IOS's scheduler loop head. See docs ch. 15.
    #[arg(long, default_value = "auto")]
    idle_pc: c1700emu::idle::AnchorChoice,

    /// Idle detection: realtime (skip idle time, sleep so the router keeps
    /// the wall clock), warp (skip idle time freely) or off.
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

    /// CPU engine: `interp` (the interpreter, the reference) or `jit` (hot
    /// code translated to host machine code with Cranelift). See docs ch. 17.
    #[arg(long, value_enum, default_value = "interp")]
    cpu: CpuEngine,

    /// JIT: how many times a block runs interpreted before it is translated.
    #[arg(long, default_value_t = ppc8xx::jit::DEFAULT_THRESHOLD)]
    jit_threshold: u32,

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
    // Wall-clock seconds since start (for timing questions such as network
    // latency), level, target. "\r\n" so log lines stay readable while the
    // terminal is in raw mode.
    let start = std::time::Instant::now();
    builder.format(move |buf, rec| {
        write!(buf, "[{:10.6} {:5} {}] {}\r\n", start.elapsed().as_secs_f64(), rec.level(), rec.target(), rec.args())
    });
    if let Some(path) = &args.log {
        let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        builder.target(env_logger::Target::Pipe(Box::new(std::io::BufWriter::new(file))));
    }
    builder.init();
    Ok(())
}

/// The command line with the options of its `--config FILE` (if any) put
/// in front of it, so that the command line's own options win.
fn with_config_file(argv: Vec<String>) -> Result<Vec<String>, String> {
    let mut path = None;
    for (i, arg) in argv.iter().enumerate() {
        if let Some(p) = arg.strip_prefix("--config=") {
            path = Some(p.to_string());
        } else if arg == "--config" {
            path = argv.get(i + 1).cloned();
        }
    }
    let Some(path) = path else { return Ok(argv) };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("--config {path}: {e}"))?;
    let options = config_to_args(&text).map_err(|e| format!("--config {path}: {e}"))?;
    Ok(argv[..1].iter().cloned().chain(options).chain(argv[1..].iter().cloned()).collect())
}

/// Turn a TOML configuration into command-line options: `key = value` gives
/// `--key=value`, `key = true` gives `--key` (false: nothing), and an array
/// repeats the option (`flash-file = ["a.bin", "b.bin"]`). Unknown keys are
/// reported by the option parser like unknown options.
fn config_to_args(text: &str) -> Result<Vec<String>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut args = Vec::new();
    for (key, value) in table {
        let key = key.replace('_', "-");
        let one = |v: &toml::Value| -> Result<String, String> {
            match v {
                toml::Value::String(s) => Ok(s.clone()),
                toml::Value::Integer(n) => Ok(n.to_string()),
                toml::Value::Float(f) => Ok(f.to_string()),
                other => Err(format!("{key}: unsupported value {other}")),
            }
        };
        match &value {
            toml::Value::Boolean(true) => args.push(format!("--{key}")),
            toml::Value::Boolean(false) => {}
            toml::Value::Array(items) => {
                for item in items {
                    args.push(format!("--{key}={}", one(item)?));
                }
            }
            v => args.push(format!("--{key}={}", one(v)?)),
        }
    }
    Ok(args)
}

fn main() -> Result<(), String> {
    let args = Args::parse_from(with_config_file(std::env::args().collect())?);
    setup_logging(&args)?;
    if args.nvram_info {
        let bytes = std::fs::read(&args.nvram).map_err(|e| format!("{}: {e}", args.nvram.display()))?;
        print!("{}", nvram_config::describe(&bytes));
        return Ok(());
    }

    let builtin_rom = args.rom.as_os_str() == "builtin";
    let rom = if builtin_rom {
        fakerom::image()
    } else {
        let rom_path = find_file(&args.rom);
        std::fs::read(&rom_path).map_err(|e| format!("cannot read ROM {}: {e}", rom_path.display()))?
    };

    let net = (args.net == NetChoice::Virtual && !args.no_net).then(|| NetConfig {
        server_ip: args.net_server_ip,
        gateway_ip: args.net_gateway,
        prefix_len: args.net_prefix,
        tftp_root: args.tftp_root.clone(),
    });
    let pm_id = cookie::pm_id_for_model(&args.model)
        .ok_or_else(|| format!("unknown model {:?}: use one of 1720, 1721, 1750, 1751, 1760", args.model))?;
    let flash_mb = args.flash_size.unwrap_or(if matches!(args.model.as_str(), "1751" | "1760") { 32 } else { 16 });
    let wics = [args.wic0, args.wic1, None, None];
    let cfg = BoardConfig { dram_banks: dram_banks(args.ram)?, flash_mb, board_id: args.board_id, net, wics };
    let mut nvram = Nvram::new(32 * 1024, Some(args.nvram.clone())).map_err(|e| e.to_string())?;
    if cookie::ensure_in_nvram(nvram.bytes_mut(), &cookie::Cookie::default()) {
        log::info!("NVRAM had no valid cookie: wrote a default one (MAC 02:17:00:00:00:01)");
    }
    cookie::set_pm_id(nvram.bytes_mut(), pm_id);
    if let Some(serial) = &args.serial {
        cookie::set_serial(nvram.bytes_mut(), serial)?;
    }
    if let Some(mac) = args.mac {
        cookie::set_mac(nvram.bytes_mut(), mac);
    }
    if let Some(cr) = args.confreg {
        nvram_config::set_confreg(nvram.bytes_mut(), cr);
    }
    if !args.rommon_vars.is_empty() {
        let vars: Vec<(String, String)> = args
            .rommon_vars
            .iter()
            .map(|a| a.split_once('=').map(|(n, v)| (n.to_string(), v.to_string())).ok_or(format!("--rommon-var {a}: expected NAME=VALUE")))
            .collect::<Result<_, _>>()?;
        nvram_config::set_rommon_vars(nvram.bytes_mut(), &vars)?;
    }
    if let Some(path) = &args.startup_config {
        let text = std::fs::read_to_string(path).map_err(|e| format!("--startup-config {}: {e}", path.display()))?;
        nvram_config::set_startup_config(nvram.bytes_mut(), &text)?;
    }


    let mut machine = Machine::new(rom, &cfg, nvram);
    if builtin_rom {
        machine.use_builtin_rom();
    }
    // Input threads (network, console) wake the emulation loop from idle sleeps.
    let wakeup = Wakeup::default();
    let host_link = match &args.net {
        _ if args.no_net => None,
        NetChoice::Tap(name) => Some(HostLink::tap(name, wakeup.clone())),
        NetChoice::Udp(port, remote) => Some(HostLink::udp(*port, *remote, wakeup.clone())),
        NetChoice::Virtual | NetChoice::None => None,
    };
    if let Some(link) = host_link {
        let link = link.map_err(|e| e.to_string())?;
        log::info!("Ethernet port connected to {}", link.what);
        machine.board.net = Some(Link::Host(link));
    }
    let serials = [(0, 0, args.serial0), (0, 1, args.serial0_1), (1, 0, args.serial1), (1, 1, args.serial1_1)];
    for (slot, port, spec) in serials {
        if let Some(SerialSpec { local, remote, dce }) = spec {
            let link = HostLink::udp(local, remote, wakeup.clone()).map_err(|e| e.to_string())?;
            log::info!("serial port {port} of WIC slot {slot} connected to {}{}", link.what, if dce { " (DCE)" } else { "" });
            machine.board.connect_serial(slot, port, link, dce)?;
        }
    }
    machine.set_engine(args.cpu, args.jit_threshold);
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
    if let Some(path) = &args.restore {
        let start = std::time::Instant::now();
        machine.load_snapshot(path)?;
        log::info!("restored {} in {:.2} s: pc={:#010x}, {} instructions", path.display(), start.elapsed().as_secs_f64(), machine.cpu.pc, machine.cpu.cycles);
    }
    let console = Console::new(!args.no_stdio, args.tcp, wakeup.clone()).map_err(|e| format!("console: {e}"))?;
    let aux = args.aux_tcp.map(|port| Console::aux(port, wakeup.clone())).transpose().map_err(|e| format!("AUX port: {e}"))?;
    let mut emu = Emulator::new(machine, console, aux);
    emu.wakeup = wakeup;
    emu.snapshot_path = args.snapshot.clone();
    emu.core_path = args.core.clone();

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

fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let bytes: Vec<u8> = s.split([':', '-']).map(|b| u8::from_str_radix(b, 16)).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    bytes.try_into().map_err(|_| "expected six hex bytes, e.g. 02:17:00:00:00:02".to_string())
}

/// `--net`.
#[derive(Clone, Debug, PartialEq)]
enum NetChoice {
    Virtual,
    None,
    Tap(String),
    Udp(u16, std::net::SocketAddr),
}

fn parse_net(s: &str) -> Result<NetChoice, String> {
    use std::net::ToSocketAddrs;
    match s {
        "virtual" => Ok(NetChoice::Virtual),
        "none" => Ok(NetChoice::None),
        _ => {
            if let Some(name) = s.strip_prefix("tap:").filter(|n| !n.is_empty()) {
                return Ok(NetChoice::Tap(name.to_string()));
            }
            let spec = s.strip_prefix("udp:").ok_or("expected virtual, none, tap:NAME or udp:LPORT:RHOST:RPORT")?;
            let (local, remote) = spec.split_once(':').ok_or("expected udp:LPORT:RHOST:RPORT")?;
            let local = local.parse().map_err(|e| format!("bad local port {local:?}: {e}"))?;
            let remote = remote
                .to_socket_addrs()
                .map_err(|e| format!("bad remote {remote:?}: {e}"))?
                .next()
                .ok_or_else(|| format!("{remote:?} does not resolve"))?;
            Ok(NetChoice::Udp(local, remote))
        }
    }
}

/// Where a serial port is cabled: a UDP tunnel, DTE or DCE end.
#[derive(Clone, Copy, Debug)]
struct SerialSpec {
    local: u16,
    remote: std::net::SocketAddr,
    dce: bool,
}

fn parse_serial(s: &str) -> Result<SerialSpec, String> {
    let (link, dce) = match s.strip_suffix(",dce") {
        Some(link) => (link, true),
        None => (s, false),
    };
    match parse_net(link)? {
        NetChoice::Udp(local, remote) => Ok(SerialSpec { local, remote, dce }),
        _ => Err("expected udp:LPORT:RHOST:RPORT[,dce]".into()),
    }
}

fn parse_watch(s: &str) -> Result<(u32, u32), String> {
    let (a, l) = s.split_once(':').ok_or("expected START:LEN (hex)")?;
    let hex = |x: &str| u32::from_str_radix(x.trim_start_matches("0x"), 16).map_err(|e| e.to_string());
    Ok((hex(a)?, hex(l)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_becomes_options() {
        let args = config_to_args(
            r#"
            # a 1760 booting IOS
            model = "1760"
            ram = 128
            cpu = "jit"
            stats = true
            trace = false
            flash-file = ["a.bin", "b.bin"]
            jit_threshold = 10
            "#,
        )
        .unwrap();
        // (Keys come out sorted; only the order inside an array matters.)
        assert_eq!(
            args,
            ["--cpu=jit", "--flash-file=a.bin", "--flash-file=b.bin", "--jit-threshold=10", "--model=1760", "--ram=128", "--stats"]
        );
        assert!(config_to_args("model = { a = 1 }").is_err(), "tables are not options");
        assert!(config_to_args("model = ").is_err(), "TOML syntax errors are reported");
    }

    /// The command line overrides the file; unknown keys are rejected.
    #[test]
    fn command_line_overrides_the_config_file() {
        let file = std::env::temp_dir().join(format!("c1700emu-test-{}.toml", std::process::id()));
        std::fs::write(&file, "model = \"1760\"\nram = 128\n").unwrap();
        let argv = |extra: &[&str]| {
            let mut v = vec!["c1700emu".to_string(), format!("--config={}", file.display())];
            v.extend(extra.iter().map(|s| s.to_string()));
            with_config_file(v).unwrap()
        };
        let a = Args::try_parse_from(argv(&[])).unwrap();
        assert_eq!((a.model.as_str(), a.ram), ("1760", 128));
        let a = Args::try_parse_from(argv(&["--ram", "64"])).unwrap();
        assert_eq!((a.model.as_str(), a.ram), ("1760", 64));
        std::fs::write(&file, "no-such-option = 1\n").unwrap();
        assert!(Args::try_parse_from(argv(&[])).is_err());
        std::fs::remove_file(&file).unwrap();
    }
}
