//! The grab state of one evdev device, kept apart from the ioctls so it can be tested.
//!
//! Grabbing is instant, so it happens on the input loop. Letting go isn't: `EVIOCGRAB 0` waits out
//! an RCU grace period in the kernel, about 60 ms a device, so each release runs on a thread of its
//! own and the loop carries on. The kernel stops routing events to the grab at the start of that
//! call, so from then on the desktop gets the device's events, and those stamped before it reached
//! only this process: `delivered_only_here` tells them apart, so they can be put back on the desktop.

use std::io;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a failed release waits before it's tried again, so a device that keeps refusing
/// doesn't start a thread every time round the loop.
pub const RELEASE_RETRY: Duration = Duration::from_millis(100);

/// What grabs and releases a device. `release` runs on its own thread and returns the time, in the
/// device's event clock (microseconds), taken just before the kernel was asked to let go.
pub trait GrabOps<D>: Send + Sync {
    fn grab(&self, device: &D) -> io::Result<()>;
    /// Says when it starts through `started`, before it asks the kernel.
    fn release(&self, device: &D, started: &dyn Fn(i64)) -> io::Result<()>;
}

/// A release under way on its thread.
struct Job {
    /// When the kernel was asked, or `i64::MAX` until then.
    started_us: AtomicI64,
    done: Mutex<Option<io::Result<()>>>,
}

enum Phase {
    Free,
    Held,
    Releasing(Arc<Job>),
    /// Unplugged. Its grab went with it, and nothing more is tried.
    Gone,
}

fn unplugged(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODEV)
}

/// What happened to a grab, for the journal.
#[derive(Debug, PartialEq)]
pub enum GrabNews {
    Enabled,
    Released,
    /// The device is gone (unplugged), so there's nothing more to grab or let go.
    Gone,
    /// The first failure since the last success; `grab` says which way.
    Failed { grab: bool, error: String },
}

pub struct GrabSlot {
    phase: Phase,
    /// The last grab or release failed and was logged.
    failed: bool,
    retry_release_at: Option<Instant>,
    /// When the last finished release started: events stamped before it reached only this process.
    released_at: Option<i64>,
}

impl Default for GrabSlot {
    fn default() -> Self {
        GrabSlot { phase: Phase::Free, failed: false, retry_release_at: None, released_at: None }
    }
}

impl GrabSlot {
    /// Grabbed, and not on the way out: what the device reads now goes to this process alone.
    pub fn held(&self) -> bool {
        matches!(self.phase, Phase::Held)
    }

    pub fn releasing(&self) -> bool {
        matches!(self.phase, Phase::Releasing(_))
    }

    pub fn gone(&self) -> bool {
        matches!(self.phase, Phase::Gone)
    }

