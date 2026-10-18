//! Waking the emulation loop from other threads.
//!
//! When the guest has nothing to do, the loop sleeps: in realtime idle mode
//! until the wall clock catches up with virtual time (up to 20 ms), and
//! while ROMMON waits for a key (5 ms). Input that arrives meanwhile, a
//! typed key or a frame from a TAP device or UDP tunnel, would wait for the
//! sleep to end; with a [`Wakeup`] its thread ends the sleep at once
//! (docs ch. 18.5).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A flag with a condition variable, shared by the loop and the threads
/// that receive input.
#[derive(Clone, Default)]
pub struct Wakeup(Arc<(Mutex<bool>, Condvar)>);

impl Wakeup {
    /// Something arrived for the guest: end the current (or next) sleep.
    pub fn notify(&self) {
        let (pending, cond) = &*self.0;
        *pending.lock().unwrap() = true;
        cond.notify_one();
    }

    /// Sleep for `duration`, or until notified. A notification that came
    /// while the loop was busy ends the next sleep at once.
    pub fn sleep(&self, duration: Duration) {
        let (pending, cond) = &*self.0;
        let guard = pending.lock().unwrap();
        let (mut guard, _) = cond.wait_timeout_while(guard, duration, |p| !*p).unwrap();
        *guard = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn notify_ends_a_sleep_early() {
        let w = Wakeup::default();
        let other = w.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            other.notify();
        });
        let start = Instant::now();
        w.sleep(Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(2), "slept {:?}", start.elapsed());
        t.join().unwrap();
        // Nothing pending any more: a sleep runs to its end.
        let start = Instant::now();
        w.sleep(Duration::from_millis(30));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }
}
