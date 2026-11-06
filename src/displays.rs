//! What each monitor can do, and changing its resolution and refresh rate.
//!
//! The list comes from a slow look at the operating system, so it is kept here and read from
//! memory: `/api/displays` costs nothing, and the app's 200 ms poll of `/api/local` stays quick.
//!
//! A change has to be safe when it leaves the screen blank. It stays pending for `REVERT_AFTER`
//! and undoes itself here, in the service, unless someone keeps it. That doesn't depend on the
//! app staying open, and a pending change is saved first, so that the service starting again
//! undoes one it was killed in the middle of.

use crate::platform::{DisplayInfo, DisplayMode, Platform};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long the person has to keep a new mode before it undoes itself.
pub const REVERT_AFTER: Duration = Duration::from_secs(15);
/// How often to look at the monitors again. They rarely change, and the look isn't free.
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// How long a note about what happened stays in the status.
const NOTE_FOR: Duration = Duration::from_secs(10);
const PENDING_FILE: &str = "display-change.json";
const NO_ROOM: &str = "Keep or undo the last screen change first.";
const NOT_OFFERED: &str = "That screen doesn't offer that mode.";

/// What to undo if the service stops before a change is settled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Pending {
    output: String,
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

struct Change {
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
    note: Option<(String, Instant)>,
}

pub struct DisplayManager {
    platform: Arc<dyn Platform>,
    state: Mutex<State>,
    decided: Condvar,
    revert_after: Duration,
    /// Where a pending change is saved. `None` never saves.
    file: Option<PathBuf>,
    /// Runs after the monitors change, so the layout can follow.
    on_change: Box<dyn Fn() + Send + Sync>,
}

fn refusal(error: &str) -> String {
    json!({ "ok": false, "error": error }).to_string()
}

impl DisplayManager {
    pub fn new(platform: Arc<dyn Platform>, on_change: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        let file = Some(crate::settings::config_dir().join(PENDING_FILE));
        Self::with(platform, REVERT_AFTER, file, on_change)
    }

