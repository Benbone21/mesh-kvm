//! Debian's decision at the shared edge, apart from the router, uinput and the socket, so that it
//! can be tested: count a push at KWin's position, judge it, hold it, decide it again when the next
//! position comes, and say what happened in the link log.

use crate::crossing::{judge, CrossLine, CursorTrack, Gate, Refusal, RefusalLog, Sample, Spot, TrackerNews};
use crate::protocol::NodeId;
use crate::spatial::{EdgeWatch, SpatialConfig};

/// Where the decision's lines go.
pub trait EdgeLog {
    fn tracker(&mut self, news: TrackerNews, age_ms: u64);
    fn refused(&mut self, line: (Refusal, u32));
    fn crossed(&mut self, line: CrossLine);
}

/// The link log.
pub struct LinkLog;

impl EdgeLog for LinkLog {
    fn tracker(&mut self, news: TrackerNews, age_ms: u64) {
        let details = crate::linklog::Details::new();
        let details = match news {
            TrackerNews::Silent => details.text("state", "silent").num("last_position_ms", age_ms),
            TrackerNews::Back => details.text("state", "back"),
        };
        crate::linklog::record("cursor_tracker", details);
    }

    fn refused(&mut self, line: (Refusal, u32)) {
        crate::crossing::record_refusal(line);
    }

    fn crossed(&mut self, line: CrossLine) {
        line.record();
    }
}

/// What the decision needs of the router.
pub trait Router {
    /// Debian has control, and a push may cross.
    fn may_cross(&self) -> bool;
    /// Crosses, putting the cursor at `entry` on the laptop. True if control went.
    fn cross(&mut self, entry: (i32, i32), from_peer: bool) -> bool;
}

/// A completed push waiting on a position it can trust.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Held {
    from_peer: bool,
    since_ms: u64,
}

/// Debian's edge: the push, the cursor's position, a push held for a better one, and the refusals.
pub struct DebianEdge {
    pub edge: EdgeWatch,
    pub track: CursorTrack,
    holding: Option<Held>,
    refusals: RefusalLog,
}

/// The moment, on both clocks the decision uses.
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub us: i64,
    pub ms: u64,
}

impl DebianEdge {
    pub fn new(now_us: i64, start: (i32, i32)) -> Self {
        Self { edge: EdgeWatch::new(), track: CursorTrack::new(now_us, start), holding: None, refusals: RefusalLog::new() }
    }

    /// Control changed hands: any push is forgotten, and the settle starts.
    pub fn arrived(&mut self, now_ms: u64) {
        self.edge.arrived(now_ms);
        self.holding = None;
    }

