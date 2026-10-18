//! Idle detection for IOS: don't emulate millions of instructions of a
//! router that is only waiting for its next timer tick.
//!
//! IOS doesn't use the MPC860's low-power modes (no MSR[POW], PLPRCR is
//! only written by the boot ROM). When nothing is ready to run, its
//! scheduler (`0x80617618` in 12.4(8)) loops through the blocked processes
//! calling each one's wake-up predicate, about 330 instructions per pass,
//! until an interrupt changes something. A 60-second profile of an idle
//! router spent all 1.27 billion instructions in that loop (docs ch. 15).
//!
//! The approach is Dynamips' *idle PC*, adapted to an instruction-counted
//! clock:
//!
//! * an **anchor** address in the scheduler loop (`--idle-pc`), executed
//!   once per pass;
//! * the loop counts as idle when the anchor is reached [`CONFIRM`] times
//!   in a row **at exactly the same instruction distance** with interrupts
//!   enabled. Identical passes mean nothing changed: IOS is waiting for an
//!   interrupt. A process that runs, or an interrupt in mid-pass, changes
//!   the distance and resets the streak, so a bad anchor costs speed, not
//!   correctness;
//! * then virtual time jumps to just before the next timer interrupt
//!   (decrementer or PIT), never past it, so interrupts arrive at exactly
//!   the same virtual time as without skipping.
//!
//! Dynamips' clock is the host's, so it can simply sleep. Ours counts
//! instructions, so the skip itself is what saves the work; the emulator
//! then sleeps so that virtual time never runs ahead of the wall clock.

/// What to do with idle time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum IdleMode {
    /// No idle detection.
    Off,
    /// Skip idle time with no wall-clock limit: virtual time races ahead
    /// while the router is idle (protocol timers, long uptimes in seconds).
    /// IOS's console logs out after 10 idle minutes, so press RETURN first
    /// (docs ch. 21).
    Warp,
    /// Skip idle time, but never let virtual time run ahead of the wall
    /// clock: an idle router sleeps on the host and keeps real time.
    Realtime,
}

/// Idle anchors of known images, found by profiling an idle router (docs
/// ch. 15). `--idle-pc auto` uses them after checking that the code at the
/// address is the expected one, so another image, or the memory before IOS
/// is decompressed, never triggers a wrong anchor.
pub struct KnownAnchor {
    pub pc: u32,
    pub code: [u32; 4],
    pub image: &'static str,
}

pub const KNOWN_ANCHORS: &[KnownAnchor] = &[KnownAnchor {
    // scheduler_find_next_process: outer loop head (lis/lwz/lwz/cmpwi)
    pc: 0x8061_7648,
    code: [0x3D20_830D, 0x8129_7A50, 0x8009_0000, 0x2C00_0000],
    image: "IOS 12.4(8) c1700-adventerprisek9-mz",
}];

/// Where the anchor comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorChoice {
    /// Any of [`KNOWN_ANCHORS`] whose code matches.
    Auto,
    /// An address given by the user (`--idle-pc ADDR`), not checked.
    Fixed(u32),
}

impl std::str::FromStr for AnchorChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s == "auto" {
            return Ok(AnchorChoice::Auto);
        }
        u32::from_str_radix(s.trim_start_matches("0x"), 16).map(AnchorChoice::Fixed).map_err(|e| format!("{s}: {e}"))
    }
}

/// Identical passes needed before calling the loop idle.
const CONFIRM: u32 = 3;
/// Longest period (in anchor hits) of a repeating pass: the anchor may be
/// reached several times per pass, e.g. a helper the scheduler calls for
/// every process it checks.
const MAX_PERIOD: usize = 8;
/// A pass longer than this many instructions isn't the idle loop.
const MAX_PASS: u64 = 20_000;
/// Longest single skip when no timer interrupt is armed (1 ms at 48 MHz).
const MAX_SKIP: u64 = 48_000;
/// Skips shorter than this aren't worth it.
const MIN_SKIP: u64 = 1_000;

