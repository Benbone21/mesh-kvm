//! What each monitor can do, and changing its resolution and refresh rate.
//!
//! The list comes from a slow look at the operating system, so it is kept here and read from
//! memory: `/api/displays` costs nothing, and the app's 200 ms poll of `/api/local` stays quick.
//!
//! A change has to be safe when it leaves the screen blank. It stays pending for `REVERT_AFTER`
//! and undoes itself here, in the service, unless someone keeps it. That doesn't depend on the
//! app staying open. Nothing that can stall stands between a change and its countdown: every call
//! to the operating system runs against a limit, and looking at the monitors or moving the layout
//! happens elsewhere. A change is saved before it is made, and it stays owed until the old mode
//! has been seen back, so a failed revert and a service killed halfway are both tried again.

use crate::platform::{DisplayInfo, DisplayMode, Platform};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the person has to keep a new mode before it undoes itself.
pub const REVERT_AFTER: Duration = Duration::from_secs(15);
/// How often to look at the monitors again. They rarely change, and the look isn't free.
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// How often to look while a screen hasn't been seen back on its old mode.
const LOOK_WHILE_OWED: Duration = Duration::from_secs(5);
/// How long a note about what happened stays in the status.
const NOTE_FOR: Duration = Duration::from_secs(10);
const PENDING_FILE: &str = "display-change.json";
const NO_ROOM: &str = "Keep or undo the last screen change first.";
const NOT_OFFERED: &str = "That screen doesn't offer that mode.";
const KEEP_THERE: &str = "A new mode can only be kept on the device with that screen, where you can see it.";
const NO_ANSWER: &str = "The desktop didn't answer.";

/// What to undo if a change isn't settled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Pending {
    output: String,
    /// What the person calls the screen.
    #[serde(default)]
    name: String,
    from: DisplayMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Applying,
    Waiting { until: Instant },
    GoingBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Keep,
    Undo,
}

/// What the layout needs to know, in the order it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutNews {
    /// A trial is about to start: note where the devices sit.
    TrialStarting,
    /// A monitor's size may have changed.
    MonitorsChanged,
    /// The trial was undone: the devices go back where they sat.
    TrialUndone,
    /// The trial was kept, or never happened: where they sit now stands.
    TrialOver,
}

struct Change {
    /// Answers name this, so that one meant for an earlier change can't settle this one.
    id: u64,
    output: String,
    name: String,
    from: DisplayMode,
    to: DisplayMode,
    stage: Stage,
    decision: Option<Decision>,
}

#[derive(Default)]
struct State {
    list: Vec<DisplayInfo>,
    change: Option<Change>,
    /// A screen not yet seen back on its old mode. No new change starts until it has been.
    owed: Option<Pending>,
    note: Option<(String, Instant)>,
}

/// How long the operating system gets to answer before it counts as not answering. They sit
/// above the deadlines the platforms put on their own commands.
#[derive(Debug, Clone, Copy)]
struct Limits {
    list: Duration,
    change: Duration,
}

const LIMITS: Limits = Limits { list: Duration::from_secs(10), change: Duration::from_secs(15) };

pub struct DisplayManager {
    platform: Arc<dyn Platform>,
    state: Mutex<State>,
    decided: Condvar,
    revert_after: Duration,
    limits: Limits,
    next_id: AtomicU64,
    /// Where a pending change is saved. `None` never saves.
    file: Option<PathBuf>,
    /// Passes news to the layout on a thread of its own, as that can take a while.
    news: Mutex<Sender<LayoutNews>>,
}

fn refusal(error: &str) -> String {
    json!({ "ok": false, "error": error }).to_string()
}

/// Runs `work` on its own thread and waits up to `limit`. `None` means it didn't answer, and a
/// call that never returns then costs a thread and nothing else.
fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (done, answer) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("mesh-kvm-display-call".to_string()).spawn(move || {
        let _ = done.send(work());
    });
    spawned.ok()?;
    answer.recv_timeout(limit).ok()
}

impl DisplayManager {
    pub fn new(platform: Arc<dyn Platform>, on_news: Box<dyn FnMut(LayoutNews) + Send>) -> Arc<Self> {
        let file = Some(crate::settings::config_dir().join(PENDING_FILE));
        Self::with(platform, REVERT_AFTER, LIMITS, file, on_news)
    }

