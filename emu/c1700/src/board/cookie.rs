//! The Cisco "serial cookie": board identity (MAC address, serial number,
//! processor type) stored at the start of NVRAM, with a backup copy at
//! NVRAM+0x65A.
//!
//! Block layout (132 bytes), from `cookie_is_valid` (0xFFF0B1D8) and the
//! ROMMON cookie editor's field table at 0xFFF2FCAC (name, length; the
//! offsets are cumulative), whose names include "Recommended value" hints:
//!
//! ```text
//! +0   u16  magic 0x8D74
//! +2   [128] data:
//!      0x00     version           (Dynamips' cookie: 01)
//!      0x01     vendor            (recommended 01 = cisco)
//!      0x02-07  ethernet MAC address
//!      0x08     processor         (recommended 33 = C1700; the ROM requires it)
//!      0x09     NVRAM size        (32K = 00, 64K = 01)
//!      0x0A     CPU speed         (50 MHz = 01)
//!      0x0C-0D  on-board PM ID    (recommended for a 1720: 00 B2)
//!      0x0E-0F  MAC addresses allocated
//!      0x10-17  (unnamed)
//!      0x18-22  (unnamed, 11 bytes) processor board serial, see below
//!      0x23-24  deviation
//!      0x2D     board config      (recommended 04)
//! +130 u16  checksum = 16-bit sum of the 65 halfwords at +0..+130
//! ```
//!
//! The serial ("Processor board ID" in `show version`) is a Cisco serial
//! `LLLYYWWSSSS` (plant, year, week, unit) packed in 9 bytes at 0x18: three
//! characters, YY and WW as one byte each printed with `%02x`, four
//! characters. That is how IOS 12.4(8) rebuilds it (0x8012377c), and
//! Dynamips' default cookie holds "FTX0945W0MY" exactly so.

pub const MAGIC: u16 = 0x8D74;
pub const BLOCK_LEN: usize = 132;
pub const PRIMARY_OFFSET: usize = 0x000;
pub const BACKUP_OFFSET: usize = 0x65A;
/// Processor code the ROM checks for (anything else prints
/// "WARNING: Unknown Processor type found in cookie").
pub const PROCESSOR_C1700: u8 = 0x33;

/// C1700 chassis models and their mainboard "PM ID" (cookie bytes 0x0C-0x0D).
/// Values verified twice: they are the cases of IOS 12.4(8)'s mainboard-name
/// switch (0x8013F598) and Dynamips' `c1700_mainboard_id` table.
/// Voice images (e.g. adventerprisek9) refuse to run on a 1720/1721:
/// "%C1700-3-BADIMAGE: ... The platform you are running is not voice capable."
pub const MODELS: &[(&str, u16)] = &[
    ("1720", 0x00B2),
    ("1721", 0x035A),
    ("1750", 0x00C9),
    ("1751", 0x024D),
    ("1760", 0x0316),
];

/// Data offset of the processor board serial.
const SERIAL: usize = 0x18;
/// Serial of a fresh cookie: plant "EMU" makes it obviously emulated.
pub const DEFAULT_SERIAL: &str = "EMU1234C170";

/// Pack a Cisco serial `LLLYYWWSSSS` into the 9 cookie bytes IOS expects:
/// 3 characters, YY and WW as two hex bytes, 4 characters.
pub fn encode_serial(serial: &str) -> Result<[u8; 9], String> {
    let b = serial.as_bytes();
    let bad = || format!("serial {serial:?} must look like LLLYYWWSSSS: 3 letters/digits, 4 hex digits, 4 letters/digits");
    if b.len() != 11 || !b.iter().all(u8::is_ascii_alphanumeric) {
        return Err(bad());
    }
    let hex = |s: &[u8]| u8::from_str_radix(std::str::from_utf8(s).unwrap(), 16).map_err(|_| bad());
    let mut out = [0u8; 9];
    out[0..3].copy_from_slice(&b[0..3]);
    out[3] = hex(&b[3..5])?;
    out[4] = hex(&b[5..7])?;
    out[5..9].copy_from_slice(&b[7..11]);
    Ok(out)
}

/// Apply `edit` to the data part of both cookie copies (keeping everything
/// else, recomputing the checksums). Returns false if there is no valid cookie.
fn patch(nvram: &mut [u8], edit: impl Fn(&mut [u8])) -> bool {
    let mut changed = false;
    for base in [PRIMARY_OFFSET, BACKUP_OFFSET] {
        let block = &mut nvram[base..base + BLOCK_LEN];
        if !is_valid(block) {
            continue;
        }
        edit(&mut block[2..130]);
        let sum = checksum16(&block[0..130]);
        block[130..132].copy_from_slice(&sum.to_be_bytes());
        changed = true;
    }
    changed
}

/// Set the processor board serial in both cookie copies.
pub fn set_serial(nvram: &mut [u8], serial: &str) -> Result<bool, String> {
    let bytes = encode_serial(serial)?;
    Ok(patch(nvram, |data| data[SERIAL..SERIAL + 9].copy_from_slice(&bytes)))
}

pub fn pm_id_for_model(model: &str) -> Option<u16> {
    MODELS.iter().find(|(m, _)| *m == model).map(|&(_, id)| id)
}

/// Set the mainboard PM ID in both cookie copies (keeping everything else,
/// recomputing the checksums). Returns false if there is no valid cookie.
pub fn set_pm_id(nvram: &mut [u8], pm_id: u16) -> bool {
    patch(nvram, |data| data[0x0C..0x0E].copy_from_slice(&pm_id.to_be_bytes()))
}

