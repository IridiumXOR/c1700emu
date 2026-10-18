//! What ROMMON and IOS keep in the NVRAM, and how to edit it from outside
//! (`--confreg`, `--rommon-var`, `--startup-config`, `--nvram-info`; docs
//! ch. 7.7). Layouts from the boot ROM (`sub_FFF0B728`, `sub_FFF0B8BC`) and
//! IOS 12.4(8) (`sub_80C82648`, `sub_80C85B24`), checked against an NVRAM
//! written by IOS itself.
//!
//! | offset | owner | content |
//! |---|---|---|
//! | 0x000 | ROMMON | cookie (MAC, serial, mainboard ID; `cookie.rs`) |
//! | 0x090 | ROMMON | configuration register, its complement, magic 0xFEEDFACE |
//! | 0x094-0x65A | ROMMON | the monitor area: variables at 0x148 (`NAME\0VALUE\0`... `\0`), aliases at 0x448, checksum word at 0x658 (16-bit sum of the area = 0) |
//! | 0x808 | IOS | startup-config header (magic 0xABCD), text after it |
//! | ... | IOS | private config (magic 0xFEDC) after the text; NVRAM files (0xDCBA) near the end |
//!
//! The IOS checksum is the complement of the ones'-complement sum of the
//! 16-bit words from the header to the end of the NVRAM.

const CONFREG: usize = 0x90;
const MON_MAGIC: usize = 0x94;
const MON_END: usize = 0x65A;
const MON_CHECKSUM: usize = 0x658;
const MON_VARS: usize = 0x148;
const MON_ALIASES: usize = 0x448;
const MAGIC_FEEDFACE: u32 = 0xFEED_FACE;

/// The ROM's defaults for a fresh monitor area (`unk_FFF2F318`,
/// `unk_FFF2F32C`): the prompt variable and the standard aliases.
const DEFAULT_VARS: &[(&str, &str)] = &[("PS1", "rommon ! > ")];
const DEFAULT_ALIASES: &[u8] = b"r\0repeat\0h\0history\0?\0help\0b\0boot\0ls\0dir\0i\0reset\0k\0stack\0\0";

const IOS_HEADER: usize = 0x808;
const IOS_MAGIC: u16 = 0xABCD;
const IOS_PRIVATE_MAGIC: u16 = 0xFEDC;
const IOS_HEADER_LEN: usize = 36;

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn sum16(b: &[u8]) -> u16 {
    b.chunks(2).fold(0u16, |s, w| s.wrapping_add(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)])))
}

/// The configuration register, if NVRAM holds a valid one.
pub fn confreg(nv: &[u8]) -> Option<u16> {
    let (value, inverse) = (be16(nv, CONFREG), be16(nv, CONFREG + 2));
    (be32(nv, CONFREG + 4) == MAGIC_FEEDFACE && value == !inverse).then_some(value)
}

/// Is the monitor area intact (what the ROM checks before using it)?
pub fn monitor_area_valid(nv: &[u8]) -> bool {
    be32(nv, MON_MAGIC) == MAGIC_FEEDFACE && sum16(&nv[MON_MAGIC..MON_END]) == 0
}

/// The ROMMON variables (`set` at the ROMMON prompt), if the area is valid.
pub fn rommon_vars(nv: &[u8]) -> Option<Vec<(String, String)>> {
    monitor_area_valid(nv).then(|| parse_pairs(&nv[MON_VARS..MON_ALIASES]))
}

fn parse_pairs(area: &[u8]) -> Vec<(String, String)> {
    let mut fields = area.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let mut pairs = Vec::new();
    while let Some(name) = fields.next().filter(|n| !n.is_empty()) {
        pairs.push((name, fields.next().unwrap_or_default()));
    }
    pairs
}

/// Write the configuration register (as `confreg` at the ROMMON prompt
/// does) and keep the monitor area valid.
pub fn set_confreg(nv: &mut [u8], value: u16) {
    nv[CONFREG..CONFREG + 2].copy_from_slice(&value.to_be_bytes());
    nv[CONFREG + 2..CONFREG + 4].copy_from_slice(&(!value).to_be_bytes());
    nv[CONFREG + 4..CONFREG + 8].copy_from_slice(&MAGIC_FEEDFACE.to_be_bytes());
    if !monitor_area_valid(nv) {
        reset_monitor_area(nv);
    }
}

