//! Whether a push at the shared edge may cross, from a cursor position that can be late.
//!
//! On Debian the cursor's position comes from a KWin script, over D-Bus, a moment after the cursor
//! got there, and a backlog can make it later still. Deciding a crossing on a late position is how
//! the side display could drop into the laptop: the mouse had already carried the cursor left onto
//! it along the bottom line while the position said it was still above the laptop. So each position
//! carries when KWin took it, the mouse movement since then is added on to estimate where the cursor
//! is now, and a push completes only on a position that is fresh, or that nothing has moved since,
//! with both the position and the estimate on the span. Otherwise the push waits for the next
//! position, which comes within a few milliseconds of any movement.
//!
//! Both devices also wait a moment after control arrives before a push can cross back: see
//! `Settle`.
//!
//! Pure: times are passed in, positions' in microseconds on one monotonic clock.

use crate::spatial::SHARED_EDGE_INSET;

#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
pub use position::*;

/// What became of a push that has pushed enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The position is too old to go by. The push waits for a newer one.
    Stale,
    /// In the guard strip at the end of the span, beside a monitor that doesn't cross.
    Guard,
    /// Too soon after control arrived. The push starts again.
    Settle,
    /// The estimate has left the span.
    OffSpan,
}

impl Refusal {
    pub fn word(self) -> &'static str {
        match self {
            Refusal::Stale => "stale",
            Refusal::Guard => "guard",
            Refusal::Settle => "settle",
            Refusal::OffSpan => "off_span",
        }
    }
}

/// Refusals for the link log: one line a second at most, with how many there were. A push held
/// for one reason counts once, however many times it is looked at again.
#[derive(Default)]
pub struct RefusalLog {
    last_line_ms: Option<u64>,
    since_line: u32,
    ongoing: Option<Refusal>,
}

impl RefusalLog {
    pub const fn new() -> Self {
        Self { last_line_ms: None, since_line: 0, ongoing: None }
    }

    /// A completed push was refused. Returns the reason and the count for a line, when one is due.
    pub fn refused(&mut self, reason: Refusal, now_ms: u64) -> Option<(Refusal, u32)> {
        if self.ongoing == Some(reason) {
            return None;
        }
        self.ongoing = Some(reason);
        self.since_line += 1;
        if self.last_line_ms.is_some_and(|last| now_ms.saturating_sub(last) < 1000) {
            return None;
        }
        self.last_line_ms = Some(now_ms);
        Some((reason, std::mem::take(&mut self.since_line)))
    }

    /// The push crossed, or isn't a completed one any more.
    pub fn cleared(&mut self) {
        self.ongoing = None;
    }
}

/// What a crossing's line in the link log says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrossLine {
    pub to_laptop: bool,
    /// Where the push crossed, on this device.
    pub at: (i32, i32),
    /// Where the cursor goes on the other device.
    pub entry: (i32, i32),
    pub sample_age_ms: u64,
    /// Mouse movement since the position, added on for the estimate.
    pub est: (i32, i32),
    /// How long the completed push waited.
    pub held_ms: u64,
    /// See `Settle::full_left_ms`.
    pub settle_left_ms: u64,
}

impl CrossLine {
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("dir", if self.to_laptop { "to_laptop" } else { "to_debian" }.to_string()),
            ("at", format!("{},{}", self.at.0, self.at.1)),
            ("entry", format!("{},{}", self.entry.0, self.entry.1)),
            ("sample_age_ms", self.sample_age_ms.to_string()),
            ("est_dx", self.est.0.to_string()),
            ("est_dy", self.est.1.to_string()),
            ("held_ms", self.held_ms.to_string()),
            ("settle_left_ms", self.settle_left_ms.to_string()),
        ]
    }

    /// Writes the line.
    pub fn record(&self) {
        let details = self.fields().into_iter().fold(crate::linklog::Details::new(), |d, (k, v)| d.num(k, v));
        crate::linklog::record("cross", details);
    }
}

/// Writes a `cross_refused` line.
pub fn record_refusal((reason, count): (Refusal, u32)) {
    crate::linklog::record("cross_refused", crate::linklog::Details::new().num("reason", reason.word()).num("count", count));
}

/// How long after control arrives a push can't cross back, at most.
pub const SETTLE_MS: u64 = 250;
/// And at least, once the cursor has been well away from the edge.
pub const SETTLE_FLOOR_MS: u64 = 60;
/// How much further from the edge than where it came in the cursor has to have been for a push
/// back to be meant.
pub const SETTLE_CLEAR: i32 = 12;

