//! The MPC8xx memory management unit (MPC860UM chapter 8).
//!
//! The 8xx MMU is unlike the classic PowerPC one: no segment registers, no
//! BATs, no hardware table walk. It has two independent 32-entry,
//! fully-associative TLBs (instruction and data) that **software** refills:
//!
//! 1. An access whose page isn't in the TLB raises an *ITLB miss* (0x1100) or
//!    *DTLB miss* (0x1200) exception. The hardware has already put the
//!    missing effective page number in `Mx_EPN`.
//! 2. The handler walks the two-level page table with a little hardware help:
//!    reading `M_TWB` returns the address of the level-one descriptor
//!    (table base + EA bits 0-9 × 4), and after the descriptor is written to
//!    `MD_TWC`, reading `MD_TWC` returns the address of the level-two
//!    descriptor (the PTE).
//! 3. Writing the PTE to `Mx_RPN` loads the TLB entry (at index
//!    `Mx_CTR[INDX]`, which then counts down), and `rfi` retries the access.
//!
//! An entry whose segment (level one) or page (level two) valid bit is 0 is
//! still loaded and *matches*, but using it raises an *ITLB error* (0x1300)
//! or *DTLB error* (0x1400), the 8xx's page faults, as do protection
//! violations, writes to pages whose changed bit C is 0, and instruction
//! fetches from guarded pages.
//!
//! Page sizes are 4 KB, 16 KB, 512 KB and 8 MB; protection comes from the
//! page's PP bits combined with one of 16 access protection groups
//! (`Mx_AP`). Translation is enabled separately for fetches (`MSR[IR]`)
//! and data (`MSR[DR]`); exceptions turn both off.
//!
//! The write-through (WT) and cache-inhibit (CI) attributes are kept in the
//! entries but have no effect: the emulator has no cache model.
//!
//! A linear search of 32 entries on every access would dominate the
//! emulator's run time, so each TLB is fronted by a small direct-mapped
//! cache of recent 4 KB translations ([`FastCache`]), flushed whenever
//! anything that could change a translation is written.

/// MMU special-purpose registers (MPC860UM Table 8-6).
pub mod spr {
    pub const MI_CTR: u16 = 784;
    pub const MI_AP: u16 = 786;
    pub const MI_EPN: u16 = 787;
    pub const MI_TWC: u16 = 789;
    pub const MI_RPN: u16 = 790;
    pub const MD_CTR: u16 = 792;
    pub const M_CASID: u16 = 793;
    pub const MD_AP: u16 = 794;
    pub const MD_EPN: u16 = 795;
    pub const M_TWB: u16 = 796;
    pub const MD_TWC: u16 = 797;
    pub const MD_RPN: u16 = 798;
    pub const M_TW: u16 = 799;
    pub const MI_CAM: u16 = 816;
    pub const MI_RAM0: u16 = 817;
    pub const MI_RAM1: u16 = 818;
    pub const MD_CAM: u16 = 824;
    pub const MD_RAM0: u16 = 825;
    pub const MD_RAM1: u16 = 826;
}

// Mx_CTR fields (Tables 8-7, 8-8). PowerPC bit n = 0x8000_0000 >> n.
const CTR_GPM: u32 = 0x8000_0000; // domain manager group protection mode
const CTR_PPM: u32 = 0x4000_0000; // 1 KB (sub-page) protection resolution
const CTR_RSV4: u32 = 0x0800_0000; // entries 28-31 locked out of replacement
const CTR_TWAM: u32 = 0x0400_0000; // MD_CTR only: 4 KB tablewalk assist
const CTR_PPCS: u32 = 0x0200_0000; // match user/supervisor via RPN[24-27]
const CTR_INDX_SHIFT: u32 = 8; // bits 19-23
const MI_CTR_MASK: u32 = 0xEA00_1F00;
const MD_CTR_MASK: u32 = 0xFE00_1F00;

// Mx_EPN fields (Table 8-9).
const EPN_MASK: u32 = 0xFFFF_F000;
const EPN_EV: u32 = 0x0000_0200;
const EPN_ASID: u32 = 0x0000_000F;

// Mx_TWC fields (Tables 8-10, 8-11) — also the level-one descriptor layout.
const TWC_L2TB: u32 = 0xFFFF_F000;
const TWC_APG_SHIFT: u32 = 5; // bits 23-26
const TWC_G: u32 = 0x0000_0010;
const TWC_PS_SHIFT: u32 = 2; // bits 28-29
const TWC_V: u32 = 0x0000_0001;

// Mx_RPN fields (Tables 8-12, 8-13) — the level-two descriptor (PTE) layout.
const RPN_PP_SHIFT: u32 = 10; // bits 20-21
const RPN_PP1: u32 = 0x0000_0200; // bit 22: extended encoding (modes 1/3)
const RPN_C: u32 = 0x0000_0100; // bit 23: changed (data)
const RPN_SUB_SHIFT: u32 = 4; // bits 24-27: sub-page valid / PPCS bits
const RPN_SPS: u32 = 0x0000_0008;
const RPN_SH: u32 = 0x0000_0004;
const RPN_V: u32 = 0x0000_0001;

