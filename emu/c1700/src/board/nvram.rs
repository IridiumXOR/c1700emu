//! NVRAM: a parallel EEPROM on the I/O bus (0x68000000).
//!
//! Evidence it is an EEPROM rather than battery SRAM: `nvram_write`
//! (0xFFF0BB00) writes in 32-byte pages and then *polls* until the last byte
//! reads back correctly — the "data polling" protocol of 28C-series
//! EEPROMs. A plain byte array satisfies that protocol (the data is there
//! immediately), so that is all we model, plus persistence to a host file.
//!
//! Layout used by ROMMON (docs/04-hardware-map.md):
//!
//! | offset | size | content                                              |
//! |--------|------|------------------------------------------------------|
//! | 0x000  | 132  | cookie: u16 magic 0x8D74, 128 B data, u16 checksum   |
//! | 0x090  | 2    | configuration register                               |
//! | 0x092  | 2    | ~configuration register                              |
//! | 0x094  | 4    | 0xFEEDFACE (confreg valid)                           |
//! | 0x65A  | 132  | backup copy of the cookie                            |

use std::path::PathBuf;

pub struct Nvram {
    data: Vec<u8>,
    path: Option<PathBuf>,
    dirty: bool,
}

impl Nvram {
    /// Create an NVRAM of `size` bytes, loading `path` if it exists.
    /// A blank EEPROM reads all ones.
    pub fn new(size: usize, path: Option<PathBuf>) -> std::io::Result<Self> {
        let mut data = vec![0xFF; size];
        if let Some(p) = &path {
            if p.exists() {
                let content = std::fs::read(p)?;
                let n = content.len().min(size);
                data[..n].copy_from_slice(&content[..n]);
                log::info!("NVRAM loaded from {} ({} bytes)", p.display(), n);
            }
        }
        Ok(Nvram { data, path, dirty: false })
    }

    pub fn size(&self) -> usize {
        self.data.len()
    }

    pub fn read8(&self, off: u32) -> u8 {
        self.data[off as usize % self.data.len()]
    }

    pub fn write8(&mut self, off: u32, value: u8) {
        let n = self.data.len();
        let slot = &mut self.data[off as usize % n];
        if *slot != value {
            *slot = value;
            self.dirty = true;
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        &mut self.data
    }

    /// Write the content back to the host file if it changed.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let (true, Some(p)) = (self.dirty, &self.path) {
            std::fs::write(p, &self.data)?;
            self.dirty = false;
            log::debug!("NVRAM saved to {}", p.display());
        }
        Ok(())
    }
}

impl Drop for Nvram {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            log::error!("could not save NVRAM: {e}");
        }
    }
}