/// The wait after control arrives before a push can cross back, so that a hand still moving
/// doesn't bounce the cursor straight back. It ends at `SETTLE_MS`, or at `SETTLE_FLOOR_MS` once
/// the cursor has been `SETTLE_CLEAR` further in than it came in, so a quick return that was meant
/// isn't held up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settle {
    since_ms: Option<u64>,
    cleared: bool,
}

impl Settle {
    pub const fn new() -> Self {
        Self { since_ms: None, cleared: false }
    }

    /// Control arrived at `now_ms`.
    pub fn arrived(&mut self, now_ms: u64) {
        self.since_ms = Some(now_ms);
        self.cleared = false;
    }

    /// The cursor is `distance` pixels in from the shared edge, if it is over a monitor.
    pub fn observe(&mut self, distance: Option<i32>) {
        if distance.is_some_and(|d| d >= SHARED_EDGE_INSET + SETTLE_CLEAR) {
            self.cleared = true;
        }
    }

    /// How much of the full `SETTLE_MS` was still to go, for the log: more than 0 on a crossing
    /// means a quick return went through early.
    pub fn full_left_ms(&self, now_ms: u64) -> u64 {
        self.since_ms.map_or(0, |since| SETTLE_MS.saturating_sub(now_ms.saturating_sub(since)))
    }

    /// How much longer a push has to wait, or 0.
    pub fn left_ms(&self, now_ms: u64) -> u64 {
        let Some(since) = self.since_ms else { return 0 };
        let wait = if self.cleared { SETTLE_FLOOR_MS } else { SETTLE_MS };
        wait.saturating_sub(now_ms.saturating_sub(since))
    }
}


