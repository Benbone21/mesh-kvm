//! The screen layout, which every device keeps its own copy of, and the API both services serve.
//!
//! Positions and shared settings are edited on either device. Each edit raises `revision`, and
//! the heartbeats tell the other device, which fetches the newer copy. An edit names only the
//! parts it changes, and each part keeps the stamp of its own last edit, so edits to different
//! parts never undo each other, on either device. Monitors are facts rather than edits: each
//! device reports its own, and they never raise the revision.

use crate::crossing::{Refusal, Settle};
use crate::displays::{DisplayManager, LayoutNews};
use crate::platform::Platform;
use crate::input::link::LinkQuality;
use crate::secure::pairing::PairingService;
pub use crate::protocol::MonitorRect;
use crate::protocol::{NodeId, Payload, SwitchEdge};
use crate::router::{EdgeSpan, EdgeTracker, KEEP_CURSOR};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// How far in from a screen edge a cursor enters, on both machines.
const ENTRY_INSET: i32 = 28;
/// How far in from the edge it came in by. Less, so that a push back is a short way, and still
/// enough off the edge that the cursor isn't pushing at it.
pub const SHARED_EDGE_INSET: i32 = 12;
const LAYOUT_FILE: &str = "layout.json";
/// Anything from a file or the network is held to these, so the maths below can't overflow.
const MAX_COORDINATE: i32 = 1 << 20;
const MAX_MONITOR_SIZE: i32 = 1 << 16;
const MAX_MONITORS: usize = 16;
const MAX_NAME_CHARS: usize = 32;
const MAX_EDGE_RESISTANCE: i32 = 400;
/// A laptop this close to lining up with a monitor of its own size is lined up when the layout
/// loads. Further off than this, it was put there on purpose.
const ALIGN_SNAP: i32 = 8;
/// How far along the edge from a monitor that doesn't cross a push can't cross either. See
/// `EdgeWatch::guarded`.
pub const GUARD: i32 = 48;
/// How far ahead of this copy another copy's revision can be and still be believed. No one makes
/// this many edits, so a bigger jump is a bad value, which would otherwise win every comparison
/// from then on and leave no edit able to beat it.
const MAX_REVISION_JUMP: u64 = 1 << 32;
/// A saved revision above this is a bad value too, and starts again.
const MAX_REVISION: u64 = 1 << 48;
/// What a device's screen is taken to be until it reports its own.
const PLACEHOLDER_SIZE: (i32, i32) = (1920, 1080);
/// A documentation-only address: asking for the route to it sends nothing.
const ANY_ROUTED_ADDRESS: &str = "192.0.2.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchSide {
    /// Windows is to the RIGHT of Debian (crosses Debian's Right edge <-> Windows' Left edge)
    Right,
    /// Windows is to the LEFT of Debian (crosses Debian's Left edge <-> Windows' Right edge)
    Left,
    /// Windows is BELOW Debian (crosses Debian's Bottom edge <-> Windows' Top edge)
    Bottom,
    /// Windows is ABOVE Debian (crosses Debian's Top edge <-> Windows' Bottom edge)
    Top,
}

fn opposite(edge: SwitchEdge) -> SwitchEdge {
    match edge {
        SwitchEdge::Left => SwitchEdge::Right,
        SwitchEdge::Right => SwitchEdge::Left,
        SwitchEdge::Top => SwitchEdge::Bottom,
        SwitchEdge::Bottom => SwitchEdge::Top,
    }
}

fn default_true() -> bool {
    true
}

fn default_editor() -> NodeId {
    NodeId::Debian
}

/// What a part's stamp is before anything has edited it.
fn no_stamp() -> (u64, NodeId) {
    (0, NodeId::Debian)
}

/// One device in the layout: where its desktop sits, and the monitors on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceLayout {
    pub node: NodeId,
    #[serde(default)]
    pub name: String,
    /// Where this device's desktop origin sits in the shared layout.
    pub x: i32,
    pub y: i32,
    /// In this device's own desktop coordinates, as it reports them.
    #[serde(default)]
    pub monitors: Vec<MonitorRect>,
    /// Stands in for all the monitors when set, in place of their native sizes.
    #[serde(default)]
    pub custom_size: Option<(i32, i32)>,
}

impl DeviceLayout {
    fn placeholder(node: NodeId) -> Self {
        let (w, h) = PLACEHOLDER_SIZE;
        let monitors = vec![MonitorRect { name: "Display".to_string(), x: 0, y: 0, w, h }];
        Self { node, name: String::new(), x: 0, y: 0, monitors, custom_size: None }
    }

    /// This device's screens in the shared layout.
    pub fn rects(&self) -> Vec<MonitorRect> {
        if let Some((w, h)) = self.custom_size {
            return vec![MonitorRect { name: self.name.clone(), x: self.x, y: self.y, w, h }];
        }
        let shifted = |m: &MonitorRect| MonitorRect { x: m.x + self.x, y: m.y + self.y, ..m.clone() };
        self.monitors.iter().map(shifted).collect()
    }
}

/// `(left, top, right, bottom)` around all of `rects`, with right and bottom exclusive.
fn bounds(rects: &[MonitorRect]) -> Option<(i32, i32, i32, i32)> {
    let left = rects.iter().map(|m| m.x).min()?;
    let top = rects.iter().map(|m| m.y).min()?;
    let right = rects.iter().map(|m| m.x + m.w).max()?;
    let bottom = rects.iter().map(|m| m.y + m.h).max()?;
    Some((left, top, right, bottom))
}

/// Which copy of the layout is newer: higher revision, and on a tie the higher device id, so that
/// two devices comparing the same pair always agree.
pub fn is_newer(candidate: (u64, NodeId), current: (u64, NodeId)) -> bool {
    (candidate.0, candidate.1 as u8) > (current.0, current.1 as u8)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpatialConfig {
    /// Raised by each edit. A copy with a higher one replaces a copy with a lower one.
    #[serde(default)]
    pub revision: u64,
    /// The device that made the edit at `revision`.
    #[serde(default = "default_editor")]
    pub editor: NodeId,
    /// The edit that last moved a device or changed its size: revision and device.
    #[serde(default = "no_stamp")]
    arrangement_stamp: (u64, NodeId),
    /// The edit that last changed `edge_resistance`.
    #[serde(default = "no_stamp")]
    resistance_stamp: (u64, NodeId),
    pub devices: Vec<DeviceLayout>,
    /// Which side of Debian the Windows screen is attached to
    pub attach_side: TouchSide,
    /// How many raw mouse counts of pushing against the boundary are needed to cross
    pub edge_resistance: i32,
    /// Automatically keep mouse captive when a fullscreen or pointer-locked game is running.
    /// This device's own choice: it doesn't travel with the layout.
    #[serde(default = "default_true")]
    pub auto_game_lock: bool,
    /// Manually lock mouse & keyboard captive to the currently focused device (Ctrl+Alt+G).
    /// This device's own choice: it doesn't travel with the layout.
    #[serde(default)]
    pub manual_game_lock: bool,
}

/// Where one device goes in an edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevicePlace {
    pub node: NodeId,
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub custom_size: Option<(i32, i32)>,
}

/// How the devices meet. It changes as a whole: half of one arrangement and half of another
/// isn't an arrangement anyone chose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Arrangement {
    pub devices: Vec<DevicePlace>,
    pub attach_side: TouchSide,
}

/// The body of `POST /api/layout`: only the parts that changed. Whatever is left out stays as it
/// is here, so an edit made from an old view of the layout can't undo a newer edit to anything else.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutEdit {
    pub arrangement: Option<Arrangement>,
    pub edge_resistance: Option<i32>,
}

/// Where the devices sat before a trial of a new screen mode.
#[derive(Debug, Clone, PartialEq)]
struct HeldPlace {
    arrangement: Arrangement,
    stamp: (u64, NodeId),
}

/// The layout file's fields from before devices were equal.
#[derive(Deserialize)]
struct LegacyLayout {
    debian_monitors: Vec<MonitorRect>,
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
    win_native_w: Option<i32>,
    win_native_h: Option<i32>,
    #[serde(default)]
    resolution_mode: String,
    attach_side: TouchSide,
    mouse_speed: Option<f32>,
    edge_resistance: i32,
    #[serde(default = "default_true")]
    auto_game_lock: bool,
    #[serde(default)]
    manual_game_lock: bool,
}

impl LegacyLayout {
    fn into_config(self) -> SpatialConfig {
        let (w, h) = (self.win_native_w.unwrap_or(self.win_w), self.win_native_h.unwrap_or(self.win_h));
        let custom = self.resolution_mode == "custom";
        SpatialConfig {
            // Above a device that has never had a layout, so that this arrangement is the one kept.
            revision: 1,
            editor: NodeId::Debian,
            arrangement_stamp: no_stamp(),
            resistance_stamp: no_stamp(),
            devices: vec![
                DeviceLayout {
                    node: NodeId::Debian,
                    name: String::new(),
                    x: 0,
                    y: 0,
                    monitors: self.debian_monitors,
                    custom_size: None,
                },
                DeviceLayout {
                    node: NodeId::Windows,
                    name: String::new(),
                    x: self.win_x,
                    y: self.win_y,
                    monitors: vec![MonitorRect { name: "Laptop display".to_string(), x: 0, y: 0, w, h }],
                    custom_size: custom.then_some((self.win_w, self.win_h)),
                },
            ],
            attach_side: self.attach_side,
            edge_resistance: self.edge_resistance,
            auto_game_lock: self.auto_game_lock,
            manual_game_lock: self.manual_game_lock,
        }
    }
}

/// What a layout file held.
#[derive(Debug, PartialEq)]
pub struct ParsedLayout {
    pub config: SpatialConfig,
    /// Debian's pointer speed, which an old layout file carried and settings now do.
    pub old_pointer_speed: Option<f32>,
}

/// Reads a layout in either form, or `None` if it is neither. Old field names become the device
/// list, and every value is brought into range.
pub fn parse_layout(text: &str) -> Option<ParsedLayout> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let (mut config, old_pointer_speed) = if value.get("devices").is_some() {
        (serde_json::from_value::<SpatialConfig>(value).ok()?, None)
    } else {
        let old: LegacyLayout = serde_json::from_value(value).ok()?;
        let speed = old.mouse_speed;
        (old.into_config(), speed)
    };
    config.repair();
    Some(ParsedLayout { config, old_pointer_speed })
}

/// Brings reported monitors into range and drops any that can't be real.
pub fn clean_monitors(monitors: Vec<MonitorRect>) -> Vec<MonitorRect> {
    monitors
        .into_iter()
        .filter(|m| m.w > 0 && m.h > 0)
        .take(MAX_MONITORS)
        .map(|m| MonitorRect {
            name: m.name.chars().take(MAX_NAME_CHARS).collect(),
            x: m.x.clamp(-MAX_COORDINATE, MAX_COORDINATE),
            y: m.y.clamp(-MAX_COORDINATE, MAX_COORDINATE),
            w: m.w.min(MAX_MONITOR_SIZE),
            h: m.h.min(MAX_MONITOR_SIZE),
        })
        .collect()
}

fn clean_size(size: Option<(i32, i32)>) -> Option<(i32, i32)> {
    size.filter(|&(w, h)| w > 0 && h > 0).map(|(w, h)| (w.min(MAX_MONITOR_SIZE), h.min(MAX_MONITOR_SIZE)))
}

impl SpatialConfig {
    /// The layout of a device that has never had one. `edge` is the side of this device the other
    /// one sits on, and `None` puts the laptop to the right of the desktop.
    pub fn fresh(me: NodeId, edge: Option<SwitchEdge>) -> Self {
        let edge = edge.unwrap_or(match me {
            NodeId::Debian => SwitchEdge::Right,
            NodeId::Windows => SwitchEdge::Left,
        });
        let debian_edge = if me == NodeId::Debian { edge } else { opposite(edge) };
        let attach_side = match debian_edge {
            SwitchEdge::Right => TouchSide::Right,
            SwitchEdge::Left => TouchSide::Left,
            SwitchEdge::Bottom => TouchSide::Bottom,
            SwitchEdge::Top => TouchSide::Top,
        };
        let mut config = Self {
            revision: 0,
            editor: me,
            arrangement_stamp: no_stamp(),
            resistance_stamp: no_stamp(),
            devices: vec![DeviceLayout::placeholder(NodeId::Debian), DeviceLayout::placeholder(NodeId::Windows)],
            attach_side,
            edge_resistance: 55,
            auto_game_lock: true,
            manual_game_lock: false,
        };
        config.resnap();
        config
    }

    /// Leaves exactly one entry per device, with every value in range.
    fn repair(&mut self) {
        let mut devices = Vec::with_capacity(2);
        for node in [NodeId::Debian, NodeId::Windows] {
            let found = self.devices.iter().find(|d| d.node == node).cloned();
            devices.push(found.unwrap_or_else(|| DeviceLayout::placeholder(node)));
        }
        for device in &mut devices {
            device.name = device.name.chars().take(MAX_NAME_CHARS).collect();
            device.x = device.x.clamp(-MAX_COORDINATE, MAX_COORDINATE);
            device.y = device.y.clamp(-MAX_COORDINATE, MAX_COORDINATE);
            device.monitors = clean_monitors(std::mem::take(&mut device.monitors));
            device.custom_size = clean_size(device.custom_size);
        }
        self.devices = devices;
        self.edge_resistance = self.edge_resistance.clamp(0, MAX_EDGE_RESISTANCE);
        if self.revision > MAX_REVISION {
            self.revision = 0;
        }
        // A copy from before parts had stamps, or with a bad one: all of it dates from its revision.
        let whole_copy = self.stamp();
        for stamp in [&mut self.arrangement_stamp, &mut self.resistance_stamp] {
            if stamp.0 == 0 || stamp.0 > whole_copy.0 {
                *stamp = whole_copy;
            }
        }
    }

    pub fn device(&self, node: NodeId) -> Option<&DeviceLayout> {
        self.devices.iter().find(|d| d.node == node)
    }

    fn device_mut(&mut self, node: NodeId) -> Option<&mut DeviceLayout> {
        self.devices.iter_mut().find(|d| d.node == node)
    }

    fn rects(&self, node: NodeId) -> Vec<MonitorRect> {
        self.device(node).map(DeviceLayout::rects).unwrap_or_default()
    }

    /// This device's own monitors, in its own desktop coordinates.
    pub fn monitors(&self, node: NodeId) -> &[MonitorRect] {
        self.device(node).map_or(&[], |d| &d.monitors)
    }

    fn stamp(&self) -> (u64, NodeId) {
        (self.revision, self.editor)
    }

