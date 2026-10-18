//! Snapshots: the whole machine saved to a file and restored later
//! (docs ch. 20).
//!
//! File layout:
//!
//! ```text
//! "C1700SNP"         8 bytes magic
//! version            u32 LE
//! ROM CRC-32         u32 LE   (a snapshot only fits the ROM it was taken with)
//! lz4 frame          the state walk of Machine::visit_state, little-endian
//! ```
//!
//! The state walk is the `visit_state` method of every component (see
//! `ppc8xx::state`): the same code saves and restores. A snapshot is
//! restored into a machine built from the same configuration (`--model`,
//! `--ram`, flash size); the walk checks the sizes and stops with an error
//! if they differ.

use crate::machine::Machine;
use ppc8xx::state::Visitor;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"C1700SNP";
/// Bump when the state walk changes.
const VERSION: u32 = 1;

/// Writes every visited field.
struct Saver<W: Write> {
    out: W,
    error: Option<io::Error>,
}

impl<W: Write> Saver<W> {
    fn put(&mut self, bytes: &[u8]) {
        if self.error.is_none() {
            if let Err(e) = self.out.write_all(bytes) {
                self.error = Some(e);
            }
        }
    }
}

impl<W: Write> Visitor for Saver<W> {
    fn u8(&mut self, v: &mut u8) {
        self.put(&[*v]);
    }
    fn u16(&mut self, v: &mut u16) {
        self.put(&v.to_le_bytes());
    }
    fn u32(&mut self, v: &mut u32) {
        self.put(&v.to_le_bytes());
    }
    fn u64(&mut self, v: &mut u64) {
        self.put(&v.to_le_bytes());
    }
    fn bool(&mut self, v: &mut bool) {
        self.put(&[*v as u8]);
    }
    fn bytes_fixed(&mut self, v: &mut [u8]) {
        self.put(&(v.len() as u64).to_le_bytes());
        self.put(v);
    }
    fn bytes(&mut self, v: &mut Vec<u8>) {
        self.put(&(v.len() as u64).to_le_bytes());
        self.put(v);
    }
    fn fail(&mut self, _msg: String) {}
    fn loading(&self) -> bool {
        false
    }
}

/// Overwrites every visited field with what it reads.
struct Loader<R: Read> {
    input: R,
    error: Option<String>,
}

impl<R: Read> Loader<R> {
    fn get<const N: usize>(&mut self) -> [u8; N] {
        let mut b = [0u8; N];
        if self.error.is_none() {
            if let Err(e) = self.input.read_exact(&mut b) {
                self.error = Some(format!("snapshot ends early: {e}"));
            }
        }
        b
    }
    fn len(&mut self) -> usize {
        u64::from_le_bytes(self.get()) as usize
    }
}

impl<R: Read> Visitor for Loader<R> {
    fn u8(&mut self, v: &mut u8) {
        *v = self.get::<1>()[0];
    }
    fn u16(&mut self, v: &mut u16) {
        *v = u16::from_le_bytes(self.get());
    }
    fn u32(&mut self, v: &mut u32) {
        *v = u32::from_le_bytes(self.get());
    }
    fn u64(&mut self, v: &mut u64) {
        *v = u64::from_le_bytes(self.get());
    }
    fn bool(&mut self, v: &mut bool) {
        *v = self.get::<1>()[0] != 0;
    }
    fn bytes_fixed(&mut self, v: &mut [u8]) {
        let n = self.len();
        if n != v.len() {
            return self.fail(format!("snapshot holds a {n}-byte buffer where this machine has {} bytes", v.len()));
        }
        if self.error.is_none() {
            if let Err(e) = self.input.read_exact(v) {
                self.error = Some(format!("snapshot ends early: {e}"));
            }
        }
    }
    fn bytes(&mut self, v: &mut Vec<u8>) {
        let n = self.len();
        if self.error.is_some() || n > 1 << 30 {
            return self.fail(format!("snapshot is damaged (a {n}-byte buffer)"));
        }
        v.resize(n, 0);
        if let Err(e) = self.input.read_exact(v) {
            self.error = Some(format!("snapshot ends early: {e}"));
        }
    }
    fn fail(&mut self, msg: String) {
        self.error.get_or_insert(msg);
    }
    fn loading(&self) -> bool {
        true
    }
}

impl Machine {
    /// Save the whole machine to `path`.
    pub fn save_snapshot(&mut self, path: &Path) -> Result<(), String> {
        let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut out = BufWriter::new(file);
        out.write_all(MAGIC).map_err(|e| e.to_string())?;
        out.write_all(&VERSION.to_le_bytes()).map_err(|e| e.to_string())?;
        out.write_all(&crc32fast::hash(&self.board.rom).to_le_bytes()).map_err(|e| e.to_string())?;
        let mut saver = Saver { out: lz4_flex::frame::FrameEncoder::new(out), error: None };
        self.visit_state(&mut saver);
        if let Some(e) = saver.error {
            return Err(format!("{}: {e}", path.display()));
        }
        let out = saver.out.finish().map_err(|e| e.to_string())?;
        out.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Restore the machine from `path`. The machine must have been built
    /// with the configuration the snapshot was taken with.
    pub fn load_snapshot(&mut self, path: &Path) -> Result<(), String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut input = BufReader::new(file);
        let mut header = [0u8; 16];
        input.read_exact(&mut header).map_err(|_| format!("{}: not a snapshot", path.display()))?;
        if &header[0..8] != MAGIC {
            return Err(format!("{}: not a c1700emu snapshot", path.display()));
        }
        let version = u32::from_le_bytes(header[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(format!("{}: snapshot format {version}, this emulator reads format {VERSION}", path.display()));
        }
        let rom_crc = u32::from_le_bytes(header[12..16].try_into().unwrap());
        if rom_crc != crc32fast::hash(&self.board.rom) {
            return Err(format!("{}: taken with a different boot ROM", path.display()));
        }
        let mut loader = Loader { input: lz4_flex::frame::FrameDecoder::new(input), error: None };
        self.visit_state(&mut loader);
        match loader.error {
            Some(e) => Err(format!("{}: {e}", path.display())),
            None => Ok(()),
        }
    }
}