/// Debian's side: only Debian's positions come late.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod position {
    use super::Refusal;
    use crate::protocol::MonitorRect;
    use std::collections::VecDeque;

    /// A position older than this, with the mouse moving since, is not believed.
    pub const FRESH_MS: u64 = 30;
    /// No position for this long while the mouse moves means the tracker has stopped. Crossing then
    /// goes by the last position, as it did before positions were checked, rather than not at all.
    pub const TRACKER_SILENT_MS: u64 = 1500;
    /// Movement older than this is forgotten.
    const MOTION_KEPT_US: i64 = 3_000_000;
    /// The longest the D-Bus path is believed to have taken. Beyond it, a timestamp is wrong.
    const MOST_LAG_MS: i64 = 5000;
    /// KWin stamps positions with `Date.now()` modulo this, so that it fits a D-Bus int32.
    pub const KWIN_STAMP_MODULO: i64 = 1_000_000_000;
    /// A position numbered this far below the newest is a script that started again.
    const RESTART_GAP: u32 = 1024;

    /// One position from KWin.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Sample {
        pub x: i32,
        pub y: i32,
        pub seq: u32,
        /// When KWin took it.
        pub taken_us: i64,
    }

    /// When KWin took a position stamped `kwin_ms`, which arrived at `received_us` with the wall clock
    /// at `wall_ms`. A stamp that makes no sense is taken as arriving at once.
    pub fn taken_at(kwin_ms: Option<i64>, wall_ms: i64, received_us: i64) -> i64 {
        let lag_ms = kwin_ms.map_or(0, |stamp| (wall_ms - stamp).rem_euclid(KWIN_STAMP_MODULO));
        let lag_ms = if lag_ms > MOST_LAG_MS { 0 } else { lag_ms };
        received_us - lag_ms * 1000
    }

    /// The newest position, shared between the thread reading KWin and the input loop. x, y and the
    /// time are kept together, so a reader never sees one position's x with another's y.
    #[derive(Default)]
    pub struct SampleSlot {
        newest: std::sync::Mutex<Option<Sample>>,
    }

    impl SampleSlot {
        /// Keeps `sample` if it is newer than what is there. True if it was.
        pub fn offer(&self, sample: Sample) -> bool {
            let mut newest = self.newest.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let newer = match *newest {
                None => true,
                Some(old) => {
                    let ahead = sample.seq.wrapping_sub(old.seq);
                    (ahead != 0 && ahead < u32::MAX / 2) || old.seq.wrapping_sub(sample.seq) > RESTART_GAP
                }
            };
            if newer {
                *newest = Some(sample);
            }
            newer
        }

        pub fn newest(&self) -> Option<Sample> {
            *self.newest.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    /// How far the position the crossing goes by can be trusted.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Trust {
        /// Younger than `FRESH_MS`.
        Fresh,
        /// Older, but nothing has moved the cursor since.
        Still,
        /// Older, and the mouse has moved since.
        Stale,
        /// The cursor was warped, and no position since has said where it went.
        Unverified,
        /// No position for `TRACKER_SILENT_MS` while the mouse moved.
        TrackerSilent,
    }

    /// Where the cursor is, as far as can be told.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Whereabouts {
        /// The newest position, or where the cursor was warped to if that came after it.
        pub sample: (i32, i32),
        /// `sample` with the movement since added on, kept on the monitors.
        pub estimate: (i32, i32),
        /// How old `sample` is.
        pub age_ms: u64,
        /// The movement since `sample`, in raw counts.
        pub moved: (i32, i32),
        pub trust: Trust,
    }

    /// Whether a completed push crosses, and at which point.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Gate {
        Cross { at: (i32, i32) },
        Hold(Refusal),
    }

    /// Decides a completed push from where the cursor is. `estimate_on_span` says whether the estimate
    /// is on the span too (the push itself was counted at `sample`), and `guarded` whether either is
    /// in a guard strip. A silent tracker crosses at the last position, as before any of this.
    pub fn judge(here: &Whereabouts, estimate_on_span: bool, guarded: bool) -> Gate {
        match here.trust {
            _ if guarded => Gate::Hold(Refusal::Guard),
            Trust::TrackerSilent => Gate::Cross { at: here.sample },
            Trust::Stale | Trust::Unverified => Gate::Hold(Refusal::Stale),
            Trust::Fresh | Trust::Still if !estimate_on_span => Gate::Hold(Refusal::OffSpan),
            Trust::Fresh | Trust::Still => Gate::Cross { at: here.estimate },
        }
    }

    /// A change in whether the tracker is heard from, to be logged once each.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TrackerNews {
        Silent,
        Back,
    }

    /// The cursor's position and the movement since, on Debian.
    pub struct CursorTrack {
        newest: Option<Sample>,
        /// The last warp: when, and to where.
        warp: (i64, i32, i32),
        /// Movement as it was read: when, and by how much.
        moves: VecDeque<(i64, i32, i32)>,
        silent: bool,
        started_us: i64,
    }

    impl CursorTrack {
        /// `start` stands for the cursor until the first position arrives, unverified.
        pub fn new(now_us: i64, start: (i32, i32)) -> Self {
            Self { newest: None, warp: (now_us, start.0, start.1), moves: VecDeque::new(), silent: false, started_us: now_us }
        }

        /// Takes the newest position from KWin. Returns news of the tracker coming back.
        pub fn heard(&mut self, sample: Sample) -> Option<TrackerNews> {
            if self.newest == Some(sample) {
                return None;
            }
            self.newest = Some(sample);
            std::mem::take(&mut self.silent).then_some(TrackerNews::Back)
        }

        /// True if `sample` is one this hasn't had yet.
        pub fn is_new(&self, sample: &Sample) -> bool {
            self.newest.as_ref() != Some(sample)
        }

        /// The mouse moved the cursor by `(dx, dy)` counts.
        pub fn moved(&mut self, now_us: i64, dx: i32, dy: i32) {
            if dx != 0 || dy != 0 {
                self.moves.push_back((now_us, dx, dy));
            }
            while self.moves.front().is_some_and(|m| now_us - m.0 > MOTION_KEPT_US) {
                self.moves.pop_front();
            }
        }

        /// The cursor was put at `(x, y)`. Positions taken before now no longer count.
        pub fn warped(&mut self, now_us: i64, x: i32, y: i32) {
            self.warp = (now_us, x, y);
        }

        /// The position to show: the newest position, or the warp if that is newer.
        pub fn shown(&self) -> (i32, i32) {
            let (base, _, _) = self.base();
            base
        }

        /// The base position, when it dates from, and whether a position confirmed it.
        fn base(&self) -> ((i32, i32), i64, bool) {
            match self.newest {
                Some(s) if s.taken_us >= self.warp.0 => ((s.x, s.y), s.taken_us, true),
                _ => ((self.warp.1, self.warp.2), self.warp.0, false),
            }
        }

        /// Where the cursor is at `now_us`, on `monitors`. Returns news of the tracker going silent.
        pub fn locate(&mut self, now_us: i64, monitors: &[MonitorRect]) -> (Whereabouts, Option<TrackerNews>) {
            let (sample, since_us, confirmed) = self.base();
            let moved = self.moves.iter().filter(|m| m.0 > since_us).fold((0i32, 0i32), |(x, y), m| {
                (x.saturating_add(m.1), y.saturating_add(m.2))
            });
            let estimate = clamp_to(monitors, (sample.0.saturating_add(moved.0), sample.1.saturating_add(moved.1)));
            let age_ms = u64::try_from(now_us - since_us).unwrap_or(0) / 1000;
            let still = estimate == clamp_to(monitors, sample);
            // Silent: nothing from KWin for a long while, though the mouse has been moving it.
            let heard_us = self.newest.map_or(self.started_us, |s| s.taken_us);
            let quiet_ms = u64::try_from(now_us - heard_us).unwrap_or(0) / 1000;
            let moving_lately = self.moves.back().is_some_and(|m| now_us - m.0 <= (TRACKER_SILENT_MS as i64) * 1000);
            let silent = quiet_ms > TRACKER_SILENT_MS && moving_lately && !still;
            let mut news = None;
            if silent && !self.silent {
                self.silent = true;
                news = Some(TrackerNews::Silent);
            }
            let trust = if self.silent {
                Trust::TrackerSilent
            } else if !confirmed {
                Trust::Unverified
            } else if age_ms <= FRESH_MS {
                Trust::Fresh
            } else if still {
                Trust::Still
            } else {
                Trust::Stale
            };
            (Whereabouts { sample, estimate, age_ms, moved, trust }, news)
        }
    }

    /// The closest pixel to `p` on any of `monitors`, or `p` with none to go on.
    fn clamp_to(monitors: &[MonitorRect], (x, y): (i32, i32)) -> (i32, i32) {
        let on = |m: &MonitorRect| (x.clamp(m.x, m.x + m.w - 1), y.clamp(m.y, m.y + m.h - 1));
        let distance = |&(px, py): &(i32, i32)| {
            let (dx, dy) = (i64::from(px) - i64::from(x), i64::from(py) - i64::from(y));
            dx * dx + dy * dy
        };
        monitors.iter().filter(|m| m.w > 0 && m.h > 0).map(on).min_by_key(distance).unwrap_or((x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MonitorRect;

    fn rect(name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorRect {
        MonitorRect { name: name.into(), x, y, w, h }
    }

    /// DP-3 beside DP-2, both ending at y = 1440.
    fn desk() -> Vec<MonitorRect> {
        vec![rect("DP-3", 0, 360, 1920, 1080), rect("DP-2", 1920, 0, 2560, 1440)]
    }

    const MS: i64 = 1000;

    fn sample(x: i32, y: i32, seq: u32, taken_ms: i64) -> Sample {
        Sample { x, y, seq, taken_us: taken_ms * MS }
    }

    fn track_with(s: Sample) -> CursorTrack {
        let mut t = CursorTrack::new(0, (3200, 720));
        t.heard(s);
        t
    }

    #[test]
    fn the_settle_ends_at_250_ms_or_once_the_cursor_has_been_away_from_the_edge_and_after_60() {
        let mut settle = Settle::new();
        assert_eq!(settle.left_ms(0), 0, "nothing arrived yet");
        settle.arrived(1000);
        assert_eq!(settle.left_ms(1000), SETTLE_MS);
        assert_eq!(settle.left_ms(1100), 150);
        assert_eq!(settle.left_ms(1250), 0);
        // Into the screen 12 px past where it came in, at 24 from the edge, and back.
        settle.arrived(2000);
        settle.observe(Some(SHARED_EDGE_INSET + SETTLE_CLEAR - 1));
        assert_eq!(settle.left_ms(2100), 150, "23 px isn't away");
        settle.observe(Some(SHARED_EDGE_INSET + SETTLE_CLEAR));
        settle.observe(Some(0));
        assert_eq!(settle.left_ms(2059), 1, "60 ms at least");
        assert_eq!(settle.left_ms(2060), 0);
        // Each arrival starts again.
        settle.arrived(3000);
        assert_eq!(settle.left_ms(3100), 150);
        settle.observe(None);
        assert_eq!(settle.left_ms(3100), 150);
    }

    #[test]
    fn refusals_make_a_line_a_second_at_most_with_their_count() {
        let mut log = RefusalLog::new();
        assert_eq!(log.refused(Refusal::Guard, 0), Some((Refusal::Guard, 1)));
        assert_eq!(log.refused(Refusal::Guard, 10), None, "the same push held again is the same refusal");
        log.cleared();
        assert_eq!(log.refused(Refusal::Guard, 20), None);
        assert_eq!(log.refused(Refusal::Stale, 30), None, "a new reason counts too");
        log.cleared();
        assert_eq!(log.refused(Refusal::Settle, 1000), Some((Refusal::Settle, 3)));
        assert_eq!(Refusal::OffSpan.word(), "off_span");
    }

    #[test]
    fn a_crossing_line_has_every_field() {
        let line = CrossLine { to_laptop: true, at: (2500, 1439), entry: (580, 12), sample_age_ms: 4, est: (-3, 20), held_ms: 2, settle_left_ms: 0 };
        let text: Vec<String> = line.fields().iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(text.join(" "), "dir=to_laptop at=2500,1439 entry=580,12 sample_age_ms=4 est_dx=-3 est_dy=20 held_ms=2 settle_left_ms=0");
        let mut settle = Settle::new();
        settle.arrived(0);
        assert_eq!(settle.full_left_ms(90), 160);
        assert_eq!(settle.full_left_ms(400), 0);
    }

    #[test]
    fn a_stamp_says_how_long_the_position_took_to_arrive() {
        // Taken 7 ms before it arrived, by the wall clock.
        assert_eq!(taken_at(Some(1_234_993), 1_235_000, 50 * MS), 43 * MS);
        // Across the wrap of the stamp.
        assert_eq!(taken_at(Some(999_999_998), 1_000_000_003, 50 * MS), 45 * MS);
        // No stamp, or one that can't be right, counts as arriving at once.
        assert_eq!(taken_at(None, 1_235_000, 50 * MS), 50 * MS);
        assert_eq!(taken_at(Some(1_240_000), 1_235_000, 50 * MS), 50 * MS);
        assert_eq!(taken_at(Some(1_000), 1_235_000, 50 * MS), 50 * MS);
    }

    #[test]
    fn the_slot_keeps_only_the_newest_and_follows_a_restarted_script() {
        let slot = SampleSlot::default();
        assert!(slot.offer(sample(1, 1, 10, 0)));
        assert!(!slot.offer(sample(2, 2, 9, 0)), "an older one is dropped");
        assert!(!slot.offer(sample(2, 2, 10, 0)), "and the same one again");
        assert!(slot.offer(sample(3, 3, 12, 0)));
        assert_eq!(slot.newest().map(|s| (s.x, s.seq)), Some((3, 12)));
        // A script loaded again counts from 1.
        assert!(slot.offer(sample(4, 4, 5000, 0)));
        assert!(slot.offer(sample(5, 5, 1, 0)));
        assert_eq!(slot.newest().map(|s| s.x), Some(5));
        // Numbers wrap.
        let slot = SampleSlot::default();
        slot.offer(sample(1, 1, u32::MAX, 0));
        assert!(slot.offer(sample(2, 2, 0, 0)));
    }

    #[test]
    fn a_stale_position_and_a_long_move_left_along_the_bottom_does_not_cross_from_the_side_display() {
        // KWin last said 2500,1439, above the laptop, 100 ms ago. Since then the mouse has carried
        // the cursor 800 px left along the bottom line, onto DP-3, still pushing down.
        let mut t = track_with(sample(2500, 1439, 1, 0));
        for i in 1..=40 {
            t.moved(i * 2 * MS, -20, 2);
        }
        let (here, _) = t.locate(100 * MS, &desk());
        assert_eq!(here.trust, Trust::Stale);
        assert_eq!(here.estimate, (1700, 1439));
        assert_eq!(here.moved, (-800, 80));
        assert_eq!(judge(&here, false, false), Gate::Hold(Refusal::Stale));
        assert_eq!(judge(&here, true, false), Gate::Hold(Refusal::Stale), "even if the estimate were on the span");
    }

    #[test]
    fn a_fresh_position_crosses_at_the_estimate() {
        let mut t = track_with(sample(2500, 1439, 1, 95));
        t.moved(97 * MS, 3, 30);
        let (here, _) = t.locate(100 * MS, &desk());
        assert_eq!(here.trust, Trust::Fresh);
        assert_eq!(here.age_ms, 5);
        assert_eq!(here.estimate, (2503, 1439), "down is held at the bottom edge");
        assert_eq!(judge(&here, true, false), Gate::Cross { at: (2503, 1439) });
        assert_eq!(judge(&here, false, false), Gate::Hold(Refusal::OffSpan));
        assert_eq!(judge(&here, true, true), Gate::Hold(Refusal::Guard));
    }

    #[test]
    fn the_guard_strip_holds_even_a_silent_tracker() {
        let mut t = track_with(sample(1930, 1439, 1, 0));
        for i in 1..=200 {
            t.moved(i * 10 * MS, 1, 0);
        }
        let (here, _) = t.locate(2000 * MS, &desk());
        assert_eq!(here.trust, Trust::TrackerSilent);
        assert_eq!(judge(&here, true, true), Gate::Hold(Refusal::Guard));
    }

    #[test]
    fn pushing_straight_down_needs_no_new_position() {
        // KWin says nothing while the cursor is held against the edge, as it doesn't move.
        let mut t = track_with(sample(2500, 1439, 1, 0));
        for i in 1..=30 {
            t.moved(i * 10 * MS, 0, 5);
        }
        let (here, _) = t.locate(300 * MS, &desk());
        assert_eq!(here.trust, Trust::Still);
        assert_eq!(judge(&here, true, false), Gate::Cross { at: (2500, 1439) });
    }

    #[test]
    fn a_held_push_completes_when_a_fresh_position_confirms_it() {
        let mut t = track_with(sample(2500, 1439, 1, 0));
        t.moved(40 * MS, -30, 20);
        let (here, _) = t.locate(60 * MS, &desk());
        assert_eq!(judge(&here, true, false), Gate::Hold(Refusal::Stale));
        // KWin's next position arrives: the cursor did move 30 px left, and is still over the laptop.
        t.heard(sample(2470, 1439, 2, 58));
        let (here, _) = t.locate(61 * MS, &desk());
        assert_eq!(here.trust, Trust::Fresh);
        assert_eq!(judge(&here, true, false), Gate::Cross { at: (2470, 1439) });
    }

    #[test]
    fn after_a_warp_nothing_counts_until_a_position_from_after_it() {
        let mut t = track_with(sample(2500, 1439, 1, 0));
        t.warped(100 * MS, 2600, 1427);
        assert_eq!(t.shown(), (2600, 1427));
        // A position KWin took before the warp, arriving after it.
        t.heard(sample(2500, 1439, 2, 99));
        let (here, _) = t.locate(110 * MS, &desk());
        assert_eq!((here.trust, here.sample), (Trust::Unverified, (2600, 1427)));
        assert_eq!(judge(&here, true, false), Gate::Hold(Refusal::Stale));
        // KWin put it somewhere else after all, and says so.
        t.heard(sample(2610, 1430, 3, 105));
        let (here, _) = t.locate(110 * MS, &desk());
        assert_eq!((here.trust, here.sample), (Trust::Fresh, (2610, 1430)));
        assert_eq!(t.shown(), (2610, 1430));
    }

    #[test]
    fn a_silent_tracker_falls_back_to_the_last_position_and_says_so_once() {
        let mut t = track_with(sample(2500, 1439, 1, 0));
        for i in 1..=200 {
            t.moved(i * 10 * MS, 5, 0);
        }
        let (here, news) = t.locate(2000 * MS, &desk());
        assert_eq!(news, Some(TrackerNews::Silent));
        assert_eq!(here.trust, Trust::TrackerSilent);
        assert_eq!(judge(&here, false, false), Gate::Cross { at: (2500, 1439) });
        let (_, news) = t.locate(2001 * MS, &desk());
        assert_eq!(news, None, "once");
        assert_eq!(t.heard(sample(3000, 1439, 2, 2002)), Some(TrackerNews::Back));
        let (here, news) = t.locate(2003 * MS, &desk());
        assert_eq!((here.trust, news), (Trust::Fresh, None));
    }

    #[test]
    fn a_tracker_that_never_started_falls_back_too() {
        let mut t = CursorTrack::new(0, (3200, 720));
        t.moved(10 * MS, 4, 0);
        assert_eq!(t.locate(20 * MS, &desk()).0.trust, Trust::Unverified);
        for i in 1..=170 {
            t.moved(i * 10 * MS, 4, 0);
        }
        let (here, news) = t.locate(1700 * MS, &desk());
        assert_eq!((here.trust, news), (Trust::TrackerSilent, Some(TrackerNews::Silent)));
    }

    #[test]
    fn a_still_mouse_is_no_sign_of_a_silent_tracker() {
        let mut t = track_with(sample(2500, 700, 1, 0));
        let (here, news) = t.locate(60_000 * MS, &desk());
        assert_eq!((here.trust, news), (Trust::Still, None));
    }
}