const TLB_ENTRIES: usize = 32;
/// Entries 28..31 are the lockable ones (`RSV4I`/`RSV4D`).
const FIRST_RESERVED: u8 = 28;

/// Status bits reported by the TLB error exceptions (Tables 6-15, 6-16):
/// SRR1 for the ITLB, DSISR for the DTLB.
pub const FAULT_NO_TRANSLATION: u32 = 0x4000_0000; // bit 1: segment/page invalid
pub const FAULT_GUARDED: u32 = 0x1000_0000; // bit 3 (ITLB): fetch from guarded page
pub const FAULT_PROTECTION: u32 = 0x0800_0000; // bit 4: protection / changed bit
pub const FAULT_STORE: u32 = 0x0200_0000; // bit 6 (DTLB): the access was a store

/// What the core is doing with an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

/// A translation that failed. The core turns it into the matching exception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// No TLB entry matches: ITLB/DTLB miss (software reloads the TLB).
    Miss,
    /// An entry matches but the access isn't allowed: ITLB/DTLB error, with
    /// the `FAULT_*` status bits.
    Error(u32),
}

/// One TLB entry, decoded from the EPN, TWC and RPN values it was loaded with.
#[derive(Clone, Copy, Debug, Default)]
struct Entry {
    /// `EPN[EV]`: the entry takes part in matching.
    present: bool,
    epn: u32,
    asid: u32,
    /// Address bits compared / replaced: 0xFFFF_F000 for 4 KB … 0xFF80_0000 for 8 MB.
    page_mask: u32,
    rpn: u32,
    /// Raw level-one attributes (TWC) and level-two descriptor (RPN).
    twc: u32,
    pte: u32,
}

impl Entry {
    fn covers(&self, ea: u32) -> bool {
        self.present && (ea & self.page_mask) == self.epn
    }
    fn overlaps(&self, other: &Entry) -> bool {
        let mask = self.page_mask & other.page_mask; // the larger page decides
        self.present && (self.epn & mask) == (other.epn & mask)
    }
    fn apg(&self) -> u32 {
        (self.twc >> TWC_APG_SHIFT) & 0xF
    }
    fn sub_bits(&self) -> u32 {
        (self.pte >> RPN_SUB_SHIFT) & 0xF
    }
}

/// Page size from the level-one PS field and the level-two SPS bit (Table 8-5).
fn page_mask(twc: u32, pte: u32) -> u32 {
    match ((twc >> TWC_PS_SHIFT) & 3, pte & RPN_SPS != 0) {
        (0, false) => 0xFFFF_F000, // 4 KB
        (0, true) => 0xFFFF_C000,  // 16 KB
        (1, _) => 0xFFF8_0000,     // 512 KB
        (3, _) => 0xFF80_0000,     // 8 MB
        _ => 0xFFFF_F000,          // reserved encoding
    }
}

/// Permission bits cached per 4 KB page for the fast path.
const P_SUP_READ: u8 = 1;
const P_SUP_WRITE: u8 = 2;
const P_SUP_EXEC: u8 = 4;
const P_USER_READ: u8 = 8;
const P_USER_WRITE: u8 = 16;
const P_USER_EXEC: u8 = 32;

#[derive(Clone, Copy, Default)]
struct FastEntry {
    /// `(ea >> 12) + 1`, 0 = empty.
    tag: u32,
    pa_page: u32,
    perms: u8,
}

/// Direct-mapped memo of recent successful translations (4 KB granularity).
#[derive(Clone)]
struct FastCache {
    entries: Box<[FastEntry; 256]>,
    /// Number of flushes: any change that can alter a translation flushes,
    /// so the JIT's copies stay valid while this doesn't move.
    flushes: u64,
}

impl FastCache {
    fn new() -> Self {
        FastCache { entries: Box::new([FastEntry::default(); 256]), flushes: 0 }
    }
    #[inline]
    fn lookup(&self, ea: u32, need: u8) -> Option<u32> {
        let e = &self.entries[(ea >> 12) as usize & 255];
        (e.tag == (ea >> 12) + 1 && e.perms & need != 0).then_some(e.pa_page | (ea & 0xFFF))
    }
    fn insert(&mut self, ea: u32, pa_page: u32, perms: u8) {
        self.entries[(ea >> 12) as usize & 255] = FastEntry { tag: (ea >> 12) + 1, pa_page, perms };
    }
    fn flush(&mut self) {
        self.entries.fill(FastEntry::default());
        self.flushes += 1;
    }
}

