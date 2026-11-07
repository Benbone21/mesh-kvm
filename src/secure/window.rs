// Replay window: a 2048-packet sliding bitmap, as in RFC 6479.

pub const WINDOW: u64 = 2048;

const WORDS: usize = (WINDOW / 64) as usize;

#[derive(Default)]
pub struct ReplayWindow {
    bits: [u64; WORDS],
    newest: u64,
    any: bool,
}

impl ReplayWindow {
    // Before decrypting. Changes nothing.
    pub fn may_be_new(&self, counter: u64) -> bool {
        if !self.any || counter > self.newest {
            return true;
        }
        if self.newest - counter >= WINDOW {
            return false;
        }
        !self.is_set(counter)
    }

    // After the tag checks out. True if the counter was new.
    pub fn accept(&mut self, counter: u64) -> bool {
        if !self.may_be_new(counter) {
            return false;
        }
        if !self.any {
            self.any = true;
            self.newest = counter;
        } else if counter > self.newest {
            // The slots of the counters that fall out of the window are the
            // slots of the counters that come in.
            let jump = counter - self.newest;
            if jump >= WINDOW {
                self.bits = [0; WORDS];
            } else {
                self.clear_run(self.newest + 1, jump);
            }
            self.newest = counter;
        }
        let (word, mask) = Self::slot(counter);
        self.bits[word] |= mask;
        true
    }

    fn slot(counter: u64) -> (usize, u64) {
        let slot = (counter % WINDOW) as usize;
        (slot / 64, 1u64 << (slot % 64))
    }

    fn is_set(&self, counter: u64) -> bool {
        let (word, mask) = Self::slot(counter);
        self.bits[word] & mask != 0
    }

    // Clears the slots of `len` counters from `first`, at most one lap.
    fn clear_run(&mut self, first: u64, len: u64) {
        let mut at = first;
        let mut left = len;
        while left > 0 {
            let slot = at % WINDOW;
            let word = (slot / 64) as usize;
            let bit = slot % 64;
            let n = left.min(64 - bit);
            let mask = if n == 64 { !0 } else { ((1u64 << n) - 1) << bit };
            self.bits[word] &= !mask;
            at += n;
            left -= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_in_any_order_are_each_taken_once() {
        let mut w = ReplayWindow::default();
        for c in [5, 3, 9, 1, 8, 2, 100, 50, 99] {
            assert!(w.may_be_new(c));
            assert!(w.accept(c), "{c} should be new");
        }
        for c in [5, 3, 9, 1, 8, 2, 100, 50, 99] {
            assert!(!w.accept(c), "{c} should be a repeat");
        }
    }

    #[test]
    fn a_repeat_is_refused() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(7));
        assert!(!w.may_be_new(7));
        assert!(!w.accept(7));
    }

    #[test]
    fn may_be_new_changes_nothing() {
        let mut w = ReplayWindow::default();
        assert!(w.may_be_new(4));
        assert!(w.may_be_new(4));
        assert!(w.accept(4));
    }

    #[test]
    fn the_oldest_in_the_window_is_taken_and_one_older_is_not() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5000));
        assert!(w.accept(2953));
        assert!(!w.accept(2952));
        assert!(!w.may_be_new(2952));
    }

    #[test]
    fn a_jump_clears_the_bits_it_passes() {
        let mut w = ReplayWindow::default();
        for c in 0..200 {
            assert!(w.accept(c));
        }
        // 200 and 2248 share a slot; the old bit must not make 2248 a repeat.
        assert!(w.accept(2248));
        assert!(!w.accept(2248));
        // 200..2248 were never seen and are still in the window.
        assert!(w.accept(1000));
        assert!(w.accept(201));
        // 199 is now older than the window.
        assert!(!w.accept(199));
        // A bit set in the window survives a short advance.
        assert!(!w.accept(1000));
        assert!(w.accept(2300));
        assert!(!w.accept(1000));
    }

    #[test]
    fn a_jump_of_a_billion_is_cheap() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(3));
        assert!(w.accept(1_000_000_000));
        assert!(!w.accept(3));
        assert!(!w.accept(1_000_000_000));
        assert!(w.accept(999_999_999));
        assert!(w.accept(u64::MAX));
        assert!(!w.accept(u64::MAX));
    }

    #[test]
    fn counter_zero_is_taken_once() {
        let mut w = ReplayWindow::default();
        assert!(w.may_be_new(0));
        assert!(w.accept(0));
        assert!(!w.may_be_new(0));
        assert!(!w.accept(0));
        assert!(w.accept(1));
    }
}