    /// Records the monitors `node` reports, keeping it against the same edge. True if anything
    /// changed. No monitors at all means detection failed, so the last known ones stay.
    pub fn report_monitors(&mut self, node: NodeId, name: &str, monitors: Vec<MonitorRect>) -> bool {
        let monitors = clean_monitors(monitors);
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let Some(device) = self.device_mut(node) else { return false };
        let renamed = !name.is_empty() && device.name != name;
        if renamed {
            device.name = name;
        }
        let moved = !monitors.is_empty() && device.monitors != monitors;
        if moved {
            device.monitors = monitors;
            self.resnap();
        }
        renamed || moved
    }

    /// Moves the laptop back onto the edge it is attached to, after a size changed.
    pub fn resnap(&mut self) {
        let Some((left, top, right, bottom)) = bounds(&self.rects(NodeId::Debian)) else { return };
        let Some((win_left, win_top, win_right, win_bottom)) = bounds(&self.rects(NodeId::Windows)) else {
            return;
        };
        let (win_w, win_h) = (win_right - win_left, win_bottom - win_top);
        // `max` then `min`, so a screen too small for the range doesn't panic.
        let keep = |v: i32, lo: i32, hi: i32| v.min(hi).max(lo);
        let keep_y = || keep(win_top, top - win_h + 240, bottom - 240);
        let keep_x = || keep(win_left, left - win_w + 280, right - 280);
        // The monitor under the laptop's middle decides how far down or up it goes.
        let center_x = win_left + win_w / 2;
        let debian = self.rects(NodeId::Debian);
        let under = debian.iter().find(|m| center_x >= m.x && center_x <= m.x + m.w);
        let (x, y) = match self.attach_side {
            TouchSide::Right => (right, keep_y()),
            TouchSide::Left => (left - win_w, keep_y()),
            TouchSide::Bottom => (keep_x(), under.map_or(bottom, |m| m.y + m.h)),
            TouchSide::Top => (keep_x(), under.map_or(top, |m| m.y) - win_h),
        };
        if let Some(laptop) = self.device_mut(NodeId::Windows) {
            laptop.x += x - win_left;
            laptop.y += y - win_top;
        }
    }

    /// Lines the laptop up exactly with the one monitor of the desktop it nearly lines up with, as
    /// an edit made on `me`. That is a monitor of the same width (the same height beside the
    /// desktop) that the laptop touches, and starts within `ALIGN_SNAP` pixels of. Nothing ever made
    /// a few pixels off a choice, and they put the cursor a few pixels off on every crossing. True
    /// if it moved.
    pub fn align_exactly(&mut self, me: NodeId) -> bool {
        let Some((wl, wt, wr, wb)) = bounds(&self.rects(NodeId::Windows)) else { return false };
        let (ww, wh) = (wr - wl, wb - wt);
        let near = |offset: i32| offset != 0 && offset.abs() <= ALIGN_SNAP;
        let shifts: Vec<(i32, i32)> = self
            .rects(NodeId::Debian)
            .iter()
            .filter_map(|m| {
                let (dx, dy) = (m.x - wl, m.y - wt);
                let fits = match self.attach_side {
                    TouchSide::Bottom => wt == m.y + m.h && m.w == ww && near(dx),
                    TouchSide::Top => wb == m.y && m.w == ww && near(dx),
                    TouchSide::Right => wl == m.x + m.w && m.h == wh && near(dy),
                    TouchSide::Left => wr == m.x && m.h == wh && near(dy),
                };
                let along_x = matches!(self.attach_side, TouchSide::Bottom | TouchSide::Top);
                fits.then_some(if along_x { (dx, 0) } else { (0, dy) })
            })
            .collect();
        let [(dx, dy)] = shifts[..] else { return false };
        let mut arrangement = self.arrangement();
        for place in arrangement.devices.iter_mut().filter(|p| p.node == NodeId::Windows) {
            place.x += dx;
            place.y += dy;
        }
        self.apply_edit(&LayoutEdit { arrangement: Some(arrangement), edge_resistance: None }, me)
    }

    /// The edge of `node` that faces the other device.
    pub fn facing_edge(&self, node: NodeId) -> SwitchEdge {
        let debian_edge = match self.attach_side {
            TouchSide::Right => SwitchEdge::Right,
            TouchSide::Left => SwitchEdge::Left,
            TouchSide::Bottom => SwitchEdge::Bottom,
            TouchSide::Top => SwitchEdge::Top,
        };
        if node == NodeId::Debian {
            debian_edge
        } else {
            opposite(debian_edge)
        }
    }

    /// The part of `node`'s edge that touches the other device, in `node`'s desktop pixels along
    /// that edge.
    ///
    /// Only the closest monitors count, so a shorter monitor beside a taller one is not treated
    /// as touching. With nothing alongside, the span is empty (`start > end`), which no push can
    /// cross.
    pub fn edge_span(&self, node: NodeId) -> EdgeSpan {
        let edge = self.facing_edge(node);
        let empty = EdgeSpan { edge, start: 0, end: -1 };
        let Some(device) = self.device(node) else { return empty };
        let side = |m: &MonitorRect, edge: SwitchEdge| match edge {
            SwitchEdge::Right => m.x + m.w,
            SwitchEdge::Left => m.x,
            SwitchEdge::Bottom => m.y + m.h,
            SwitchEdge::Top => m.y,
        };
        let along_x = matches!(edge, SwitchEdge::Top | SwitchEdge::Bottom);
        let along = |m: &MonitorRect| if along_x { (m.x, m.x + m.w) } else { (m.y, m.y + m.h) };
        let origin = if along_x { device.x } else { device.y };
        let (mine, theirs) = (device.rects(), self.rects(node.other()));
        // (gap between the two monitors, overlap start, overlap end), device-local and inclusive.
        let mut overlaps = Vec::new();
        for m in &mine {
            for o in &theirs {
                let (start, end) = (along(m).0.max(along(o).0), along(m).1.min(along(o).1));
                if start < end {
                    let gap = (side(m, edge) - side(o, opposite(edge))).abs();
                    overlaps.push((gap, start - origin, end - 1 - origin));
                }
            }
        }
        let Some(nearest) = overlaps.iter().map(|o| o.0).min() else { return empty };
        let touching = || overlaps.iter().filter(|o| o.0 == nearest);
        EdgeSpan {
            edge,
            start: touching().map(|o| o.1).min().unwrap_or(0),
            end: touching().map(|o| o.2).max().unwrap_or(-1),
        }
    }

    /// Where a cursor at `(x, y)` on `me` lands on the other device, in that device's own desktop
    /// coordinates: on its nearest monitor, and in from any edge. `KEEP_CURSOR` in either
    /// coordinate means no warp, so it is returned for both.
    pub fn entry_on_peer(&self, me: NodeId, x: i32, y: i32) -> (i32, i32) {
        if x == KEEP_CURSOR || y == KEEP_CURSOR {
            return (KEEP_CURSOR, KEEP_CURSOR);
        }
        let (Some(from), Some(to)) = (self.device(me), self.device(me.other())) else { return (x, y) };
        let rects = to.rects();
        let point = (x.saturating_add(from.x), y.saturating_add(from.y));
        let shared = self.facing_edge(me.other());
        let (px, py) = inset_on_monitor(&rects, clamp_to_monitors(&rects, point), Some(shared));
        (px - to.x, py - to.y)
    }

    /// Keeps a warp the peer asked for on one of `me`'s own monitors.
    pub fn place_on_own(&self, me: NodeId, x: i32, y: i32) -> (i32, i32) {
        if x == KEEP_CURSOR || y == KEEP_CURSOR {
            return (KEEP_CURSOR, KEEP_CURSOR);
        }
        let monitors = self.monitors(me);
        inset_on_monitor(monitors, clamp_to_monitors(monitors, (x, y)), None)
    }

    fn take_arrangement_of(&mut self, other: &SpatialConfig) {
        for device in &mut self.devices {
            if let Some(theirs) = other.device(device.node) {
                device.x = theirs.x;
                device.y = theirs.y;
                device.custom_size = theirs.custom_size;
            }
        }
        self.attach_side = other.attach_side;
        self.arrangement_stamp = other.arrangement_stamp;
    }

    /// True if a copy with this stamp should replace this one.
    fn should_take(&self, stamp: (u64, NodeId)) -> bool {
        is_newer(stamp, self.stamp()) && stamp.0 <= self.revision.saturating_add(MAX_REVISION_JUMP)
    }

    /// Takes what is newer in a copy from the other device, if that copy is the newer one. True if
    /// it was. Monitors stay as reported to this device, and the game locks stay its own.
    ///
    /// A part edited here more recently than in that copy stays, and this copy then moves one
    /// past it, so that the other device comes and fetches that part in turn.
    pub fn adopt(&mut self, theirs: &SpatialConfig) -> bool {
        if !self.should_take(theirs.stamp()) {
            return false;
        }
        let mut kept = false;
        match (is_newer(theirs.arrangement_stamp, self.arrangement_stamp), is_newer(self.arrangement_stamp, theirs.arrangement_stamp)) {
            (true, _) => self.take_arrangement_of(theirs),
            (_, keep) => kept |= keep,
        }
        match (is_newer(theirs.resistance_stamp, self.resistance_stamp), is_newer(self.resistance_stamp, theirs.resistance_stamp)) {
            (true, _) => {
                self.edge_resistance = theirs.edge_resistance;
                self.resistance_stamp = theirs.resistance_stamp;
            }
            (_, keep) => kept |= keep,
        }
        self.revision = theirs.revision.saturating_add(u64::from(kept));
        self.editor = theirs.editor;
        true
    }

    /// Applies an edit made on `me`, one past this copy, and only to the parts it names. True if
    /// it named any. An edit carries no revision of its own: it comes from whoever sent it.
    pub fn apply_edit(&mut self, edit: &LayoutEdit, me: NodeId) -> bool {
        let stamp = (self.revision.saturating_add(1), me);
        if let Some(arrangement) = &edit.arrangement {
            for place in &arrangement.devices {
                if let Some(device) = self.device_mut(place.node) {
                    device.x = place.x.clamp(-MAX_COORDINATE, MAX_COORDINATE);
                    device.y = place.y.clamp(-MAX_COORDINATE, MAX_COORDINATE);
                    device.custom_size = clean_size(place.custom_size);
                }
            }
            self.attach_side = arrangement.attach_side;
            self.arrangement_stamp = stamp;
        }
        if let Some(resistance) = edit.edge_resistance {
            self.edge_resistance = resistance.clamp(0, MAX_EDGE_RESISTANCE);
            self.resistance_stamp = stamp;
        }
        let edited = edit.arrangement.is_some() || edit.edge_resistance.is_some();
        if edited {
            (self.revision, self.editor) = stamp;
        }
        edited
    }

    fn arrangement(&self) -> Arrangement {
        let place = |d: &DeviceLayout| DevicePlace { node: d.node, x: d.x, y: d.y, custom_size: d.custom_size };
        Arrangement { devices: self.devices.iter().map(place).collect(), attach_side: self.attach_side }
    }

    /// Where the devices sit, to come back to after a trial of a new screen mode.
    fn held_place(&self) -> HeldPlace {
        HeldPlace { arrangement: self.arrangement(), stamp: self.arrangement_stamp }
    }

    /// Puts the devices back after a trial was undone: a size that changed for a moment moved them
    /// to keep them on their edge, and it moved them on both devices, so this goes out as an edit.
    /// An arrangement someone edited during the trial stands. True if anything moved.
    fn return_to(&mut self, held: &HeldPlace, me: NodeId) -> bool {
        if self.arrangement_stamp != held.stamp || self.arrangement() == held.arrangement {
            return false;
        }
        self.apply_edit(&LayoutEdit { arrangement: Some(held.arrangement.clone()), edge_resistance: None }, me)
    }

    /// Drops any custom size, as an edit: the screens go back to what the devices report.
    pub fn reset_sizes(&mut self, me: NodeId) {
        for device in &mut self.devices {
            device.custom_size = None;
        }
        self.resnap();
        self.revision = self.revision.saturating_add(1);
        self.editor = me;
        self.arrangement_stamp = self.stamp();
    }
}

/// The closest pixel to `(x, y)` on any of `monitors`, or the point itself with none to go on.
fn clamp_to_monitors(monitors: &[MonitorRect], (x, y): (i32, i32)) -> (i32, i32) {
    let on_monitor = |m: &MonitorRect| (x.clamp(m.x, m.x + m.w - 1), y.clamp(m.y, m.y + m.h - 1));
    let distance = |&(px, py): &(i32, i32)| {
        let (dx, dy) = (i128::from(px) - i128::from(x), i128::from(py) - i128::from(y));
        dx * dx + dy * dy
    };
    monitors.iter().map(on_monitor).min_by_key(distance).unwrap_or((x, y))
}

/// Pulls a point `ENTRY_INSET` pixels in from any monitor edge it sits on, so entry does not land
/// on the boundary pixel, and `SHARED_EDGE_INSET` from the `shared` one it came in by. Stays inside
/// the monitor.
fn inset_on_monitor(monitors: &[MonitorRect], p: (i32, i32), shared: Option<SwitchEdge>) -> (i32, i32) {
    let by = |edge: SwitchEdge| if shared == Some(edge) { SHARED_EDGE_INSET } else { ENTRY_INSET };
    let inset = |v: i32, lo: i32, size: i32, (low, high): (SwitchEdge, SwitchEdge)| {
        let hi = lo + size - 1;
        let (lo_i, hi_i) = (lo + by(low), hi - by(high));
        if lo_i > hi_i {
            // Too small to inset on both sides: use the middle.
            return lo + (hi - lo) / 2;
        }
        if v == lo {
            lo_i
        } else if v == hi {
            hi_i
        } else {
            v
        }
    };
    match monitors.iter().find(|m| contains(m, p)) {
        Some(m) => (
            inset(p.0, m.x, m.w, (SwitchEdge::Left, SwitchEdge::Right)),
            inset(p.1, m.y, m.h, (SwitchEdge::Top, SwitchEdge::Bottom)),
        ),
        None => p,
    }
}

fn contains(m: &MonitorRect, (x, y): (i32, i32)) -> bool {
    (m.x..m.x + m.w).contains(&x) && (m.y..m.y + m.h).contains(&y)
}

/// The edge of this device that touches the other one, and how hard the cursor has pushed at it.
/// The span comes from the other device's `EdgeMap`, and from this device's own layout until one
/// arrives, as an other device that doesn't send them can still be crossed to.
pub struct EdgeWatch {
    span: Option<EdgeSpan>,
    tracker: EdgeTracker,
    heard_from_peer: bool,
    settle: Settle,
}

/// What a movement at the edge came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// Not a completed push across.
    Pushing,
    Crossed,
    /// A completed push that doesn't cross, and why.
    Refused(Refusal),
}

impl EdgeWatch {
    pub const fn new() -> Self {
        Self { span: None, tracker: EdgeTracker::new(0), heard_from_peer: false, settle: Settle::new() }
    }

