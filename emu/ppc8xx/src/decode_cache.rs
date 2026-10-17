//! A small memo table in front of the `powerpc` crate's decoder.
//!
//! `Opcode::detect` finds the opcode by scanning the candidates for the
//! instruction's 6-bit primary opcode. For primary opcode 31 (`mr`, `mflr`,
//! `mtspr`, `cmpw`, indexed loads/stores...) that list is long: ~40 ns per
//! decode versus ~4 ns for everything else (`examples/decode_bench.rs`).
//! Since decoding is a pure function of the 32-bit instruction word, we can
//! remember recent results in a direct-mapped table. A hit costs one
//! comparison. The *tag* is the word itself, so self-modifying code and code
//! loaded into RAM stay correct whatever slot it lands in.
//!
//! The table is indexed by the instruction's **address**: sequential code
//! uses consecutive slots (8 per 64-byte cache line), so a loop's working set
//! stays in the host's L1 cache. The first version indexed by a hash of the
//! word, which scattered every lookup over the 128 KB table; `perf` showed
//! the lookup as the hottest spot of the whole interpreter.

use powerpc::{Extensions, Ins, Opcode};

const ENTRIES: usize = 1 << 14; // 16 Ki entries x 8 bytes = 128 KB

pub struct DecodeCache {
    entries: Box<[(u32, Opcode)]>,
}

impl DecodeCache {
    pub fn new() -> Self {
        // Every slot starts as a *valid* memo of word 0, so a lookup can never
        // return a wrong opcode.
        let zero = Ins::new(0, Extensions::none()).op;
        DecodeCache { entries: vec![(0, zero); ENTRIES].into_boxed_slice() }
    }

    /// Decode `word`, fetched from address `pc` (used only to pick the slot).
    #[inline]
    pub fn decode(&mut self, word: u32, pc: u32) -> Ins {
        let slot = (pc >> 2) as usize & (ENTRIES - 1);
        let entry = &mut self.entries[slot];
        if entry.0 != word {
            *entry = (word, Ins::new(word, Extensions::none()).op);
        }
        Ins { code: word, op: entry.1 }
    }
}

impl Default for DecodeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for DecodeCache {
    fn clone(&self) -> Self {
        DecodeCache { entries: self.entries.clone() }
    }
}