/// Set ROMMON variables (`NAME=VALUE`; an empty value removes the variable),
/// as typing them at the ROMMON prompt and `sync` would.
pub fn set_rommon_vars(nv: &mut [u8], assignments: &[(String, String)]) -> Result<(), String> {
    if !monitor_area_valid(nv) {
        reset_monitor_area(nv);
    }
    let mut vars = parse_pairs(&nv[MON_VARS..MON_ALIASES]);
    for (name, value) in assignments {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!("bad ROMMON variable {name:?}"));
        }
        vars.retain(|(n, _)| n != name);
        if !value.is_empty() {
            vars.push((name.clone(), value.clone()));
        }
    }
    let mut area = Vec::new();
    for (n, v) in &vars {
        area.extend_from_slice(n.as_bytes());
        area.push(0);
        area.extend_from_slice(v.as_bytes());
        area.push(0);
    }
    area.push(0);
    if area.len() > MON_ALIASES - MON_VARS {
        return Err(format!("ROMMON variables take {} bytes, the area holds {}", area.len(), MON_ALIASES - MON_VARS));
    }
    nv[MON_VARS..MON_ALIASES].fill(0);
    nv[MON_VARS..MON_VARS + area.len()].copy_from_slice(&area);
    fix_monitor_checksum(nv);
    Ok(())
}

/// A fresh monitor area as the ROM makes it when it finds the area corrupt.
fn reset_monitor_area(nv: &mut [u8]) {
    nv[MON_MAGIC..MON_END].fill(0);
    nv[MON_MAGIC..MON_MAGIC + 4].copy_from_slice(&MAGIC_FEEDFACE.to_be_bytes());
    let mut at = MON_VARS;
    for (n, v) in DEFAULT_VARS {
        for f in [n.as_bytes(), v.as_bytes()] {
            nv[at..at + f.len()].copy_from_slice(f);
            at += f.len() + 1;
        }
    }
    nv[MON_ALIASES..MON_ALIASES + DEFAULT_ALIASES.len()].copy_from_slice(DEFAULT_ALIASES);
    fix_monitor_checksum(nv);
}

/// The ROM's rule (`sub_FFF0B8BC`): the checksum word makes the 16-bit sum
/// of the area 0.
fn fix_monitor_checksum(nv: &mut [u8]) {
    nv[MON_CHECKSUM..MON_CHECKSUM + 2].fill(0);
    let sum = sum16(&nv[MON_MAGIC..MON_END]);
    nv[MON_CHECKSUM..MON_CHECKSUM + 2].copy_from_slice(&sum.wrapping_neg().to_be_bytes());
}