/// One of the two TLBs with its control/source registers.
#[derive(Clone)]
struct Tlb {
    entries: [Entry; TLB_ENTRIES],
    ctr: u32,
    epn: u32,
    twc: u32,
    rpn: u32,
    ap: u32,
    /// Debug read-out registers (CAM, RAM0, RAM1), refreshed by writing CAM.
    debug: [u32; 3],
    fast: FastCache,
}

impl Tlb {
    fn new() -> Self {
        Tlb {
            entries: [Entry::default(); TLB_ENTRIES],
            ctr: 0,
            epn: 0,
            twc: 0,
            rpn: 0,
            ap: 0,
            debug: [0; 3],
            fast: FastCache::new(),
        }
    }

    fn index(&self) -> usize {
        ((self.ctr >> CTR_INDX_SHIFT) & 0x1F) as usize
    }

    /// `mtspr Mx_RPN`: load the entry at `INDX` from EPN/TWC/RPN, invalidate
    /// any other entry it overlaps (§8.3.2), then decrement `INDX` modulo 32,
    /// or modulo 28 when the four top entries are reserved.
    fn load(&mut self, pte: u32) {
        self.rpn = pte;
        let twc = self.twc;
        let mask = page_mask(twc, pte);
        let new = Entry {
            present: self.epn & EPN_EV != 0,
            epn: self.epn & mask,
            asid: self.epn & EPN_ASID,
            page_mask: mask,
            rpn: pte & mask,
            twc,
            pte,
        };
        let idx = self.index();
        log::debug!(target: "mmu", "TLB load [{idx:2}] EPN {:#010x}{} TWC {twc:#010x} PTE {pte:#010x} -> {:#010x}, {} KB",
            new.epn, if new.present { "" } else { " (not valid)" }, new.rpn, (!mask >> 10) + 1);
        if new.present {
            for (i, e) in self.entries.iter_mut().enumerate() {
                let same_space = new.pte & RPN_SH != 0 || e.pte & RPN_SH != 0 || e.asid == new.asid;
                if i != idx && same_space && e.overlaps(&new) {
                    e.present = false;
                }
            }
        }
        self.entries[idx] = new;
        let top = if self.ctr & CTR_RSV4 != 0 { FIRST_RESERVED as usize - 1 } else { TLB_ENTRIES - 1 };
        let next = if idx == 0 { top } else { (idx - 1).min(top) };
        self.ctr = (self.ctr & !(0x1F << CTR_INDX_SHIFT)) | ((next as u32) << CTR_INDX_SHIFT);
        self.fast.flush();
    }

    fn invalidate_all(&mut self) {
        let keep_reserved = self.ctr & CTR_RSV4 != 0;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if !(keep_reserved && i >= FIRST_RESERVED as usize) {
                e.present = false;
            }
        }
        self.ctr &= !(0x1F << CTR_INDX_SHIFT); // "replacement counters are cleared"
        self.fast.flush();
    }

    fn invalidate(&mut self, ea: u32) {
        for e in self.entries.iter_mut() {
            if e.covers(ea) {
                e.present = false;
            }
        }
        self.fast.flush();
    }

    /// The entry that translates `ea` for this privilege level and address space.
    fn find(&self, ea: u32, user: bool, casid: u32) -> Option<usize> {
        let ppcs = self.ctr & CTR_PPCS != 0;
        let ppm = self.ctr & CTR_PPM != 0;
        self.entries.iter().position(|e| {
            if !e.covers(ea) || (e.pte & RPN_SH == 0 && e.asid != casid) {
                return false;
            }
            let sub = e.sub_bits();
            if ppcs {
                // 1000 = supervisor only, 0100 = user only, 1100 = both
                sub & if user { 0b0100 } else { 0b1000 } != 0
            } else if !ppm && e.page_mask == 0xFFFF_F000 {
                // mode 1/3: bits 24-27 are the validity of the four 1 KB sub-pages
                sub & (0b1000 >> ((ea >> 10) & 3)) != 0
            } else {
                true
            }
        })
    }

    fn read_debug(&mut self) {
        // MI_CAM/MD_CAM etc. (§8.8.12) are only used by debuggers; expose the
        // raw EPN/TWC/RPN of the entry at INDX, which is enough to inspect it.
        let e = self.entries[self.index()];
        self.debug = [e.epn | if e.present { EPN_EV } else { 0 } | e.asid, e.twc, e.pte];
    }
}