    fn with(
        platform: Arc<dyn Platform>,
        revert_after: Duration,
        limits: Limits,
        file: Option<PathBuf>,
        mut on_news: Box<dyn FnMut(LayoutNews) + Send>,
    ) -> Arc<Self> {
        let (news, heard) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("mesh-kvm-display-news".to_string()).spawn(move || {
            for item in heard {
                on_news(item);
            }
        });
        if let Err(e) = spawned {
            crate::settings::log_note(&format!("Couldn't start passing screen changes to the layout: {e}"));
        }
        let owed = file.as_deref().and_then(read_pending);
        let first_id = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |t| t.as_millis() as u64);
        Arc::new(Self {
            platform,
            state: Mutex::new(State { owed, ..Default::default() }),
            decided: Condvar::new(),
            revert_after,
            limits,
            next_id: AtomicU64::new(first_id),
            file,
            news: Mutex::new(news),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tell_layout(&self, news: LayoutNews) {
        let _ = self.news.lock().unwrap_or_else(PoisonError::into_inner).send(news);
    }

    /// Starts looking at the monitors, and putting back whatever is owed.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-displays".to_string()).spawn(move || loop {
            this.look();
            this.settle_owed();
            let owed = this.state().owed.is_some();
            std::thread::sleep(if owed { LOOK_WHILE_OWED } else { LOOK_EVERY });
        });
        if let Err(e) = spawned {
            crate::settings::log_note(&format!("Couldn't start watching the monitors: {e}"));
        }
    }

    fn read(&self) -> Option<Vec<DisplayInfo>> {
        let platform = self.platform.clone();
        within(self.limits.list, move || platform.displays())
    }

    /// Reads the monitors, with no lock held. A desktop that doesn't answer leaves what was known
    /// on show, but nothing is offered for changing until it answers again.
    fn look(&self) {
        match self.read() {
            Some(list) => self.state().list = list,
            None => {
                for display in &mut self.state().list {
                    display.can_change = false;
                }
            }
        }
    }

    fn set_mode(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
        let (platform, output, mode) = (self.platform.clone(), output.to_string(), mode.clone());
        within(self.limits.change, move || platform.set_display_mode(&output, &mode)).unwrap_or_else(|| Err(NO_ANSWER.to_string()))
    }

    /// `/api/displays`.
    pub fn list_json(&self) -> String {
        json!({ "displays": self.state().list }).to_string()
    }

    /// True while a change is under way or a screen is still owed its old mode.
    pub fn trial_running(&self) -> bool {
        let state = self.state();
        state.change.is_some() || state.owed.is_some()
    }

    /// What `/api/local` says about a change: the one waiting for an answer, and the last outcome.
    pub fn status_json(&self) -> Value {
        let mut state = self.state();
        state.note = state.note.take().filter(|(_, at)| at.elapsed() < NOTE_FOR);
        let change = state.change.as_ref().map(|c| {
            let (stage, left) = match c.stage {
                Stage::Applying => ("applying", 0),
                Stage::Waiting { until } => {
                    ("waiting", until.saturating_duration_since(Instant::now()).as_millis() as u64)
                }
                Stage::GoingBack => ("going_back", 0),
            };
            json!({ "id": c.id, "output": c.output, "name": c.name, "from": c.from, "to": c.to, "stage": stage, "ms_left": left })
        });
        json!({ "change": change, "note": state.note.as_ref().map(|(text, _)| text) })
    }

    /// Handles `set_display_mode` and returns the response body. Only a mode this monitor lists
    /// goes any further, so nothing from the request reaches the operating system as it came.
    pub fn request(self: &Arc<Self>, val: &Value) -> String {
        let text = |key: &str| val.get(key).and_then(Value::as_str).unwrap_or("");
        let number = |key: &str| val.get(key).and_then(Value::as_i64).unwrap_or(0);
        let mut state = self.state();
        if state.change.is_some() || state.owed.is_some() {
            return refusal(NO_ROOM);
        }
        let Some(display) = state.list.iter().find(|d| d.id == text("output") && d.can_change) else {
            return refusal(NOT_OFFERED);
        };
        let wanted = |m: &&DisplayMode| {
            i64::from(m.width) == number("width")
                && i64::from(m.height) == number("height")
                && i64::from(m.millihertz) == number("millihertz")
        };
        let (Some(to), Some(from)) = (display.modes.iter().find(wanted), display.current.clone()) else {
            return refusal(NOT_OFFERED);
        };
        if to.same_as(&from) {
            return crate::settings::OK_RESPONSE.to_string();
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let change = Change {
            id,
            output: display.id.clone(),
            name: display.name.clone(),
            from,
            to: to.clone(),
            stage: Stage::Applying,
            decision: None,
        };
        state.change = Some(change);
        state.note = None;
        drop(state);
        let this = self.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-display-change".to_string()).spawn(move || this.run_change());
        if spawned.is_err() {
            self.state().change = None;
            return refusal("mesh-kvm couldn't make that change. Try again.");
        }
        crate::settings::OK_RESPONSE.to_string()
    }

    /// `keep_display_mode` or `undo_display_mode`. An answer counts only for the change it names,
    /// and only during its countdown: an answer that comes too late, as the time runs out, is
    /// dropped. Only the app on this device can keep a change, as only someone here can see the
    /// screen. The other device can still send it back.
    pub fn decide(&self, val: &Value, keep: bool, from_app_here: bool) -> String {
        let id = val.get("change_id").and_then(Value::as_u64);
        let mut state = self.state();
        let waiting = state.change.as_mut().filter(|c| Some(c.id) == id).filter(|c| {
            matches!(c.stage, Stage::Waiting { until } if Instant::now() < until)
        });
        let Some(change) = waiting else { return crate::settings::OK_RESPONSE.to_string() };
        if keep && !from_app_here {
            return refusal(KEEP_THERE);
        }
        change.decision = Some(if keep { Decision::Keep } else { Decision::Undo });
        drop(state);
        self.decided.notify_all();
        crate::settings::OK_RESPONSE.to_string()
    }

    /// Reads the monitor again just before a change, as the list can be up to a look old. The
    /// result is what to go back to and what to ask for, or why this can't be done safely.
    fn fresh_plan(&self, output: &str, name: &str, to: &DisplayMode) -> Result<(DisplayMode, DisplayMode), String> {
        let unsafe_now = || format!("{name} wasn't changed, as the desktop isn't saying what it's set to.");
        let list = self.read().ok_or_else(unsafe_now)?;
        let display = list.iter().find(|d| d.id == output && d.can_change).ok_or_else(unsafe_now)?;
        let from = display.current.clone().ok_or_else(unsafe_now)?;
        let to = display.modes.iter().find(|m| m.same_as(to)).ok_or_else(|| format!("{name} no longer offers {}.", to.label()))?;
        Ok((from, to.clone()))
    }

    fn run_change(self: &Arc<Self>) {
        let (output, name, to) = {
            let state = self.state();
            let Some(c) = state.change.as_ref() else { return };
            (c.output.clone(), c.name.clone(), c.to.clone())
        };
        let (from, to) = match self.fresh_plan(&output, &name, &to) {
            Ok(plan) => plan,
            Err(note) => return self.end_change(Some(note)),
        };
        if to.same_as(&from) {
            return self.end_change(None);
        }
        if let Some(c) = &mut self.state().change {
            (c.from, c.to) = (from.clone(), to.clone());
        }
        // Saved before the change is made: it's the change that can leave a screen blank.
        let pending = Pending { output: output.clone(), name: name.clone(), from: from.clone() };
        self.save_pending(&pending);
        self.tell_layout(LayoutNews::TrialStarting);

        if let Err(reason) = self.set_mode(&output, &to) {
            // It may have got part of the way, or still be going.
            let failed = format!("{name} couldn't change to {}. {reason}", to.label());
            return self.put_back(pending, Some(failed));
        }
        let until = Instant::now() + self.revert_after;
        if let Some(c) = &mut self.state().change {
            c.stage = Stage::Waiting { until };
        }
        // This thread does nothing from here but wait: it owes the revert.
        self.tell_layout(LayoutNews::MonitorsChanged);
        self.look_elsewhere();
        if self.wait_for_decision(until) == Some(Decision::Keep) {
            self.keep(&output, &name, &to);
            return;
        }
        self.put_back(pending, None);
    }

    /// Refreshes the list without this thread waiting for it.
    fn look_elsewhere(self: &Arc<Self>) {
        let this = self.clone();
        let _ = std::thread::Builder::new().name("mesh-kvm-display-look".to_string()).spawn(move || this.look());
    }

    /// The person's answer, or `None` if the time ran out first.
    fn wait_for_decision(&self, until: Instant) -> Option<Decision> {
        let mut state = self.state();
        loop {
            if let Some(decision) = state.change.as_ref().and_then(|c| c.decision) {
                return Some(decision);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self.decided.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    fn keep(&self, output: &str, name: &str, to: &DisplayMode) {
        let (platform, kept_output, kept) = (self.platform.clone(), output.to_string(), to.clone());
        let saved = within(self.limits.change, move || platform.keep_display_mode(&kept_output, &kept));
        let note = match saved {
            Some(Ok(())) => None,
            _ => Some(format!("{name} is staying on {} for now, but it may not be kept after a restart.", to.label())),
        };
        self.clear_pending();
        self.end_change(note);
        self.tell_layout(LayoutNews::TrialOver);
    }

    /// Sends a screen back, and doesn't count it as done until it has been seen back. Going back
    /// comes first: it needs nothing read beforehand, so a stuck listing can't hold it up.
    fn put_back(&self, pending: Pending, failed: Option<String>) {
        if let Some(c) = &mut self.state().change {
            c.stage = Stage::GoingBack;
        }
        let tried = self.set_mode(&pending.output, &pending.from);
        self.look();
        let back = self.is_back(&pending);
        let label = pending.from.label();
        let name = pending.name.clone();
        let note = match (back, failed, tried) {
            (true, Some(failed), _) => failed,
            (true, None, _) => format!("{name} went back to {label}."),
            (false, Some(failed), _) => format!("{failed} mesh-kvm keeps trying to put it back to {label}."),
            (false, None, Err(reason)) => format!("{name} couldn't go back to {label}. {reason} mesh-kvm keeps trying."),
            (false, None, Ok(())) => format!("{name} hasn't been seen back on {label} yet. mesh-kvm keeps trying."),
        };
        if back {
            self.clear_pending();
            self.tell_layout(LayoutNews::MonitorsChanged);
            self.tell_layout(LayoutNews::TrialUndone);
        } else {
            self.state().owed = Some(pending);
        }
        self.end_change(Some(note));
    }

    fn is_back(&self, pending: &Pending) -> bool {
        self.current_mode(&pending.output).is_some_and(|now| now.close_to(&pending.from))
    }

    fn end_change(&self, note: Option<String>) {
        let mut state = self.state();
        state.change = None;
        state.note = note.map(|text| (text, Instant::now()));
    }

    fn current_mode(&self, output: &str) -> Option<DisplayMode> {
        self.state().list.iter().find(|d| d.id == output)?.current.clone()
    }

    /// Tries again for a screen that hasn't been seen back. Until the monitor can be seen, as with
    /// an empty list, early in a login or while it's unplugged, it just stays owed.
    fn settle_owed(&self) {
        let (owed, list_has_it) = {
            let state = self.state();
            if state.change.is_some() {
                return;
            }
            let Some(owed) = state.owed.clone() else { return };
            let listed = state.list.iter().find(|d| d.id == owed.output).cloned();
            (owed, listed)
        };
        let Some(display) = list_has_it else { return };
        if !display.current.as_ref().is_some_and(|now| now.close_to(&owed.from)) {
            // Asked for by what it is, as the handle may not have lasted.
            let mode = display.modes.iter().find(|m| m.close_to(&owed.from)).unwrap_or(&owed.from);
            if let Err(reason) = self.set_mode(&owed.output, mode) {
                crate::settings::log_note(&format!("Couldn't put the last screen change back yet: {reason}"));
            }
            self.look();
            if !self.is_back(&owed) {
                return;
            }
        }
        self.clear_pending();
        let mut state = self.state();
        state.owed = None;
        let name = if owed.name.is_empty() { owed.output.as_str() } else { owed.name.as_str() };
        state.note = Some((format!("{name} went back to {}.", owed.from.label()), Instant::now()));
        drop(state);
        self.tell_layout(LayoutNews::MonitorsChanged);
        self.tell_layout(LayoutNews::TrialUndone);
    }

    fn save_pending(&self, pending: &Pending) {
        let Some(file) = &self.file else { return };
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let saved = serde_json::to_string(pending).map_err(|e| e.to_string()).and_then(|text| {
            crate::settings::write_atomically(file, &text).map_err(|e| e.to_string())
        });
        if let Err(e) = saved {
            crate::settings::log_note(&format!("Couldn't save the screen change in progress: {e}"));
        }
    }

    fn clear_pending(&self) {
        if let Some(file) = &self.file {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[cfg(test)]
impl DisplayManager {
    /// A manager with a change counting down and nothing behind it, to test what asks it things.
    pub fn for_test_with_waiting_change() -> Arc<Self> {
        struct Blank;
        impl Platform for Blank {
            fn node(&self) -> crate::protocol::NodeId {
                crate::protocol::NodeId::Debian
            }
            fn label(&self) -> &'static str {
                ""
            }
            fn os_name(&self) -> &'static str {
                ""
            }
            fn monitors(&self) -> Vec<crate::protocol::MonitorRect> {
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
        let manager = Self::with(Arc::new(Blank), REVERT_AFTER, LIMITS, None, Box::new(|_| {}));
        let mode = DisplayMode { width: 1, height: 1, millihertz: 1, id: String::new() };
        manager.state().change = Some(Change {
            id: 7,
            output: "DP-2".into(),
            name: "Big".into(),
            from: mode.clone(),
            to: mode,
            stage: Stage::Waiting { until: Instant::now() + REVERT_AFTER },
            decision: None,
        });
        manager
    }
}

fn read_pending(file: &Path) -> Option<Pending> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MonitorRect, NodeId};
    use std::sync::atomic::Ordering;

    fn mode(width: i32, height: i32, hz: u32) -> DisplayMode {
        DisplayMode { width, height, millihertz: hz * 1000, id: format!("{width}x{height}@{hz}") }
    }

    /// A monitor that remembers what it was told, and can be told to fail.
    #[derive(Default)]
    struct FakeScreens {
        current: Mutex<Option<DisplayMode>>,
        calls: Mutex<Vec<DisplayMode>>,
        kept: Mutex<Vec<DisplayMode>>,
        refuse: Mutex<Option<String>>,
        /// While true, listing never answers.
        stuck: std::sync::atomic::AtomicBool,
        /// While true, nothing is listed.
        gone: std::sync::atomic::AtomicBool,
        /// While true, a change reports success and does nothing.
        ignores_changes: std::sync::atomic::AtomicBool,
        can_change: std::sync::atomic::AtomicBool,
    }

    impl FakeScreens {
        fn at(mode: DisplayMode) -> Arc<Self> {
            let fake = Self { can_change: true.into(), ..Default::default() };
            *fake.current.lock().unwrap() = Some(mode);
            Arc::new(fake)
        }

        fn calls(&self) -> Vec<DisplayMode> {
            self.calls.lock().unwrap().clone()
        }

        fn now(&self) -> DisplayMode {
            self.current.lock().unwrap().clone().unwrap()
        }
    }

    impl Platform for FakeScreens {
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
        fn displays(&self) -> Vec<DisplayInfo> {
            while self.stuck.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            if self.gone.load(Ordering::SeqCst) {
                return Vec::new();
            }
            vec![DisplayInfo {
                id: "DP-2".into(),
                name: "Big screen".into(),
                x: 0,
                y: 0,
                current: self.current.lock().unwrap().clone(),
                modes: vec![mode(2560, 1440, 144), mode(2560, 1440, 60), mode(1920, 1080, 60)],
                can_change: self.can_change.load(Ordering::SeqCst),
            }]
        }
        fn keep_display_mode(&self, _: &str, mode: &DisplayMode) -> Result<(), String> {
            self.kept.lock().unwrap().push(mode.clone());
            Ok(())
        }
        fn set_display_mode(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
            assert_eq!(output, "DP-2");
            self.calls.lock().unwrap().push(mode.clone());
            if let Some(reason) = self.refuse.lock().unwrap().clone() {
                return Err(reason);
            }
            if !self.ignores_changes.load(Ordering::SeqCst) {
                *self.current.lock().unwrap() = Some(mode.clone());
            }
            Ok(())
        }
    }

    const SOON: Duration = Duration::from_millis(60);

    const QUICK: Limits = Limits { list: Duration::from_millis(30), change: Duration::from_millis(200) };

    fn manager(fake: &Arc<FakeScreens>, revert_after: Duration, file: Option<PathBuf>) -> Arc<DisplayManager> {
        let manager = DisplayManager::with(fake.clone(), revert_after, QUICK, file, Box::new(|_| {}));
        manager.look();
        manager
    }

    fn change_id(manager: &DisplayManager) -> Value {
        manager.status_json()["change"]["id"].clone()
    }

    /// The app here answering the change now waiting.
    fn answer(manager: &DisplayManager, keep: bool) -> String {
        manager.decide(&json!({ "change_id": change_id(manager) }), keep, true)
    }

    fn owed(manager: &DisplayManager) -> bool {
        manager.state().owed.is_some()
    }

    fn ask_for(manager: &Arc<DisplayManager>, m: &DisplayMode) -> Value {
        let body = json!({ "output": "DP-2", "width": m.width, "height": m.height, "millihertz": m.millihertz });
        serde_json::from_str(&manager.request(&body)).unwrap()
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    fn stage(manager: &DisplayManager) -> Option<String> {
        manager.status_json()["change"]["stage"].as_str().map(str::to_string)
    }

    fn settled(manager: &DisplayManager) -> bool {
        manager.status_json()["change"].is_null()
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-displays-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(PENDING_FILE)
    }

    #[test]
    fn a_new_mode_goes_back_by_itself_when_nobody_answers() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let manager = manager(&fake, SOON, None);
        assert_eq!(ask_for(&manager, &mode(1920, 1080, 60))["ok"], true);
        wait_until("the change", || fake.calls().len() == 1);
        assert_eq!(fake.now(), mode(1920, 1080, 60));
        wait_until("it to go back", || settled(&manager));
        assert_eq!(fake.now(), was);
        assert_eq!(fake.calls().len(), 2);
        let note = manager.status_json()["note"].as_str().unwrap().to_string();
        assert_eq!(note, "Big screen went back to 2560 \u{d7} 1440 at 144 Hz.");
    }

    #[test]
    fn keeping_it_stops_the_countdown() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        let left = manager.status_json()["change"]["ms_left"].as_u64().unwrap();
        assert!(left > 50_000 && left <= 60_000, "{left}");
        answer(&manager, true);
        wait_until("it to settle", || settled(&manager));
        assert_eq!(fake.now(), mode(1920, 1080, 60));
        assert_eq!(fake.calls().len(), 1, "never put back");
        assert!(manager.status_json()["note"].is_null());
    }

    #[test]
    fn undoing_it_goes_back_at_once() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        answer(&manager, false);
        wait_until("it to go back", || settled(&manager));
        assert_eq!(fake.now(), was);
    }

    #[test]
    fn only_a_mode_the_monitor_lists_is_accepted() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        let reply = ask_for(&manager, &mode(1234, 567, 60));
        assert_eq!((reply["ok"].as_bool(), reply["error"].as_str()), (Some(false), Some(NOT_OFFERED)));
        let body = json!({ "output": "DP-2; rm -rf /", "width": 1920, "height": 1080, "millihertz": 60000 });
        assert!(manager.request(&body).contains(NOT_OFFERED));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn asking_for_the_mode_it_is_in_changes_nothing() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        assert_eq!(ask_for(&manager, &mode(2560, 1440, 144))["ok"], true);
        assert!(settled(&manager));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_second_change_waits_for_the_first_to_be_settled() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        let reply = ask_for(&manager, &mode(2560, 1440, 60));
        assert_eq!(reply["error"], NO_ROOM);
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        answer(&manager, true);
        wait_until("it to settle", || settled(&manager));
    }

    #[test]
    fn a_change_that_fails_says_why_and_needs_no_answer() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        *fake.refuse.lock().unwrap() = Some("The display driver said no.".into());
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to fail", || settled(&manager));
        assert_eq!(fake.now(), was);
        let status = manager.status_json();
        let note = status["note"].as_str().unwrap();
        assert!(note.contains("couldn't change") && note.contains("The display driver said no."), "{note}");
    }

    #[test]
    fn a_failed_revert_stays_owed_and_is_tried_again_until_seen_back() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let file = temp_file("owed");
        let manager = manager(&fake, Duration::from_secs(60), Some(file.clone()));
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        *fake.refuse.lock().unwrap() = Some("Gone.".into());
        answer(&manager, false);
        wait_until("it to finish", || settled(&manager));
        assert!(manager.status_json()["note"].as_str().unwrap().contains("keeps trying"));
        assert!(owed(&manager) && file.exists());
        assert_eq!(ask_for(&manager, &mode(2560, 1440, 60))["error"], NO_ROOM, "nothing new while this is owed");

        manager.look();
        manager.settle_owed();
        assert!(owed(&manager), "still refused");
        *fake.refuse.lock().unwrap() = None;
        manager.look();
        manager.settle_owed();
        assert_eq!(fake.now(), was);
        assert!(!owed(&manager) && !file.exists());
    }

    #[test]
    fn a_revert_that_reports_success_but_changes_nothing_stays_owed() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        fake.ignores_changes.store(true, Ordering::SeqCst);
        answer(&manager, false);
        wait_until("it to finish", || settled(&manager));
        assert!(owed(&manager));
    }

    #[test]
    fn an_owed_screen_waits_out_an_empty_list_and_goes_back_when_it_returns() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        *fake.refuse.lock().unwrap() = Some("Unplugged.".into());
        fake.gone.store(true, Ordering::SeqCst);
        answer(&manager, false);
        wait_until("it to finish", || settled(&manager));
        for _ in 0..3 {
            manager.look();
            manager.settle_owed();
            assert!(owed(&manager), "an empty list proves nothing");
        }
        *fake.refuse.lock().unwrap() = None;
        fake.gone.store(false, Ordering::SeqCst);
        manager.look();
        manager.settle_owed();
        assert_eq!(fake.now(), was);
        assert!(!owed(&manager));
    }

    #[test]
    fn a_listing_that_hangs_after_the_change_cannot_stop_the_revert() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let manager = manager(&fake, SOON, None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the change", || fake.calls().len() == 1);
        // The manager's own listing has seen the change before anything hangs: otherwise a
        // listing from before it can pass for the screen being back, and nothing is owed.
        let current_width = |m: &DisplayManager| serde_json::from_str::<Value>(&m.list_json()).ok().map(|v| v["displays"][0]["current"]["width"].clone());
        wait_until("the change seen", || current_width(&manager) == Some(Value::from(1920)));
        // The change is made, and from here on no listing ever answers.
        fake.stuck.store(true, Ordering::SeqCst);
        wait_until("the revert", || fake.calls().len() == 2);
        assert_eq!(fake.now(), was);
        wait_until("it to finish", || settled(&manager));
        assert!(owed(&manager), "not seen back yet");
        fake.stuck.store(false, Ordering::SeqCst);
        manager.look();
        manager.settle_owed();
        assert!(!owed(&manager));
    }

    #[test]
    fn a_layout_that_hangs_cannot_stop_the_revert() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let hung = Box::new(|_| std::thread::sleep(Duration::from_secs(600)));
        let manager = DisplayManager::with(fake.clone(), SOON, QUICK, None, hung);
        manager.look();
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the revert", || fake.calls().len() == 2);
        assert_eq!(fake.now(), was);
    }

    #[test]
    fn a_desktop_that_stops_answering_offers_no_changes_and_starts_none() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        fake.stuck.store(true, Ordering::SeqCst);
        manager.look();
        assert!(manager.list_json().contains(r#""can_change":false"#));
        assert!(manager.list_json().contains("Big screen"), "what was known stays on show");
        assert_eq!(ask_for(&manager, &mode(1920, 1080, 60))["error"], NOT_OFFERED);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_change_is_dropped_if_the_mode_in_use_cant_be_read_just_before_it() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        // The list said it was fine a moment ago, and now the desktop has stopped answering.
        fake.stuck.store(true, Ordering::SeqCst);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to give up", || settled(&manager));
        assert!(fake.calls().is_empty());
        assert!(manager.status_json()["note"].as_str().unwrap().contains("wasn't changed"));
        fake.stuck.store(false, Ordering::SeqCst);
    }

    #[test]
    fn a_change_is_dropped_if_the_monitor_stopped_being_changeable() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        fake.can_change.store(false, Ordering::SeqCst);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to give up", || settled(&manager));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn going_back_uses_the_mode_read_just_before_the_change_not_the_stale_list() {
        let stale = mode(2560, 1440, 144);
        let fake = FakeScreens::at(stale);
        let manager = manager(&fake, SOON, None);
        // Someone changes the mode outside mesh-kvm after the last look.
        let really = mode(2560, 1440, 60);
        *fake.current.lock().unwrap() = Some(really.clone());
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to go back", || fake.calls().len() == 2);
        assert_eq!(fake.now(), really);
    }

    #[test]
    fn a_late_answer_cannot_settle_the_next_change() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        let old = change_id(&manager);
        answer(&manager, false);
        wait_until("it to go back", || settled(&manager));
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the next countdown", || stage(&manager).as_deref() == Some("waiting"));
        assert_ne!(change_id(&manager), old);
        manager.decide(&json!({ "change_id": old }), true, true);
        manager.decide(&json!({}), true, true);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(stage(&manager).as_deref(), Some("waiting"), "its countdown carries on");
        assert!(fake.kept.lock().unwrap().is_empty());
        answer(&manager, false);
        wait_until("it to settle", || settled(&manager));
    }

    #[test]
    fn an_answer_before_the_countdown_starts_counts_for_nothing() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        fake.stuck.store(true, Ordering::SeqCst);
        ask_for(&manager, &mode(1920, 1080, 60));
        assert_eq!(stage(&manager).as_deref(), Some("applying"));
        answer(&manager, true);
        fake.stuck.store(false, Ordering::SeqCst);
        wait_until("it to settle", || settled(&manager));
        assert!(fake.kept.lock().unwrap().is_empty());
    }

    #[test]
    fn only_the_app_here_can_keep_a_change_and_either_can_undo_it() {
        let was = mode(2560, 1440, 144);
        let fake = FakeScreens::at(was.clone());
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        let id = json!({ "change_id": change_id(&manager) });
        assert!(manager.decide(&id, true, false).contains(KEEP_THERE));
        assert_eq!(stage(&manager).as_deref(), Some("waiting"));
        assert!(fake.kept.lock().unwrap().is_empty());
        assert_eq!(manager.decide(&id, false, false), crate::settings::OK_RESPONSE);
        wait_until("it to go back", || settled(&manager));
        assert_eq!(fake.now(), was);
    }

    #[test]
    fn a_mode_is_made_lasting_only_when_kept() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, SOON, None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to go back", || settled(&manager) && fake.calls().len() == 2);
        assert!(fake.kept.lock().unwrap().is_empty(), "a reverted trial leaves nothing lasting");

        let manager = self::manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        answer(&manager, true);
        wait_until("it to settle", || settled(&manager));
        assert_eq!(*fake.kept.lock().unwrap(), [mode(1920, 1080, 60)]);
    }

    #[test]
    fn the_list_is_read_from_memory() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = DisplayManager::with(fake.clone(), SOON, QUICK, None, Box::new(|_| {}));
        assert_eq!(manager.list_json(), r#"{"displays":[]}"#, "nothing yet, and no waiting for it");
        manager.look();
        assert!(manager.list_json().contains("Big screen"));
    }

    fn heard_by_layout(fake: &Arc<FakeScreens>, revert_after: Duration) -> (Arc<DisplayManager>, Arc<Mutex<Vec<LayoutNews>>>) {
        let heard: Arc<Mutex<Vec<LayoutNews>>> = Arc::default();
        let log = heard.clone();
        let manager = DisplayManager::with(fake.clone(), revert_after, QUICK, None, Box::new(move |news| log.lock().unwrap().push(news)));
        manager.look();
        (manager, heard)
    }

    #[test]
    fn the_layout_hears_a_trial_start_and_end_in_order() {
        use LayoutNews::*;
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let (manager, heard) = heard_by_layout(&fake, SOON);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to go back", || heard.lock().unwrap().len() == 4);
        assert_eq!(*heard.lock().unwrap(), [TrialStarting, MonitorsChanged, MonitorsChanged, TrialUndone]);

        let (manager, heard) = heard_by_layout(&fake, Duration::from_secs(60));
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        answer(&manager, true);
        wait_until("it to settle", || heard.lock().unwrap().len() == 3);
        assert_eq!(*heard.lock().unwrap(), [TrialStarting, MonitorsChanged, TrialOver]);
    }

    #[test]
    fn a_trial_counts_as_running_until_it_is_answered() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        assert!(!manager.trial_running());
        ask_for(&manager, &mode(1920, 1080, 60));
        assert!(manager.trial_running());
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        assert!(manager.trial_running());
        answer(&manager, true);
        wait_until("it to settle", || !manager.trial_running());
    }

    #[test]
    fn a_change_is_saved_while_pending_and_gone_once_settled() {
        let file = temp_file("saved");
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), Some(file.clone()));
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        let saved = read_pending(&file).unwrap();
        assert_eq!((saved.output.as_str(), saved.from), ("DP-2", mode(2560, 1440, 144)));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        answer(&manager, true);
        wait_until("it to settle", || settled(&manager));
        assert!(!file.exists());
    }

    #[test]
    fn a_service_that_died_mid_change_has_it_undone_by_the_next_one() {
        let file = temp_file("leftover");
        let was = mode(2560, 1440, 144);
        std::fs::write(&file, serde_json::to_string(&Pending { output: "DP-2".into(), name: String::new(), from: was.clone() }).unwrap()).unwrap();
        let fake = FakeScreens::at(mode(1920, 1080, 60));
        let manager = manager(&fake, SOON, Some(file.clone()));
        manager.settle_owed();
        assert_eq!(fake.now(), was);
        assert!(!file.exists());
    }

    #[test]
    fn a_leftover_change_waits_until_the_monitor_can_be_seen() {
        let file = temp_file("early");
        let was = mode(2560, 1440, 144);
        std::fs::write(&file, serde_json::to_string(&Pending { output: "DP-9".into(), name: String::new(), from: was }).unwrap()).unwrap();
        let fake = FakeScreens::at(mode(1920, 1080, 60));
        let manager = manager(&fake, SOON, Some(file.clone()));
        manager.settle_owed();
        assert!(fake.calls().is_empty());
        assert!(file.exists(), "the next look tries again");
    }
}
