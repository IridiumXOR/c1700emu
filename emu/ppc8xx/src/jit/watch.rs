//! A helper for [`Platform`](crate::Platform) implementations: the set of
//! host pages that hold translated code, and the pages of that set written
//! since the JIT last asked.
//!
//! The platform decides what a "page" number means (it must be the same
//! number for every alias of the same memory). It calls
//! [`CodeWatch::note_write`] on every write to plain memory, from the CPU,
//! a DMA engine or a debugger alike. That costs one test while nothing is
//! watched (the interpreter alone never watches anything).

#[derive(Clone, Default)]
pub struct CodeWatch {
    /// One bit per page number.
    bits: Vec<u64>,
    /// Watched pages written since the last [`CodeWatch::take`].
    written: Vec<u32>,
    /// Number of watched pages.
    count: usize,
}

impl CodeWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start watching `page`. Returns `true` if it wasn't watched yet.
    pub fn watch(&mut self, page: u32) -> bool {
        let (word, bit) = (page as usize / 64, 1u64 << (page % 64));
        if word >= self.bits.len() {
            self.bits.resize(word + 1, 0);
        }
        let new = self.bits[word] & bit == 0;
        if new {
            self.bits[word] |= bit;
            self.count += 1;
        }
        new
    }

    #[inline]
    pub fn is_watched(&self, page: u32) -> bool {
        self.bits.get(page as usize / 64).is_some_and(|w| w & (1 << (page % 64)) != 0)
    }

    /// Record a write to `page`. A watched page stops being watched and is
    /// reported once by [`CodeWatch::take`]. Only the "anything watched?"
    /// test is inlined, to keep the interpreter's store path small.
    #[inline(always)]
    pub fn note_write(&mut self, page: u32) {
        if self.count != 0 {
            self.note_watched(page);
        }
    }

    #[inline(never)]
    fn note_watched(&mut self, page: u32) {
        if self.is_watched(page) {
            self.hit(page);
        }
    }

    /// [`CodeWatch::note_write`] for every page in `first..=last`.
    pub fn note_range(&mut self, first: u32, last: u32) {
        if self.count != 0 {
            for page in first..=last {
                self.note_write(page);
            }
        }
    }

    #[cold]
    fn hit(&mut self, page: u32) {
        self.bits[page as usize / 64] &= !(1 << (page % 64));
        self.count -= 1;
        self.written.push(page);
    }

    /// Next written page, if any.
    #[inline]
    pub fn take(&mut self) -> Option<u32> {
        self.written.pop()
    }

    /// Watch nothing.
    pub fn clear(&mut self) {
        self.bits.fill(0);
        self.count = 0;
        self.written.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_ends_the_watch_and_is_reported_once() {
        let mut w = CodeWatch::new();
        w.note_write(5); // nothing watched: ignored
        assert!(w.watch(5));
        assert!(!w.watch(5));
        w.note_write(4);
        w.note_write(5);
        w.note_write(5);
        assert_eq!(w.take(), Some(5));
        assert_eq!(w.take(), None);
        assert!(!w.is_watched(5));
        w.watch(200);
        w.note_range(190, 210);
        assert_eq!(w.take(), Some(200));
    }
}
