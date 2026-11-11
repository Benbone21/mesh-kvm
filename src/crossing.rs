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

    /// A position older than this, with the mouse moving since, is believed only well inside the
    /// span: see `judge`.
    pub const FRESH_MS: u64 = 12;
    /// How much further than the raw counts an older position's cursor may have gone, for pointer
    /// acceleration, either way along the edge.
    pub const STALE_REACH: i32 = 3;
    /// An older position than this never crosses with movement since, wherever the reach says the
    /// cursor is: acceleration can carry it further than `STALE_REACH` times the counts.
    pub const STALE_CROSS_MS: u64 = 120;
    /// A move this close after the moment KWin took a position may already be in it: its millisecond
    /// stamp is rounded down. Only movement after this, or after the position arrived if that was
    /// sooner, is added on.
    const ANSWER_SLACK_US: i64 = 3000;
    /// Movement older than this is forgotten.
    const MOTION_KEPT_US: i64 = 1_500_000;
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
        /// When it arrived here.
        pub received_us: i64,
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
        /// No position for `HELD_SILENT_MS` while a push was held.
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
        /// How far the mouse went along the edge since `sample`, every count counted, both ways.
        pub path_along: i32,
        pub trust: Trust,
    }

    /// Whether a completed push crosses, and at which point.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Gate {
        Cross { at: (i32, i32) },
        Hold(Refusal),
    }

    /// What a point on the edge is: on the span, and in a guard strip.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Spot {
        pub on_span: bool,
        pub guarded: bool,
    }

    /// Decides a completed push from where the cursor is (the push itself was counted at `sample`).
    /// `along_x` says the edge runs along x, and `spot` tells what a point is.
    ///
    /// A fresh or still position crosses at the estimate if that is on the span. An older one, with
    /// movement since, crosses only if it is under `STALE_CROSS_MS` old and wherever the cursor could
    /// have got to is: `STALE_REACH` times the path along the edge, either way from the position,
    /// all on the span and out of the guard strips. So a lagging D-Bus doesn't hold up a push deep inside the span, and still can't
    /// let one through near its ends. A silent tracker crosses at the last position, as before any
    /// of this.
    pub fn judge(here: &Whereabouts, along_x: bool, spot: impl Fn((i32, i32)) -> Spot) -> Gate {
        let (at_sample, at_estimate) = (spot(here.sample), spot(here.estimate));
        if at_sample.guarded || at_estimate.guarded {
            return Gate::Hold(Refusal::Guard);
        }
        match here.trust {
            Trust::TrackerSilent => Gate::Cross { at: here.sample },
            Trust::Unverified => Gate::Hold(Refusal::Stale),
            Trust::Fresh | Trust::Still if !at_estimate.on_span => Gate::Hold(Refusal::OffSpan),
            Trust::Fresh | Trust::Still => Gate::Cross { at: here.estimate },
            Trust::Stale if here.age_ms > STALE_CROSS_MS => Gate::Hold(Refusal::Stale),
            Trust::Stale => {
                // The whole path, not the net movement: a flick one way and a slow return can leave
                // the accelerated cursor far from where the counts net out.
                let reach = here.path_along.saturating_mul(STALE_REACH);
                let ends = [-reach, reach].map(|r| {
                    if along_x {
                        (here.sample.0.saturating_add(r), here.estimate.1)
                    } else {
                        (here.estimate.0, here.sample.1.saturating_add(r))
                    }
                });
                let sure = at_estimate.on_span && ends.into_iter().map(&spot).all(|s| s.on_span && !s.guarded);
                if sure {
                    Gate::Cross { at: here.estimate }
                } else {
                    Gate::Hold(Refusal::Stale)
                }
            }
        }
    }

    /// A change in whether the tracker is heard from, to be logged once each.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TrackerNews {
        Silent,
        Back,
    }

    /// A held push with no position for this long has a silent tracker: near an edge, KWin sends
    /// one every 16 ms, so this is nine gone missing. It crosses at the last position, as before
    /// positions were checked.
    pub const HELD_SILENT_MS: u64 = 150;
    /// This many silences within `SILENCES_WITHIN_MS` and the tracker is started again.
    pub const SILENCES_FOR_RESPAWN: usize = 3;
    pub const SILENCES_WITHIN_MS: u64 = 60_000;
    /// And no more often than this.
    pub const RESPAWN_GAP_MS: u64 = 30_000;

    /// When to start the tracker again: silent often lately, and not too soon after the last time.
    #[derive(Default)]
    pub struct Respawn {
        silences: VecDeque<u64>,
        last_ms: Option<u64>,
    }

    impl Respawn {
        /// The tracker fell silent at `now_ms`. True if it should start again now.
        pub fn silenced(&mut self, now_ms: u64) -> bool {
            self.silences.push_back(now_ms);
            while self.silences.front().is_some_and(|&at| now_ms.saturating_sub(at) > SILENCES_WITHIN_MS) {
                self.silences.pop_front();
            }
            let rested = self.last_ms.is_none_or(|last| now_ms.saturating_sub(last) >= RESPAWN_GAP_MS);
            let due = self.silences.len() >= SILENCES_FOR_RESPAWN && rested;
            if due {
                self.last_ms = Some(now_ms);
                self.silences.clear();
            }
            due
        }
    }

    /// Movement since the base position, summed as it comes.
    #[derive(Clone, Copy, Debug, Default, PartialEq)]
    struct Since {
        dx: i32,
        dy: i32,
        path_x: i32,
        path_y: i32,
    }

    impl Since {
        fn add(&mut self, dx: i32, dy: i32) {
            self.dx = self.dx.saturating_add(dx);
            self.dy = self.dy.saturating_add(dy);
            self.path_x = self.path_x.saturating_add(dx.saturating_abs());
            self.path_y = self.path_y.saturating_add(dy.saturating_abs());
        }
    }

    /// The cursor's position and the movement since, on Debian.
    ///
    /// Each move costs a sum and an insert, and each look at the position nothing more: `since`
    /// keeps the sums, and `moves` is only walked when a position comes.
    pub struct CursorTrack {
        newest: Option<Sample>,
        /// The last warp: when, and to where.
        warp: (i64, i32, i32),
        /// Movement, in time order, for `MOTION_KEPT_US`. Two mice can stamp out of order.
        moves: VecDeque<(i64, i32, i32)>,
        /// The newest move dropped from `moves`.
        dropped_until: i64,
        since: Since,
        /// Movement since the base position was dropped before it could be summed.
        forgot: bool,
        started_us: i64,
    }

    impl CursorTrack {
        /// `start` stands for the cursor until the first position arrives, unverified.
        pub fn new(now_us: i64, start: (i32, i32)) -> Self {
            Self {
                newest: None,
                warp: (now_us, start.0, start.1),
                moves: VecDeque::new(),
                dropped_until: i64::MIN,
                since: Since::default(),
                forgot: false,
                started_us: now_us,
            }
        }

        /// Movement after this is movement the base position doesn't show.
        fn counted_after(&self) -> i64 {
            match self.newest {
                Some(s) if s.taken_us >= self.warp.0 => (s.taken_us + ANSWER_SLACK_US).min(s.received_us),
                _ => self.warp.0,
            }
        }

        /// Sums the kept movement since the base again.
        fn recount(&mut self) {
            let after = self.counted_after();
            self.since = Since::default();
            // Newest first, back to the base only.
            for &(_, dx, dy) in self.moves.iter().rev().take_while(|m| m.0 > after) {
                self.since.add(dx, dy);
            }
            self.forgot = self.dropped_until > after;
        }

        /// Takes the newest position from KWin.
        pub fn heard(&mut self, sample: Sample) {
            if self.newest != Some(sample) {
                self.newest = Some(sample);
                self.recount();
            }
        }

        /// True if `sample` is one this hasn't had yet.
        pub fn is_new(&self, sample: &Sample) -> bool {
            self.newest.as_ref() != Some(sample)
        }

        /// When the newest position arrived, or when this started with none yet.
        pub fn heard_at_us(&self) -> i64 {
            self.newest.map_or(self.started_us, |s| s.received_us)
        }

        /// The mouse moved the cursor by `(dx, dy)` counts at `at_us`. A move of nothing isn't one.
        pub fn moved(&mut self, at_us: i64, dx: i32, dy: i32) {
            if dx == 0 && dy == 0 {
                return;
            }
            let place = self.moves.partition_point(|m| m.0 <= at_us);
            self.moves.insert(place, (at_us, dx, dy));
            if at_us > self.counted_after() {
                self.since.add(dx, dy);
            }
            let newest = self.moves.back().map_or(at_us, |m| m.0);
            while let Some(&(at, ..)) = self.moves.front().filter(|m| newest - m.0 > MOTION_KEPT_US) {
                self.dropped_until = self.dropped_until.max(at);
                self.moves.pop_front();
            }
        }

        /// The cursor was put at `(x, y)`. Positions taken before now no longer count.
        pub fn warped(&mut self, now_us: i64, x: i32, y: i32) {
            self.warp = (now_us, x, y);
            self.recount();
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

        /// Where the cursor is at `now_us`, on `monitors`, for an edge along x or not. Whether the
        /// tracker is silent is for whoever holds a push to say: see `HELD_SILENT_MS`.
        pub fn locate(&self, now_us: i64, monitors: &[MonitorRect], along_x: bool) -> Whereabouts {
            let (sample, since_us, confirmed) = self.base();
            let Since { dx, dy, path_x, path_y } = self.since;
            let estimate = clamp_to(monitors, (sample.0.saturating_add(dx), sample.1.saturating_add(dy)));
            let age_ms = u64::try_from(now_us - since_us).unwrap_or(0) / 1000;
            let path_along = if along_x { path_x } else { path_y };
            // Nothing along the edge, and nothing across it that the edge didn't stop.
            let still = !self.forgot && path_along == 0 && estimate == clamp_to(monitors, sample);
            let trust = if !confirmed || self.forgot {
                Trust::Unverified
            } else if age_ms <= FRESH_MS {
                Trust::Fresh
            } else if still {
                Trust::Still
            } else {
                Trust::Stale
            };
            Whereabouts { sample, estimate, age_ms, moved: (dx, dy), path_along, trust }
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
        Sample { x, y, seq, taken_us: taken_ms * MS, received_us: taken_ms * MS }
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

    /// The owner's desk: the laptop under DP-2, the bright edge 1920..4479 along y = 1439, and the
    /// guard strip beside DP-3 at 1920..1967.
    fn spot((x, y): (i32, i32)) -> Spot {
        Spot { on_span: y == 1439 && (1920..=4479).contains(&x), guarded: y == 1439 && (1920..1968).contains(&x) }
    }

    fn nowhere(_: (i32, i32)) -> Spot {
        Spot { on_span: false, guarded: false }
    }

    fn all_guarded(_: (i32, i32)) -> Spot {
        Spot { on_span: true, guarded: true }
    }

    #[test]
    fn a_stale_position_and_a_long_move_left_along_the_bottom_does_not_cross_from_the_side_display() {
        // KWin last said 2500,1439, above the laptop, 100 ms ago. Since then the mouse has carried
        // the cursor 800 px left along the bottom line, onto DP-3, still pushing down.
        let mut t = track_with(sample(2500, 1439, 1, 0));
        for i in 1..=40 {
            t.moved(i * 2 * MS, -20, 2);
        }
        let here = t.locate(100 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Stale);
        assert_eq!(here.estimate, (1700, 1439));
        assert_eq!(here.moved, (-800, 80));
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
    }

    #[test]
    fn a_stale_position_deep_in_the_span_with_a_jittering_hand_crosses() {
        // D-Bus is 100 ms behind, and the hand wobbles a few px along the edge while pushing.
        let mut t = track_with(sample(3000, 1439, 1, 0));
        for (i, dx) in [3, -2, 4, -1].into_iter().enumerate() {
            t.moved((20 + 20 * i as i64) * MS, dx, 15);
        }
        let here = t.locate(100 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Stale);
        assert_eq!(judge(&here, true, spot), Gate::Cross { at: (3004, 1439) });
    }

    #[test]
    fn a_stale_position_within_reach_of_the_end_or_the_guard_holds() {
        // Three times the movement either way could be past the end of the span.
        let mut t = track_with(sample(4450, 1439, 1, 0));
        t.moved(50 * MS, 12, 30);
        let here = t.locate(100 * MS, &desk(), true);
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
        // Or in the guard strip beside DP-3.
        let mut t = track_with(sample(1990, 1439, 1, 0));
        t.moved(50 * MS, -10, 30);
        let here = t.locate(100 * MS, &desk(), true);
        assert_eq!(here.estimate, (1980, 1439));
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
        // Fresh, the same place crosses.
        let mut t = track_with(sample(1990, 1439, 1, 95));
        t.moved(97 * MS, -10, 30);
        assert_eq!(judge(&t.locate(100 * MS, &desk(), true), true, spot), Gate::Cross { at: (1980, 1439) });
    }

    #[test]
    fn a_fresh_position_crosses_at_the_estimate() {
        let mut t = track_with(sample(2500, 1439, 1, 95));
        t.moved(97 * MS, 3, 30);
        let here = t.locate(100 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Fresh);
        assert_eq!(here.age_ms, 5);
        assert_eq!(here.estimate, (2503, 1439), "down is held at the bottom edge");
        assert_eq!(judge(&here, true, spot), Gate::Cross { at: (2503, 1439) });
        assert_eq!(judge(&here, true, nowhere), Gate::Hold(Refusal::OffSpan));
        assert_eq!(judge(&here, true, all_guarded), Gate::Hold(Refusal::Guard));
        // Fresh means under FRESH_MS.
        let mut t = track_with(sample(2500, 1439, 1, 80));
        t.moved(97 * MS, 3, 30);
        assert_eq!(t.locate(100 * MS, &desk(), true).trust, Trust::Stale);
    }

    #[test]
    fn pushing_straight_down_needs_no_new_position() {
        // KWin says nothing while the cursor is held against the edge, as it doesn't move.
        let mut t = track_with(sample(2500, 1439, 1, 0));
        for i in 1..=30 {
            t.moved(i * 10 * MS, 0, 5);
        }
        let here = t.locate(300 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Still);
        assert_eq!(judge(&here, true, spot), Gate::Cross { at: (2500, 1439) });
    }

    #[test]
    fn a_held_push_completes_when_a_fresh_position_confirms_it() {
        // Stale near the guard strip, so it waits.
        let mut t = track_with(sample(2000, 1439, 1, 0));
        t.moved(40 * MS, -30, 20);
        let here = t.locate(60 * MS, &desk(), true);
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
        // KWin's next position arrives: the cursor did move 30 px left, and is clear of the strip.
        t.heard(sample(1970, 1439, 2, 58));
        let here = t.locate(61 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Fresh);
        assert_eq!(judge(&here, true, spot), Gate::Cross { at: (1970, 1439) });
    }

    #[test]
    fn after_a_warp_nothing_counts_until_a_position_from_after_it() {
        let mut t = track_with(sample(2500, 1439, 1, 0));
        t.warped(100 * MS, 2600, 1427);
        assert_eq!(t.shown(), (2600, 1427));
        // A position KWin took before the warp, arriving after it.
        t.heard(sample(2500, 1439, 2, 99));
        let here = t.locate(110 * MS, &desk(), true);
        assert_eq!((here.trust, here.sample), (Trust::Unverified, (2600, 1427)));
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
        // KWin put it somewhere else after all, and says so.
        t.heard(sample(2610, 1430, 3, 105));
        let here = t.locate(110 * MS, &desk(), true);
        assert_eq!((here.trust, here.sample), (Trust::Fresh, (2610, 1430)));
        assert_eq!(t.shown(), (2610, 1430));
    }

    #[test]
    fn a_flick_one_way_and_a_slow_return_holds_however_little_it_nets() {
        // Fast left (accelerated, so the real cursor went much further), then slowly back right.
        let mut t = track_with(sample(3000, 1439, 1, 0));
        t.moved(20 * MS, -300, 5);
        for i in 0..29 {
            t.moved((30 + i) * MS, 10, 1);
        }
        let here = t.locate(80 * MS, &desk(), true);
        assert_eq!(here.trust, Trust::Stale);
        assert_eq!((here.moved.0, here.path_along), (-10, 590));
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale));
    }

    #[test]
    fn an_old_stale_position_never_crosses_until_a_fresh_one_comes() {
        let mut t = track_with(sample(3000, 1439, 1, 0));
        t.moved(60 * MS, 2, 20);
        t.moved(110 * MS, -1, 20);
        let here = t.locate(130 * MS, &desk(), true);
        assert_eq!((here.trust, here.age_ms), (Trust::Stale, 130));
        assert_eq!(judge(&here, true, spot), Gate::Hold(Refusal::Stale), "past STALE_CROSS_MS, deep in the span or not");
        t.heard(sample(3001, 1439, 2, 125));
        assert_eq!(judge(&t.locate(130 * MS, &desk(), true), true, spot), Gate::Cross { at: (3001, 1439) });
    }

    #[test]
    fn the_last_count_before_a_position_isnt_added_on_again() {
        // KWin's stamp is rounded down to the millisecond, so a move 0.9 ms after it is in it.
        let mut t = track_with(sample(2500, 1439, 1, 0));
        t.moved(900, 4, 0);
        t.heard(Sample { x: 2504, y: 1439, seq: 2, taken_us: 0, received_us: 2 * MS });
        assert_eq!(t.locate(3 * MS, &desk(), true).estimate, (2504, 1439));
        // A move after the position arrived can't be in it, however soon after its stamp.
        t.moved(2 * MS + 100, 3, 0);
        assert_eq!(t.locate(3 * MS, &desk(), true).estimate, (2507, 1439));
    }

    #[test]
    fn three_silences_in_a_minute_start_the_tracker_again_at_most_every_30_s() {
        let mut respawn = Respawn::default();
        assert!(!respawn.silenced(0));
        assert!(!respawn.silenced(10_000));
        assert!(respawn.silenced(20_000));
        assert!(!respawn.silenced(21_000));
        assert!(!respawn.silenced(22_000));
        assert!(!respawn.silenced(23_000), "30 s since the last start");
        assert!(respawn.silenced(50_000));
        // Spread over more than a minute, they don't add up.
        let mut respawn = Respawn::default();
        for at in [0, 40_000, 80_000, 120_000] {
            assert!(!respawn.silenced(at), "{at}");
        }
    }

    #[test]
    fn two_mice_out_of_order_still_add_up() {
        let mut t = track_with(sample(2500, 700, 1, 0));
        t.moved(10 * MS, 5, 0);
        t.moved(8 * MS, 3, 0);
        // KWin took this after the 8 ms move, and it arrived before the 10 ms one.
        t.heard(Sample { x: 2503, y: 700, seq: 2, taken_us: 9 * MS, received_us: 9 * MS + 500 });
        let here = t.locate(11 * MS, &desk(), true);
        assert_eq!(here.estimate, (2508, 700));
    }

    #[test]
    fn movement_too_old_to_add_on_leaves_the_position_untrusted() {
        let mut t = track_with(sample(2500, 1439, 1, 0));
        t.moved(100 * MS, -400, 0);
        for i in 0..10 {
            t.moved((1700 + 10 * i) * MS, 0, 5);
        }
        assert_ne!(t.locate(1800 * MS, &desk(), true).trust, Trust::Still, "the move left is forgotten, not undone");
    }

}