/// What a TLB entry allows: `(sup_read, sup_write, user_read, user_write)` for
/// data pages, `(sup_exec, user_exec)` folded into the read flags for
/// instruction pages.
fn page_permissions(entry: &Entry, ctr: u32, ea: u32, instruction: bool) -> (bool, bool, bool, bool) {
    let pte = entry.pte;
    if ctr & CTR_PPM != 0 {
        // Mode 2: one PP pair per 1 KB sub-page (Table 8-4 left column).
        let pp = if entry.page_mask == 0xFFFF_F000 { (pte >> (10 - 2 * ((ea >> 10) & 3))) & 3 } else { (pte >> RPN_PP_SHIFT) & 3 };
        return if instruction {
            match pp {
                0 => (false, false, false, false),
                1 => (true, false, false, false),
                _ => (true, false, true, false),
            }
        } else {
            match pp {
                0 => (false, false, false, false),
                1 => (true, true, false, false),
                2 => (true, true, true, false),
                _ => (true, true, true, true),
            }
        };
    }
    let pp = (pte >> RPN_PP_SHIFT) & 3;
    let extended = pte & RPN_PP1 != 0;
    match (instruction, extended, pp) {
        // instruction pages: basic 00 = supervisor only, else everyone;
        // extended 00 = nobody, 01 = supervisor only, 1x reserved
        (true, false, 0) => (true, false, false, false),
        (true, false, _) => (true, false, true, false),
        (true, true, 1) => (true, false, false, false),
        (true, true, _) => (false, false, false, false),
        // data pages, basic: 00 RW/-, 01 RW/RO, 10 RW/RW, 11 RO/RO
        (false, false, 0) => (true, true, false, false),
        (false, false, 1) => (true, true, true, false),
        (false, false, 2) => (true, true, true, true),
        (false, false, _) => (true, false, true, false),
        // data pages, extended: 00 -/-, 01 RO/-, 1x reserved
        (false, true, 1) => (true, false, false, false),
        (false, true, _) => (false, false, false, false),
    }
}

/// Apply the access protection group (`Mx_AP`, Table 8-16) to the page
/// permissions. Returns `None` for "no access", or the permissions as seen
/// by a supervisor and by a user access.
fn group_permissions(entry: &Entry, ctr: u32, ap: u32, perms: (bool, bool, bool, bool)) -> (bool, bool, bool, bool) {
    let gp = (ap >> (30 - 2 * entry.apg())) & 3;
    let (sr, sw, ur, uw) = perms;
    if ctr & CTR_GPM != 0 {
        // domain manager mode: 00 no access, 01/10 client (page bits), 11 manager (free access)
        match gp {
            0 => (false, false, false, false),
            3 => (true, true, true, true),
            _ => perms,
        }
    } else {
        // default mode: 00 everyone is treated as supervisor, 01 page bits,
        // 10 user/supervisor swapped, 11 everyone is treated as user
        match gp {
            0 => (sr, sw, sr, sw),
            1 => perms,
            2 => (ur, uw, sr, sw),
            _ => (ur, uw, ur, uw),
        }
    }
}

/// The instruction and data MMUs.
#[derive(Clone)]
pub struct Mmu {
    itlb: Tlb,
    dtlb: Tlb,
    m_twb: u32,
    m_casid: u32,
    m_tw: u32,
    /// Counters for `--stats`.
    pub itlb_misses: u64,
    pub dtlb_misses: u64,
    pub tlb_errors: u64,
}

impl Default for Mmu {
    fn default() -> Self {
        Self::new()
    }
}

impl Mmu {
    pub fn new() -> Self {
        let mut mmu = Mmu {
            itlb: Tlb::new(),
            dtlb: Tlb::new(),
            m_twb: 0,
            m_casid: 0,
            m_tw: 0,
            itlb_misses: 0,
            dtlb_misses: 0,
            tlb_errors: 0,
        };
        mmu.dtlb.ctr = CTR_TWAM; // MD_CTR reset value: 4 KB tablewalk assist
        mmu
    }

    /// Translate `ea`; on a miss, record it in `Mx_EPN`/`Mx_TWC` like the
    /// hardware does before the exception (§8.10.1).
    #[inline]
    pub fn translate(&mut self, ea: u32, access: Access, user: bool) -> Result<u32, Fault> {
        if let Some(pa) = self.cached(ea, access, user) {
            return Ok(pa);
        }
        self.translate_slow(ea, access, user)
    }

    /// The translation of `ea` if it is in the fast cache. An entry there
    /// means the whole 4 KB granule around `ea` translates the same way with
    /// the same permission (no 1 KB sub-page differences), until
    /// [`Mmu::data_generation`] (or, for fetches, the next TLB change) moves.
    #[inline]
    pub fn cached(&self, ea: u32, access: Access, user: bool) -> Option<u32> {
        let need = match (access, user) {
            (Access::Fetch, false) => P_SUP_EXEC,
            (Access::Fetch, true) => P_USER_EXEC,
            (Access::Load, false) => P_SUP_READ,
            (Access::Load, true) => P_USER_READ,
            (Access::Store, false) => P_SUP_WRITE,
            (Access::Store, true) => P_USER_WRITE,
        };
        let tlb = if access == Access::Fetch { &self.itlb } else { &self.dtlb };
        tlb.fast.lookup(ea, need)
    }

    /// Changes whenever a data translation may have changed (TLB load or
    /// invalidation, protection or address-space registers).
    pub fn data_generation(&self) -> u64 {
        self.dtlb.fast.flushes
    }