pub struct Idle {
    pub mode: IdleMode,
    /// How the anchor is chosen (`--idle-pc`), `None` with `--idle off`.
    choice: Option<AnchorChoice>,
    /// The anchor in use (fixed, or a known one once its code matched).
    pub anchor: Option<u32>,
    /// Known anchors still to be checked against the code (auto mode).
    pub candidates: Vec<u32>,
    /// 4 KB pages of the anchor and candidates: the machine only runs its
    /// per-address checks on these pages.
    pub hook_pages: Vec<u32>,
    last_hit: u64,
    /// The most recent distances between anchor hits (ring, newest at `pos`).
    deltas: [u64; 2 * MAX_PERIOD],
    pos: usize,
    streak: u32,
    /// Statistics for `--stats`.
    pub skips: u64,
    pub skipped_cycles: u64,
    /// Why anchor hits didn't count: interrupts off, pass too long, pass
    /// length changed; and the most common pass lengths seen.
    pub rejected: [u64; 3],
    pub pass_lengths: std::collections::HashMap<u64, u64>,
}

impl Idle {
    /// Snapshot state (docs ch. 20): the detector's progress. What is
    /// detected (mode, anchor choice) comes from the command line of the
    /// run that restores: a snapshot saved with `--idle off` restored with
    /// idle detection on gets it, and vice versa. The pass-length histogram
    /// is statistics.
    pub fn visit_state(&mut self, v: &mut dyn ppc8xx::state::Visitor) {
        use ppc8xx::state;
        let (mut anchor, mut candidates, mut hook_pages) = (self.anchor, self.candidates.clone(), self.hook_pages.clone());
        state::opt_u32(v, &mut anchor);
        state::vec_u32(v, &mut candidates);
        state::vec_u32(v, &mut hook_pages);
        if v.loading() {
            match &self.choice {
                None => {}
                Some(AnchorChoice::Fixed(_)) => {} // as configured
                Some(AnchorChoice::Auto) => match anchor {
                    // A recognised anchor is still right: memory is restored too.
                    Some(pc) => {
                        self.anchor = Some(pc);
                        self.candidates.clear();
                        self.hook_pages = vec![pc >> 12];
                    }
                    None => self.reset(true),
                },
            }
        }
        v.u64(&mut self.last_hit);
        state::u64s(v, &mut self.deltas);
        state::usize(v, &mut self.pos);
        v.u32(&mut self.streak);
        v.u64(&mut self.skips);
        v.u64(&mut self.skipped_cycles);
        state::u64s(v, &mut self.rejected);
    }

    pub fn new(mode: IdleMode, choice: Option<AnchorChoice>) -> Self {
        let (anchor, candidates) = match (mode, choice.clone()) {
            (IdleMode::Off, _) | (_, None) => (None, vec![]),
            (_, Some(AnchorChoice::Fixed(pc))) => (Some(pc), vec![]),
            (_, Some(AnchorChoice::Auto)) => (None, KNOWN_ANCHORS.iter().map(|k| k.pc).collect()),
        };
        let hook_pages = anchor.iter().chain(&candidates).map(|pc| pc >> 12).collect();
        Idle {
            mode,
            choice: if mode == IdleMode::Off { None } else { choice },
            anchor,
            candidates,
            hook_pages,
            last_hit: 0,
            deltas: [u64::MAX; 2 * MAX_PERIOD],
            pos: 0,
            streak: 0,
            skips: 0,
            skipped_cycles: 0,
            rejected: [0; 3],
            pass_lengths: Default::default(),
        }
    }

    /// Is `pc` a known anchor still waiting for its code check?
    #[inline]
    pub fn is_candidate(&self, pc: u32) -> bool {
        !self.candidates.is_empty() && self.candidates.contains(&pc)
    }

    /// Check a candidate against the code found at its address (`code` =
    /// the four words there); adopt it if it matches.
    pub fn check_candidate(&mut self, pc: u32, code: [u32; 4]) {
        if let Some(k) = KNOWN_ANCHORS.iter().find(|k| k.pc == pc && k.code == code) {
            log::info!(target: "idle", "idle anchor {pc:#010x} recognised: {}", k.image);
            self.anchor = Some(pc);
            self.candidates.clear();
            self.hook_pages = vec![pc >> 12];
        }
    }

    /// After a reset the code may be different: look for known anchors again.
    pub fn reset(&mut self, auto: bool) {
        if auto && self.mode != IdleMode::Off {
            self.anchor = None;
            self.candidates = KNOWN_ANCHORS.iter().map(|k| k.pc).collect();
            self.hook_pages = self.candidates.iter().map(|pc| pc >> 12).collect();
        }
    }

