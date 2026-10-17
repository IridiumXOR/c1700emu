//! The Cisco "simple flash file system" used on the C1700 boot flash, so
//! IOS images can be put into the emulated flash and booted by ROMMON with
//! no TFTP step.
//!
//! Format, from ROMMON's own code (`0xFFF1DDB0` writes a file,
//! `find_partition` 0xFFF21E10 walks them) and a file it wrote:
//!
//! ```text
//! +0x00 u32 0xBAD00B1E          magic
//! +0x04 u32 length              of the data
//! +0x08 u16 ~csum16(data)       Internet checksum (one's complement of the
//!                               one's-complement 16-bit sum)
//! +0x0A u16 0xFFFF              flags
//! +0x0C u32 (never written by ROMMON: stack garbage; we use 0xFFFFFFFF)
//! +0x10 char name[48]
//! +0x40 data, then the next header at 0x40 + round_up(length, 4)
//! ```
//!
//! An erased word (0xFFFFFFFF) where a header should be ends the list.

pub const MAGIC: u32 = 0xBAD0_0B1E;
pub const HEADER_LEN: usize = 0x40;
const NAME_LEN: usize = 48;

/// Internet checksum of `data` (big-endian 16-bit words, odd byte padded).
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for pair in data.chunks(2) {
        let w = u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]);
        sum += w as u32;
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn header(name: &str, data: &[u8]) -> Result<[u8; HEADER_LEN], String> {
    if name.is_empty() || name.len() >= NAME_LEN {
        return Err(format!("flash file name {name:?} must be 1..{} characters", NAME_LEN - 1));
    }
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(&MAGIC.to_be_bytes());
    h[4..8].copy_from_slice(&(data.len() as u32).to_be_bytes());
    h[8..10].copy_from_slice(&checksum(data).to_be_bytes());
    h[10..16].fill(0xFF);
    h[16..16 + name.len()].copy_from_slice(name.as_bytes());
    Ok(h)
}

/// Build a flash image of `size` bytes holding `files` in order.
pub fn build(files: &[(String, Vec<u8>)], size: usize) -> Result<Vec<u8>, String> {
    let mut image = vec![0xFFu8; size];
    let mut pos = 0usize;
    for (name, data) in files {
        let end = pos + HEADER_LEN + data.len();
        if end > size {
            return Err(format!("{name} ({} bytes) does not fit: flash is {} MB", data.len(), size >> 20));
        }
        image[pos..pos + HEADER_LEN].copy_from_slice(&header(name, data)?);
        image[pos + HEADER_LEN..end].copy_from_slice(data);
        pos = pos + HEADER_LEN + data.len().next_multiple_of(4);
    }
    Ok(image)
}

/// One file found in a flash image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub offset: usize,
    pub length: usize,
    pub checksum_ok: bool,
}

/// List the files of a flash image (stops at the first erased header).
pub fn list(image: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + HEADER_LEN <= image.len() {
        let magic = u32::from_be_bytes(image[pos..pos + 4].try_into().unwrap());
        if magic != MAGIC {
            break;
        }
        let length = u32::from_be_bytes(image[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let stored = u16::from_be_bytes([image[pos + 8], image[pos + 9]]);
        let name_bytes = &image[pos + 16..pos + 16 + NAME_LEN];
        let name = String::from_utf8_lossy(&name_bytes[..name_bytes.iter().position(|&b| b == 0).unwrap_or(NAME_LEN)]).into_owned();
        let data_end = (pos + HEADER_LEN + length).min(image.len());
        let checksum_ok = checksum(&image[pos + HEADER_LEN..data_end]) == stored;
        out.push(Entry { name, offset: pos, length, checksum_ok });
        pos += HEADER_LEN + length.next_multiple_of(4);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden test: the header ROMMON itself wrote for a 312-byte hello.bin
    /// (`dir flash:` showed checksum 0x8f5d). Bytes 0x0C..0x10 were stack
    /// garbage in ROMMON's version, so they are excluded.
    #[test]
    fn header_matches_what_rommon_wrote() {
        let rommon = [
            0xba, 0xd0, 0x0b, 0x1e, 0x00, 0x00, 0x01, 0x38, 0x8f, 0x5d, 0xff, 0xff, 0xff, 0xf3, 0x00, 0x00, b'h', b'e', b'l',
            b'l', b'o', b'.', b'b', b'i', b'n',
        ];
        // the very file ROMMON stored (built by tools/mkimage.py)
        let data = include_bytes!("testdata/hello.bin");
        assert_eq!(checksum(data), 0x8f5d, "checksum ROMMON computed for this file");
        let h = header("hello.bin", data).unwrap();
        assert_eq!(&h[0..10], &rommon[0..10]);
        assert_eq!(&h[10..12], &rommon[10..12]);
        assert_eq!(&h[16..25], &rommon[16..25]);
    }

    #[test]
    fn build_then_list_round_trips() {
        let files = vec![("a.bin".to_string(), vec![1, 2, 3]), ("ios.bin".to_string(), vec![7; 1000])];
        let img = build(&files, 1 << 20).unwrap();
        let l = list(&img);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].name.as_str(), l[0].offset, l[0].length), ("a.bin", 0, 3));
        assert_eq!(l[1].offset, 0x40 + 4, "next header after data rounded up to 4");
        assert!(l.iter().all(|e| e.checksum_ok));
        assert!(build(&files, 512).is_err(), "too big for the flash");
    }
}
