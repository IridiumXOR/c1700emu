//! Memory dumps in ELF core format (docs ch. 20), readable by GDB and
//! other ELF tools:
//!
//! ```text
//! gdb -q -ex "set architecture powerpc:common" -ex "core-file c1700-core.elf"
//! (gdb) info registers
//! (gdb) x/8i $pc
//! ```
//!
//! The file is an ELF32 big-endian PowerPC `ET_CORE`:
//!
//! * one `PT_NOTE` with an `NT_PRSTATUS` note: the registers, in the layout
//!   of Linux on 32-bit PowerPC (r0-r31, nip, msr, orig_r3, ctr, lr, xer,
//!   cr, mq, trap, dar, dsisr, result), which GDB knows;
//! * one `PT_LOAD` per run of physical memory (RAM, flash, boot ROM, the
//!   IMMR registers and DPRAM, the NVRAM) at its physical address;
//! * with translation on, one more `PT_LOAD` per valid TLB mapping, at the
//!   effective address the guest uses (`p_paddr` = the physical one), so a
//!   debugger can follow IOS's pointers. These point at the same bytes in
//!   the file as the physical copy, and so do physical aliases (the board
//!   decodes the SDRAM at more than one address): nothing is stored twice.
//!
//! The header says OS/ABI "GNU/Linux" so that GDB applies its Linux
//! PowerPC register layout to the note.

use crate::machine::Machine;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PF_R: u32 = 4;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const EM_PPC: u16 = 20;
const ET_CORE: u16 = 4;
const ELFOSABI_GNU: u8 = 3;
const NT_PRSTATUS: u32 = 1;
const EHDR_SIZE: u32 = 52;
const PHDR_SIZE: u32 = 32;
/// `struct elf_prstatus` on 32-bit PowerPC: 72 bytes of process info,
/// 48 registers, `pr_fpvalid`.
const PRSTATUS_SIZE: usize = 72 + 48 * 4 + 4;

struct Segment {
    vaddr: u32,
    paddr: u32,
    /// Index of the physical segment whose data it shows, and the offset in it.
    data: usize,
    offset: u32,
    len: u32,
}

impl Machine {
    /// Write the machine's memory and registers to `path` as an ELF core
    /// file. Returns the number of memory segments written.
    pub fn write_core(&mut self, path: &Path) -> Result<usize, String> {
        let physical = self.board.physical_segments();
        let mut segments: Vec<Segment> = Vec::new();
        // Effective-address views first: a debugger looks them up first.
        if self.cpu.msr & (ppc8xx::msr::IR | ppc8xx::msr::DR) != 0 {
            for (ea, pa, size) in self.cpu.mmu.mappings() {
                let inside = physical.iter().position(|(start, bytes)| pa >= *start && pa as u64 + size as u64 <= *start as u64 + bytes.len() as u64);
                if let Some(i) = inside {
                    segments.push(Segment { vaddr: ea, paddr: pa, data: i, offset: pa - physical[i].0, len: size });
                }
            }
        }
        for (i, (start, bytes)) in physical.iter().enumerate() {
            segments.push(Segment { vaddr: *start, paddr: *start, data: i, offset: 0, len: bytes.len() as u32 });
        }

        let note = self.prstatus_note();
        let phnum = 1 + segments.len() as u32;
        let note_offset = EHDR_SIZE + phnum * PHDR_SIZE;
        // File offset of each physical segment's data. Aliases (the board
        // decodes the same SDRAM at several addresses) share their bytes.
        let mut data_offset = vec![0u32; physical.len()];
        let mut stored = vec![true; physical.len()];
        let mut offset = note_offset + note.len() as u32;
        for (i, (_, bytes)) in physical.iter().enumerate() {
            if let Some(j) = (0..i).find(|&j| stored[j] && physical[j].1 == *bytes) {
                data_offset[i] = data_offset[j];
                stored[i] = false;
                continue;
            }
            offset = (offset + 3) & !3;
            data_offset[i] = offset;
            offset += bytes.len() as u32;
        }

        let mut out = Vec::with_capacity(note_offset as usize + note.len());
        // ELF header
        out.extend_from_slice(&[0x7F, b'E', b'L', b'F', 1, 2, 1, ELFOSABI_GNU, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&ET_CORE.to_be_bytes());
        out.extend_from_slice(&EM_PPC.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes()); // e_version
        out.extend_from_slice(&0u32.to_be_bytes()); // e_entry
        out.extend_from_slice(&EHDR_SIZE.to_be_bytes()); // e_phoff
        out.extend_from_slice(&0u32.to_be_bytes()); // e_shoff
        out.extend_from_slice(&0u32.to_be_bytes()); // e_flags
        out.extend_from_slice(&(EHDR_SIZE as u16).to_be_bytes());
        out.extend_from_slice(&(PHDR_SIZE as u16).to_be_bytes());
        out.extend_from_slice(&(phnum as u16).to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // no section headers
        // program headers
        let phdr = |out: &mut Vec<u8>, kind: u32, offset: u32, vaddr: u32, paddr: u32, size: u32, flags: u32, align: u32| {
            for v in [kind, offset, vaddr, paddr, size, size, flags, align] {
                out.extend_from_slice(&v.to_be_bytes());
            }
        };
        phdr(&mut out, PT_NOTE, note_offset, 0, 0, note.len() as u32, PF_R, 4);
        for s in &segments {
            phdr(&mut out, PT_LOAD, data_offset[s.data] + s.offset, s.vaddr, s.paddr, s.len, PF_R | PF_W | PF_X, 4);
        }
        out.extend_from_slice(&note);

        let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = BufWriter::new(file);
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        w.write_all(&out).map_err(io)?;
        let mut written = out.len() as u32;
        for (i, (_, bytes)) in physical.iter().enumerate().filter(|&(i, _)| stored[i]) {
            let pad = data_offset[i] - written;
            w.write_all(&vec![0u8; pad as usize]).map_err(io)?;
            w.write_all(bytes).map_err(io)?;
            written = data_offset[i] + bytes.len() as u32;
        }
        w.flush().map_err(io)?;
        Ok(segments.len())
    }

    /// The `NT_PRSTATUS` note with the CPU registers.
    fn prstatus_note(&self) -> Vec<u8> {
        let c = &self.cpu;
        let mut desc = vec![0u8; PRSTATUS_SIZE];
        desc[24..28].copy_from_slice(&1u32.to_be_bytes()); // pr_pid: GDB wants a thread id
        let mut regs = [0u32; 48];
        regs[..32].copy_from_slice(&c.gpr);
        regs[32] = c.pc; // nip
        regs[33] = c.msr;
        regs[34] = c.gpr[3]; // orig_r3
        regs[35] = c.ctr;
        regs[36] = c.lr;
        regs[37] = c.xer;
        regs[38] = c.cr;
        regs[41] = c.dar;
        regs[42] = c.dsisr;
        for (i, r) in regs.iter().enumerate() {
            desc[72 + 4 * i..76 + 4 * i].copy_from_slice(&r.to_be_bytes());
        }
        let mut note = Vec::new();
        note.extend_from_slice(&5u32.to_be_bytes()); // namesz ("CORE\0")
        note.extend_from_slice(&(PRSTATUS_SIZE as u32).to_be_bytes());
        note.extend_from_slice(&NT_PRSTATUS.to_be_bytes());
        note.extend_from_slice(b"CORE\0\0\0\0"); // padded to 4
        note.extend_from_slice(&desc);
        note
    }
}