    fn translate_slow(&mut self, ea: u32, access: Access, user: bool) -> Result<u32, Fault> {
        let instruction = access == Access::Fetch;
        let casid = self.m_casid;
        let tlb = if instruction { &mut self.itlb } else { &mut self.dtlb };
        let Some(i) = tlb.find(ea, user, casid) else {
            tlb.epn = (ea & EPN_MASK) | EPN_EV | casid;
            let n = if instruction {
                tlb.twc = TWC_V; // APG 0, not guarded, small page, valid
                self.itlb_misses += 1;
                self.itlb_misses
            } else {
                tlb.twc = (tlb.twc & TWC_L2TB) | TWC_V;
                self.dtlb_misses += 1;
                self.dtlb_misses
            };
            if n <= 16 || n.is_power_of_two() {
                log::debug!(target: "mmu", "{} miss #{n} at {ea:#010x} ({})",
                    if instruction { "ITLB" } else { "DTLB" }, if user { "user" } else { "supervisor" });
            }
            return Err(Fault::Miss);
        };
        let e = tlb.entries[i];
        let store = access == Access::Store;
        let store_bit = if store { FAULT_STORE } else { 0 };
        if e.twc & TWC_V == 0 || e.pte & RPN_V == 0 {
            self.tlb_errors += 1;
            return Err(Fault::Error(FAULT_NO_TRANSLATION | store_bit));
        }
        if instruction && e.twc & TWC_G != 0 {
            self.tlb_errors += 1;
            return Err(Fault::Error(FAULT_GUARDED));
        }
        let (sr, sw, ur, uw) = group_permissions(&e, tlb.ctr, tlb.ap, page_permissions(&e, tlb.ctr, ea, instruction));
        // A clear changed bit write-protects a data page: the write invalidates
        // the entry and raises a DTLB error so software can set C (§8.6).
        let changed = instruction || e.pte & RPN_C != 0;
        let allowed = match (access, user) {
            (Access::Store, false) => sw && changed,
            (Access::Store, true) => uw && changed,
            (_, false) => sr,
            (_, true) => ur,
        };
        if !allowed {
            if store && (if user { uw } else { sw }) && !changed {
                tlb.entries[i].present = false;
                tlb.fast.flush();
            }
            self.tlb_errors += 1;
            return Err(Fault::Error(FAULT_PROTECTION | store_bit));
        }
        let pa = e.rpn | (ea & !e.page_mask);
        // Cache the 4 KB granule unless 1 KB sub-pages could differ inside it.
        let uniform = e.page_mask != 0xFFFF_F000 || tlb.ctr & CTR_PPM == 0 && (tlb.ctr & CTR_PPCS != 0 || e.sub_bits() == 0xF);
        if uniform && tlb.ctr & CTR_PPCS == 0 {
            let perms = if instruction {
                (if sr { P_SUP_EXEC } else { 0 }) | if ur { P_USER_EXEC } else { 0 }
            } else {
                (if sr { P_SUP_READ } else { 0 })
                    | (if sw && changed { P_SUP_WRITE } else { 0 })
                    | (if ur { P_USER_READ } else { 0 })
                    | (if uw && changed { P_USER_WRITE } else { 0 })
            };
            tlb.fast.insert(ea, pa & 0xFFFF_F000, perms);
        } else if uniform {
            // With PPCS the entry only matches one privilege level: cache
            // the permissions of the level that matched.
            let bits = match (instruction, user) {
                (true, false) => if sr { P_SUP_EXEC } else { 0 },
                (true, true) => if ur { P_USER_EXEC } else { 0 },
                (false, false) => (if sr { P_SUP_READ } else { 0 }) | if sw && changed { P_SUP_WRITE } else { 0 },
                (false, true) => (if ur { P_USER_READ } else { 0 }) | if uw && changed { P_USER_WRITE } else { 0 },
            };
            tlb.fast.insert(ea, pa & 0xFFFF_F000, bits);
        }
        Ok(pa)
    }

    /// Translate without side effects (debugger memory access). `None` if
    /// no TLB entry maps `ea` usably.
    pub fn probe(&self, ea: u32, instruction: bool) -> Option<u32> {
        let tlb = if instruction { &self.itlb } else { &self.dtlb };
        let e = &tlb.entries[tlb.find(ea, false, self.m_casid)?];
        (e.twc & TWC_V != 0 && e.pte & RPN_V != 0).then(|| e.rpn | (ea & !e.page_mask))
    }

    /// `tlbie`: invalidate the entries mapping `ea` in both TLBs, whatever
    /// their ASID (§8.10.4).
    pub fn tlbie(&mut self, ea: u32) {
        self.itlb.invalidate(ea);
        self.dtlb.invalidate(ea);
    }

