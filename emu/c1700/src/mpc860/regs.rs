//! Offsets of the IMMR registers the emulator gives behaviour to
//! (offset from the IMMR base, MPC860UM chapter 2 "Memory Map").
//! The full list with names is in IDA's `cfg/ppc.cfg`, section `.mpc860`.

// --- SIU ---
pub const SIUMCR: u32 = 0x000;
pub const SYPCR: u32 = 0x004;
pub const SWSR: u32 = 0x00E;
pub const SIPEND: u32 = 0x010;
pub const SIMASK: u32 = 0x014;
pub const SIEL: u32 = 0x018;
pub const SIVEC: u32 = 0x01C;

// --- memory controller ---
pub const BR0: u32 = 0x100;
pub const OR0: u32 = 0x104;
pub const OR7: u32 = 0x13C;
pub const MAR: u32 = 0x164;
pub const MCR: u32 = 0x168;
pub const MAMR: u32 = 0x170;
pub const MBMR: u32 = 0x174;
pub const MDR: u32 = 0x17C;

// --- system integration timers ---
pub const TBSCR: u32 = 0x200;
pub const RTCSC: u32 = 0x220;
pub const RTC: u32 = 0x224;
pub const PISCR: u32 = 0x240;
pub const PITC: u32 = 0x244;
pub const PITR: u32 = 0x248;

// --- clocks and reset ---
pub const SCCR: u32 = 0x280;
pub const PLPRCR: u32 = 0x284;
pub const RSR: u32 = 0x288;

// --- CPM ---
pub const CIVR: u32 = 0x930;
pub const CICR: u32 = 0x940;
pub const CIPR: u32 = 0x944;
pub const CIMR: u32 = 0x948;
pub const CISR: u32 = 0x94C;
pub const CPCR: u32 = 0x9C0;
pub const SPMODE: u32 = 0xAA0;
pub const SPIE: u32 = 0xAA6;
pub const SPIM: u32 = 0xAAA;
pub const SPCOM: u32 = 0xAAD;
/// SPI parameter RAM: default location and the relocation pointer (SPI_BASE).
pub const SPI_PARAM_DEFAULT: u32 = 0x3D80;
pub const SPI_BASE_PTR: u32 = 0x3DAC;
/// Start of the dual-port RAM inside the IMMR window (BD offsets are relative to it).
pub const DPRAM: u32 = 0x2000;

// --- Fast Ethernet Controller (MPC860T only) ---
pub const FEC_BASE: u32 = 0xE00;
pub const FEC_END: u32 = 0x1000;