    /// Control arrived at this device at `now_ms`: any push so far is forgotten, and the settle
    /// starts.
    pub fn arrived(&mut self, now_ms: u64) {
        self.reset();
        self.settle.arrived(now_ms);
    }

    /// How much of the full settle was still to go: see `Settle::full_left_ms`.
    pub fn settle_full_left_ms(&self, now_ms: u64) -> u64 {
        self.settle.full_left_ms(now_ms)
    }

    /// For a completed push: true if it is too soon after control arrived. The count then starts
    /// again, as a push during the settle never counted.
    pub fn too_soon(&mut self, now_ms: u64) -> bool {
        let soon = self.settle.left_ms(now_ms) > 0;
        if soon {
            self.tracker.clear();
        }
        soon
    }

    /// How far `(x, y)` is in from the shared edge's side of the monitor under it.
    fn edge_distance(&self, monitors: &[MonitorRect], (x, y): (i32, i32)) -> Option<i32> {
        let span = self.span?;
        let m = monitors.iter().find(|m| contains(m, (x, y)))?;
        Some(match span.edge {
            SwitchEdge::Left => x - m.x,
            SwitchEdge::Right => m.x + m.w - 1 - x,
            SwitchEdge::Top => y - m.y,
            SwitchEdge::Bottom => m.y + m.h - 1 - y,
        })
    }

    /// Takes the edge from the other device's `EdgeMap`.
    pub fn set_span(&mut self, span: EdgeSpan, resistance: i32) {
        self.heard_from_peer = true;
        self.apply(span, resistance);
    }

    /// Takes the edge from this device's own layout, unless the other device has said.
    pub fn use_own_layout(&mut self, config: &SpatialConfig, me: NodeId) {
        if !self.heard_from_peer {
            self.apply(config.edge_span(me), config.edge_resistance);
        }
    }

    fn apply(&mut self, span: EdgeSpan, resistance: i32) {
        self.span = Some(span);
        self.tracker.set_resistance(resistance.clamp(0, MAX_EDGE_RESISTANCE));
    }

    /// Forgets any push so far, as after a focus change.
    pub fn reset(&mut self) {
        let span = EdgeSpan { edge: SwitchEdge::Left, start: 0, end: -1 };
        self.tracker.push(span, 0, 0, 1, 1, 0, 0, true);
    }

    /// True when moving by `(dx, dy)` to `(x, y)` completes a push across the shared edge. The
    /// count then starts again.
    #[cfg(test)]
    pub fn push(&mut self, monitors: &[MonitorRect], at: (i32, i32), delta: (i32, i32), buttons_held: bool) -> bool {
        let done = self.press(monitors, at, delta, buttons_held);
        if done {
            self.tracker.clear();
        }
        done
    }

    /// What moving by `(dx, dy)` to `(x, y)` at `now_ms` comes to: a crossing is a completed
    /// push, after the settle and outside the guard strip. A push completed in the strip waits
    /// there, and crosses once the cursor moves out of it along the edge. The laptop's cursor
    /// position is never late, so it decides at once.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn cross(&mut self, monitors: &[MonitorRect], at: (i32, i32), delta: (i32, i32), buttons_held: bool, now_ms: u64) -> Push {
        if !self.press(monitors, at, delta, buttons_held) {
            return Push::Pushing;
        }
        if self.too_soon(now_ms) {
            return Push::Refused(Refusal::Settle);
        }
        if self.guarded(monitors, at) {
            return Push::Refused(Refusal::Guard);
        }
        self.tracker.clear();
        Push::Crossed
    }

    /// As `push`, but a completed push stays complete until `clear`, as long as the cursor stays
    /// on the span and doesn't move in: it can wait there for a newer position to confirm it.
    ///
    /// The tracker works on one rectangle, so it is given the monitor under the cursor, and a
    /// point that isn't on an outer edge of the desktop doesn't count: the cursor there just
    /// carries on to the next monitor.
    pub fn press(&mut self, monitors: &[MonitorRect], (x, y): (i32, i32), (dx, dy): (i32, i32), buttons_held: bool) -> bool {
        let Some(span) = self.span else { return false };
        self.settle.observe(self.edge_distance(monitors, (x, y)));
        let Some(m) = outer_monitor(monitors, span.edge, (x, y)) else {
            self.reset();
            return false;
        };
        let shift = if matches!(span.edge, SwitchEdge::Top | SwitchEdge::Bottom) { m.x } else { m.y };
        let local = EdgeSpan { edge: span.edge, start: span.start - shift, end: span.end - shift };
        self.tracker.press(local, x - m.x, y - m.y, m.w, m.h, dx, dy, buttons_held)
    }

    /// True if the shared edge runs along x: the top or bottom.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn along_x(&self) -> bool {
        self.span.is_some_and(|s| matches!(s.edge, SwitchEdge::Top | SwitchEdge::Bottom))
    }

    /// Starts the count again, as after a crossing.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn clear(&mut self) {
        self.tracker.clear();
    }

    /// True if `(x, y)` is on the shared edge's span: on the outer edge of a monitor, alongside the
    /// other device.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn on_span(&self, monitors: &[MonitorRect], (x, y): (i32, i32)) -> bool {
        let Some(span) = self.span else { return false };
        let Some(m) = outer_monitor(monitors, span.edge, (x, y)) else { return false };
        let (on_edge, along) = match span.edge {
            SwitchEdge::Left => (x == m.x, y),
            SwitchEdge::Right => (x == m.x + m.w - 1, y),
            SwitchEdge::Top => (y == m.y, x),
            SwitchEdge::Bottom => (y == m.y + m.h - 1, x),
        };
        on_edge && (span.start..=span.end).contains(&along)
    }

    /// True if `(x, y)` is in a guard strip: within `GUARD` pixels of the end of the span, where a
    /// monitor that doesn't touch the other device carries on the same edge. Going along that edge
    /// onto that monitor is no push across, so the last pixels before it don't cross. An end at the
    /// desktop's own corner has nothing beside it, and no strip.
    pub fn guarded(&self, monitors: &[MonitorRect], (x, y): (i32, i32)) -> bool {
        let Some(span) = self.span else { return false };
        let Some(m) = outer_monitor(monitors, span.edge, (x, y)) else { return false };
        // A monitor's line on that edge, its pixels along it (end exclusive), and its last pixel on it.
        let side = |m: &MonitorRect| match span.edge {
            SwitchEdge::Left => (m.x, (m.y, m.y + m.h), (m.x, m.y + m.h - 1)),
            SwitchEdge::Right => (m.x + m.w, (m.y, m.y + m.h), (m.x + m.w - 1, m.y + m.h - 1)),
            SwitchEdge::Top => (m.y, (m.x, m.x + m.w), (m.x + m.w - 1, m.y)),
            SwitchEdge::Bottom => (m.y + m.h, (m.x, m.x + m.w), (m.x + m.w - 1, m.y + m.h - 1)),
        };
        let along = if matches!(span.edge, SwitchEdge::Top | SwitchEdge::Bottom) { x } else { y };
        let (line, ..) = side(m);
        monitors.iter().filter(|o| *o != m).any(|o| {
            let (o_line, (start, end), corner) = side(o);
            let touches = start <= span.end && end > span.start;
            let distance = if along >= end { along - end } else { start - 1 - along };
            o_line == line && !touches && outer_monitor(monitors, span.edge, corner).is_some() && (0..GUARD).contains(&distance)
        })
    }
}

/// The monitor under `(x, y)`, if the point is on the outer edge of the desktop facing `edge`.
fn outer_monitor(monitors: &[MonitorRect], edge: SwitchEdge, (x, y): (i32, i32)) -> Option<&MonitorRect> {
    let beyond = match edge {
        SwitchEdge::Left => (x - 1, y),
        SwitchEdge::Right => (x + 1, y),
        SwitchEdge::Top => (x, y - 1),
        SwitchEdge::Bottom => (x, y + 1),
    };
    let monitor = monitors.iter().find(|m| contains(m, (x, y)))?;
    (!monitors.iter().any(|m| contains(m, beyond))).then_some(monitor)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveTelemetry {
    pub active_node: String,
    pub panic_mode: bool,
    /// Where the cursor is on this device, in its own desktop coordinates.
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub peer_addr: String,
    pub game_captive_active: bool,
    pub game_reason: String,
    #[serde(default)]
    pub peer_online: bool,
    pub main_conflict: bool,
    #[serde(default)]
    pub link: LinkQuality,
}

impl LiveTelemetry {
    fn new(me: NodeId, peer_addr: String) -> Self {
        Self {
            active_node: me.to_string(),
            panic_mode: false,
            cursor_x: 0,
            cursor_y: 0,
            peer_addr,
            game_captive_active: false,
            game_reason: String::new(),
            peer_online: false,
            main_conflict: false,
            link: LinkQuality::default(),
        }
    }
}

pub struct SharedSpatialState {
    pub me: NodeId,
    /// `--peer` was given at start, so the address can't be changed from the app.
    pub peer_pinned: bool,
    pub config: SpatialConfig,
    pub telemetry: LiveTelemetry,
    pub pending_manual_switch: bool,
    pub pending_panic: bool,
    /// The newest revision the other device has said it holds.
    peer_revision: Option<(u64, NodeId)>,
    /// Where the layout is saved. `None` never saves.
    file: Option<PathBuf>,
}

/// What the input loop picks up from the app.
pub struct StudioRequests {
    pub manual_switch: bool,
    pub panic: bool,
    pub config: SpatialConfig,
}

/// Takes pending app requests with a config snapshot, or `None` if the lock is busy. The input
/// loop must never wait here: it runs the link timeout and the failsafe hotkeys.
pub fn take_studio_requests(state: &RwLock<SharedSpatialState>) -> Option<StudioRequests> {
    let mut guard = state.try_write().ok()?;
    Some(StudioRequests {
        manual_switch: std::mem::take(&mut guard.pending_manual_switch),
        panic: std::mem::take(&mut guard.pending_panic),
        config: guard.config.clone(),
    })
}

/// The saved layout in `dir`, or else the first old one in `old_dirs`, which are only ever read:
/// an old file stays where it is. A saved layout at revision 0 was never edited, so a start that
/// couldn't see the old file doesn't cost the arrangement in it.
fn load_layout(dir: &Path, old_dirs: &[PathBuf]) -> Option<ParsedLayout> {
    let read = |dir: &Path| fs::read_to_string(dir.join(LAYOUT_FILE)).ok().and_then(|text| parse_layout(&text));
    let saved = read(dir);
    if saved.as_ref().is_some_and(|s| s.config.revision > 0) {
        return saved;
    }
    let is_old = |parsed: &ParsedLayout| parsed.old_pointer_speed.is_some() || parsed.config.revision > 0;
    old_dirs.iter().filter(|old| old.as_path() != dir).filter_map(|old| read(old)).find(is_old).or(saved)
}

impl SharedSpatialState {
    /// Loads this device's copy of the layout. The monitors come fresh from the hardware.
    pub fn load(platform: &dyn Platform, peer_addr: String, peer_pinned: bool) -> Self {
        let dir = crate::settings::config_dir();
        let me = platform.node();
        let parsed = load_layout(&dir, &platform.old_layout_dirs());
        let (mut config, old_speed) = match parsed {
            Some(p) => (p.config, p.old_pointer_speed),
            None => (SpatialConfig::fresh(me, crate::settings::switch_edge()), None),
        };
        if let Some(speed) = old_speed {
            crate::settings::adopt_pointer_speed(speed);
        }
        config.report_monitors(me, &crate::settings::device_name(), platform.monitors());
        if config.align_exactly(me) {
            crate::settings::log_note("Lined the laptop up exactly with the monitor it was a few pixels off.");
        }
        let state = Self {
            me,
            peer_pinned,
            config,
            telemetry: LiveTelemetry::new(me, peer_addr),
            pending_manual_switch: false,
            pending_panic: false,
            peer_revision: None,
            file: Some(dir.join(LAYOUT_FILE)),
        };
        state.save();
        state
    }

    pub fn save(&self) {
        let Some(path) = &self.file else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            if let Err(e) = crate::settings::write_atomically(path, &json) {
                crate::settings::log_note(&format!("Couldn't save the layout: {e}"));
            }
        }
    }

    /// True when the other device has said it holds a newer layout than this one.
    fn peer_is_newer(&self) -> bool {
        self.peer_revision.is_some_and(|theirs| self.config.should_take(theirs))
    }
}

/// What goes to the other device with each heartbeat, the same from both: this device's screens,
/// the edge of the other device that touches them, the manual game lock, and the layout it holds.
pub fn heartbeat_payloads(config: &SpatialConfig, me: NodeId) -> Vec<Payload> {
    let span = config.edge_span(me.other());
    vec![
        Payload::Heartbeat,
        Payload::PeerDisplayInfo {
            name: config.device(me).map_or_else(String::new, |d| d.name.clone()),
            monitors: config.monitors(me).to_vec(),
        },
        Payload::EdgeMap { edge: span.edge, start: span.start, end: span.end, resistance: config.edge_resistance },
        Payload::GameLock { locked: config.manual_game_lock },
        Payload::LayoutRevision { revision: config.revision, editor: config.editor },
    ]
}

/// Records what the other device reported about its screens. Never waits for the lock: a busy
/// lock just means the next heartbeat does this instead.
pub fn note_peer_displays(state: &RwLock<SharedSpatialState>, name: &str, monitors: Vec<MonitorRect>) {
    if let Ok(mut guard) = state.try_write() {
        let peer = guard.me.other();
        if guard.config.report_monitors(peer, name, monitors) {
            guard.save();
        }
    }
}

/// Records the revision the other device says it holds. The sync thread does the rest.
pub fn note_peer_revision(state: &RwLock<SharedSpatialState>, revision: u64, editor: NodeId) {
    if let Ok(mut guard) = state.try_write() {
        guard.peer_revision = Some((revision, editor));
    }
}

/// Pushes this device's monitors, which can change while it runs, into its layout.
fn refresh_own_monitors(state: &RwLock<SharedSpatialState>, platform: &dyn Platform) {
    let (monitors, name) = (platform.monitors(), crate::settings::device_name());
    if let Ok(mut guard) = state.write() {
        let me = guard.me;
        if guard.config.report_monitors(me, &name, monitors) {
            guard.save();
        }
    }
}

/// Fetches the other device's copy, if it says it has a newer one. What arrives is held to the
/// same limits as a saved file.
fn pull_layout(state: &RwLock<SharedSpatialState>, fetch: impl FnOnce() -> Option<String>) {
    if !state.read().is_ok_and(|g| g.peer_is_newer()) {
        return;
    }
    let Some(body) = fetch() else { return };
    let config = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|state| parse_layout(&state.get("config")?.to_string()));
    if let (Some(theirs), Ok(mut guard)) = (config, state.write()) {
        if guard.config.adopt(&theirs.config) {
            guard.save();
        }
    }
}

