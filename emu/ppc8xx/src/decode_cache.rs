//! A small memo table in front of the `powerpc` crate's decoder.
//!
//! `Opcode::detect` finds the opcode by scanning the candidates for the
//! instruction's 6-bit primary opcode. For primary opcode 31 (`mr`, `mflr`,
//! `mtspr`, `cmpw`, indexed loads/stores...) that list is long: ~40 ns per
//! decode versus ~4 ns for everything else (`examples/decode_bench.rs`).
//! Since decoding is a pure function of the 32-bit instruction word, we can
//! remember recent results in a direct-mapped table indexed by a hash of the
//! word. A hit costs one comparison. Because the key is the word itself (not
//! the address), self-modifying code and code loaded into RAM stay correct.

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

    #[inline]
    pub fn decode(&mut self, word: u32) -> Ins {
        // Fibonacci hashing: multiply by 2^32/phi and keep the top bits.
        let slot = (word.wrapping_mul(0x9E37_79B1) >> (32 - 14)) as usize;
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