/// IOS's checksum: the complement of the ones'-complement sum of the words
/// from the header to the end of the NVRAM.
fn ios_checksum(region: &[u8]) -> u16 {
    let mut sum: u32 = region.chunks(2).map(|w| u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The IOS startup-config text, if NVRAM holds a valid one.
pub fn startup_config(nv: &[u8]) -> Option<String> {
    let h = IOS_HEADER;
    if nv.len() < h + IOS_HEADER_LEN || be16(nv, h) != IOS_MAGIC || be16(nv, h + 2) != 1 {
        return None; // (format 2 would be compressed: not handled)
    }
    let mut copy = nv[h..].to_vec();
    copy[4..6].fill(0);
    if ios_checksum(&copy) != be16(nv, h + 4) {
        return None;
    }
    let (start, size) = (be32(nv, h + 8) as usize, be32(nv, h + 16) as usize);
    let text = nv.get(h + start..h + start + size)?;
    Some(String::from_utf8_lossy(text).into_owned())
}

/// Install `text` as the IOS startup-config (what `write memory` stores;
/// format 1, uncompressed, without a private config).
pub fn set_startup_config(nv: &mut [u8], text: &str) -> Result<(), String> {
    let mut text = text.replace("\r\n", "\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let h = IOS_HEADER;
    let room = nv.len().saturating_sub(h + IOS_HEADER_LEN + 16);
    if text.len() > room {
        return Err(format!("startup-config is {} bytes, the NVRAM holds {room}", text.len()));
    }
    let start = IOS_HEADER_LEN as u32;
    let size = text.len() as u32;
    let mut header = Vec::with_capacity(IOS_HEADER_LEN);
    header.extend_from_slice(&IOS_MAGIC.to_be_bytes());
    header.extend_from_slice(&1u16.to_be_bytes()); // format 1: plain text
    header.extend_from_slice(&0u16.to_be_bytes()); // checksum, below
    header.extend_from_slice(&0x0C04u16.to_be_bytes()); // written by 12.4
    for v in [start, start + size, size, 0, 0, 0xFFFF, 0] {
        header.extend_from_slice(&v.to_be_bytes());
    }
    nv[h..h + IOS_HEADER_LEN].copy_from_slice(&header);
    let text_at = h + IOS_HEADER_LEN;
    nv[text_at..text_at + text.len()].copy_from_slice(text.as_bytes());
    // No private config: IOS only takes one whose magic sits right after
    // the text (aligned), so make sure no stale one is found there.
    let end = text_at + text.len();
    for at in [(end + 3) & !3, (end + 7) & !7] {
        if be16(nv, at) == IOS_PRIVATE_MAGIC {
            nv[at..at + 2].fill(0xFF);
        }
    }
    let sum = ios_checksum(&nv[h..]);
    nv[h + 4..h + 6].copy_from_slice(&sum.to_be_bytes());
    Ok(())
}

/// A readable summary for `--nvram-info`.
pub fn describe(nv: &[u8]) -> String {
    if nv.len() < IOS_HEADER + IOS_HEADER_LEN {
        return format!("not a C1700 NVRAM image ({} bytes, expected 32768)\n", nv.len());
    }
    let mut out = String::new();
    match confreg(nv) {
        Some(c) => out += &format!("configuration register: {c:#06x}\n"),
        None => out += "configuration register: not set (the ROM uses its default)\n",
    }
    match rommon_vars(nv) {
        Some(vars) => {
            out += "ROMMON variables:\n";
            for (n, v) in vars {
                out += &format!("  {n}={v}\n");
            }
        }
        None => out += "ROMMON variables: none (monitor area not initialised)\n",
    }
    match startup_config(nv) {
        Some(text) => out += &format!("IOS startup-config ({} bytes):\n{text}", text.len()),
        None => out += "IOS startup-config: none\n",
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rommon_vars_and_confreg_keep_the_area_valid() {
        let mut nv = vec![0xFFu8; 32 * 1024];
        assert_eq!(rommon_vars(&nv), None);
        set_confreg(&mut nv, 0x2102);
        assert_eq!(confreg(&nv), Some(0x2102));
        assert!(monitor_area_valid(&nv));
        assert_eq!(rommon_vars(&nv).unwrap(), [("PS1".to_string(), "rommon ! > ".to_string())]);
        let set = |n: &str, v: &str| (n.to_string(), v.to_string());
        set_rommon_vars(&mut nv, &[set("IP_ADDRESS", "10.0.2.15"), set("TFTP_SERVER", "10.0.2.2")]).unwrap();
        set_rommon_vars(&mut nv, &[set("IP_ADDRESS", "10.0.2.16"), set("PS1", "")]).unwrap();
        assert!(monitor_area_valid(&nv));
        assert_eq!(rommon_vars(&nv).unwrap(), [set("TFTP_SERVER", "10.0.2.2"), set("IP_ADDRESS", "10.0.2.16")]);
        assert!(set_rommon_vars(&mut nv, &[set("A=B", "x")]).is_err());
    }

    /// Layout and checksum as in an NVRAM IOS 12.4(8) wrote itself (docs
    /// ch. 7.7); a round trip, and a corrupted byte is noticed.
    #[test]
    fn startup_config_round_trip_matches_ios_format() {
        let mut nv = vec![0xFFu8; 32 * 1024];
        let text = "!\nversion 12.4\nhostname emutest\n!\nend\n";
        set_startup_config(&mut nv, text).unwrap();
        assert_eq!(be16(&nv, IOS_HEADER), 0xABCD);
        assert_eq!(be32(&nv, IOS_HEADER + 8), 36);
        assert_eq!(startup_config(&nv).as_deref(), Some(text));
        nv[IOS_HEADER + 40] ^= 1; // corrupt the text: the checksum notices
        assert_eq!(startup_config(&nv), None);
        // CRLF files are accepted and stored the IOS way.
        set_startup_config(&mut nv, "hostname a\r\nend").unwrap();
        assert_eq!(startup_config(&nv).as_deref(), Some("hostname a\nend\n"));
    }
}