/// Starts watching this device's monitors. The layout follows a mode change at once, rather than
/// at the next look at the monitors, and a trial that is undone leaves the devices where they sat.
pub fn start_displays(state: Arc<RwLock<SharedSpatialState>>, platform: Arc<dyn Platform>) -> Arc<DisplayManager> {
    let source = platform.clone();
    let mut held: Option<HeldPlace> = None;
    let displays = DisplayManager::new(platform, Box::new(move |news| follow_display_news(&state, source.as_ref(), &mut held, news)));
    displays.start();
    displays
}

fn follow_display_news(
    state: &RwLock<SharedSpatialState>,
    platform: &dyn Platform,
    held: &mut Option<HeldPlace>,
    news: LayoutNews,
) {
    match news {
        LayoutNews::TrialStarting => *held = state.read().ok().map(|g| g.config.held_place()),
        LayoutNews::MonitorsChanged => refresh_own_monitors(state, platform),
        LayoutNews::TrialOver => *held = None,
        LayoutNews::TrialUndone => {
            let Some(place) = held.take() else { return };
            if let Ok(mut g) = state.write() {
                let me = g.me;
                if g.config.return_to(&place, me) {
                    g.save();
                }
            }
        }
    }
}

const SYNC_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
/// How many sync passes between looks at this device's monitors. `xrandr` isn't free.
const PASSES_PER_MONITOR_CHECK: u32 = 10;

/// Keeps this device's copy of the layout current, off the input thread: monitor detection can
/// hang, and the other device can take a while to answer.
pub fn start_sync(state: Arc<RwLock<SharedSpatialState>>, platform: Arc<dyn Platform>) {
    let spawned = std::thread::Builder::new().name("mesh-kvm-layout-sync".to_string()).spawn(move || {
        let mut passes = 0u32;
        loop {
            std::thread::sleep(SYNC_EVERY);
            pull_layout(&state, || match crate::secure::control::peer_call("GET", "/api/state", "") {
                Ok((200, body)) => Some(body),
                _ => None,
            });
            passes = passes.wrapping_add(1);
            if passes.is_multiple_of(PASSES_PER_MONITOR_CHECK) {
                refresh_own_monitors(&state, platform.as_ref());
            }
        }
    });
    if let Err(e) = spawned {
        crate::settings::log_note(&format!("Couldn't start keeping the layout in step: {e}"));
    }
}

// ============================================================================================
// The API both services serve
// ============================================================================================

fn action_of(val: &serde_json::Value) -> &str {
    val.get("action").and_then(|v| v.as_str()).unwrap_or("")
}