#[derive(Clone, Debug)]
pub struct Cookie {
    pub mac: [u8; 6],
    /// Cisco serial `LLLYYWWSSSS`, e.g. "FTX0945W0MY" (see `encode_serial`).
    pub serial: String,
}

impl Default for Cookie {
    fn default() -> Self {
        // Locally-administered MAC (bit 1 of the first byte) so it can never
        // clash with a real Cisco device on the network.
        Cookie { mac: [0x02, 0x17, 0x00, 0x00, 0x00, 0x01], serial: DEFAULT_SERIAL.to_string() }
    }
}

/// `checksum16` (0xFFF0BA6C): sum of big-endian halfwords, truncated.
pub fn checksum16(bytes: &[u8]) -> u16 {
    bytes.chunks(2).fold(0u16, |acc, h| acc.wrapping_add(u16::from_be_bytes([h[0], h[1]])))
}

impl Cookie {
    /// Build the 132-byte NVRAM block.
    pub fn to_block(&self) -> [u8; BLOCK_LEN] {
        let mut b = [0u8; BLOCK_LEN];
        b[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        let data = &mut b[2..130];
        data[0x00] = 1; // version
        data[0x01] = 1; // vendor: cisco
        data[0x02..0x08].copy_from_slice(&self.mac);
        data[0x08] = PROCESSOR_C1700;
        data[0x09] = 0x00; // NVRAM size: 32 KB
        data[0x0A] = 0x01; // CPU speed: 50 MHz
        data[0x0C..0x0E].copy_from_slice(&[0x00, 0xB2]); // on-board PM ID of a 1720
        data[0x0E..0x10].copy_from_slice(&[0x00, 0x01]); // one MAC address allocated
        data[0x2D] = 0x04; // board config
        let serial = encode_serial(&self.serial).expect("default serial is well-formed");
        data[SERIAL..SERIAL + 9].copy_from_slice(&serial);
        let sum = checksum16(&b[0..130]);
        b[130..132].copy_from_slice(&sum.to_be_bytes());
        b
    }
}

/// Does `block` pass the ROM's validity test?
pub fn is_valid(block: &[u8]) -> bool {
    block.len() >= BLOCK_LEN
        && u16::from_be_bytes([block[0], block[1]]) == MAGIC
        && checksum16(&block[0..130]) == u16::from_be_bytes([block[130], block[131]])
}

/// Make sure the NVRAM holds a valid cookie (both copies); returns true if
/// it had to write one.
pub fn ensure_in_nvram(nvram: &mut [u8], cookie: &Cookie) -> bool {
    let primary_ok = is_valid(&nvram[PRIMARY_OFFSET..]);
    let backup_ok = is_valid(&nvram[BACKUP_OFFSET..]);
    if primary_ok && backup_ok {
        return false;
    }
    let block = cookie.to_block();
    nvram[PRIMARY_OFFSET..PRIMARY_OFFSET + BLOCK_LEN].copy_from_slice(&block);
    nvram[BACKUP_OFFSET..BACKUP_OFFSET + BLOCK_LEN].copy_from_slice(&block);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_id_can_be_changed_and_stays_valid() {
        let mut nv = vec![0xFFu8; 32 * 1024];
        ensure_in_nvram(&mut nv, &Cookie::default());
        assert!(set_pm_id(&mut nv, pm_id_for_model("1760").unwrap()));
        assert!(is_valid(&nv[PRIMARY_OFFSET..]) && is_valid(&nv[BACKUP_OFFSET..]));
        assert_eq!(&nv[2 + 0x0C..2 + 0x0E], &[0x03, 0x16]);
    }

    #[test]
    fn default_cookie_is_valid() {
        let b = Cookie::default().to_block();
        assert!(is_valid(&b));
        assert_eq!(b[10], PROCESSOR_C1700);
        assert_eq!(ios_board_id(&b[2..130]), DEFAULT_SERIAL);
    }

    /// The board ID exactly as IOS 12.4(8) builds it (0x8012377c): bytes
    /// 0x18-0x1A if alphanumeric, "%02x" of 0x1B and 0x1C, bytes 0x1D-0x20.
    fn ios_board_id(data: &[u8]) -> String {
        let chars = |r: std::ops::Range<usize>| -> String {
            data[r].iter().filter(|c| c.is_ascii_alphanumeric()).map(|&c| c as char).collect()
        };
        format!("{}{:02x}{:02x}{}", chars(0x18..0x1B), data[0x1B], data[0x1C], chars(0x1D..0x21))
    }

    #[test]
    fn serial_round_trips_through_the_ios_decoder() {
        let mut nv = vec![0xFFu8; 32 * 1024];
        ensure_in_nvram(&mut nv, &Cookie::default());
        assert_eq!(set_serial(&mut nv, "FTX0945W0MY"), Ok(true));
        assert!(is_valid(&nv[PRIMARY_OFFSET..]) && is_valid(&nv[BACKUP_OFFSET..]));
        assert_eq!(ios_board_id(&nv[2..130]), "FTX0945W0MY");
        // Dynamips' default cookie bytes at 0x18
        assert_eq!(&nv[2 + 0x18..2 + 0x21], &[0x46, 0x54, 0x58, 0x09, 0x45, 0x57, 0x30, 0x4D, 0x59]);
        assert!(set_serial(&mut nv, "FTX09XXW0MY").is_err(), "YYWW must be hex digits");
        assert!(set_serial(&mut nv, "SHORT").is_err());
    }
}
