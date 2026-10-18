//! A helper for [`Platform`](crate::Platform) implementations: which parts
//! of memory hold translated code, and which of those were written since
//! the JIT last asked.
//!
//! Memory is watched per 4-byte **word** inside 4 KB pages: exactly the
//! instruction words that were translated. Code and data can be neighbours:
//! ROMMON copies its checksum routine onto its stack and runs it there, so
//! the stack frame next to that code is written all the time (docs ch. 17.10,
//! 22). Watching whole pages, or even 32-byte lines, threw code away on every
//! one of those writes.
//!
//! The platform decides what a page number means (it must be the same for
//! every alias of the same memory). It calls [`CodeWatch::note_write`] on
//! every write to plain memory, from the CPU, a DMA engine or a debugger
//! alike. That costs one test while nothing is watched (the interpreter
//! alone never watches anything).

use std::collections::HashMap;

/// Watched words in a 4 KB page.
pub const WORDS_PER_PAGE: u32 = 1024;

/// Identifier of a watched word, as reported by [`CodeWatch::take`]:
/// `page * WORDS_PER_PAGE + offset / 4`.
pub fn word_id(page: u32, offset_in_page: u32) -> u32 {
    page * WORDS_PER_PAGE + (offset_in_page & 0xFFF) / 4
}

/// Which words of a page are watched.
type Mask = [u64; (WORDS_PER_PAGE / 64) as usize];

#[derive(Clone, Default)]
pub struct CodeWatch {
    /// One bit per page number: some line of it is watched.
    pages: Vec<u64>,
    /// Watched words of each such page.
    words: HashMap<u32, Mask>,
    /// Watched words written since the last [`CodeWatch::take`].
    written: Vec<u32>,
}

impl CodeWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start watching the bytes `offset..offset + len` of `page` (inside it).
    pub fn watch(&mut self, page: u32, offset: u32, len: u32) {
        let (word, bit) = (page as usize / 64, 1u64 << (page % 64));
        if word >= self.pages.len() {
            self.pages.resize(word + 1, 0);
        }
        self.pages[word] |= bit;
        let first = (offset & 0xFFF) / 4;
        let last = ((offset & 0xFFF) + len.max(1) - 1).min(0xFFF) / 4;
        let mask = self.words.entry(page).or_default();
        for w in first..=last {
            mask[w as usize / 64] |= 1 << (w % 64);
        }
    }

    /// Is any word of `page` watched? (Then writes to it must be seen.)
    #[inline]
    pub fn is_watched(&self, page: u32) -> bool {
        self.pages.get(page as usize / 64).is_some_and(|w| w & (1 << (page % 64)) != 0)
    }

    /// Record a write at `offset` in `page` (any width: halfwords and words
    /// are aligned, so they lie in one word; misaligned stores arrive as
    /// bytes). A watched word stops being watched and is reported once by
    /// [`CodeWatch::take`]. Only the "anything watched?" test is inlined, to
    /// keep the interpreter's store path small.
    #[inline(always)]
    pub fn note_write(&mut self, page: u32, offset: u32) {
        if !self.words.is_empty() {
            self.note_watched(page, offset);
        }
    }

    #[inline(never)]
    fn note_watched(&mut self, page: u32, offset: u32) {
        if !self.is_watched(page) {
            return;
        }
        let w = (offset & 0xFFF) / 4;
        let Some(mask) = self.words.get_mut(&page) else { return };
        let (i, bit) = (w as usize / 64, 1u64 << (w % 64));
        if mask[i] & bit == 0 {
            return;
        }
        mask[i] &= !bit;
        if mask.iter().all(|&m| m == 0) {
            self.words.remove(&page);
            self.pages[page as usize / 64] &= !(1 << (page % 64));
        }
        self.written.push(word_id(page, offset));
    }

    /// Record writes to every byte of the pages `first..=last` (bulk fills).
    pub fn note_pages(&mut self, first: u32, last: u32) {
        if self.words.is_empty() {
            return;
        }
        for page in first..=last {
            if let Some(mask) = self.words.remove(&page) {
                self.pages[page as usize / 64] &= !(1 << (page % 64));
                let watched = (0..WORDS_PER_PAGE).filter(|&w| mask[w as usize / 64] & (1 << (w % 64)) != 0);
                self.written.extend(watched.map(|w| page * WORDS_PER_PAGE + w));
            }
        }
    }

    /// Next written word, if any.
    #[inline]
    pub fn take(&mut self) -> Option<u32> {
        self.written.pop()
    }

    /// Watch nothing.
    pub fn clear(&mut self) {
        self.pages.fill(0);
        self.words.clear();
        self.written.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_to_a_watched_word_ends_its_watch_and_is_reported_once() {
        let mut w = CodeWatch::new();
        w.note_write(5, 0); // nothing watched: ignored
        w.watch(5, 0x40, 8); // words 0x40 and 0x44
        assert!(w.is_watched(5));
        w.note_write(5, 0x3E); // the halfword just before the code: not reported
        assert_eq!(w.take(), None);
        w.note_write(5, 0x46);
        w.note_write(5, 0x44);
        assert_eq!(w.take(), Some(word_id(5, 0x44)));
        assert_eq!(w.take(), None);
        w.note_write(5, 0x40);
        assert_eq!(w.take(), Some(word_id(5, 0x40)));
        assert!(!w.is_watched(5), "the page has no watched word left");
        w.watch(200, 0xFFC, 0x20); // clamped to the page: the last word only
        w.watch(201, 0, 4);
        w.note_pages(199, 201);
        let mut got = vec![w.take().unwrap(), w.take().unwrap()];
        got.sort();
        assert_eq!(got, vec![word_id(200, 0xFFC), word_id(201, 0)]);
        assert_eq!(w.take(), None);
    }
}