    /// Reading the device said it has gone. A release under way finishes by itself.
    pub fn mark_gone(&mut self) {
        self.phase = Phase::Gone;
        self.released_at = None;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Whether an event stamped `at_us`, already read, went to this process and not the desktop.
    /// Read after the event: a release whose start isn't recorded yet hadn't asked the kernel when
    /// the event arrived.
    pub fn delivered_only_here(&self, at_us: i64) -> bool {
        match &self.phase {
            Phase::Held => true,
            Phase::Releasing(job) => at_us < job.started_us.load(Ordering::SeqCst),
            Phase::Free => self.released_at.is_some_and(|start| at_us < start),
            Phase::Gone => false,
        }
    }

    /// Picks up a finished release.
    pub fn poll(&mut self, now: Instant) -> Option<GrabNews> {
        let Phase::Releasing(job) = &self.phase else {
            return None;
        };
        let result = job.done.lock().ok()?.take()?;
        let started = job.started_us.load(Ordering::SeqCst);
        match result {
            Ok(()) => {
                self.phase = Phase::Free;
                self.failed = false;
                self.retry_release_at = None;
                self.released_at = Some(started);
                Some(GrabNews::Released)
            }
            Err(error) if unplugged(&error) => {
                self.mark_gone();
                Some(GrabNews::Gone)
            }
            Err(error) => {
                self.phase = Phase::Held;
                self.retry_release_at = Some(now + RELEASE_RETRY);
                self.note_failure(false, error)
            }
        }
    }

    /// Moves toward `want`: grabs at once, starts a release on its own thread, and leaves a
    /// release under way alone whatever is wanted, so a new grab waits until it has landed.
    pub fn drive<D: Clone + Send + 'static>(
        &mut self,
        want: bool,
        device: &D,
        ops: &Arc<dyn GrabOps<D>>,
        now: Instant,
    ) -> Vec<GrabNews> {
        let mut news: Vec<GrabNews> = self.poll(now).into_iter().collect();
        match (&self.phase, want) {
            (Phase::Free, true) => match ops.grab(device) {
                Ok(()) => {
                    self.phase = Phase::Held;
                    self.failed = false;
                    self.released_at = None;
                    news.push(GrabNews::Enabled);
                }
                Err(error) if unplugged(&error) => {
                    self.mark_gone();
                    news.push(GrabNews::Gone);
                }
                Err(error) => news.extend(self.note_failure(true, error)),
            },
            // Grabbed and wanted so: whatever failed before is over, and a new failure is news.
            (Phase::Held, true) => {
                self.failed = false;
                self.retry_release_at = None;
            }
            (Phase::Held, false) if self.retry_release_at.is_none_or(|at| now >= at) => {
                let job = Arc::new(Job { started_us: AtomicI64::new(i64::MAX), done: Mutex::new(None) });
                self.phase = Phase::Releasing(Arc::clone(&job));
                let (ops, device) = (Arc::clone(ops), device.clone());
                let spawned = std::thread::Builder::new().name("evdev-release".into()).spawn(move || {
                    let result = ops.release(&device, &|at| job.started_us.store(at, Ordering::SeqCst));
                    if let Ok(mut done) = job.done.lock() {
                        *done = Some(result);
                    }
                });
                if let Err(error) = spawned {
                    self.phase = Phase::Held;
                    self.retry_release_at = Some(now + RELEASE_RETRY);
                    news.extend(self.note_failure(false, error));
                }
            }
            _ => {}
        }
        news
    }

    fn note_failure(&mut self, grab: bool, error: io::Error) -> Option<GrabNews> {
        if self.failed {
            return None;
        }
        self.failed = true;
        Some(GrabNews::Failed { grab, error: error.to_string() })
    }
}

