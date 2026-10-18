//! Machine state for snapshots (docs ch. 20).
//!
//! Every component that has state implements one method, `visit_state(&mut
//! self, v: &mut dyn Visitor)`, which walks its fields in a fixed order. The
//! same walk saves (the visitor reads each field and writes it out) and
//! restores (the visitor overwrites each field with what it reads). One
//! field list for both directions, so saving and loading can't drift apart.
//!
//! Restoring happens *into* a machine built normally from the same
//! configuration: things that aren't state (ROM contents, chip descriptions,
//! file paths, caches) are simply not visited and keep their values.

/// Saves or loads the fields it is given.
pub trait Visitor {
    fn u8(&mut self, v: &mut u8);
    fn u16(&mut self, v: &mut u16);
    fn u32(&mut self, v: &mut u32);
    fn u64(&mut self, v: &mut u64);
    fn bool(&mut self, v: &mut bool);
    /// A buffer whose length is fixed by the configuration (RAM, DPRAM):
    /// when loading, the stored length must match.
    fn bytes_fixed(&mut self, v: &mut [u8]);
    /// A buffer of any length (FIFOs, queues).
    fn bytes(&mut self, v: &mut Vec<u8>);
    /// Loading: the data doesn't fit this machine (records the first error).
    fn fail(&mut self, msg: String);
    fn loading(&self) -> bool;
}

/// A `usize` (stored as u64).
pub fn usize(v: &mut dyn Visitor, x: &mut usize) {
    let mut n = *x as u64;
    v.u64(&mut n);
    *x = n as usize;
}

pub fn opt_u32(v: &mut dyn Visitor, x: &mut Option<u32>) {
    let mut some = x.is_some();
    let mut value = x.unwrap_or(0);
    v.bool(&mut some);
    v.u32(&mut value);
    *x = some.then_some(value);
}

pub fn u32s(v: &mut dyn Visitor, xs: &mut [u32]) {
    for x in xs {
        v.u32(x);
    }
}

pub fn u64s(v: &mut dyn Visitor, xs: &mut [u64]) {
    for x in xs {
        v.u64(x);
    }
}

pub fn u16s(v: &mut dyn Visitor, xs: &mut [u16]) {
    for x in xs {
        v.u16(x);
    }
}

/// A vector of u32 of any length.
pub fn vec_u32(v: &mut dyn Visitor, xs: &mut Vec<u32>) {
    let mut n = xs.len();
    usize(v, &mut n);
    if v.loading() {
        xs.resize(n, 0);
    }
    u32s(v, xs);
}