    /// True while a completed push waits for a position.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        self.holding.is_some()
    }

    /// Looks at the tracker, whether or not a crossing could happen, so that a tracker gone silent
    /// is noticed while the laptop has control or a game lock is on too.
    pub fn look(&mut self, cfg: &SpatialConfig, now: Now, log: &mut impl EdgeLog) {
        let (here, news) = self.track.locate(now.us, cfg.monitors(NodeId::Debian), self.edge.along_x());
        if let Some(news) = news {
            log.tracker(news, here.age_ms);
        }
    }

    /// Counts movement of the Debian cursor by `delta` against the shared edge, from either
    /// device's mouse, and crosses to the laptop when the push completes on a position that can be
    /// trusted. Otherwise the push waits, complete, for the next position or movement.
    pub fn push(
        &mut self,
        cfg: &SpatialConfig,
        delta: (i32, i32),
        from_peer: bool,
        buttons_held: bool,
        now: Now,
        router: &mut impl Router,
        log: &mut impl EdgeLog,
    ) {
        if !router.may_cross() {
            self.holding = None;
            return;
        }
        let monitors = cfg.monitors(NodeId::Debian);
        let (here, news) = self.track.locate(now.us, monitors, self.edge.along_x());
        if let Some(news) = news {
            log.tracker(news, here.age_ms);
        }
        if !self.edge.press(monitors, here.sample, delta, buttons_held) {
            self.holding = None;
            self.refusals.cleared();
            return;
        }
        if self.edge.too_soon(now.ms) {
            self.holding = None;
            if let Some(line) = self.refusals.refused(Refusal::Settle, now.ms) {
                log.refused(line);
            }
            self.refusals.cleared();
            return;
        }
        let edge = &self.edge;
        let spot = |p| Spot { on_span: edge.on_span(monitors, p), guarded: edge.guarded(monitors, p) };
        match judge(&here, edge.along_x(), spot) {
            Gate::Cross { at } => {
                self.edge.clear();
                self.refusals.cleared();
                let held_ms = self.holding.take().map_or(0, |h| now.ms.saturating_sub(h.since_ms));
                let settle_left_ms = self.edge.settle_full_left_ms(now.ms);
                let entry = cfg.entry_on_peer(NodeId::Debian, at.0, at.1);
                if router.cross(entry, from_peer) {
                    self.arrived(now.ms);
                    let line = CrossLine { to_laptop: true, at, entry, sample_age_ms: here.age_ms, est: here.moved, held_ms, settle_left_ms };
                    log.crossed(line);
                }
            }
            Gate::Hold(reason) => {
                if self.holding.is_none() {
                    self.holding = Some(Held { from_peer, since_ms: now.ms });
                }
                if let Some(line) = self.refusals.refused(reason, now.ms) {
                    log.refused(line);
                }
            }
        }
    }

    /// Takes KWin's newest position, and decides again a push that was waiting on it.
    pub fn hear(
        &mut self,
        cfg: &SpatialConfig,
        sample: Sample,
        buttons_held: bool,
        now: Now,
        router: &mut impl Router,
        log: &mut impl EdgeLog,
    ) {
        if !self.track.is_new(&sample) {
            return;
        }
        if let Some(news) = self.track.heard(sample) {
            log.tracker(news, 0);
        }
        if let Some(held) = self.holding {
            self.push(cfg, (0, 0), held.from_peer, buttons_held, now, router, log);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::SHARED_EDGE_INSET;

    #[derive(Default)]
    struct FakeRouter {
        blocked: bool,
        refuses: bool,
        crossings: Vec<((i32, i32), bool)>,
    }

    impl Router for FakeRouter {
        fn may_cross(&self) -> bool {
            !self.blocked
        }
        fn cross(&mut self, entry: (i32, i32), from_peer: bool) -> bool {
            self.crossings.push((entry, from_peer));
            !self.refuses
        }
    }

    #[derive(Default)]
    struct FakeLog(Vec<String>);

    impl EdgeLog for FakeLog {
        fn tracker(&mut self, news: TrackerNews, _: u64) {
            self.0.push(format!("cursor_tracker {news:?}"));
        }
        fn refused(&mut self, (reason, count): (Refusal, u32)) {
            self.0.push(format!("cross_refused reason={} count={count}", reason.word()));
        }
        fn crossed(&mut self, line: CrossLine) {
            let text: Vec<String> = line.fields().iter().map(|(k, v)| format!("{k}={v}")).collect();
            self.0.push(format!("cross {}", text.join(" ")));
        }
    }

    const MS: i64 = 1000;

    fn at(ms: i64) -> Now {
        Now { us: ms * MS, ms: ms as u64 }
    }

    fn sample(x: i32, seq: u32, taken_ms: i64, received_ms: i64) -> Sample {
        Sample { x, y: 1439, seq, taken_us: taken_ms * MS, received_us: received_ms * MS }
    }

    /// The owner's layout, the laptop lined up under DP-2, and KWin's first position near DP-3.
    fn setup() -> (SpatialConfig, DebianEdge, FakeRouter, FakeLog) {
        let mut cfg = crate::spatial::test_layouts::real();
        cfg.align_exactly(NodeId::Debian);
        let mut d = DebianEdge::new(0, (3200, 720));
        d.edge.use_own_layout(&cfg, NodeId::Debian);
        let (mut router, mut log) = (FakeRouter::default(), FakeLog::default());
        d.hear(&cfg, sample(2000, 1, 0, 1), false, at(1), &mut router, &mut log);
        (cfg, d, router, log)
    }

    /// Pushes down while drifting left towards DP-3, with KWin behind, to a held push.
    fn held(cfg: &SpatialConfig, d: &mut DebianEdge, router: &mut FakeRouter, log: &mut FakeLog) {
        for t in [40, 50] {
            d.track.moved(t * MS, -15, 30);
            d.push(cfg, (-15, 30), false, false, at(t + 10), router, log);
        }
        assert!(d.holding());
    }

    #[test]
    fn a_held_push_crosses_when_the_next_position_arrives_with_the_entry_and_the_lines() {
        let (cfg, mut d, mut router, mut log) = setup();
        held(&cfg, &mut d, &mut router, &mut log);
        assert!(router.crossings.is_empty());
        assert_eq!(log.0, vec!["cross_refused reason=stale count=1"]);
        // KWin's position, taken after the last move: 1970, clear of the guard strip.
        d.hear(&cfg, sample(1970, 2, 59, 61), false, at(62), &mut router, &mut log);
        assert_eq!(router.crossings, vec![((50, SHARED_EDGE_INSET), false)]);
        assert!(!d.holding());
        assert_eq!(log.0[1], "cross dir=to_laptop at=1970,1439 entry=50,12 sample_age_ms=3 est_dx=0 est_dy=0 held_ms=2 settle_left_ms=0");
    }

    #[test]
    fn a_change_of_control_drops_a_held_push() {
        let (cfg, mut d, mut router, mut log) = setup();
        held(&cfg, &mut d, &mut router, &mut log);
        d.arrived(61);
        assert!(!d.holding());
        d.hear(&cfg, sample(1970, 2, 59, 61), false, at(62), &mut router, &mut log);
        assert!(router.crossings.is_empty(), "no push was waiting");
    }

    #[test]
    fn a_new_position_decides_only_a_push_that_is_waiting() {
        let (cfg, mut d, mut router, mut log) = setup();
        d.hear(&cfg, sample(3000, 2, 59, 61), false, at(62), &mut router, &mut log);
        assert!(router.crossings.is_empty());
        assert!(log.0.is_empty());
    }

    #[test]
    fn a_crossing_the_router_refuses_writes_no_line_and_the_push_starts_again() {
        let (cfg, mut d, _, mut log) = setup();
        let mut router = FakeRouter { refuses: true, ..Default::default() };
        d.hear(&cfg, sample(3000, 2, 99, 100), false, at(100), &mut router, &mut log);
        d.push(&cfg, (0, 99), false, false, at(101), &mut router, &mut log);
        assert_eq!(router.crossings.len(), 1);
        assert!(log.0.iter().all(|l| !l.starts_with("cross ")), "{:?}", log.0);
        d.push(&cfg, (0, 10), false, false, at(102), &mut router, &mut log);
        assert_eq!(router.crossings.len(), 1);
    }

    #[test]
    fn a_tracker_gone_silent_is_noticed_while_crossing_is_off() {
        let (cfg, mut d, _, mut log) = setup();
        let mut router = FakeRouter { blocked: true, ..Default::default() };
        for ms in 2..=1700 {
            d.track.moved(ms * MS, 1, 0);
            d.push(&cfg, (1, 0), false, false, at(ms), &mut router, &mut log);
        }
        assert!(log.0.is_empty(), "pushes when crossing is off look at nothing");
        d.look(&cfg, at(1700), &mut log);
        assert_eq!(log.0, vec!["cursor_tracker Silent"]);
    }
}