/// Waits up to `limit` for every release under way to land, and returns what landed. False in the
/// last place when some are still going.
pub fn wait_for_releases<'a>(
    slots: impl Iterator<Item = &'a mut GrabSlot>,
    limit: Duration,
    mut news: impl FnMut(usize, GrabNews),
) -> bool {
    let mut slots: Vec<&mut GrabSlot> = slots.collect();
    let deadline = Instant::now() + limit;
    loop {
        let now = Instant::now();
        for (i, slot) in slots.iter_mut().enumerate() {
            if let Some(n) = slot.poll(now) {
                news(i, n);
            }
        }
        if !slots.iter().any(|s| s.releasing()) {
            return true;
        }
        if now >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// What to put on the desktop for one key or button read off a device, as `Some(pressed)`.
/// `injected` holds what this has pressed and not yet let go. A press goes there only if it
/// reached this process alone and isn't going to the peer; its release always follows it, wherever
/// that went, so nothing is left down. A key the desktop saw go down itself is left to the desktop.
pub fn hand_back_press(
    injected: &mut std::collections::HashSet<u16>,
    code: u16,
    value: i32,
    only_here: bool,
    forwarded: bool,
) -> Option<bool> {
    match value {
        0 => injected.remove(&code).then_some(false),
        1 if only_here && !forwarded && !injected.contains(&code) => {
            injected.insert(code);
            Some(true)
        }
        // Auto-repeat: the desktop repeats a key it holds by itself.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Condvar;

    /// Grabs at once; each release waits until the gate opens, then takes `release_ms`.
    #[derive(Default)]
    struct Fake {
        gate: Mutex<bool>,
        opened: Condvar,
        release_ms: u64,
        fail_release: AtomicBool,
        fail_grab: AtomicBool,
        grabs: AtomicUsize,
        releases: AtomicUsize,
        clock: AtomicI64,
        /// Unplugged: everything fails with ENODEV.
        gone: AtomicBool,
    }

    impl Fake {
        fn open(release_ms: u64) -> Arc<Fake> {
            Arc::new(Fake { gate: Mutex::new(true), release_ms, ..Default::default() })
        }
        fn shut() -> Arc<Fake> {
            Arc::new(Fake::default())
        }
        fn pipe(self: Arc<Fake>) -> Arc<dyn GrabOps<usize>> {
            self as Arc<dyn GrabOps<usize>>
        }
        fn let_go(&self) {
            *self.gate.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    impl GrabOps<usize> for Fake {
        fn grab(&self, _: &usize) -> io::Result<()> {
            if self.gone.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            if self.fail_grab.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            self.grabs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn release(&self, _: &usize, started: &dyn Fn(i64)) -> io::Result<()> {
            let mut open = self.gate.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
            drop(open);
            started(self.clock.load(Ordering::SeqCst));
            std::thread::sleep(Duration::from_millis(self.release_ms));
            self.releases.fetch_add(1, Ordering::SeqCst);
            if self.gone.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            if self.fail_release.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            Ok(())
        }
    }

    fn ops(fake: &Arc<Fake>) -> Arc<dyn GrabOps<usize>> {
        Arc::clone(fake) as Arc<dyn GrabOps<usize>>
    }

    fn settle(slot: &mut GrabSlot) -> Vec<GrabNews> {
        let mut all = Vec::new();
        assert!(wait_for_releases(std::iter::once(slot), Duration::from_secs(2), |_, n| all.push(n)));
        all
    }

    #[test]
    fn a_grab_is_immediate() {
        let fake = Fake::open(0);
        let mut slot = GrabSlot::default();
        assert_eq!(slot.drive(true, &0, &ops(&fake), Instant::now()), vec![GrabNews::Enabled]);
        assert!(slot.held());
        assert_eq!(fake.grabs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_slow_release_does_not_hold_up_the_caller() {
        let fake = Fake::open(300);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        slot.drive(true, &0, &ops, Instant::now());
        let start = Instant::now();
        let news = slot.drive(false, &0, &ops, Instant::now());
        assert!(start.elapsed() < Duration::from_millis(50), "took {:?}", start.elapsed());
        assert!(news.is_empty(), "released is only said once it has landed");
        assert!(slot.releasing() && !slot.held());
        assert_eq!(settle(&mut slot), vec![GrabNews::Released]);
        assert!(!slot.held() && !slot.releasing());
    }

    #[test]
    fn five_releases_overlap() {
        let fake = Fake::open(60);
        let ops = ops(&fake);
        let mut slots: Vec<GrabSlot> = (0..5).map(|_| GrabSlot::default()).collect();
        for (i, slot) in slots.iter_mut().enumerate() {
            slot.drive(true, &i, &ops, Instant::now());
        }
        let start = Instant::now();
        for (i, slot) in slots.iter_mut().enumerate() {
            slot.drive(false, &i, &ops, Instant::now());
        }
        let mut released = 0;
        assert!(wait_for_releases(slots.iter_mut(), Duration::from_secs(2), |_, n| {
            assert_eq!(n, GrabNews::Released);
            released += 1;
        }));
        assert_eq!(released, 5);
        assert!(start.elapsed() < Duration::from_millis(200), "took {:?}", start.elapsed());
    }

    #[test]
    fn no_grab_while_a_release_is_in_flight_and_the_last_wish_wins() {
        let fake = Fake::shut();
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        // Back to the laptop before the release lands.
        assert!(slot.drive(true, &0, &ops, Instant::now()).is_empty());
        assert_eq!(fake.grabs.load(Ordering::SeqCst), 1, "no grab on top of a release");
        assert!(!slot.held());
        fake.let_go();
        assert_eq!(settle(&mut slot), vec![GrabNews::Released]);
        assert_eq!(slot.drive(true, &0, &ops, Instant::now()), vec![GrabNews::Enabled]);
        assert!(slot.held());
    }

    #[test]
    fn away_and_back_and_away_again_ends_released() {
        let fake = Fake::shut();
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        fake.let_go();
        settle(&mut slot);
        assert!(slot.drive(false, &0, &ops, Instant::now()).is_empty());
        assert!(!slot.held() && !slot.releasing());
        assert_eq!(fake.releases.load(Ordering::SeqCst), 1);
        assert_eq!(fake.grabs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_release_stays_grabbed_and_is_retried_after_a_pause() {
        let fake = Fake::open(0);
        fake.fail_release.store(true, Ordering::SeqCst);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        let t0 = Instant::now();
        slot.drive(true, &0, &ops, t0);
        slot.drive(false, &0, &ops, t0);
        let news = settle(&mut slot);
        assert!(matches!(news.as_slice(), [GrabNews::Failed { grab: false, .. }]), "{news:?}");
        assert!(slot.held() && slot.failed());
        // Still grabbed and not wanted, so the device reaches nothing but this process.
        assert!(slot.delivered_only_here(i64::MAX - 1));
        assert!(slot.drive(false, &0, &ops, t0).is_empty());
        assert!(!slot.releasing(), "not straight away");
        fake.fail_release.store(false, Ordering::SeqCst);
        // The pause runs from when the failure was picked up.
        slot.drive(false, &0, &ops, Instant::now() + RELEASE_RETRY);
        assert!(slot.releasing());
        assert_eq!(settle(&mut slot), vec![GrabNews::Released]);
        assert!(!slot.failed());
    }

    #[test]
    fn a_release_that_fails_again_after_the_device_was_wanted_back_is_logged_again() {
        let fake = Fake::open(0);
        fake.fail_release.store(true, Ordering::SeqCst);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        assert!(matches!(settle(&mut slot).as_slice(), [GrabNews::Failed { grab: false, .. }]));
        // Wanted grabbed again, which it still is: that episode is over.
        assert!(slot.drive(true, &0, &ops, Instant::now()).is_empty());
        assert!(!slot.failed());
        slot.drive(false, &0, &ops, Instant::now());
        assert!(matches!(settle(&mut slot).as_slice(), [GrabNews::Failed { grab: false, .. }]), "a new failure is news");
    }

    #[test]
    fn a_device_that_went_away_mid_release_is_not_tried_again() {
        let fake = Fake::open(0);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        let t0 = Instant::now();
        slot.drive(true, &0, &ops, t0);
        fake.gone.store(true, Ordering::SeqCst);
        slot.drive(false, &0, &ops, t0);
        assert_eq!(settle(&mut slot), vec![GrabNews::Gone]);
        assert!(slot.gone() && !slot.held() && !slot.releasing());
        assert!(!slot.delivered_only_here(0), "nothing more comes from it");
        for later in 1..5 {
            assert!(slot.drive(false, &0, &ops, t0 + RELEASE_RETRY * later * 2).is_empty());
            assert!(slot.drive(true, &0, &ops, t0 + RELEASE_RETRY * later * 2).is_empty());
        }
        assert_eq!(fake.releases.load(Ordering::SeqCst), 1, "no thread every 100 ms");
        assert_eq!(fake.grabs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_device_that_went_away_before_a_grab_is_not_tried_again() {
        let fake = Fake::open(0);
        fake.gone.store(true, Ordering::SeqCst);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        assert_eq!(slot.drive(true, &0, &ops, Instant::now()), vec![GrabNews::Gone]);
        assert!(slot.drive(true, &0, &ops, Instant::now()).is_empty());
        let mut read_gone = GrabSlot::default();
        read_gone.drive(true, &0, &Fake::open(0).pipe(), Instant::now());
        read_gone.mark_gone();
        assert!(read_gone.gone() && !read_gone.held());
        assert!(wait_for_releases(std::iter::once(&mut read_gone), Duration::from_millis(10), |_, _| {}));
    }

    #[test]
    fn a_failed_grab_is_logged_once_and_retried_every_time() {
        let fake = Fake::open(0);
        fake.fail_grab.store(true, Ordering::SeqCst);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        let news = slot.drive(true, &0, &ops, Instant::now());
        assert!(matches!(news.as_slice(), [GrabNews::Failed { grab: true, .. }]));
        assert!(!slot.held() && slot.failed());
        assert!(slot.drive(true, &0, &ops, Instant::now()).is_empty());
        fake.fail_grab.store(false, Ordering::SeqCst);
        assert_eq!(slot.drive(true, &0, &ops, Instant::now()), vec![GrabNews::Enabled]);
        assert!(!slot.failed());
    }

    #[test]
    fn shutdown_waits_for_a_release_but_not_for_ever() {
        let fake = Fake::shut();
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        let start = Instant::now();
        assert!(!wait_for_releases(std::iter::once(&mut slot), Duration::from_millis(100), |_, _| {}));
        assert!(start.elapsed() >= Duration::from_millis(100) && start.elapsed() < Duration::from_secs(1));
        let opener = Arc::clone(&fake);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            opener.let_go();
        });
        let mut news = Vec::new();
        assert!(wait_for_releases(std::iter::once(&mut slot), Duration::from_secs(2), |_, n| news.push(n)));
        assert_eq!(news, vec![GrabNews::Released]);
        t.join().unwrap();
    }

    #[test]
    fn events_before_the_release_started_reached_only_this_process() {
        let fake = Fake::shut();
        fake.clock.store(1_000, Ordering::SeqCst);
        let ops = ops(&fake);
        let mut slot = GrabSlot::default();
        assert!(!slot.delivered_only_here(5), "never grabbed");
        slot.drive(true, &0, &ops, Instant::now());
        assert!(slot.delivered_only_here(5_000));
        slot.drive(false, &0, &ops, Instant::now());
        // The release hasn't asked the kernel yet: whatever was read so far was grabbed.
        assert!(slot.delivered_only_here(999_999));
        fake.let_go();
        settle(&mut slot);
        assert!(slot.delivered_only_here(999));
        assert!(!slot.delivered_only_here(1_000));
        slot.drive(true, &0, &ops, Instant::now());
        slot.drive(false, &0, &ops, Instant::now());
        assert!(slot.delivered_only_here(2_000), "a new release starts over");
    }

    #[test]
    fn hand_back_puts_each_lost_press_on_the_desktop_once() {
        let mut injected = HashSet::new();
        // Pressed under the grab, after focus came back: the desktop never saw it.
        assert_eq!(hand_back_press(&mut injected, 30, 1, true, false), Some(true));
        assert_eq!(hand_back_press(&mut injected, 30, 2, true, false), None, "repeat");
        assert_eq!(hand_back_press(&mut injected, 30, 1, true, false), None, "already down");
        // Its release lands after the grab is gone, but must follow the press.
        assert_eq!(hand_back_press(&mut injected, 30, 0, false, false), Some(false));
        assert_eq!(hand_back_press(&mut injected, 30, 0, false, false), None);
        // The desktop saw this one itself.
        assert_eq!(hand_back_press(&mut injected, 31, 1, false, false), None);
        assert_eq!(hand_back_press(&mut injected, 31, 0, false, false), None);
        // Going to the laptop.
        assert_eq!(hand_back_press(&mut injected, 32, 1, true, true), None);
        assert_eq!(hand_back_press(&mut injected, 32, 0, true, true), None);
        assert!(injected.is_empty());
    }
}