    /// `tlbia`: invalidate everything except the reserved entries.
    pub fn tlbia(&mut self) {
        self.itlb.invalidate_all();
        self.dtlb.invalidate_all();
    }

    /// `mfspr` of an MMU register; `None` if `n` isn't one.
    pub fn read_spr(&mut self, n: u16) -> Option<u32> {
        let twam = self.dtlb.ctr & CTR_TWAM != 0;
        Some(match n {
            spr::MI_CTR => self.itlb.ctr,
            spr::MD_CTR => self.dtlb.ctr,
            spr::MI_AP => self.itlb.ap,
            spr::MD_AP => self.dtlb.ap,
            spr::MI_EPN => self.itlb.epn,
            spr::MD_EPN => self.dtlb.epn,
            spr::MI_TWC => self.itlb.twc,
            spr::MI_RPN => self.itlb.rpn,
            spr::MD_RPN => self.dtlb.rpn,
            spr::M_CASID => self.m_casid,
            spr::M_TW => self.m_tw,
            // Hardware assist 1: address of the level-one descriptor.
            spr::M_TWB => {
                if twam {
                    (self.m_twb & 0xFFFF_F000) | ((self.dtlb.epn >> 20) & 0xFFC)
                } else {
                    (self.m_twb & 0xFFFF_C000) | ((self.dtlb.epn >> 18) & 0x3FFC)
                }
            }
            // Hardware assist 2: address of the level-two descriptor, L2 base +
            // EA[10-19] * 4 (Table 8-11). Index bits that lie *inside* a large
            // page don't take part, so each page has exactly one level-two
            // entry (Table 8-1): for 8 MB pages the index is 0 ("the level-two
            // base is used directly", §8.7); for 512 KB pages EA[10-12] still
            // select one of the eight pages of the 4 MB segment. IOS relies on
            // this for its boot ROM mapping (0xFFF00000, EA[10-12] = 0b110).
            spr::MD_TWC => {
                let twc = self.dtlb.twc;
                let index = if twam { (self.dtlb.epn >> 10) & 0xFFC } else { (self.dtlb.epn >> 8) & 0xFFC };
                match (twc >> TWC_PS_SHIFT) & 3 {
                    3 => twc & TWC_L2TB,
                    1 => (twc & TWC_L2TB) | (index & 0xE00),
                    _ => (twc & TWC_L2TB) | index,
                }
            }
            spr::MI_CAM => self.itlb.debug[0],
            spr::MI_RAM0 => self.itlb.debug[1],
            spr::MI_RAM1 => self.itlb.debug[2],
            spr::MD_CAM => self.dtlb.debug[0],
            spr::MD_RAM0 => self.dtlb.debug[1],
            spr::MD_RAM1 => self.dtlb.debug[2],
            _ => return None,
        })
    }

    /// `mtspr` of an MMU register; `false` if `n` isn't one.
    pub fn write_spr(&mut self, n: u16, v: u32) -> bool {
        match n {
            spr::MI_CTR => {
                self.itlb.ctr = v & MI_CTR_MASK;
                self.itlb.fast.flush();
            }
            spr::MD_CTR => {
                self.dtlb.ctr = v & MD_CTR_MASK;
                self.dtlb.fast.flush();
            }
            spr::MI_AP => {
                self.itlb.ap = v;
                self.itlb.fast.flush();
            }
            spr::MD_AP => {
                self.dtlb.ap = v;
                self.dtlb.fast.flush();
            }
            spr::MI_EPN => self.itlb.epn = v & (EPN_MASK | EPN_EV | EPN_ASID),
            spr::MD_EPN => self.dtlb.epn = v & (EPN_MASK | EPN_EV | EPN_ASID),
            spr::MI_TWC => self.itlb.twc = v & 0x0000_01FD,
            spr::MD_TWC => self.dtlb.twc = v & 0xFFFF_F1FF,
            spr::MI_RPN => self.itlb.load(v),
            spr::MD_RPN => self.dtlb.load(v),
            spr::M_TWB => self.m_twb = v & 0xFFFF_F000,
            spr::M_CASID => {
                self.m_casid = v & 0xF;
                self.itlb.fast.flush();
                self.dtlb.fast.flush();
            }
            spr::M_TW => self.m_tw = v,
            spr::MI_CAM => self.itlb.read_debug(),
            spr::MD_CAM => self.dtlb.read_debug(),
            spr::MI_RAM0 | spr::MI_RAM1 | spr::MD_RAM0 | spr::MD_RAM1 => {} // no-ops
            _ => return false,
        }
        true
    }