/// Builds the API handler (plugged into the shared `webui` server).
pub fn studio_handler(
    state: Arc<RwLock<SharedSpatialState>>,
    platform: Arc<dyn Platform>,
    displays: Arc<DisplayManager>,
    pairing: Arc<PairingService>,
) -> crate::webui::Handler {
    Arc::new(move |req: &crate::webui::HttpRequest| -> Option<String> {
        let path = req.path.split('?').next().unwrap_or("");
        // Pairing decides by `Source` itself, as one of its actions is the peer's alone.
        if let Some(reply) = pairing.api(req) {
            return Some(reply);
        }
        match (req.method.as_str(), path) {
            ("GET", "/api/state") => {
                let g = state.read().ok()?;
                Some(serde_json::json!({ "config": g.config, "telemetry": g.telemetry }).to_string())
            }
            ("GET", "/api/local") => {
                let g = state.read().ok()?;
                Some(local_status(&g, platform.as_ref(), &displays, pairing.secure_label()).to_string())
            }
            ("GET", "/api/displays") => Some(displays.list_json()),
            ("POST", "/api/local/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = action_of(&val);
                if act == "set_main_device" {
                    return Some(set_main_device(&state, &val));
                }
                if act == "set_peer" {
                    return Some(set_peer(&state, &val, req.from_app_here));
                }
                if act == "set_device_name" {
                    return Some(set_device_name(&state, &val));
                }
                if act == crate::quit::ACTION {
                    return Some(crate::quit::answer(req.from_app_here));
                }
                if act == "set_audio_device" {
                    return Some(crate::settings::set_audio_device(&val));
                }
                if let Some(reply) = crate::update::handle_action(&val, req.from_app_here) {
                    return Some(reply);
                }
                // These reach the operating system, under the same rule as every other change:
                // `webui` has already turned away anything but the app here and the other device.
                match act {
                    "set_display_mode" => return Some(displays.request(&val)),
                    "keep_display_mode" => return Some(displays.decide(&val, true, req.from_app_here)),
                    "undo_display_mode" => return Some(displays.decide(&val, false, req.from_app_here)),
                    _ => {}
                }
                if !crate::settings::handle_common_action(act, &val) {
                    match act {
                        "set_autostart" => {
                            platform.set_autostart(val.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
                        }
                        // Focus / panic / game lock are layout-wide actions; accept them here too
                        "toggle_focus" | "panic" | "toggle_game_lock" => {
                            apply_layout_action(&state, platform.as_ref(), act);
                        }
                        _ => return Some(r#"{"ok":false,"error":"unknown action"}"#.to_string()),
                    }
                }
                Some(crate::settings::OK_RESPONSE.to_string())
            }
            ("POST", "/api/layout") => {
                let refused = || Some(r#"{"ok":false,"error":"That isn't a layout change."}"#.to_string());
                let Ok(edit) = serde_json::from_str::<LayoutEdit>(&req.body) else { return refused() };
                let mut g = state.write().ok()?;
                let me = g.me;
                if !g.config.apply_edit(&edit, me) {
                    return refused();
                }
                g.save();
                Some(crate::settings::OK_RESPONSE.to_string())
            }
            ("POST", "/api/action") => {
                let val: serde_json::Value = serde_json::from_str(&req.body).ok()?;
                let act = action_of(&val);
                if act == "set_main_device" {
                    return Some(set_main_device(&state, &val));
                }
                // Legacy audio action names from older UIs
                let handled_audio = match act {
                    "toggle_audio_mute" => crate::settings::handle_common_action(
                        "set_audio_rx_muted",
                        &serde_json::json!({}),
                    ),
                    "set_audio_volume" => {
                        crate::settings::handle_common_action("set_audio_rx_volume", &val)
                    }
                    other => crate::settings::handle_common_action(other, &val),
                };
                if !handled_audio {
                    apply_layout_action(&state, platform.as_ref(), act);
                }
                Some(crate::settings::OK_RESPONSE.to_string())
            }
            _ => None,
        }
    })
}

/// This device's `/api/local`.
fn local_status(g: &SharedSpatialState, platform: &dyn Platform, displays: &DisplayManager, secure: &str) -> serde_json::Value {
    let peer_ip = g.telemetry.peer_addr.split(':').next().unwrap_or("").to_string();
    // With no peer yet, the address other devices would use is the one on the default route.
    let toward = if peer_ip.is_empty() { ANY_ROUTED_ADDRESS } else { peer_ip.as_str() };
    let mut status = serde_json::json!({
        "node": g.me.to_string().to_lowercase(),
        "label": platform.label(),
        "hostname": crate::webui::hostname(),
        "ip": crate::webui::local_ip_toward(toward),
        "paired": !peer_ip.is_empty(),
        "peer_pinned": g.peer_pinned,
        "device_name": crate::settings::device_name(),
        "peer_ip": peer_ip,
        "version": crate::update::trust::running_version().to_string(),
        "update": crate::update::status_json(),
        "os": platform.os_name(),
        "peer_online": g.telemetry.peer_online,
        "active_node": g.telemetry.active_node,
        "panic_mode": g.telemetry.panic_mode,
        "game_lock": g.telemetry.game_captive_active,
        "game_reason": g.telemetry.game_reason,
        "displays": g.config.monitors(g.me),
        "cursor": [g.telemetry.cursor_x, g.telemetry.cursor_y],
        "cursor_hidden": crate::input::cursor_hidden(),
        "autostart": platform.autostart_enabled(),
        "input_devices": platform.input_devices(),
        "audio": crate::settings::audio_status_json(),
        "display_change": displays.status_json(),
        "link": crate::settings::link_status_json(&g.telemetry.link),
        "secure": secure,
        // The other device runs a version that doesn't encrypt, by what either channel heard.
        "old_peer": g.telemetry.link.old_peer || crate::audio::other_version_arriving(std::time::Instant::now()),
    });
    crate::settings::add_toggle_status(&mut status, g.telemetry.main_conflict);
    status
}

fn set_main_device(state: &Arc<RwLock<SharedSpatialState>>, val: &serde_json::Value) -> String {
    let link_up = state.read().is_ok_and(|g| g.telemetry.peer_online);
    crate::settings::set_main_device(val, link_up)
}

/// Renames this device, and puts the name in the layout at once, so the app sees it on its next
/// poll. The other device hears of it with the next heartbeat.
fn set_device_name(state: &Arc<RwLock<SharedSpatialState>>, val: &serde_json::Value) -> String {
    let reply = crate::settings::set_device_name(val);
    if let Ok(mut g) = state.write() {
        let me = g.me;
        if g.config.report_monitors(me, &crate::settings::device_name(), Vec::new()) {
            g.save();
        }
    }
    reply
}

fn set_peer(state: &Arc<RwLock<SharedSpatialState>>, val: &serde_json::Value, from_this_device: bool) -> String {
    let pinned = state.read().is_ok_and(|g| g.peer_pinned);
    let is_own_address = |ip: &str| crate::webui::local_ip_toward(ip) == ip;
    crate::settings::set_peer(val, crate::settings::PeerChange { from_this_device, pinned, is_own_address: &is_own_address })
}

fn apply_layout_action(state: &Arc<RwLock<SharedSpatialState>>, platform: &dyn Platform, act: &str) {
    // Monitor detection can hang, so it runs before the lock.
    let detected = (act == "reset_native_resolutions").then(|| platform.monitors());
    let Ok(mut g) = state.write() else { return };
    let me = g.me;
    if let Some(monitors) = detected {
        g.config.report_monitors(me, "", monitors);
        g.config.reset_sizes(me);
        g.save();
        return;
    }
    match act {
        "toggle_focus" => g.pending_manual_switch = true,
        "panic" => g.pending_panic = true,
        "toggle_game_lock" => {
            g.config.manual_game_lock = !g.config.manual_game_lock;
            g.save();
        }
        "toggle_auto_game_lock" => {
            g.config.auto_game_lock = !g.config.auto_game_lock;
            g.save();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crossing::SETTLE_CLEAR;

    const OLD_LAYOUT: &str = r#"{
        "debian_monitors": [
            {"name": "DP-3 (240Hz Native)", "x": 0, "y": 360, "w": 1920, "h": 1080},
            {"name": "DP-2 (360Hz Native)", "x": 1920, "y": 0, "w": 2560, "h": 1440}
        ],
        "deb_total_w": 4480, "deb_total_h": 1440,
        "win_x": 1925, "win_y": 1440, "win_w": 2560, "win_h": 1600,
        "win_native_w": 2560, "win_native_h": 1600,
        "resolution_mode": "native", "attach_side": "Bottom", "mouse_speed": 0.35,
        "edge_resistance": 55, "auto_game_lock": true, "manual_game_lock": false
    }"#;

    fn rect(name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorRect {
        MonitorRect { name: name.into(), x, y, w, h }
    }

    fn dp3() -> MonitorRect {
        rect("DP-3", 0, 360, 1920, 1080)
    }

    fn dp2() -> MonitorRect {
        rect("DP-2", 1920, 0, 2560, 1440)
    }

    /// DP-3 (1920x1080 at 0,360) beside DP-2 (2560x1440 at 1920,0): both end at y = 1440.
    fn layout(attach_side: TouchSide, win_x: i32, win_y: i32) -> SpatialConfig {
        SpatialConfig {
            revision: 0,
            editor: NodeId::Debian,
            arrangement_stamp: no_stamp(),
            resistance_stamp: no_stamp(),
            devices: vec![
                DeviceLayout {
                    node: NodeId::Debian,
                    name: String::new(),
                    x: 0,
                    y: 0,
                    monitors: vec![dp3(), dp2()],
                    custom_size: None,
                },
                DeviceLayout {
                    node: NodeId::Windows,
                    name: String::new(),
                    x: win_x,
                    y: win_y,
                    monitors: vec![rect("Laptop", 0, 0, 1920, 1080)],
                    custom_size: None,
                },
            ],
            attach_side,
            edge_resistance: 55,
            auto_game_lock: true,
            manual_game_lock: false,
        }
    }

    fn span(edge: SwitchEdge, start: i32, end: i32) -> EdgeSpan {
        EdgeSpan { edge, start, end }
    }

    fn laptop_span(c: &SpatialConfig) -> EdgeSpan {
        c.edge_span(NodeId::Windows)
    }

    /// A laptop point, as it lands on Debian.
    fn to_debian(c: &SpatialConfig, x: i32, y: i32) -> (i32, i32) {
        c.entry_on_peer(NodeId::Windows, x, y)
    }

    fn laptop_mut(c: &mut SpatialConfig) -> &mut DeviceLayout {
        c.device_mut(NodeId::Windows).unwrap()
    }

    fn debian_mut(c: &mut SpatialConfig) -> &mut DeviceLayout {
        c.device_mut(NodeId::Debian).unwrap()
    }

    // ---- Migration ----

    #[test]
    fn old_field_names_become_the_device_list() {
        let parsed = parse_layout(OLD_LAYOUT).unwrap();
        let c = parsed.config;
        assert_eq!(parsed.old_pointer_speed, Some(0.35));
        assert_eq!(c.monitors(NodeId::Debian), &[rect("DP-3 (240Hz Native)", 0, 360, 1920, 1080), rect("DP-2 (360Hz Native)", 1920, 0, 2560, 1440)]);
        let laptop = c.device(NodeId::Windows).unwrap();
        assert_eq!((laptop.x, laptop.y), (1925, 1440));
        assert_eq!(laptop.monitors, vec![rect("Laptop display", 0, 0, 2560, 1600)]);
        assert_eq!(laptop.custom_size, None);
        assert_eq!(c.attach_side, TouchSide::Bottom);
        assert_eq!(c.edge_resistance, 55);
    }

    #[test]
    fn the_real_layout_is_lined_up_exactly_under_dp2_once() {
        let mut c = parse_layout(OLD_LAYOUT).unwrap().config;
        let before = c.stamp();
        assert!(c.align_exactly(NodeId::Debian));
        assert_eq!(laptop_x(&c), 1920);
        assert_eq!(c.device(NodeId::Windows).unwrap().y, 1440);
        // The normal way of an edit: one past this copy, and the arrangement's stamp with it, so
        // that the other device fetches it.
        assert_eq!(c.stamp(), (before.0 + 1, NodeId::Debian));
        assert_eq!(c.arrangement_stamp, c.stamp());
        assert!(!c.align_exactly(NodeId::Debian), "lined up, there's nothing more to do");
        assert_eq!(c.stamp(), (before.0 + 1, NodeId::Debian));
    }

    #[test]
    fn only_a_near_miss_of_one_monitor_the_same_size_is_lined_up() {
        let laptop_2560 = |x: i32| {
            let mut c = layout(TouchSide::Bottom, x, 1440);
            laptop_mut(&mut c).monitors = vec![rect("Laptop", 0, 0, 2560, 1600)];
            c
        };
        for x in [1912, 1928] {
            let mut c = laptop_2560(x);
            assert!(c.align_exactly(NodeId::Windows), "{x}");
            assert_eq!(laptop_x(&c), 1920);
        }
        for x in [1911, 1929, 1920, 2240] {
            let mut c = laptop_2560(x);
            assert!(!c.align_exactly(NodeId::Windows), "{x}");
            assert_eq!(laptop_x(&c), x);
        }
        // A narrower laptop near DP-2's start is a choice, not a near miss.
        let mut c = layout(TouchSide::Bottom, 1925, 1440);
        assert!(!c.align_exactly(NodeId::Debian));
        // Beside the desktop it is the heights that have to match, and y that lines up.
        let mut c = layout(TouchSide::Right, 4480, 4);
        laptop_mut(&mut c).monitors = vec![rect("Laptop", 0, 0, 1920, 1440)];
        assert!(c.align_exactly(NodeId::Debian));
        assert_eq!(c.device(NodeId::Windows).unwrap().y, 0);
        // And under DP-3, with its own width.
        let mut c = layout(TouchSide::Bottom, 6, 1440);
        assert!(c.align_exactly(NodeId::Debian));
        assert_eq!(laptop_x(&c), 0);
    }

    #[test]
    fn down_and_back_up_again_lands_on_the_same_column() {
        let mut c = parse_layout(OLD_LAYOUT).unwrap().config;
        c.align_exactly(NodeId::Debian);
        for k in (1..2559).step_by(7).chain([1, 2557, 2558]) {
            let (lx, ly) = c.entry_on_peer(NodeId::Debian, 1920 + k, 1439);
            assert_eq!(lx, k, "down at {k}");
            assert!(ly > 0);
            let (dx, dy) = c.entry_on_peer(NodeId::Windows, lx, 0);
            assert_eq!(dx, 1920 + k, "back up at {k}");
            assert!(dy < 1439);
        }
    }

    #[test]
    fn a_migrated_layout_outranks_a_device_that_never_had_one() {
        let migrated = parse_layout(OLD_LAYOUT).unwrap().config;
        let fresh = SpatialConfig::fresh(NodeId::Windows, None);
        assert!(is_newer(migrated.stamp(), fresh.stamp()));
    }

    #[test]
    fn an_old_custom_size_is_kept_and_old_files_without_native_fields_still_parse() {
        let text = OLD_LAYOUT.replace(r#""resolution_mode": "native""#, r#""resolution_mode": "custom""#).replace(
            r#""win_w": 2560, "win_h": 1600"#,
            r#""win_w": 1920, "win_h": 1200"#,
        );
        let c = parse_layout(&text).unwrap().config;
        let laptop = c.device(NodeId::Windows).unwrap();
        assert_eq!(laptop.custom_size, Some((1920, 1200)));
        assert_eq!(laptop.monitors[0].w, 2560);

        let oldest = r#"{"debian_monitors":[],"deb_total_w":0,"deb_total_h":0,"win_x":5,"win_y":6,"win_w":1280,"win_h":800,"attach_side":"Left","edge_resistance":9}"#;
        let parsed = parse_layout(oldest).unwrap();
        assert_eq!(parsed.old_pointer_speed, None);
        assert_eq!(parsed.config.device(NodeId::Windows).unwrap().monitors[0].w, 1280);
    }

    #[test]
    fn a_new_layout_survives_a_round_trip_with_no_old_speed() {
        let c = parse_layout(OLD_LAYOUT).unwrap().config;
        let back = parse_layout(&serde_json::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back, ParsedLayout { config: c, old_pointer_speed: None });
    }

    #[test]
    fn nonsense_is_not_a_layout() {
        for text in ["{nope", "", "null", "42", "{}", r#"{"devices":3}"#, r#"{"devices":[{"node":"Mac","x":0,"y":0}],"attach_side":"Top","edge_resistance":1}"#] {
            assert_eq!(parse_layout(text), None, "{text}");
        }
    }

    #[test]
    fn values_out_of_range_are_repaired_on_the_way_in() {
        let mut c = layout(TouchSide::Bottom, i32::MAX, i32::MIN);
        c.edge_resistance = 9000;
        debian_mut(&mut c).monitors = vec![rect(&"n".repeat(99), i32::MIN, 0, i32::MAX, i32::MAX), rect("gone", 0, 0, 0, 5)];
        laptop_mut(&mut c).custom_size = Some((-1, 7));
        let c = parse_layout(&serde_json::to_string(&c).unwrap()).unwrap().config;
        assert_eq!(c.edge_resistance, 400);
        let m = &c.monitors(NodeId::Debian)[0];
        assert_eq!(c.monitors(NodeId::Debian).len(), 1);
        assert_eq!((m.name.chars().count(), m.x, m.w), (32, -MAX_COORDINATE, MAX_MONITOR_SIZE));
        let laptop = c.device(NodeId::Windows).unwrap();
        assert_eq!((laptop.x, laptop.y, laptop.custom_size), (MAX_COORDINATE, -MAX_COORDINATE, None));
        // None of the maths can overflow or panic on what is left.
        for node in [NodeId::Debian, NodeId::Windows] {
            let _ = c.edge_span(node);
            let _ = c.entry_on_peer(node, i32::MAX, i32::MIN + 1);
        }
        let mut c = c;
        c.resnap();
    }

    #[test]
    fn a_missing_device_gets_a_placeholder_and_a_repeat_is_dropped() {
        let text = r#"{"devices":[{"node":"Debian","x":1,"y":2},{"node":"Debian","x":3,"y":4}],"attach_side":"Top","edge_resistance":1}"#;
        let c = parse_layout(text).unwrap().config;
        assert_eq!(c.devices.len(), 2);
        assert_eq!(c.device(NodeId::Debian).unwrap().x, 1);
        assert_eq!(c.monitors(NodeId::Windows).len(), 1);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-spatial-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_saved_layout_wins_and_the_old_file_is_read_but_never_touched() {
        let (config, old) = (temp("config"), temp("old"));
        fs::write(old.join(LAYOUT_FILE), OLD_LAYOUT).unwrap();

        // First run: nothing saved yet, so the old file is imported, and stays where it is.
        let olds = [old.clone()];
        let imported = load_layout(&config, &olds).unwrap();
        assert_eq!(imported.old_pointer_speed, Some(0.35));
        assert_eq!(fs::read_to_string(old.join(LAYOUT_FILE)).unwrap(), OLD_LAYOUT);

        // Later runs: the saved copy wins, and the old speed isn't taken again.
        let mut saved = imported.config;
        saved.edge_resistance = 77;
        fs::write(config.join(LAYOUT_FILE), serde_json::to_string(&saved).unwrap()).unwrap();
        let again = load_layout(&config, &olds).unwrap();
        assert_eq!(again.config.edge_resistance, 77);
        assert_eq!(again.old_pointer_speed, None);

        // Windows has no old location, and a corrupt save falls back rather than failing.
        assert_eq!(load_layout(&old.join("nowhere"), &[]), None);
        fs::write(config.join(LAYOUT_FILE), "{nope").unwrap();
        assert_eq!(load_layout(&config, &olds).unwrap().config.edge_resistance, 55);
        let _ = fs::remove_dir_all(config);
        let _ = fs::remove_dir_all(old);
    }

    #[test]
    fn a_first_start_that_missed_the_old_file_does_not_lose_it_for_good() {
        let (config, elsewhere, old) = (temp("missed-config"), temp("missed-elsewhere"), temp("missed-old"));
        // The first start ran somewhere with no old file, and saved a layout nobody has edited.
        let fresh = SpatialConfig::fresh(NodeId::Debian, None);
        fs::write(config.join(LAYOUT_FILE), serde_json::to_string(&fresh).unwrap()).unwrap();
        assert_eq!(load_layout(&config, &[elsewhere.clone()]).unwrap().config, fresh);
        // A later start can see the old file, and takes it, looking through every folder.
        fs::write(old.join(LAYOUT_FILE), OLD_LAYOUT).unwrap();
        let found = load_layout(&config, &[elsewhere.clone(), old.clone()]).unwrap();
        assert_eq!(found.old_pointer_speed, Some(0.35));
        assert_eq!(found.config.device(NodeId::Windows).unwrap().x, 1925);
        // An untouched layout that happens to sit in one of those folders isn't an old file.
        fs::write(elsewhere.join(LAYOUT_FILE), serde_json::to_string(&fresh).unwrap()).unwrap();
        assert_eq!(load_layout(&config, &[elsewhere.clone()]).unwrap().config, fresh);
        for dir in [config, elsewhere, old] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_fresh_layout_puts_the_other_device_on_the_edge_asked_for() {
        // Debian's right edge and the laptop's left edge are the same arrangement.
        assert_eq!(SpatialConfig::fresh(NodeId::Debian, None).attach_side, TouchSide::Right);
        assert_eq!(SpatialConfig::fresh(NodeId::Windows, None).attach_side, TouchSide::Right);
        assert_eq!(SpatialConfig::fresh(NodeId::Debian, Some(SwitchEdge::Bottom)).attach_side, TouchSide::Bottom);
        assert_eq!(SpatialConfig::fresh(NodeId::Windows, Some(SwitchEdge::Bottom)).attach_side, TouchSide::Top);
        let c = SpatialConfig::fresh(NodeId::Debian, None);
        assert_eq!(c.device(NodeId::Windows).unwrap().x, PLACEHOLDER_SIZE.0);
    }

    // ---- Revisions ----

    #[test]
    fn the_higher_revision_wins_and_a_tie_goes_the_same_way_on_both_devices() {
        assert!(is_newer((5, NodeId::Debian), (4, NodeId::Windows)));
        assert!(!is_newer((4, NodeId::Windows), (5, NodeId::Debian)));
        // Each device compares the pair from its own side, and they must agree.
        assert!(is_newer((5, NodeId::Windows), (5, NodeId::Debian)));
        assert!(!is_newer((5, NodeId::Debian), (5, NodeId::Windows)));
        assert!(!is_newer((5, NodeId::Debian), (5, NodeId::Debian)));
    }

    /// Both devices' copies of the same arrangement.
    fn pair() -> (SpatialConfig, SpatialConfig) {
        (layout(TouchSide::Bottom, 2240, 1440), layout(TouchSide::Bottom, 2240, 1440))
    }

    /// Moves the laptop to `x`, on the bottom edge.
    fn moved_to(x: i32) -> LayoutEdit {
        let place = DevicePlace { node: NodeId::Windows, x, y: 1440, custom_size: None };
        LayoutEdit { arrangement: Some(Arrangement { devices: vec![place], attach_side: TouchSide::Bottom }), edge_resistance: None }
    }

    fn resistance_of(value: i32) -> LayoutEdit {
        LayoutEdit { edge_resistance: Some(value), ..Default::default() }
    }

    fn laptop_x(c: &SpatialConfig) -> i32 {
        c.device(NodeId::Windows).unwrap().x
    }

    #[test]
    fn an_edit_raises_the_revision_and_reaches_the_other_copy() {
        let (mut debian, mut laptop) = pair();
        assert!(laptop.apply_edit(&moved_to(100), NodeId::Windows));
        assert_eq!(laptop.stamp(), (1, NodeId::Windows));
        assert!(debian.adopt(&laptop));
        assert_eq!(laptop_x(&debian), 100);
        assert_eq!(debian.stamp(), laptop.stamp());
        // Settled: neither side takes anything further from the other.
        assert!(!laptop.adopt(&debian));
        assert!(!debian.adopt(&laptop));
    }

    #[test]
    fn an_edit_made_while_the_other_device_is_away_lands_when_it_returns() {
        let (mut debian, mut laptop) = pair();
        // Two edits on one side, and nothing on the other until the link comes back.
        for x in [100, 200] {
            debian.apply_edit(&moved_to(x), NodeId::Debian);
        }
        assert!(!debian.adopt(&laptop), "the stale copy must not undo the edits");
        assert!(laptop.adopt(&debian));
        assert_eq!(laptop_x(&laptop), 200);
    }

    #[test]
    fn edits_on_both_sides_settle_on_one_copy() {
        let (mut debian, mut laptop) = pair();
        debian.apply_edit(&moved_to(111), NodeId::Debian);
        laptop.apply_edit(&moved_to(222), NodeId::Windows);
        // Both are at revision 1, and each sees the other's copy.
        let (d, l) = (debian.clone(), laptop.clone());
        let took = (debian.adopt(&l), laptop.adopt(&d));
        assert_eq!(took, (true, false));
        assert_eq!(laptop_x(&debian), laptop_x(&laptop));
        assert_eq!(laptop_x(&debian), 222);
    }

    #[test]
    fn an_edit_changes_only_the_parts_it_names() {
        let (mut debian, _) = pair();
        debian.revision = 9;
        debian.apply_edit(&moved_to(5), NodeId::Debian);
        assert_eq!(debian.revision, 10);
        // This one came from a window that hasn't seen the move, and it doesn't undo it.
        debian.apply_edit(&resistance_of(80), NodeId::Windows);
        assert_eq!((laptop_x(&debian), debian.edge_resistance), (5, 80));
        assert_eq!(debian.stamp(), (11, NodeId::Windows));
    }

    #[test]
    fn an_edit_with_nothing_in_it_changes_nothing() {
        let (mut debian, _) = pair();
        assert!(!debian.apply_edit(&LayoutEdit::default(), NodeId::Windows));
        assert_eq!(debian.stamp(), (0, NodeId::Debian));
    }

    #[test]
    fn values_in_an_edit_are_held_to_the_same_limits_as_a_file() {
        let (mut debian, _) = pair();
        let mut edit = moved_to(i32::MAX);
        edit.edge_resistance = Some(i32::MAX);
        edit.arrangement.as_mut().unwrap().devices[0].custom_size = Some((-1, 5));
        debian.apply_edit(&edit, NodeId::Debian);
        assert_eq!(laptop_x(&debian), MAX_COORDINATE);
        assert_eq!(debian.edge_resistance, MAX_EDGE_RESISTANCE);
        assert_eq!(debian.device(NodeId::Windows).unwrap().custom_size, None);
    }

    #[test]
    fn the_revision_in_an_edit_is_ignored() {
        let edit: LayoutEdit = serde_json::from_str(&format!(r#"{{"revision":{},"edge_resistance":60}}"#, u64::MAX)).unwrap();
        let (mut debian, _) = pair();
        debian.revision = 9;
        debian.apply_edit(&edit, NodeId::Debian);
        assert_eq!(debian.revision, 10);
    }

    /// Runs each copy's sync against the other until neither takes anything more.
    fn settle(debian: &mut SpatialConfig, laptop: &mut SpatialConfig) {
        for _ in 0..4 {
            let (d, l) = (debian.clone(), laptop.clone());
            debian.adopt(&l);
            laptop.adopt(&d);
        }
    }

    fn assert_settled_on(debian: &SpatialConfig, laptop: &SpatialConfig, x: i32, resistance: i32) {
        assert_eq!((laptop_x(debian), debian.edge_resistance), (x, resistance), "Debian's copy");
        assert_eq!((laptop_x(laptop), laptop.edge_resistance), (x, resistance), "the laptop's copy");
        assert_eq!(debian.stamp(), laptop.stamp());
    }

    #[test]
    fn edits_to_different_parts_on_both_devices_both_survive() {
        let (mut debian, mut laptop) = pair();
        debian.apply_edit(&resistance_of(90), NodeId::Debian);
        laptop.apply_edit(&moved_to(300), NodeId::Windows);
        settle(&mut debian, &mut laptop);
        assert_settled_on(&debian, &laptop, 300, 90);
    }

    #[test]
    fn an_edit_behind_the_other_copys_revision_is_not_lost() {
        let (mut debian, mut laptop) = pair();
        // The laptop is two edits ahead, and Debian's one edit has a lower revision than either.
        laptop.apply_edit(&moved_to(100), NodeId::Windows);
        laptop.apply_edit(&moved_to(200), NodeId::Windows);
        debian.apply_edit(&resistance_of(90), NodeId::Debian);
        settle(&mut debian, &mut laptop);
        assert_settled_on(&debian, &laptop, 200, 90);
    }

    #[test]
    fn a_copy_from_before_parts_had_stamps_counts_as_edited_at_its_revision() {
        let mut c = layout(TouchSide::Bottom, 0, 1440);
        c.revision = 7;
        c.editor = NodeId::Windows;
        let mut text: serde_json::Value = serde_json::to_value(&c).unwrap();
        text.as_object_mut().unwrap().retain(|k, _| !k.ends_with("_stamp"));
        let back = parse_layout(&text.to_string()).unwrap().config;
        assert_eq!(back.arrangement_stamp, (7, NodeId::Windows));
        assert_eq!(back.resistance_stamp, (7, NodeId::Windows));
    }

    #[test]
    fn a_huge_revision_from_the_other_device_is_not_believed() {
        let (mut debian, laptop) = pair();
        let mut theirs = laptop.clone();
        theirs.apply_edit(&moved_to(5), NodeId::Windows);
        for revision in [u64::MAX, debian.revision + MAX_REVISION_JUMP + 1] {
            theirs.revision = revision;
            assert!(!debian.adopt(&theirs), "{revision}");
        }
        assert_eq!(debian.revision, 0);
        // Sync still works afterwards.
        theirs.revision = 1;
        assert!(debian.adopt(&theirs));
        let state = shared_at(1, NodeId::Debian, None);
        note_peer_revision(&state, u64::MAX, NodeId::Windows);
        pull_layout(&state, || panic!("a bad revision must not even be fetched"));
    }

    #[test]
    fn a_bad_revision_already_saved_starts_again_so_that_sync_can_run() {
        let mut c = layout(TouchSide::Bottom, 0, 1440);
        c.revision = u64::MAX;
        let mut back = parse_layout(&serde_json::to_string(&c).unwrap()).unwrap().config;
        assert_eq!(back.revision, 0);
        back.apply_edit(&moved_to(5), NodeId::Debian);
        assert_eq!(back.revision, 1);
    }

    #[test]
    fn a_copy_from_the_other_device_keeps_what_belongs_to_this_one() {
        let (mut debian, mut laptop) = pair();
        debian.auto_game_lock = false;
        debian.manual_game_lock = true;
        laptop.edge_resistance = 90;
        laptop.attach_side = TouchSide::Right;
        // The laptop's copy of Debian's monitors is out of date.
        debian_mut(&mut laptop).monitors = vec![rect("Old", 0, 0, 10, 10)];
        laptop.revision = 3;
        laptop.arrangement_stamp = (3, NodeId::Debian);
        laptop.resistance_stamp = (3, NodeId::Debian);
        assert!(debian.adopt(&laptop));
        assert_eq!((debian.edge_resistance, debian.attach_side), (90, TouchSide::Right));
        assert_eq!(debian.monitors(NodeId::Debian), &[dp3(), dp2()]);
        assert!(!debian.auto_game_lock && debian.manual_game_lock);
    }

    /// A laptop on the right edge, low down against Debian's bottom corner.
    fn laptop_low_on_the_right() -> SpatialConfig {
        layout(TouchSide::Right, 4480, 1150)
    }

    /// What a resolution trial does to Debian: the tall monitor shrinks, then comes back.
    fn trial_shrinks_debian(c: &mut SpatialConfig) {
        c.report_monitors(NodeId::Debian, "", vec![rect("DP-2", 0, 0, 1280, 720)]);
    }

    fn debian_back_to_normal(c: &mut SpatialConfig) {
        c.report_monitors(NodeId::Debian, "", vec![dp3(), dp2()]);
    }

    #[test]
    fn an_undone_trial_leaves_the_devices_where_they_sat_and_tells_the_other_copy() {
        let mut c = laptop_low_on_the_right();
        let mut other = c.clone();
        let before = (laptop_x(&c), c.device(NodeId::Windows).unwrap().y);
        let held = c.held_place();
        trial_shrinks_debian(&mut c);
        trial_shrinks_debian(&mut other);
        assert_ne!((laptop_x(&c), c.device(NodeId::Windows).unwrap().y), before, "the trial moved it");
        debian_back_to_normal(&mut c);
        debian_back_to_normal(&mut other);
        assert_ne!((laptop_x(&c), c.device(NodeId::Windows).unwrap().y), before, "and going back alone doesn't undo that");
        assert!(c.return_to(&held, NodeId::Debian));
        assert_eq!((laptop_x(&c), c.device(NodeId::Windows).unwrap().y), before);
        assert!(other.adopt(&c));
        assert_eq!((laptop_x(&other), other.device(NodeId::Windows).unwrap().y), before);
    }

    #[test]
    fn a_trial_that_moved_nothing_is_no_edit() {
        let mut c = laptop_low_on_the_right();
        let held = c.held_place();
        assert!(!c.return_to(&held, NodeId::Debian));
        assert_eq!(c.revision, 0);
    }

    #[test]
    fn an_arrangement_edited_during_a_trial_stands() {
        let mut c = laptop_low_on_the_right();
        let held = c.held_place();
        trial_shrinks_debian(&mut c);
        c.apply_edit(&moved_to(777), NodeId::Windows);
        debian_back_to_normal(&mut c);
        let after = laptop_x(&c);
        assert!(!c.return_to(&held, NodeId::Debian));
        assert_eq!(laptop_x(&c), after);
    }

    #[test]
    fn the_layout_follows_a_trial_from_start_to_undone() {
        let state = shared_at(0, NodeId::Debian, None);
        state.write().unwrap().config = laptop_low_on_the_right();
        let (platform, mut held) = (NoScreens, None);
        let before = laptop_x(&state.read().unwrap().config);
        let y_before = state.read().unwrap().config.device(NodeId::Windows).unwrap().y;
        follow_display_news(&state, &platform, &mut held, LayoutNews::TrialStarting);
        trial_shrinks_debian(&mut state.write().unwrap().config);
        debian_back_to_normal(&mut state.write().unwrap().config);
        follow_display_news(&state, &platform, &mut held, LayoutNews::TrialUndone);
        let g = state.read().unwrap();
        assert_eq!((laptop_x(&g.config), g.config.device(NodeId::Windows).unwrap().y), (before, y_before));
        assert!(held.is_none());
    }

    #[test]
    fn a_kept_trial_is_not_undone_by_old_news() {
        let state = shared_at(0, NodeId::Debian, None);
        state.write().unwrap().config = laptop_low_on_the_right();
        let (platform, mut held) = (NoScreens, None);
        follow_display_news(&state, &platform, &mut held, LayoutNews::TrialStarting);
        trial_shrinks_debian(&mut state.write().unwrap().config);
        let moved = laptop_x(&state.read().unwrap().config);
        follow_display_news(&state, &platform, &mut held, LayoutNews::TrialOver);
        follow_display_news(&state, &platform, &mut held, LayoutNews::TrialUndone);
        assert_eq!(laptop_x(&state.read().unwrap().config), moved);
    }

    /// A device reporting no monitors, which leaves the last known ones in the layout.
    struct NoScreens;

    impl Platform for NoScreens {
        fn node(&self) -> NodeId {
            NodeId::Debian
        }
        fn label(&self) -> &'static str {
            ""
        }
        fn os_name(&self) -> &'static str {
            ""
        }
        fn monitors(&self) -> Vec<MonitorRect> {
            Vec::new()
        }
        fn autostart_enabled(&self) -> bool {
            false
        }
        fn set_autostart(&self, _: bool) {}
        fn input_devices(&self) -> &'static [&'static str] {
            &[]
        }
    }

    fn test_pairing(name: &str) -> Arc<PairingService> {
        let dir = std::env::temp_dir().join(format!("mk-spatial-{}-{name}", std::process::id()));
        Arc::new(PairingService::for_test(dir.join("pairing.json")))
    }

    #[test]
    fn the_code_is_never_in_the_status_the_peer_can_read() {
        use crate::webui::{HttpRequest, Source};
        // The status says whether another version is arriving, which the audio tests note.
        let _quiet = crate::audio::tests::no_other_version();
        let state = Arc::new(shared_at(0, NodeId::Debian, None));
        let platform: Arc<dyn Platform> = Arc::new(NoScreens);
        let pairing = test_pairing("code");
        let handler = studio_handler(state, platform, DisplayManager::for_test_with_waiting_change(), pairing.clone());
        let shown = handler(&HttpRequest::for_test("POST", "/api/local/action", r#"{"action":"pair_show"}"#, Source::Local));
        assert_eq!(shown.unwrap(), crate::settings::OK_RESPONSE);
        let mine = handler(&HttpRequest::for_test("GET", "/api/pairing", "", Source::Local)).unwrap();
        let code = serde_json::from_str::<serde_json::Value>(&mine).unwrap()["code"].as_str().unwrap().to_string();
        for source in [Source::Local, Source::Peer, Source::Browser] {
            let status = handler(&HttpRequest::for_test("GET", "/api/local", "", source)).unwrap();
            assert!(!status.contains(&code), "{source:?} can read the code");
            assert!(!status.contains("\"code\""));
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            assert_eq!(value["secure"], "unpaired");
            assert_eq!(value["old_peer"], false);
        }
        for source in [Source::Peer, Source::Browser] {
            assert!(handler(&HttpRequest::for_test("GET", "/api/pairing", "", source)).is_none());
        }
        pairing.cancel();
    }

    #[test]
    fn an_old_peer_is_in_the_status() {
        let mut g = shared_at(0, NodeId::Debian, None).into_inner().unwrap();
        g.telemetry.link.old_peer = true;
        let status = local_status(&g, &NoScreens, &DisplayManager::for_test_with_waiting_change(), "connecting");
        assert_eq!(status["old_peer"], true);
        assert_eq!(status["secure"], "connecting");
    }

    #[test]
    fn the_service_refuses_a_keep_from_the_other_device() {
        let state = Arc::new(shared_at(0, NodeId::Debian, None));
        let platform: Arc<dyn Platform> = Arc::new(NoScreens);
        let displays = DisplayManager::for_test_with_waiting_change();
        let handler = studio_handler(state, platform, displays.clone(), test_pairing("keep"));
        let id = displays.status_json()["change"]["id"].clone();
        let body = serde_json::json!({ "action": "keep_display_mode", "change_id": id }).to_string();
        let from_peer = crate::webui::HttpRequest::for_test("POST", "/api/local/action", &body, crate::webui::Source::Peer);
        assert!(handler(&from_peer).unwrap().contains("only be kept on the device"));
        let here = crate::webui::HttpRequest::for_test("POST", "/api/local/action", &body, crate::webui::Source::Local);
        assert_eq!(handler(&here).unwrap(), crate::settings::OK_RESPONSE);
    }

    #[test]
    fn the_other_device_cant_quit_this_one() {
        let state = Arc::new(shared_at(0, NodeId::Debian, None));
        let platform: Arc<dyn Platform> = Arc::new(NoScreens);
        let handler = studio_handler(state, platform, DisplayManager::for_test_with_waiting_change(), test_pairing("quit"));
        let body = serde_json::json!({ "action": crate::quit::ACTION }).to_string();
        let from_peer = crate::webui::HttpRequest::for_test("POST", "/api/local/action", &body, crate::webui::Source::Peer);
        assert!(handler(&from_peer).unwrap().contains("can only be quit from this device"));
    }

    #[test]
    fn the_service_refuses_a_microphone_whoever_asks() {
        let state = Arc::new(shared_at(0, NodeId::Debian, None));
        let platform: Arc<dyn Platform> = Arc::new(NoScreens);
        let handler = studio_handler(state, platform, DisplayManager::for_test_with_waiting_change(), test_pairing("mic"));
        let body = serde_json::json!({ "action": "set_audio_device", "role": "input", "id": "mic" }).to_string();
        for source in [crate::webui::Source::Peer, crate::webui::Source::Local] {
            let request = crate::webui::HttpRequest::for_test("POST", "/api/local/action", &body, source);
            assert!(handler(&request).unwrap().contains("audio choices"));
        }
    }

    #[test]
    fn a_hardware_report_is_not_an_edit() {
        let (mut debian, _) = pair();
        assert!(debian.report_monitors(NodeId::Windows, "Laptop", vec![rect("Display 1", 0, 0, 2560, 1600)]));
        assert_eq!(debian.revision, 0);
        assert_eq!(debian.device(NodeId::Windows).unwrap().name, "Laptop");
        assert!(!debian.report_monitors(NodeId::Windows, "Laptop", vec![rect("Display 1", 0, 0, 2560, 1600)]));
    }

    #[test]
    fn a_failed_detection_keeps_the_last_known_monitors() {
        let (mut debian, _) = pair();
        assert!(!debian.report_monitors(NodeId::Debian, "", vec![]));
        assert_eq!(debian.monitors(NodeId::Debian).len(), 2);
    }

    fn shared_at(revision: u64, editor: NodeId, peer: Option<(u64, NodeId)>) -> RwLock<SharedSpatialState> {
        let mut config = layout(TouchSide::Bottom, 2240, 1440);
        config.revision = revision;
        config.editor = editor;
        RwLock::new(SharedSpatialState {
            me: NodeId::Debian,
            peer_pinned: false,
            config,
            telemetry: LiveTelemetry::new(NodeId::Debian, String::new()),
            pending_manual_switch: true,
            pending_panic: true,
            peer_revision: peer,
            file: None,
        })
    }

    fn state_body(c: &SpatialConfig) -> Option<String> {
        Some(serde_json::json!({ "config": c, "telemetry": {} }).to_string())
    }

    #[test]
    fn a_newer_copy_is_fetched_and_nothing_is_fetched_otherwise() {
        let state = shared_at(1, NodeId::Debian, None);
        pull_layout(&state, || panic!("nothing said the other copy is newer"));
        note_peer_revision(&state, 1, NodeId::Debian);
        pull_layout(&state, || panic!("the same revision isn't newer"));

        let mut theirs = layout(TouchSide::Bottom, 7, 1440);
        theirs.revision = 2;
        theirs.editor = NodeId::Windows;
        note_peer_revision(&state, 2, NodeId::Windows);
        pull_layout(&state, || state_body(&theirs));
        let g = state.read().unwrap();
        assert_eq!((laptop_x(&g.config), g.config.revision), (7, 2));
        assert!(!g.peer_is_newer());
    }

    #[test]
    fn a_failed_fetch_tries_again_and_a_bad_reply_changes_nothing() {
        let state = shared_at(1, NodeId::Debian, Some((2, NodeId::Windows)));
        pull_layout(&state, || None);
        pull_layout(&state, || Some("not json".into()));
        pull_layout(&state, || Some(r#"{"telemetry":{}}"#.into()));
        let g = state.read().unwrap();
        assert_eq!(g.config.revision, 1);
        assert!(g.peer_is_newer());
    }

    // ---- Heartbeats ----

    /// One device as far as the other's packets go: its layout and its edge.
    struct Side {
        state: RwLock<SharedSpatialState>,
        edge: EdgeWatch,
        locked: bool,
    }

    impl Side {
        fn new(me: NodeId, config: SpatialConfig) -> Self {
            let state = shared_at(0, me, None);
            {
                let mut g = state.write().unwrap();
                g.me = me;
                g.config = config;
            }
            Self { state, edge: EdgeWatch::new(), locked: false }
        }

        /// Does with a packet what both input loops do with it.
        fn receive(&mut self, payload: Payload) {
            match payload {
                Payload::PeerDisplayInfo { name, monitors } => note_peer_displays(&self.state, &name, monitors),
                Payload::EdgeMap { edge, start, end, resistance } => {
                    self.edge.set_span(EdgeSpan { edge, start, end }, resistance);
                }
                Payload::GameLock { locked } => self.locked = locked,
                Payload::LayoutRevision { revision, editor } => note_peer_revision(&self.state, revision, editor),
                _ => {}
            }
        }

        fn config(&self) -> SpatialConfig {
            self.state.read().unwrap().config.clone()
        }

        fn heartbeat(&self) -> Vec<Payload> {
            let g = self.state.read().unwrap();
            heartbeat_payloads(&g.config, g.me)
        }
    }

    fn sides() -> (Side, Side) {
        let (d, l) = pair();
        (Side::new(NodeId::Debian, d), Side::new(NodeId::Windows, l))
    }

    fn exchange(a: &mut Side, b: &mut Side) {
        for p in a.heartbeat() {
            b.receive(p);
        }
        for p in b.heartbeat() {
            a.receive(p);
        }
    }

    #[test]
    fn both_devices_send_the_same_kinds_of_packet() {
        let kinds = |side: &Side| {
            side.heartbeat().iter().map(|p| std::mem::discriminant(p)).collect::<Vec<_>>()
        };
        let (debian, laptop) = sides();
        assert_eq!(kinds(&debian), kinds(&laptop));
        assert_eq!(kinds(&debian).len(), 5);
    }

    #[test]
    fn each_device_tells_the_other_about_its_edge_and_both_can_then_cross() {
        let (mut debian, mut laptop) = sides();
        let (d_cfg, l_cfg) = (debian.config(), laptop.config());
        exchange(&mut debian, &mut laptop);
        assert!(debian.edge.push(d_cfg.monitors(NodeId::Debian), (2340, 1439), (0, 99), false));
        assert!(laptop.edge.push(l_cfg.monitors(NodeId::Windows), (100, 0), (0, -99), false));
    }

    #[test]
    fn each_device_learns_the_other_devices_monitors() {
        let (mut debian, mut laptop) = sides();
        let two = vec![rect("Display 2", -1000, -200, 1000, 800), rect("Display 1", 0, 0, 1920, 1080)];
        laptop.state.write().unwrap().config.report_monitors(NodeId::Windows, "Laptop", two.clone());
        debian.state.write().unwrap().config.report_monitors(NodeId::Debian, "Desk", vec![dp2()]);
        exchange(&mut debian, &mut laptop);
        assert_eq!(debian.config().monitors(NodeId::Windows), two.as_slice());
        assert_eq!(debian.config().device(NodeId::Windows).unwrap().name, "Laptop");
        assert_eq!(laptop.config().monitors(NodeId::Debian), &[dp2()]);
        assert_eq!(laptop.config().device(NodeId::Debian).unwrap().name, "Desk");
        // Reports don't count as edits, and both copies have the arrangement in the same place.
        assert_eq!(debian.config().revision, 0);
        let positions = |c: &SpatialConfig| c.devices.iter().map(|d| (d.x, d.y)).collect::<Vec<_>>();
        assert_eq!(positions(&debian.config()), positions(&laptop.config()));
    }

    #[test]
    fn a_game_lock_on_either_device_reaches_the_other() {
        let (mut debian, mut laptop) = sides();
        laptop.state.write().unwrap().config.manual_game_lock = true;
        exchange(&mut debian, &mut laptop);
        assert!(debian.locked && !laptop.locked);
        laptop.state.write().unwrap().config.manual_game_lock = false;
        debian.state.write().unwrap().config.manual_game_lock = true;
        exchange(&mut debian, &mut laptop);
        assert!(!debian.locked && laptop.locked);
    }

    #[test]
    fn an_edit_on_either_device_reaches_the_other_through_the_heartbeat() {
        for editor in [NodeId::Debian, NodeId::Windows] {
            let (mut debian, mut laptop) = sides();
            let (from, to) = if editor == NodeId::Debian { (&debian, &laptop) } else { (&laptop, &debian) };
            {
                let mut g = from.state.write().unwrap();
                g.config.apply_edit(&moved_to(2300), editor);
            }
            let body = state_body(&from.config());
            let to_state = &to.state;
            assert!(!to_state.read().unwrap().peer_is_newer());
            exchange(&mut debian, &mut laptop);
            let to = if editor == NodeId::Debian { &laptop } else { &debian };
            pull_layout(&to.state, || body);
            assert_eq!(laptop_x(&to.config()), 2300);
            assert_eq!(debian.config().stamp(), laptop.config().stamp());
        }
    }

    // ---- Geometry ----

    #[test]
    fn bottom_attachment_gives_top_span() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 0, 1919));
    }

    #[test]
    fn right_attachment_gives_left_span() {
        let c = layout(TouchSide::Right, 4480, 200);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Left, 0, 1079));
    }

    #[test]
    fn left_attachment_gives_right_span() {
        // DP-3 starts at y = 360, so the laptop's first 360 rows have no neighbour.
        let c = layout(TouchSide::Left, -1920, 0);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Right, 360, 1079));
    }

    #[test]
    fn top_attachment_gives_bottom_span() {
        let c = layout(TouchSide::Top, 2240, -1080);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Bottom, 0, 1919));
    }

    #[test]
    fn partial_overlap_is_clamped() {
        // Hanging 640 px past DP-2's right end.
        let c = layout(TouchSide::Bottom, 3200, 1440);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 0, 1279));
        // Hanging 200 px above DP-2's top.
        let c = layout(TouchSide::Right, 4480, -200);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Left, 200, 1079));
    }

    #[test]
    fn span_covers_every_monitor_on_the_same_line() {
        let c = layout(TouchSide::Bottom, 1500, 1440);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 0, 1919));
    }

    #[test]
    fn monitor_ending_short_of_the_laptop_does_not_count() {
        let mut c = layout(TouchSide::Bottom, 1500, 1440);
        debian_mut(&mut c).monitors[0].h = 720; // DP-3 now ends at y = 1080
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 420, 1919));
    }

    #[test]
    fn no_overlap_gives_an_empty_span() {
        let c = layout(TouchSide::Bottom, 5000, 1440);
        let s = laptop_span(&c);
        assert_eq!(s.edge, SwitchEdge::Top);
        assert!(s.start > s.end);
    }

    #[test]
    fn each_device_gets_its_own_side_of_the_shared_edge() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.edge_span(NodeId::Debian), span(SwitchEdge::Bottom, 2240, 4159));
        assert_eq!(c.edge_span(NodeId::Windows), span(SwitchEdge::Top, 0, 1919));
    }

    #[test]
    fn a_laptop_with_a_monitor_at_negative_coordinates_is_laid_out_from_its_own_origin() {
        // A second monitor to the left of the laptop's main one, so its x is negative.
        let mut c = layout(TouchSide::Bottom, 2240, 1440);
        laptop_mut(&mut c).monitors = vec![rect("Display 2", -1000, 0, 1000, 800), rect("Display 1", 0, 0, 1920, 1080)];
        // In the layout the pair spans 1240..4160, all of it under DP-3 or DP-2.
        assert_eq!(c.edge_span(NodeId::Windows), span(SwitchEdge::Top, -1000, 1919));
        assert_eq!(c.edge_span(NodeId::Debian), span(SwitchEdge::Bottom, 1240, 4159));
        // Down from Debian lands on the left monitor, whose x is negative on the laptop.
        assert_eq!(c.entry_on_peer(NodeId::Debian, 1740, 1439), (-500, 12));
        // And back up from it lands where it came from.
        assert_eq!(c.entry_on_peer(NodeId::Windows, -500, 0), (1740, 1427));
    }

    #[test]
    fn resnap_keeps_a_negative_monitor_laptop_on_the_edge() {
        let mut c = layout(TouchSide::Left, -1920, 400);
        c.report_monitors(NodeId::Windows, "", vec![rect("Display 2", -1000, 0, 1000, 800), rect("Display 1", 0, 0, 1920, 1080)]);
        // The pair is 2920 wide now, and its right side still meets Debian's left side.
        let rects = c.rects(NodeId::Windows);
        assert_eq!(bounds(&rects).unwrap().2, 0);
        assert_eq!(bounds(&rects).unwrap().0, -2920);
    }

    #[test]
    fn a_taller_laptop_stays_against_the_desktop() {
        let mut c = layout(TouchSide::Top, 2240, -1080);
        c.report_monitors(NodeId::Windows, "", vec![rect("Display 1", 0, 0, 1920, 1200)]);
        assert_eq!(c.device(NodeId::Windows).unwrap().y, -1200);
    }

    #[test]
    fn a_custom_size_stands_in_for_the_laptop_monitors() {
        let mut c = layout(TouchSide::Bottom, 1920, 1440);
        laptop_mut(&mut c).custom_size = Some((1000, 500));
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 0, 999));
        c.reset_sizes(NodeId::Windows);
        assert_eq!(laptop_span(&c), span(SwitchEdge::Top, 0, 1919));
        assert_eq!(c.stamp(), (1, NodeId::Windows));
    }

    #[test]
    fn laptop_point_converts_to_debian_coordinates() {
        // Laptop right of DP-2: (-1, 300) is just outside, so it lands on DP-2's last column.
        let c = layout(TouchSide::Right, 4480, 200);
        assert_eq!(to_debian(&c, -1, 300), (4467, 500));
    }

    #[test]
    fn debian_point_converts_to_laptop_coordinates() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.entry_on_peer(NodeId::Debian, 2340, 1439), (100, 12));
        let c = layout(TouchSide::Right, 4480, 200);
        assert_eq!(c.entry_on_peer(NodeId::Debian, 4479, 500), (12, 300));
    }

    #[test]
    fn laptop_point_clamps_to_nearest_monitor() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(to_debian(&c, 100, -1), (2340, 1427));
        assert_eq!(to_debian(&c, 100, 40), (2340, 1427));
        // Left of the laptop's origin and below DP-3.
        assert_eq!(to_debian(&c, -1000, 0), (1240, 1427));
        // The empty corner above DP-3 goes to the closer of the two monitors.
        let c = layout(TouchSide::Top, 0, 0);
        assert_eq!(to_debian(&c, 1900, 100), (1948, 100));
        assert_eq!(to_debian(&c, 100, 340), (100, 372));
    }

    #[test]
    fn keep_cursor_is_not_converted() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let keep = (KEEP_CURSOR, KEEP_CURSOR);
        assert_eq!(to_debian(&c, KEEP_CURSOR, KEEP_CURSOR), keep);
        // A half-set sentinel must not become a warp to a clamped corner.
        assert_eq!(to_debian(&c, KEEP_CURSOR, 5), keep);
        assert_eq!(to_debian(&c, 5, KEEP_CURSOR), keep);
        assert_eq!(c.place_on_own(NodeId::Debian, 5, KEEP_CURSOR), keep);
    }

    #[test]
    fn hostile_coordinates_stay_on_a_monitor() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(to_debian(&c, i32::MAX, i32::MAX), (4451, 1427));
        assert_eq!(to_debian(&c, i32::MIN + 1, i32::MIN + 1), (28, 388));
        assert_eq!(c.place_on_own(NodeId::Debian, i32::MAX, i32::MAX), (4451, 1411));
        assert_eq!(c.place_on_own(NodeId::Windows, i32::MIN + 1, i32::MAX), (28, 1051));
    }

    #[test]
    fn a_warp_inside_the_screen_is_left_alone() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        assert_eq!(c.place_on_own(NodeId::Windows, 100, 200), (100, 200));
    }

    #[test]
    fn a_point_with_no_monitors_to_go_on_stays_as_it_is() {
        let mut c = layout(TouchSide::Bottom, 2240, 1440);
        laptop_mut(&mut c).monitors.clear();
        assert_eq!(c.place_on_own(NodeId::Windows, 100, 40), (100, 40));
    }

    #[test]
    fn entry_is_inset_from_the_edge_it_lands_on_only() {
        let c = layout(TouchSide::Right, 4480, 200);
        // Interior points on the free axis keep their coordinate.
        assert_eq!(to_debian(&c, -1, 700), (4467, 900));
        // An interior point is untouched.
        assert_eq!(to_debian(&c, -500, 300), (3980, 500));
    }

    #[test]
    fn entry_is_inset_less_from_the_shared_edge_than_from_the_others() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        // Down onto the laptop: 12 in from its top, the edge it came in by.
        assert_eq!(c.entry_on_peer(NodeId::Debian, 2240, 1439), (ENTRY_INSET, SHARED_EDGE_INSET));
        // Up onto Debian: 12 in from DP-2's bottom, and the usual 28 from DP-2's left side.
        assert_eq!(to_debian(&c, 0, 0), (2240, 1427));
        assert_eq!(c.entry_on_peer(NodeId::Windows, -320, 0), (1920 + ENTRY_INSET, 1427));
        // A warp the other device asked for isn't a crossing, and keeps the usual inset.
        assert_eq!(c.place_on_own(NodeId::Debian, 3000, 1439), (3000, 1439 - ENTRY_INSET));
    }

    #[test]
    fn inset_stays_inside_a_tiny_monitor() {
        let mut c = layout(TouchSide::Right, 4480, 200);
        debian_mut(&mut c).monitors = vec![rect("T", 0, 0, 40, 40)];
        let (x, y) = to_debian(&c, -4480, -200);
        assert!((0..40).contains(&x) && (0..40).contains(&y));
    }

    fn watch(c: &SpatialConfig, node: NodeId) -> EdgeWatch {
        let mut watch = EdgeWatch::new();
        watch.set_span(c.edge_span(node), c.edge_resistance);
        watch
    }

    #[test]
    fn no_edge_can_be_crossed_before_anything_says_where_it_is() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let mut edge = EdgeWatch::new();
        assert!(!edge.push(c.monitors(NodeId::Debian), (2340, 1439), (0, 999), false));
    }

    #[test]
    fn a_device_crosses_from_its_own_layout_until_the_other_one_says_otherwise() {
        for (node, point, push) in [(NodeId::Debian, (2340, 1439), (0, 99)), (NodeId::Windows, (100, 0), (0, -99))] {
            let c = layout(TouchSide::Bottom, 2240, 1440);
            let mut edge = EdgeWatch::new();
            edge.use_own_layout(&c, node);
            assert!(edge.push(c.monitors(node), point, push, false), "{node}");

            // The other device's word wins, and the layout doesn't take it back.
            edge.set_span(span(SwitchEdge::Left, 0, 10), 5);
            edge.use_own_layout(&c, node);
            assert!(!edge.push(c.monitors(node), point, push, false), "{node}");
        }
    }

    #[test]
    fn edge_push_crosses_after_resistance() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let monitors = c.monitors(NodeId::Debian);
        let mut edge = watch(&c, NodeId::Debian);
        assert!(!edge.push(monitors, (2340, 1439), (0, 30), false));
        assert!(edge.push(monitors, (2340, 1439), (0, 30), false));
        assert!(!edge.push(monitors, (2340, 1439), (0, 30), false), "the count starts again after a crossing");
    }

    #[test]
    fn edge_push_resets_away_from_the_boundary() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let monitors = c.monitors(NodeId::Debian);
        let mut edge = watch(&c, NodeId::Debian);
        edge.push(monitors, (2340, 1439), (0, 30), false);
        assert!(!edge.push(monitors, (2340, 1000), (0, 30), false));
        assert!(!edge.push(monitors, (2340, 1439), (0, 30), false));
    }

    #[test]
    fn edge_push_ignores_a_stretch_with_nothing_beside_it() {
        // The laptop sits under DP-2 only, so the bottom of DP-3 leads nowhere.
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let mut edge = watch(&c, NodeId::Debian);
        assert!(!edge.push(c.monitors(NodeId::Debian), (500, 1439), (0, 99), false));
    }

    #[test]
    fn the_edge_between_two_of_a_devices_own_monitors_is_not_crossable() {
        // On the right of Debian, with the span running down DP-2. The right column of DP-3 lines
        // up with that span, but the cursor there is only moving on to DP-2.
        let c = layout(TouchSide::Right, 4480, 360);
        let monitors = c.monitors(NodeId::Debian);
        let mut edge = watch(&c, NodeId::Debian);
        assert!(!edge.push(monitors, (1919, 700), (500, 0), false));
        assert!(edge.push(monitors, (4479, 700), (500, 0), false));
    }

    #[test]
    fn edge_push_works_on_a_monitor_at_negative_coordinates() {
        let mut c = layout(TouchSide::Left, -2920, 0);
        laptop_mut(&mut c).monitors = vec![rect("Display 1", 0, 0, 1920, 1080), rect("Display 2", -1000, 0, 1000, 800)];
        let mut edge = watch(&c, NodeId::Windows);
        let monitors = c.monitors(NodeId::Windows).to_vec();
        // The edge is the main monitor's right side, and the span is in laptop coordinates.
        assert!(edge.push(&monitors, (1919, 700), (99, 0), false));
        // The left monitor's right side only leads on to the main one.
        assert!(!edge.push(&monitors, (-1, 700), (99, 0), false));
        // With the laptop on the other side, it is the negative monitor's left side that counts.
        let mut c = layout(TouchSide::Right, 4480 + 1000, 0);
        laptop_mut(&mut c).monitors = monitors.clone();
        let mut edge = watch(&c, NodeId::Windows);
        assert_eq!(c.edge_span(NodeId::Windows).edge, SwitchEdge::Left);
        assert!(edge.push(&monitors, (-1000, 300), (-99, 0), false));
    }

    #[test]
    fn a_cursor_off_every_monitor_clears_the_push() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let monitors = c.monitors(NodeId::Debian);
        let mut edge = watch(&c, NodeId::Debian);
        edge.push(monitors, (2340, 1439), (0, 30), false);
        assert!(!edge.push(monitors, (100, 100), (0, 30), false));
        assert!(!edge.push(monitors, (2340, 1439), (0, 30), false));
    }

    #[test]
    fn a_completed_push_is_held_until_it_crosses_or_leaves_the_edge() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let monitors = c.monitors(NodeId::Debian);
        let mut edge = watch(&c, NodeId::Debian);
        assert!(!edge.press(monitors, (2340, 1439), (0, 30), false));
        assert!(edge.press(monitors, (2340, 1439), (0, 30), false));
        // Waiting on a newer position, it stays complete, with or without more push.
        assert!(edge.press(monitors, (2340, 1439), (0, 0), false));
        assert!(edge.press(monitors, (2350, 1439), (10, 0), false));
        // Crossing starts the count again.
        edge.clear();
        assert!(!edge.press(monitors, (2340, 1439), (0, 30), false));
        // And so does leaving the edge.
        assert!(edge.press(monitors, (2340, 1439), (0, 30), false));
        assert!(!edge.press(monitors, (2340, 1400), (0, -5), false));
        assert!(!edge.press(monitors, (2340, 1439), (0, 30), false));
    }

    #[test]
    fn a_point_is_on_the_span_only_on_the_outer_edge_beside_the_other_device() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let monitors = c.monitors(NodeId::Debian);
        let edge = watch(&c, NodeId::Debian);
        assert!(edge.on_span(monitors, (2340, 1439)));
        assert!(edge.on_span(monitors, (2240, 1439)) && edge.on_span(monitors, (4159, 1439)));
        assert!(!edge.on_span(monitors, (2239, 1439)), "the bottom of DP-2 beside the laptop only");
        assert!(!edge.on_span(monitors, (500, 1439)), "DP-3 leads nowhere");
        assert!(!edge.on_span(monitors, (2340, 1438)));
        assert!(!EdgeWatch::new().on_span(monitors, (2340, 1439)), "nothing to cross to yet");
        let c = layout(TouchSide::Right, 4480, 360);
        let edge = watch(&c, NodeId::Debian);
        assert!(!edge.on_span(c.monitors(NodeId::Debian), (1919, 700)), "between two of its own monitors");
        assert!(edge.on_span(c.monitors(NodeId::Debian), (4479, 700)));
    }

    /// Long after anything arrived.
    const LATER: u64 = 10_000;

    /// The owner's layout, lined up: the laptop under DP-2, from 1920 to 4479.
    fn real_layout() -> SpatialConfig {
        let mut c = parse_layout(OLD_LAYOUT).unwrap().config;
        c.align_exactly(NodeId::Debian);
        c
    }

    #[test]
    fn the_end_of_the_span_beside_the_side_display_is_guarded() {
        let c = real_layout();
        let monitors = c.monitors(NodeId::Debian);
        let edge = watch(&c, NodeId::Debian);
        assert_eq!(c.edge_span(NodeId::Debian), span(SwitchEdge::Bottom, 1920, 4479));
        // DP-3's bottom carries on the line of DP-2's, left of where the laptop starts.
        for x in [1920, 1940, 1967] {
            assert!(edge.guarded(monitors, (x, 1439)), "{x}");
        }
        for x in [1968, 3000, 4432, 4479] {
            assert!(!edge.guarded(monitors, (x, 1439)), "{x}: the far end is the desktop's own corner");
        }
    }

    #[test]
    fn a_push_in_the_guard_strip_does_not_cross_and_just_past_it_does() {
        let c = real_layout();
        let monitors = c.monitors(NodeId::Debian);
        for x in [1920, 1967] {
            let mut edge = watch(&c, NodeId::Debian);
            assert_eq!(edge.cross(monitors, (x, 1439), (0, 99), false, LATER), Push::Refused(Refusal::Guard), "{x}");
            // Held: moving out of the strip along the edge crosses without pushing all over again.
            assert_eq!(edge.cross(monitors, (1968, 1439), (1, 0), false, LATER), Push::Crossed, "{x} then out of the strip");
        }
        for x in [1968, 4479] {
            let mut edge = watch(&c, NodeId::Debian);
            assert_eq!(edge.cross(monitors, (x, 1439), (0, 99), false, LATER), Push::Crossed, "{x}");
        }
    }

    #[test]
    fn a_single_monitor_has_no_guard() {
        let c = real_layout();
        let monitors = c.monitors(NodeId::Windows).to_vec();
        assert_eq!(monitors.len(), 1);
        let mut edge = watch(&c, NodeId::Windows);
        for x in [0, 1, 47, 2559] {
            assert!(!edge.guarded(&monitors, (x, 0)), "{x}");
        }
        assert_eq!(edge.cross(&monitors, (0, 0), (0, -99), false, LATER), Push::Crossed);
    }

    #[test]
    fn a_monitor_that_touches_the_other_device_is_no_reason_for_a_guard() {
        // Under both of Debian's monitors, the span runs across the join, and nothing is guarded.
        let c = layout(TouchSide::Bottom, 1500, 1440);
        let edge = watch(&c, NodeId::Debian);
        for x in [1500, 1919, 1920, 1967, 3419] {
            assert!(!edge.guarded(c.monitors(NodeId::Debian), (x, 1439)), "{x}");
        }
        // Beside the desktop, DP-3's right side isn't on DP-2's line, so it guards nothing either.
        let c = layout(TouchSide::Right, 4480, 0);
        let edge = watch(&c, NodeId::Debian);
        assert!(!edge.guarded(c.monitors(NodeId::Debian), (4479, 0)));
    }

    #[test]
    fn a_guard_works_along_a_side_edge_too() {
        // Two monitors stacked on the right, the laptop beside the lower one only.
        let mut c = layout(TouchSide::Right, 1920, 1080);
        debian_mut(&mut c).monitors = vec![rect("Top", 0, 0, 1920, 1080), rect("Bottom", 0, 1080, 1920, 1080)];
        laptop_mut(&mut c).monitors = vec![rect("Laptop", 0, 0, 1920, 1080)];
        c.resnap();
        let edge = watch(&c, NodeId::Debian);
        let monitors = c.monitors(NodeId::Debian);
        assert_eq!(c.edge_span(NodeId::Debian), span(SwitchEdge::Right, 1080, 2159));
        assert!(edge.guarded(monitors, (1919, 1080)) && edge.guarded(monitors, (1919, 1127)));
        assert!(!edge.guarded(monitors, (1919, 1128)) && !edge.guarded(monitors, (1919, 2159)));
    }

    /// The laptop's view of its top edge, just after control arrived at `0` ms.
    fn just_arrived_on_the_laptop() -> (SpatialConfig, EdgeWatch) {
        let c = real_layout();
        let mut edge = watch(&c, NodeId::Windows);
        edge.arrived(0);
        (c, edge)
    }

    #[test]
    fn a_quick_deliberate_return_crosses_at_about_100_ms() {
        let (c, mut edge) = just_arrived_on_the_laptop();
        let monitors = c.monitors(NodeId::Windows).to_vec();
        let entry = c.entry_on_peer(NodeId::Debian, 3000, 1439);
        assert_eq!(entry, (1080, SHARED_EDGE_INSET));
        // In, on down the screen 30 px, then straight back up and against the edge.
        assert_eq!(edge.cross(&monitors, entry, (0, 0), false, 5), Push::Pushing);
        assert_eq!(edge.cross(&monitors, (1080, 42), (0, 30), false, 40), Push::Pushing);
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -42), false, 80), Push::Pushing);
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -60), false, 100), Push::Crossed);
    }

    #[test]
    fn a_cursor_that_stays_at_the_edge_does_not_bounce_back() {
        let (c, mut edge) = just_arrived_on_the_laptop();
        let monitors = c.monitors(NodeId::Windows).to_vec();
        // Never more than 11 px further in than it came, and pushing back at the edge all the while.
        for t in (0..250).step_by(10) {
            let y = if t % 20 == 0 { 0 } else { SHARED_EDGE_INSET + SETTLE_CLEAR - 1 };
            let got = edge.cross(&monitors, (1080, y), (0, -99), false, t);
            assert_ne!(got, Push::Crossed, "at {t} ms");
        }
        // The full settle over, a push crosses as it always did.
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -99), false, 250), Push::Crossed);
    }

    #[test]
    fn a_push_refused_for_the_settle_says_so_and_starts_again() {
        let (c, mut edge) = just_arrived_on_the_laptop();
        let monitors = c.monitors(NodeId::Windows).to_vec();
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -99), false, 50), Push::Refused(Refusal::Settle));
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -10), false, 260), Push::Pushing, "the count started again");
        assert_eq!(edge.cross(&monitors, (1080, 0), (0, -50), false, 270), Push::Crossed);
    }

    #[test]
    fn a_held_button_stops_a_crossing() {
        let c = layout(TouchSide::Bottom, 2240, 1440);
        let mut edge = watch(&c, NodeId::Debian);
        assert!(!edge.push(c.monitors(NodeId::Debian), (2340, 1439), (0, 99), true));
    }

    #[test]
    fn studio_requests_are_taken_once() {
        let state = shared_at(0, NodeId::Debian, None);
        let first = take_studio_requests(&state).unwrap();
        assert!(first.manual_switch && first.panic);
        assert_eq!(laptop_x(&first.config), 2240);
        let second = take_studio_requests(&state).unwrap();
        assert!(!second.manual_switch && !second.panic);
    }

    #[test]
    fn busy_lock_skips_the_sync_and_keeps_requests() {
        let state = shared_at(0, NodeId::Debian, None);
        {
            let _studio = state.read().unwrap();
            assert!(take_studio_requests(&state).is_none());
        }
        let taken = take_studio_requests(&state).unwrap();
        assert!(taken.manual_switch && taken.panic);
    }
}
