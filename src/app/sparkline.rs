//! The last minute of each figure, for the line drawn under it.

use super::model::Snapshot;
use super::view::{self, Stat, STATS};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often a line takes a point.
pub const EVERY: Duration = Duration::from_secs(1);
/// How many points a line keeps: a minute's worth.
pub const POINTS: usize = 60;

/// One figure's points, oldest first.
#[derive(Default)]
pub struct Sparkline {
    points: VecDeque<f32>,
    /// When the newest point was due.
    taken: Option<Instant>,
    frozen: bool,
}

impl Sparkline {
    /// Takes `sample` if a point is due. A gap since the last one, from seconds with no repaint,
    /// is filled with the last value, as nothing changed in it. A figure that stops being known
    /// freezes the line, and the next known one carries it on from now.
    pub fn push(&mut self, sample: Option<f32>, now: Instant) {
        let Some(value) = sample else {
            self.frozen = !self.points.is_empty();
            return;
        };
        let taken = match self.taken.filter(|_| !self.frozen) {
            Some(taken) => taken,
            None => {
                self.frozen = false;
                self.taken = Some(now);
                self.add(value);
                return;
            }
        };
        let due = (now.saturating_duration_since(taken).as_secs_f64() / EVERY.as_secs_f64()) as u32;
        if due == 0 {
            return;
        }
        let last = self.points.back().copied().unwrap_or(value);
        for _ in 1..due.min(POINTS as u32) {
            self.add(last);
        }
        self.add(value);
        self.taken = Some(taken + EVERY * due);
    }

    fn add(&mut self, value: f32) {
        if self.points.len() == POINTS {
            self.points.pop_front();
        }
        self.points.push_back(value);
    }

    pub fn points(&self) -> &VecDeque<f32> {
        &self.points
    }

    /// The figure isn't known any more, so the line stops at its last point.
    pub fn frozen(&self) -> bool {
        self.frozen
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The points placed in a box of `size`, one step apart with the newest at the right edge.
    /// The scale tops out at the highest point, or at `floor` if that is higher.
    pub fn path(&self, size: [f32; 2], floor: f32) -> Vec<[f32; 2]> {
        let [width, height] = size;
        let top = self.points.iter().copied().fold(floor, f32::max).max(f32::EPSILON);
        let step = width / (POINTS - 1) as f32;
        let newest = self.points.len().saturating_sub(1);
        let place = |(i, value): (usize, &f32)| {
            let y = height - (value / top).clamp(0.0, 1.0) * height;
            [width - (newest - i) as f32 * step, y]
        };
        self.points.iter().enumerate().map(place).collect()
    }
}

/// A line for each of the five figures.
#[derive(Default)]
pub struct Sparklines {
    lines: [Sparkline; STATS.len()],
}

impl Sparklines {
    /// Gives each line the snapshot's figure. Unpaired devices have no link to show, so every
    /// line starts again.
    pub fn sample(&mut self, snapshot: &Snapshot, now: Instant) {
        if view::not_paired(snapshot) {
            self.lines.iter_mut().for_each(Sparkline::clear);
            return;
        }
        for (line, stat) in self.lines.iter_mut().zip(STATS) {
            line.push(view::stat_value(stat, snapshot).sample, now);
        }
    }

    /// How long until a line that is still growing takes its next point, so the window is drawn
    /// then and the lines move as the board shows. None while no line is shown or every one is
    /// frozen, so a window with nothing to show stays idle.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let growing = self.lines.iter().filter(|line| !line.frozen && !line.points.is_empty());
        growing.filter_map(|line| line.taken).map(|taken| (taken + EVERY).saturating_duration_since(now)).min()
    }

    pub fn get(&self, stat: Stat) -> &Sparkline {
        &self.lines[STATS.iter().position(|s| *s == stat).expect("every stat has a line")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixtures::*;

    #[test]
    fn one_point_a_second_and_never_more_than_sixty() {
        let (mut line, t) = (Sparkline::default(), Instant::now());
        line.push(Some(6.0), t);
        line.push(Some(9.0), t + Duration::from_millis(200));
        assert_eq!(line.points().len(), 1);
        for s in 1..=100 {
            line.push(Some(s as f32), t + Duration::from_secs(s));
        }
        assert_eq!(line.points().len(), POINTS);
        assert_eq!(*line.points().back().unwrap(), 100.0);
    }

    #[test]
    fn a_gap_with_no_repaint_repeats_the_last_value() {
        let (mut line, t) = (Sparkline::default(), Instant::now());
        line.push(Some(6.0), t);
        line.push(Some(8.0), t + Duration::from_secs(5));
        assert_eq!(line.points().iter().copied().collect::<Vec<_>>(), [6.0, 6.0, 6.0, 6.0, 6.0, 8.0]);
    }

    #[test]
    fn a_figure_that_goes_unknown_freezes_the_line() {
        let (mut line, t) = (Sparkline::default(), Instant::now());
        line.push(Some(6.0), t);
        line.push(None, t + Duration::from_secs(1));
        assert!(line.frozen());
        assert_eq!(line.points().len(), 1);
        line.push(Some(7.0), t + Duration::from_secs(3));
        assert!(!line.frozen());
        assert_eq!(*line.points().back().unwrap(), 7.0);
    }

    #[test]
    fn the_path_fills_the_box_newest_on_the_right() {
        let (mut line, t) = (Sparkline::default(), Instant::now());
        for s in 0..60 {
            line.push(Some(if s == 59 { 40.0 } else { 0.0 }), t + Duration::from_secs(s));
        }
        let path = line.path([210.0, 36.0], 20.0);
        assert_eq!(path.len(), 60);
        assert_eq!(path.last().unwrap()[0], 210.0);
        assert!(path.iter().all(|p| (0.0..=36.0).contains(&p[1])));
        assert!(path.last().unwrap()[1] < path[0][1], "the high point is drawn higher");
    }

    #[test]
    fn a_growing_line_wakes_the_window_for_its_next_point() {
        let mut lines = Sparklines::default();
        let t = Instant::now();
        assert_eq!(lines.next_wake(t), None, "nothing shown yet");
        lines.sample(&link(secure(linked(), "encrypted"), 6, 0.4), t);
        assert_eq!(lines.next_wake(t + Duration::from_millis(300)), Some(Duration::from_millis(700)));
        assert_eq!(lines.next_wake(t + Duration::from_secs(3)), Some(Duration::ZERO));
        lines.sample(&unpaired(), t + Duration::from_secs(1));
        assert_eq!(lines.next_wake(t + Duration::from_secs(1)), None, "unpaired clears every line");
    }

    #[test]
    fn frozen_lines_leave_the_window_idle() {
        let mut lines = Sparklines::default();
        let t = Instant::now();
        lines.sample(&link(secure(linked(), "encrypted"), 6, 0.4), t);
        lines.sample(&Snapshot::default(), t + Duration::from_secs(1));
        assert_eq!(lines.next_wake(t + Duration::from_secs(1)), None);
    }

    #[test]
    fn unpairing_clears_every_line() {
        let mut lines = Sparklines::default();
        let t = Instant::now();
        lines.sample(&link(secure(linked(), "encrypted"), 6, 0.4), t);
        lines.sample(&unpaired(), t + Duration::from_secs(1));
        assert!(STATS.iter().all(|s| lines.get(*s).points().is_empty()));
    }
}