    /// Number of valid entries in the (instruction, data) TLBs, for `--stats`.
    pub fn occupancy(&self) -> (usize, usize) {
        let count = |t: &Tlb| t.entries.iter().filter(|e| e.present).count();
        (count(&self.itlb), count(&self.dtlb))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load a DTLB entry the way a reload handler does.
    fn load_d(mmu: &mut Mmu, ea: u32, twc: u32, pte: u32) {
        mmu.write_spr(spr::MD_EPN, (ea & EPN_MASK) | EPN_EV);
        mmu.write_spr(spr::MD_TWC, twc);
        mmu.write_spr(spr::MD_RPN, pte);
    }

    const PTE_RW: u32 = (2 << RPN_PP_SHIFT) | RPN_C | (0xF << RPN_SUB_SHIFT) | RPN_V; // RW for everyone
    fn group_client(mmu: &mut Mmu) {
        mmu.write_spr(spr::MD_AP, 0x5555_5555);
        mmu.write_spr(spr::MI_AP, 0x5555_5555);
    }

    #[test]
    fn miss_records_epn_and_reload_assists_compute_descriptor_addresses() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        mmu.write_spr(spr::M_TWB, 0x0038_B000);
        assert_eq!(mmu.translate(0x1234_5678, Access::Load, false), Err(Fault::Miss));
        assert_eq!(mmu.read_spr(spr::MD_EPN), Some(0x1234_5000 | EPN_EV));
        // level-one descriptor address: base + EA[0-9] * 4
        assert_eq!(mmu.read_spr(spr::M_TWB), Some(0x0038_B000 | (0x048 << 2)));
        // the handler writes the L1 descriptor (L2 table at 0x0040_0000, small pages, valid)
        mmu.write_spr(spr::MD_TWC, 0x0040_0000 | TWC_V);
        // level-two descriptor address: L2 base + EA[10-19] * 4
        assert_eq!(mmu.read_spr(spr::MD_TWC), Some(0x0040_0000 | (0x345 << 2)));
        mmu.write_spr(spr::MD_RPN, 0x0070_0000 | PTE_RW);
        assert_eq!(mmu.translate(0x1234_5678, Access::Load, false), Ok(0x0070_0678));
        assert_eq!(mmu.translate(0x1234_5ABC, Access::Store, true), Ok(0x0070_0ABC));
        // index counted down from 0 (reset) to 31
        assert_eq!(mmu.read_spr(spr::MD_CTR).unwrap() >> CTR_INDX_SHIFT & 0x1F, 31);
    }

