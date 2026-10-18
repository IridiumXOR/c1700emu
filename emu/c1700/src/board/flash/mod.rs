//! Boot flash on chip select 6 (0x60000000, 16-bit port): the C1700's
//! internal "miniflash" card, the only boot storage besides the ROM
//! (no PCMCIA/CompactFlash/SD on this platform — Cisco 1750 datasheet).
//!
//! Two chip families, both understood by the ROM's flash driver *and* by IOS:
//!
//! * [`intel`]: Intel StrataFlash (28F320J3/28F640J3/28F128J3) in **x16
//!   mode**, one chip per 16 MB bank — what real C1700s use (Dynamips models
//!   the 4/8 MB parts the same way). The ROM's board hook "width 2 +
//!   rotate-16 swizzle" exists exactly for this layout (docs/08, §8.8).
//! * [`amd`]: two byte-wide AMD Am29F016 side by side (4 MB) — what this
//!   emulator used first; kept as `--flash-size 4`.
//!
//! Content can be persisted to a host file (`--flash`), and pre-populated
//! with a Cisco flash file system (`flashfs`, `--flash-file`).

pub mod amd;
pub mod intel;

use std::path::PathBuf;

/// The flash array as the CPU sees it, plus its backing file.
pub struct Storage {
    pub data: Vec<u8>,
    pub dirty: bool,
    path: Option<PathBuf>,
}

impl Storage {
    fn new(size: usize) -> Self {
        Storage { data: vec![0xFF; size], dirty: false, path: None }
    }
}

pub enum Flash {
    Amd(amd::AmdFlash),
    Intel(intel::IntelFlash),
}

/// Flash configurations offered on the command line (`--flash-size`).
pub fn for_size_mb(mb: u32) -> Result<Flash, String> {
    Ok(match mb {
        4 => Flash::Amd(amd::AmdFlash::new(&amd::AM29F016)),
        8 => Flash::Intel(intel::IntelFlash::new(&intel::I28F640J3, 1)),
        16 => Flash::Intel(intel::IntelFlash::new(&intel::I28F128J3, 1)),
        32 => Flash::Intel(intel::IntelFlash::new(&intel::I28F128J3, 2)),
        _ => return Err(format!("unsupported flash size {mb} MB (4, 8, 16 or 32)")),
    })
}

impl Flash {
    /// Snapshot state (docs ch. 20).
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        match self {
            Flash::Amd(f) => f.visit_state(v),
            Flash::Intel(f) => f.visit_state(v),
        }
    }

    fn storage(&self) -> &Storage {
        match self {
            Flash::Amd(f) => &f.store,
            Flash::Intel(f) => &f.store,
        }
    }
    fn storage_mut(&mut self) -> &mut Storage {
        match self {
            Flash::Amd(f) => &mut f.store,
            Flash::Intel(f) => &mut f.store,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Flash::Amd(f) => format!("{} MB: 2 x {}", self.size() >> 20, f.chip.name),
            Flash::Intel(f) => format!("{} MB: {} x {} (x16)", self.size() >> 20, f.banks.len(), f.chip.name),
        }
    }

    pub fn size(&self) -> u32 {
        self.storage().data.len() as u32
    }
    pub fn bytes(&self) -> &[u8] {
        &self.storage().data
    }
    pub fn is_dirty(&self) -> bool {
        self.storage().dirty
    }

    /// Replace the content (e.g. a pre-built flash file system); a shorter
    /// image leaves the rest erased.
    pub fn load(&mut self, image: &[u8]) {
        let st = self.storage_mut();
        let n = image.len().min(st.data.len());
        st.data[..n].copy_from_slice(&image[..n]);
        st.data[n..].fill(0xFF);
    }

    /// Back the flash with a host file: load it if it exists, and save
    /// changes to it from now on.
    pub fn attach_file(&mut self, path: PathBuf) -> std::io::Result<()> {
        if path.exists() {
            let image = std::fs::read(&path)?;
            let size = self.size() as usize;
            if image.len() != size {
                log::warn!("{}: {} bytes, flash is {} — using what fits", path.display(), image.len(), size);
            }
            self.load(&image);
            log::info!("flash loaded from {}", path.display());
        }
        self.storage_mut().path = Some(path);
        Ok(())
    }

    /// Write the content back to the host file if the guest changed it.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let st = self.storage_mut();
        if let (true, Some(p)) = (st.dirty, &st.path) {
            std::fs::write(p, &st.data)?;
            st.dirty = false;
            log::info!("flash saved to {}", p.display());
        }
        Ok(())
    }

    /// Mark the content as changed (e.g. after `load`) so it gets saved.
    pub fn mark_dirty(&mut self) {
        self.storage_mut().dirty = true;
    }

    pub fn read8(&self, off: u32) -> u8 {
        match self {
            Flash::Amd(f) => f.read8(off),
            Flash::Intel(f) => f.read8(off),
        }
    }
    pub fn read16(&self, off: u32) -> u16 {
        match self {
            Flash::Amd(f) => f.read16(off),
            Flash::Intel(f) => f.read16(off),
        }
    }
    pub fn write8(&mut self, off: u32, value: u8) {
        match self {
            Flash::Amd(f) => f.write8(off, value),
            Flash::Intel(f) => f.write8(off, value),
        }
    }
    pub fn write16(&mut self, off: u32, value: u16) {
        match self {
            Flash::Amd(f) => f.write16(off, value),
            Flash::Intel(f) => f.write16(off, value),
        }
    }
    /// Debugger read: the array content regardless of chip state.
    pub fn peek8(&self, off: u32) -> u8 {
        self.storage().data[(off % self.size()) as usize]
    }

    /// Reads at `off` return the array content (no ID, status or query
    /// data), for all of its 4 KB page.
    pub fn array_readable(&self, off: u32) -> bool {
        match self {
            Flash::Amd(f) => f.array_readable(),
            Flash::Intel(f) => f.array_readable(off),
        }
    }

    /// Where the byte at `off` is kept: reading there is reading the flash
    /// while [`Flash::array_readable`] holds.
    pub fn host_ptr(&mut self, off: u32) -> *mut u8 {
        let size = self.size();
        self.storage_mut().data[(off % size) as usize..].as_mut_ptr()
    }
}

impl Drop for Flash {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            log::error!("could not save flash: {e}");
        }
    }
}