    /// The CPU is at the anchor PC at `cycles`, with interrupts enabled
    /// (`ee`). Returns true when the loop is confirmed idle.
    #[inline]
    pub fn at_anchor(&mut self, cycles: u64, ee: bool) -> bool {
        let delta = cycles - self.last_hit;
        self.last_hit = cycles;
        if delta <= MAX_PASS && self.pass_lengths.len() < 4096 {
            *self.pass_lengths.entry(delta).or_insert(0) += 1;
        }
        self.pos = (self.pos + 1) % self.deltas.len();
        self.deltas[self.pos] = delta;
        let reason = if !ee {
            Some(0)
        } else if delta > MAX_PASS {
            Some(1)
        } else if self.period().is_none() {
            Some(2)
        } else {
            None
        };
        if let Some(r) = reason {
            self.rejected[r] += 1;
            self.streak = 0;
            return false;
        }
        self.streak += 1;
        self.streak >= CONFIRM
    }

    /// Smallest p such that the last p distances repeat the p before them
    /// (one whole pass seen twice in a row), with a short enough pass.
    fn period(&self) -> Option<usize> {
        let n = self.deltas.len();
        let d = |back: usize| self.deltas[(self.pos + n - back) % n];
        (1..=MAX_PERIOD).find(|&p| {
            (0..p).all(|i| d(i) == d(i + p) && d(i) != u64::MAX) && (0..p).map(d).sum::<u64>() <= MAX_PASS
        })
    }

    /// One line for `--stats`.
    pub fn report(&self) -> String {
        let mut common: Vec<(u64, u64)> = self.pass_lengths.iter().map(|(&d, &n)| (d, n)).collect();
        common.sort_by(|a, b| b.1.cmp(&a.1));
        let common: Vec<String> = common.iter().take(6).map(|(d, n)| format!("{d}x{n}")).collect();
        format!(
            "idle: {} skips, {:.1} s virtual skipped; anchor hits rejected: ints-off {}, long {}, changed {}; common pass lengths {}\r\n",
            self.skips,
            self.skipped_cycles as f64 / 48e6,
            self.rejected[0],
            self.rejected[1],
            self.rejected[2],
            common.join(" ")
        )
    }