    fn with(
        platform: Arc<dyn Platform>,
        revert_after: Duration,
        file: Option<PathBuf>,
        on_change: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self { platform, state: Mutex::default(), decided: Condvar::new(), revert_after, file, on_change })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts looking at the monitors, and undoes a change the last service left pending.
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-displays".to_string()).spawn(move || loop {
            this.look();
            this.undo_leftover();
            std::thread::sleep(LOOK_EVERY);
        });
        if let Err(e) = spawned {
            crate::settings::log_note(&format!("Couldn't start watching the monitors: {e}"));
        }
    }

    /// Reads the monitors, with no lock held, as that can take a while.
    fn look(&self) {
        let list = self.platform.displays();
        self.state().list = list;
    }

    /// `/api/displays`.
    pub fn list_json(&self) -> String {
        json!({ "displays": self.state().list }).to_string()
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
            json!({ "output": c.output, "name": c.name, "from": c.from, "to": c.to, "stage": stage, "ms_left": left })
        });
        json!({ "change": change, "note": state.note.as_ref().map(|(text, _)| text) })
    }

    /// Handles `set_display_mode` and returns the response body. Only a mode this monitor lists
    /// goes any further, so nothing from the request reaches the operating system as it came.
    pub fn request(self: &Arc<Self>, val: &Value) -> String {
        let text = |key: &str| val.get(key).and_then(Value::as_str).unwrap_or("");
        let number = |key: &str| val.get(key).and_then(Value::as_i64).unwrap_or(0);
        let mut state = self.state();
        if state.change.is_some() {
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
        let change = Change {
            output: display.id.clone(),
            name: display.name.clone(),
            from,
            to: to.clone(),
            stage: Stage::Applying,
            decision: None,
        };
        // Saved before the change is made: it's the change that can leave a screen blank.
        self.save_pending(&Pending { output: change.output.clone(), from: change.from.clone() });
        state.change = Some(change);
        state.note = None;
        drop(state);
        let this = self.clone();
        let spawned = std::thread::Builder::new().name("mesh-kvm-display-change".to_string()).spawn(move || this.run_change());
        if spawned.is_err() {
            self.state().change = None;
            self.clear_pending();
            return refusal("mesh-kvm couldn't make that change. Try again.");
        }
        crate::settings::OK_RESPONSE.to_string()
    }

    /// `keep_display_mode` or `undo_display_mode`. Fine if nothing is waiting, as the time may
    /// have run out just as the person answered.
    pub fn decide(&self, keep: bool) -> String {
        if let Some(change) = &mut self.state().change {
            change.decision = Some(if keep { Decision::Keep } else { Decision::Undo });
        }
        self.decided.notify_all();
        crate::settings::OK_RESPONSE.to_string()
    }

    fn run_change(&self) {
        let (output, name, from, to) = {
            let state = self.state();
            let Some(c) = state.change.as_ref() else { return };
            (c.output.clone(), c.name.clone(), c.from.clone(), c.to.clone())
        };
        if let Err(reason) = self.platform.set_display_mode(&output, &to) {
            // It may have got part of the way.
            self.look();
            if self.current_mode(&output).is_some_and(|now| !now.same_as(&from)) {
                let _ = self.go_back_to(&output, &from);
            }
            self.finish(Some(format!("{name} couldn't change to {}. {reason}", to.label())));
            return;
        }
        let until = Instant::now() + self.revert_after;
        if let Some(c) = &mut self.state().change {
            c.stage = Stage::Waiting { until };
        }
        self.look();
        (self.on_change)();
        if self.wait_for_decision(until) == Some(Decision::Keep) {
            self.finish(None);
            return;
        }
        if let Some(c) = &mut self.state().change {
            c.stage = Stage::GoingBack;
        }
        let note = match self.go_back_to(&output, &from) {
            Ok(()) => format!("{name} went back to {}.", from.label()),
            Err(reason) => format!("{name} couldn't go back to {}. {reason}", from.label()),
        };
        self.finish(Some(note));
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

    fn current_mode(&self, output: &str) -> Option<DisplayMode> {
        self.state().list.iter().find(|d| d.id == output)?.current.clone()
    }

    /// Asks for `mode` by what it is, as its handle may not have lasted.
    fn go_back_to(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
        let fresh = self.platform.displays();
        let display = fresh.iter().find(|d| d.id == output);
        let same = display.and_then(|d| d.modes.iter().find(|m| m.same_as(mode)));
        self.platform.set_display_mode(output, same.unwrap_or(mode))
    }

    fn finish(&self, note: Option<String>) {
        self.look();
        self.clear_pending();
        let mut state = self.state();
        state.change = None;
        state.note = note.map(|text| (text, Instant::now()));
        drop(state);
        (self.on_change)();
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

    /// Undoes a change saved by a service that stopped before it was settled. With no monitors
    /// to see yet, as early in a login, the next look tries again.
    fn undo_leftover(&self) {
        let Some(pending) = self.file.as_deref().and_then(read_pending) else { return };
        if self.state().change.is_some() {
            return;
        }
        let Some(now) = self.current_mode(&pending.output) else { return };
        if !now.same_as(&pending.from) {
            if let Err(reason) = self.go_back_to(&pending.output, &pending.from) {
                crate::settings::log_note(&format!("Couldn't undo the last screen change: {reason}"));
            }
            self.look();
            (self.on_change)();
        }
        self.clear_pending();
    }
}

fn read_pending(file: &Path) -> Option<Pending> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MonitorRect, NodeId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mode(width: i32, height: i32, hz: u32) -> DisplayMode {
        DisplayMode { width, height, millihertz: hz * 1000, id: format!("{width}x{height}@{hz}") }
    }

    /// A monitor that remembers what it was told, and can be told to fail.
    #[derive(Default)]
    struct FakeScreens {
        current: Mutex<Option<DisplayMode>>,
        calls: Mutex<Vec<DisplayMode>>,
        refuse: Mutex<Option<String>>,
    }

    impl FakeScreens {
        fn at(mode: DisplayMode) -> Arc<Self> {
            let fake = Self::default();
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
        fn can_pick_audio_source(&self) -> bool {
            false
        }
        fn displays(&self) -> Vec<DisplayInfo> {
            vec![DisplayInfo {
                id: "DP-2".into(),
                name: "Big screen".into(),
                x: 0,
                y: 0,
                current: self.current.lock().unwrap().clone(),
                modes: vec![mode(2560, 1440, 144), mode(2560, 1440, 60), mode(1920, 1080, 60)],
                can_change: true,
            }]
        }
        fn set_display_mode(&self, output: &str, mode: &DisplayMode) -> Result<(), String> {
            assert_eq!(output, "DP-2");
            self.calls.lock().unwrap().push(mode.clone());
            if let Some(reason) = self.refuse.lock().unwrap().clone() {
                return Err(reason);
            }
            *self.current.lock().unwrap() = Some(mode.clone());
            Ok(())
        }
    }

    const SOON: Duration = Duration::from_millis(60);

    fn manager(fake: &Arc<FakeScreens>, revert_after: Duration, file: Option<PathBuf>) -> Arc<DisplayManager> {
        let manager = DisplayManager::with(fake.clone(), revert_after, file, Box::new(|| {}));
        manager.look();
        manager
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
        manager.decide(true);
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
        manager.decide(false);
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
        manager.decide(true);
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
    fn a_failed_revert_is_reported() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), None);
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        *fake.refuse.lock().unwrap() = Some("Gone.".into());
        manager.decide(false);
        wait_until("it to finish", || settled(&manager));
        assert!(manager.status_json()["note"].as_str().unwrap().contains("couldn't go back"));
    }

    #[test]
    fn the_list_is_read_from_memory() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = DisplayManager::with(fake.clone(), SOON, None, Box::new(|| {}));
        assert_eq!(manager.list_json(), r#"{"displays":[]}"#, "nothing yet, and no waiting for it");
        manager.look();
        assert!(manager.list_json().contains("Big screen"));
    }

    #[test]
    fn the_layout_hears_about_a_change_and_about_going_back() {
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let heard = Arc::new(AtomicUsize::new(0));
        let counter = heard.clone();
        let manager = DisplayManager::with(fake, SOON, None, Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        manager.look();
        ask_for(&manager, &mode(1920, 1080, 60));
        wait_until("it to go back", || settled(&manager));
        wait_until("both reports", || heard.load(Ordering::SeqCst) == 2);
    }

    #[test]
    fn a_change_is_saved_while_pending_and_gone_once_settled() {
        let file = temp_file("saved");
        let fake = FakeScreens::at(mode(2560, 1440, 144));
        let manager = manager(&fake, Duration::from_secs(60), Some(file.clone()));
        ask_for(&manager, &mode(1920, 1080, 60));
        let saved = read_pending(&file).unwrap();
        assert_eq!((saved.output.as_str(), saved.from), ("DP-2", mode(2560, 1440, 144)));
        wait_until("the countdown", || stage(&manager).as_deref() == Some("waiting"));
        manager.decide(true);
        wait_until("it to settle", || settled(&manager));
        assert!(!file.exists());
    }

    #[test]
    fn a_service_that_died_mid_change_has_it_undone_by_the_next_one() {
        let file = temp_file("leftover");
        let was = mode(2560, 1440, 144);
        std::fs::write(&file, serde_json::to_string(&Pending { output: "DP-2".into(), from: was.clone() }).unwrap()).unwrap();
        let fake = FakeScreens::at(mode(1920, 1080, 60));
        let manager = manager(&fake, SOON, Some(file.clone()));
        manager.undo_leftover();
        assert_eq!(fake.now(), was);
        assert!(!file.exists());
    }

    #[test]
    fn a_leftover_change_waits_until_the_monitor_can_be_seen() {
        let file = temp_file("early");
        let was = mode(2560, 1440, 144);
        std::fs::write(&file, serde_json::to_string(&Pending { output: "DP-9".into(), from: was }).unwrap()).unwrap();
        let fake = FakeScreens::at(mode(1920, 1080, 60));
        let manager = manager(&fake, SOON, Some(file.clone()));
        manager.undo_leftover();
        assert!(fake.calls().is_empty());
        assert!(file.exists(), "the next look tries again");
    }
}