    #[test]
    fn eight_megabyte_pages_and_direct_level_two_pointer() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        mmu.write_spr(spr::MD_EPN, 0x8000_0000 | EPN_EV);
        mmu.write_spr(spr::MD_TWC, 0x0001_2000 | (3 << TWC_PS_SHIFT) | TWC_V);
        assert_eq!(mmu.read_spr(spr::MD_TWC), Some(0x0001_2000), "8 MB: L2 base used directly");
        mmu.write_spr(spr::MD_RPN, PTE_RW | RPN_SPS); // RPN 0: maps 0x8000_0000 -> 0
        assert_eq!(mmu.translate(0x807F_FFFC, Access::Load, false), Ok(0x007F_FFFC));
        assert_eq!(mmu.translate(0x8080_0000, Access::Load, false), Err(Fault::Miss));
        // 512 KB page: EA[10-12] index the level-two table, EA[13-19] don't
        mmu.write_spr(spr::MD_EPN, 0xFFF0_3000 | EPN_EV);
        mmu.write_spr(spr::MD_TWC, 0x0003_8000 | (1 << TWC_PS_SHIFT) | TWC_V);
        assert_eq!(mmu.read_spr(spr::MD_TWC), Some(0x0003_8000 | 0xC00));
    }

    #[test]
    fn invalid_page_is_an_error_not_a_miss_and_protection_is_enforced() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        load_d(&mut mmu, 0x1000, TWC_V, 0x2000 | PTE_RW & !RPN_V);
        assert_eq!(mmu.translate(0x1000, Access::Load, false), Err(Fault::Error(FAULT_NO_TRANSLATION)));
        // basic encoding 11 = read-only for everyone
        load_d(&mut mmu, 0x3000, TWC_V, 0x4000 | (3 << RPN_PP_SHIFT) | RPN_C | (0xF << RPN_SUB_SHIFT) | RPN_V);
        assert_eq!(mmu.translate(0x3004, Access::Load, true), Ok(0x4004));
        assert_eq!(mmu.translate(0x3004, Access::Store, false), Err(Fault::Error(FAULT_PROTECTION | FAULT_STORE)));
        // basic encoding 00 = supervisor only
        load_d(&mut mmu, 0x5000, TWC_V, 0x6000 | RPN_C | (0xF << RPN_SUB_SHIFT) | RPN_V);
        assert_eq!(mmu.translate(0x5000, Access::Store, false), Ok(0x6000));
        assert_eq!(mmu.translate(0x5000, Access::Load, true), Err(Fault::Error(FAULT_PROTECTION)));
    }

    #[test]
    fn clear_changed_bit_write_protects_and_drops_the_entry() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        load_d(&mut mmu, 0x7000, TWC_V, 0x8000 | PTE_RW & !RPN_C);
        assert_eq!(mmu.translate(0x7000, Access::Load, false), Ok(0x8000));
        assert_eq!(mmu.translate(0x7000, Access::Store, false), Err(Fault::Error(FAULT_PROTECTION | FAULT_STORE)));
        assert_eq!(mmu.translate(0x7000, Access::Load, false), Err(Fault::Miss), "entry invalidated");
    }

    #[test]
    fn group_protection_modes() {
        let mut mmu = Mmu::new();
        load_d(&mut mmu, 0x9000, TWC_V, 0xA000 | RPN_C | (0xF << RPN_SUB_SHIFT) | RPN_V); // supervisor RW only
        mmu.write_spr(spr::MD_AP, 0xC000_0000); // group 0 = 11: everyone treated as user
        assert_eq!(mmu.translate(0x9000, Access::Load, false), Err(Fault::Error(FAULT_PROTECTION)));
        mmu.write_spr(spr::MD_AP, 0x0000_0000); // group 0 = 00: everyone treated as supervisor
        assert_eq!(mmu.translate(0x9000, Access::Load, true), Ok(0xA000));
        mmu.write_spr(spr::MD_CTR, CTR_TWAM | CTR_GPM); // domain manager mode, group 0 = 00: no access
        assert_eq!(mmu.translate(0x9000, Access::Load, false), Err(Fault::Error(FAULT_PROTECTION)));
    }

    #[test]
    fn asid_shared_ppcs_and_guarded_fetch() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        mmu.write_spr(spr::MD_EPN, 0xB000 | EPN_EV | 3); // ASID 3, not shared
        mmu.write_spr(spr::MD_TWC, TWC_V);
        mmu.write_spr(spr::MD_RPN, 0xC000 | PTE_RW);
        assert_eq!(mmu.translate(0xB000, Access::Load, false), Err(Fault::Miss), "CASID 0 != ASID 3");
        mmu.write_spr(spr::M_CASID, 3);
        assert_eq!(mmu.translate(0xB000, Access::Load, false), Ok(0xC000));
        // PPCS: RPN[24-27] = 1000 matches supervisor accesses only
        mmu.write_spr(spr::MD_CTR, CTR_TWAM | CTR_PPCS);
        load_d(&mut mmu, 0xD000, TWC_V, 0xE000 | (2 << RPN_PP_SHIFT) | RPN_C | (0b1000 << RPN_SUB_SHIFT) | RPN_SH | RPN_V);
        assert_eq!(mmu.translate(0xD000, Access::Load, false), Ok(0xE000));
        assert_eq!(mmu.translate(0xD000, Access::Load, true), Err(Fault::Miss));
        // guarded pages can't be executed
        mmu.write_spr(spr::MI_EPN, 0xF000 | EPN_EV);
        mmu.write_spr(spr::MI_TWC, TWC_G | TWC_V);
        mmu.write_spr(spr::MI_RPN, 0xF000 | PTE_RW | RPN_SH);
        assert_eq!(mmu.translate(0xF000, Access::Fetch, false), Err(Fault::Error(FAULT_GUARDED)));
    }

    #[test]
    fn tlbia_keeps_reserved_entries_and_tlbie_drops_one_page() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        // locked entry at index 31, then reserve the top four
        mmu.write_spr(spr::MD_CTR, CTR_TWAM | (31 << CTR_INDX_SHIFT));
        load_d(&mut mmu, 0x0010_0000, TWC_V, 0x0010_0000 | PTE_RW);
        mmu.write_spr(spr::MD_CTR, CTR_TWAM | CTR_RSV4 | (27 << CTR_INDX_SHIFT));
        load_d(&mut mmu, 0x0020_0000, TWC_V, 0x0020_0000 | PTE_RW);
        assert_eq!(mmu.read_spr(spr::MD_CTR).unwrap() >> CTR_INDX_SHIFT & 0x1F, 26);
        mmu.tlbia();
        assert!(mmu.translate(0x0010_0004, Access::Load, false).is_ok(), "reserved entry survives tlbia");
        assert_eq!(mmu.translate(0x0020_0004, Access::Load, false), Err(Fault::Miss));
        mmu.tlbie(0x0010_0FFF);
        assert_eq!(mmu.translate(0x0010_0004, Access::Load, false), Err(Fault::Miss), "tlbie reaches reserved entries");
    }

    #[test]
    fn reloading_an_overlapping_page_replaces_the_old_entry() {
        let mut mmu = Mmu::new();
        group_client(&mut mmu);
        load_d(&mut mmu, 0x4000, TWC_V, 0x1_0000 | PTE_RW);
        load_d(&mut mmu, 0x4000, TWC_V, 0x2_0000 | PTE_RW);
        assert_eq!(mmu.translate(0x4010, Access::Load, false), Ok(0x2_0010));
        assert_eq!(mmu.occupancy().1, 1);
    }
}