    /// How many cycles to skip, given the cycles until the next decrementer
    /// and PIT interrupts. `None` = don't skip. Stops one timebase tick
    /// short so the event happens through normal execution.
    pub fn skip_cycles(&mut self, until_dec: Option<u64>, until_pit: Option<u64>, tb_divider: u64) -> Option<u64> {
        let horizon = [until_dec, until_pit].into_iter().flatten().min().unwrap_or(MAX_SKIP).min(MAX_SKIP * 1000);
        let skip = horizon.saturating_sub(tb_divider) / tb_divider * tb_divider;
        if skip < MIN_SKIP {
            return None;
        }
        self.streak = 0; // the next skip needs fresh evidence
        self.skips += 1;
        self.skipped_cycles += skip;
        Some(skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory snapshot visitor (saves u64s, replays them).
    #[derive(Default)]
    struct Tape {
        words: Vec<u64>,
        pos: usize,
        loading: bool,
    }

    impl ppc8xx::state::Visitor for Tape {
        fn u8(&mut self, v: &mut u8) {
            let mut x = *v as u64;
            self.u64(&mut x);
            *v = x as u8;
        }
        fn u16(&mut self, v: &mut u16) {
            let mut x = *v as u64;
            self.u64(&mut x);
            *v = x as u16;
        }
        fn u32(&mut self, v: &mut u32) {
            let mut x = *v as u64;
            self.u64(&mut x);
            *v = x as u32;
        }
        fn u64(&mut self, v: &mut u64) {
            if self.loading {
                *v = self.words[self.pos];
                self.pos += 1;
            } else {
                self.words.push(*v);
            }
        }
        fn bool(&mut self, v: &mut bool) {
            let mut x = *v as u64;
            self.u64(&mut x);
            *v = x != 0;
        }
        fn bytes_fixed(&mut self, _: &mut [u8]) {}
        fn bytes(&mut self, _: &mut Vec<u8>) {}
        fn fail(&mut self, msg: String) {
            panic!("{msg}");
        }
        fn loading(&self) -> bool {
            self.loading
        }
    }

    /// Snapshots carry the detector's progress, not its configuration:
    /// one saved with --idle off and restored into an auto-mode run must
    /// look for the anchor again (this used to leave idle detection off),
    /// and a recognised anchor survives a restore.
    #[test]
    fn restore_keeps_the_configured_idle_detection() {
        let mut tape = Tape::default();
        Idle::new(IdleMode::Off, None).visit_state(&mut tape);
        tape.loading = true;
        let mut auto = Idle::new(IdleMode::Warp, Some(AnchorChoice::Auto));
        auto.visit_state(&mut tape);
        assert!(auto.is_candidate(KNOWN_ANCHORS[0].pc), "candidates re-armed after restoring an --idle off snapshot");

        let mut recognised = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Auto));
        recognised.check_candidate(KNOWN_ANCHORS[0].pc, KNOWN_ANCHORS[0].code);
        let mut tape = Tape::default();
        recognised.visit_state(&mut tape);
        tape.loading = true;
        let mut restored = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Auto));
        restored.visit_state(&mut tape);
        assert_eq!(restored.anchor, Some(KNOWN_ANCHORS[0].pc));

        let mut tape = Tape::default();
        recognised.visit_state(&mut tape);
        tape.loading = true;
        let mut off = Idle::new(IdleMode::Off, None);
        off.visit_state(&mut tape);
        assert_eq!(off.anchor, None, "--idle off stays off");
    }

    #[test]
    fn needs_identical_short_passes_with_interrupts_enabled() {
        let mut idle = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Fixed(0x1000)));
        let mut t = 0;
        let mut hit = |idle: &mut Idle, step: u64, ee: bool| {
            t += step;
            idle.at_anchor(t, ee)
        };
        assert!(!hit(&mut idle, 330, true)); // first sighting
        assert!(!hit(&mut idle, 330, true)); // streak 1
        assert!(!hit(&mut idle, 330, true)); // streak 2
        assert!(hit(&mut idle, 330, true), "3 identical passes = idle");
        assert!(!hit(&mut idle, 331, true), "a different pass length resets");
        assert!(!hit(&mut idle, 331, false), "interrupts disabled: never idle");
        assert!(!hit(&mut idle, 50_000, true), "a long pass (a process ran) resets");
    }

    #[test]
    fn anchor_reached_twice_per_pass_is_recognised() {
        let mut idle = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Fixed(0x1000)));
        let mut t = 0;
        let mut confirmed = false;
        for _ in 0..8 {
            for step in [120, 215] {
                t += step;
                confirmed = idle.at_anchor(t, true);
            }
        }
        assert!(confirmed, "pass = 120 + 215 instructions, repeating");
    }

    #[test]
    fn auto_anchor_only_after_the_code_matches() {
        let mut idle = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Auto));
        let k = &KNOWN_ANCHORS[0];
        assert!(idle.anchor.is_none() && idle.is_candidate(k.pc));
        idle.check_candidate(k.pc, [0; 4]);
        assert!(idle.anchor.is_none(), "different code (another image): no anchor");
        idle.check_candidate(k.pc, k.code);
        assert_eq!(idle.anchor, Some(k.pc));
        assert!(!idle.is_candidate(k.pc));
        assert!(Idle::new(IdleMode::Off, Some(AnchorChoice::Auto)).candidates.is_empty());
    }

    #[test]
    fn skips_stop_one_tick_before_the_next_timer_interrupt() {
        let mut idle = Idle::new(IdleMode::Realtime, Some(AnchorChoice::Fixed(0x1000)));
        assert_eq!(idle.skip_cycles(Some(10_000), Some(192_000), 16), Some(9_984));
        assert_eq!(idle.skip_cycles(None, Some(192_000), 16), Some(191_984));
        assert_eq!(idle.skip_cycles(Some(100), None, 16), None, "too close to bother");
        assert_eq!(idle.skip_cycles(None, None, 16), Some(MAX_SKIP - 16), "no timer: bounded skip");
        assert_eq!(idle.skips, 3);
    }
}
