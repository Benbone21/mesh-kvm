//! Silences the output being sent while its sound plays on the other device, so sound comes from
//! only one of them. Losing the link always gives the output back.
//!
//! Everything is tied to the device that was muted, not to whichever is chosen or the default now:
//! those change under us when a headset, dock or screen comes and goes.

use crate::audio::{
    devices, peer_listening, reaching_peer, AUDIO_CAPTURE_SURVIVES_MUTE, AUDIO_OUTPUT_CAPTURE_LIVE, AUDIO_PEER_ONLINE, AUDIO_PLAY_ONLY_ON_OTHER,
    AUDIO_RX_MUTED, AUDIO_RX_PEAK_PCT, AUDIO_TX_SOUND_PACKETS,
};
use crate::settings::write_atomically;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How often the decision is made. It costs nothing unless something changed.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How often the speakers are looked at while muting is wanted.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Audio seen this recently still counts as flowing.
const RECENT_AUDIO: Duration = Duration::from_secs(2);
/// How long the service must be silent before the tray cleans up after it.
const SERVICE_SILENCE: Duration = Duration::from_secs(5);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const SLEEP_WAIT: Duration = Duration::from_millis(1500);
/// Once sound has been going, a silence this long keeps the output muted, so a pause between
/// tracks doesn't bring it back and take it away again.
const SILENCE_HOLD: Duration = Duration::from_secs(30);
/// Sound that stops within this long of the mute may have been stopped by it, as ordinary Windows
/// loopback is. So this is not yet a settled mute.
const SETTLED_AFTER: Duration = Duration::from_secs(5);
/// How long an unsettled mute waits out a silence. Nobody may be hearing anything, so not long.
const UNSETTLED_HOLD: Duration = Duration::from_secs(3);
/// Sound vanishing on a mute this many times running stops the muting for a while. Muting only
/// happens with a capture that a mute can't reach, so this is a safety net: what it mostly sees is
/// short sounds. A capture that really was being silenced would come round every few seconds, so
/// only ones this close together count.
const VANISHED_LIMIT: u32 = 2;
const VANISHED_WITHIN: Duration = Duration::from_secs(30);
const FIRST_BAR: Duration = Duration::from_secs(30);
const LONGEST_BAR: Duration = Duration::from_secs(600);
const FLAG_FILE: &str = "mute-state.json";
/// A record that doesn't say which device: the default is the best guess.
const UNNAMED_DEVICE: &str = "";

static STARTED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);
/// The machine is going to sleep, so the speakers stay given back until it wakes.
static SLEEPING: AtomicBool = AtomicBool::new(false);
static RELEASED_FOR_SLEEP: AtomicBool = AtomicBool::new(false);

/// Mute only while all are true and no audio from the other device is playing on this same output:
/// muting it would silence what's being received.
pub fn should_mute(
    setting: bool,
    sending: bool,
    peer_online: bool,
    reaching_peer: bool,
    receiving_audio: bool,
) -> bool {
    setting && sending && peer_online && reaching_peer && !receiving_audio
}

/// Muting stopped because sound kept stopping just after it, so the app can say so.
static BARRED: AtomicBool = AtomicBool::new(false);

/// True while muting is off because sound kept vanishing when it was on.
pub fn barred() -> bool {
    BARRED.load(Ordering::Relaxed)
}

/// The output devices of this machine. A device is named by an identity that outlives replugging.
pub trait MuteBackend {
    /// The output to mute, or None when it can't be found.
    fn default_device(&mut self) -> Option<String>;
    /// Everything to mute: usually just that one.
    fn targets(&mut self) -> Option<Vec<String>> {
        self.default_device().map(|device| vec![device])
    }
    /// None when the device is absent or can't be read.
    fn is_muted(&mut self, device: &str) -> Option<bool>;
    /// False when the device is absent or the change failed.
    fn set_muted(&mut self, device: &str, muted: bool) -> bool;
}

#[derive(Debug, Clone, Copy)]
pub struct MuteInputs {
    pub setting: bool,
    /// An output is chosen to send. A microphone doesn't play through the speakers, so sending
    /// one is no reason to silence them.
    pub send_on: bool,
    /// The capture is running.
    pub tx_active: bool,
    pub peer_online: bool,
    /// Real sound is reaching the other device's output, or was until a moment of silence.
    pub reaching_peer: bool,
    /// Sound from the other device is playing on the output that would be muted.
    pub receiving_audio: bool,
}

impl MuteInputs {
    fn wanted(self) -> bool {
        should_mute(
            self.setting,
            self.send_on && self.tx_active,
            self.peer_online,
            self.reaching_peer,
            self.receiving_audio,
        )
    }

    /// Whether the person is asking for muting. A dropped link or a quiet moment isn't sending
    /// starting again, so neither may undo a manual unmute.
    fn requested(self) -> bool {
        self.setting && self.send_on
    }
}

#[derive(Serialize, Deserialize)]
struct FlagFile {
    muted_by_mesh: bool,
    #[serde(default)]
    devices: Vec<String>,
}

/// The record of what mesh-kvm muted, so a crash can't leave anything silent for good.
pub struct MuteFlag {
    path: PathBuf,
}

impl MuteFlag {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The devices on record. A file that can't be parsed still says something was muted, and
    /// unmuting is the safe side.
    fn read(&self) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        match serde_json::from_str::<FlagFile>(&text) {
            Ok(FlagFile { muted_by_mesh: false, .. }) => Vec::new(),
            Ok(FlagFile { devices, .. }) if !devices.is_empty() => devices,
            _ => vec![UNNAMED_DEVICE.to_string()],
        }
    }

    /// Replaces the record. Having nothing to record removes the file.
    fn write(&self, devices: &[String]) -> bool {
        if devices.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return true;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let record = FlagFile { muted_by_mesh: true, devices: devices.to_vec() };
        let Ok(text) = serde_json::to_string(&record) else {
            return false;
        };
        write_atomically(&self.path, &text).is_ok()
    }
}

/// Spaces out attempts at something that keeps failing.
#[derive(Default)]
struct Retry {
    next: Option<Instant>,
    failed: bool,
}

impl Retry {
    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|at| now >= at)
    }

    fn record(&mut self, worked: bool, now: Instant) {
        self.failed = !worked;
        self.next = (!worked).then_some(now + RETRY_INTERVAL);
    }
}

/// Decides when to touch the speakers. Time comes in through `tick`, so it runs in tests as it
/// does live.
pub struct MuteController<B: MuteBackend> {
    backend: B,
    flag: MuteFlag,
    /// Every device silent because of mesh-kvm, so ours to restore. More than one when a device
    /// we muted was unplugged and the next default was muted too.
    owned: Vec<String>,
    /// The owned devices still meant to be muted. Any other owned device is waiting for restore.
    active: Vec<String>,
    /// Devices the person has taken charge of until muting is next asked for.
    hands_off: Vec<String>,
    wanted: bool,
    next_watch: Option<Instant>,
    /// Reading the default and muting it. Separate from `restoring`, so that an unplugged device
    /// waiting to be restored doesn't hold up work on the one in use.
    default_work: Retry,
    restoring: Retry,
}

impl<B: MuteBackend> MuteController<B> {
    /// A record left by a crash makes those devices ours to restore, so a backend that isn't up
    /// yet is retried like any other failed unmute.
    pub fn new(backend: B, flag: MuteFlag) -> Self {
        let owned = flag.read();
        Self {
            backend,
            flag,
            owned,
            active: Vec::new(),
            hands_off: Vec::new(),
            wanted: false,
            next_watch: None,
            default_work: Retry::default(),
            restoring: Retry::default(),
        }
    }

    /// True from a backend failure until that work next succeeds.
    pub fn failing(&self) -> bool {
        self.default_work.failed || self.restoring.failed
    }

    pub fn tick(&mut self, inputs: MuteInputs, now: Instant) {
        if !inputs.requested() {
            self.hands_off.clear();
        }
        let want = inputs.wanted();
        if want != self.wanted {
            self.wanted = want;
            self.next_watch = None;
            if !want {
                self.default_work.next = None;
                self.restoring.next = None;
            }
        }
        if !want {
            self.active.clear();
        }
        let targets = if want { self.read_targets(now) } else { None };
        self.restore_stranded(now);
        if let Some(targets) = targets {
            // Each gets its turn, however the last one went.
            let worked = targets.iter().fold(true, |worked, device| self.settle_target(device) && worked);
            self.default_work.record(worked, now);
        }
    }

    /// Gives everything back, ignoring any retry gap: there may be no later tick. Whatever fails
    /// stays on record for later.
    pub fn release(&mut self) {
        self.active.clear();
        let all = self.owned.clone();
        if !all.is_empty() {
            self.restore(&all);
        }
    }

    /// What to mute, when it is time to look. Noticing that it moved strands the old device.
    fn read_targets(&mut self, now: Instant) -> Option<Vec<String>> {
        let waiting = self.next_watch.is_some_and(|at| now < at);
        if waiting || !self.default_work.due(now) {
            return None;
        }
        self.next_watch = Some(now + WATCH_INTERVAL);
        let targets = self.backend.targets();
        match &targets {
            Some(targets) => self.active = targets.iter().filter(|device| self.owned.contains(device)).cloned().collect(),
            None => self.default_work.record(false, now),
        }
        targets
    }

    fn restore_stranded(&mut self, now: Instant) {
        let stranded: Vec<String> =
            self.owned.iter().filter(|device| !self.active.contains(device)).cloned().collect();
        if !stranded.is_empty() && self.restoring.due(now) {
            let worked = self.restore(&stranded);
            self.restoring.record(worked, now);
        }
    }

    fn settle_target(&mut self, device: &str) -> bool {
        if self.hands_off.iter().any(|d| d == device) {
            true
        } else if self.active.iter().any(|d| d == device) {
            self.watch(device)
        } else {
            self.mute(device)
        }
    }

    /// The record goes down first: a crash just after muting must still be able to undo it.
    fn mute(&mut self, device: &str) -> bool {
        match self.backend.is_muted(device) {
            None => return false,
            Some(true) => {
                self.hands_off.push(device.to_string());
                return true;
            }
            Some(false) => {}
        }
        self.owned.push(device.to_string());
        if !self.flag.write(&self.owned) {
            self.owned.pop();
            return false;
        }
        let worked = self.backend.set_muted(device, true);
        // A failure report doesn't prove nothing happened, so only a clean read gives up the claim.
        if worked || self.backend.is_muted(device) != Some(false) {
            self.active.push(device.to_string());
        } else {
            self.owned.pop();
            self.flag.write(&self.owned);
        }
        worked
    }

    /// Spots the person unmuting by hand.
    fn watch(&mut self, device: &str) -> bool {
        match self.backend.is_muted(device) {
            None => false,
            Some(true) => true,
            Some(false) => {
                self.owned.retain(|d| d != device);
                self.active.retain(|d| d != device);
                self.hands_off.push(device.to_string());
                self.flag.write(&self.owned);
                true
            }
        }
    }

    fn restore(&mut self, devices: &[String]) -> bool {
        let mut all_worked = true;
        for device in devices {
            let target = if device == UNNAMED_DEVICE {
                self.backend.default_device()
            } else {
                Some(device.clone())
            };
            if target.is_some_and(|target| self.backend.set_muted(&target, false)) {
                self.owned.retain(|d| d != device);
            } else {
                all_worked = false;
            }
        }
        self.flag.write(&self.owned);
        all_worked
    }
}

/// Remembers when something last happened.
#[derive(Default)]
struct Recent {
    last: Option<Instant>,
}

impl Recent {
    /// Notes whether it is happening now, and returns whether it happened lately.
    fn note(&mut self, happening: bool, now: Instant) -> bool {
        if happening {
            self.last = Some(now);
        }
        self.last.is_some_and(|last| now.duration_since(last) < RECENT_AUDIO)
    }
}

/// Decides whether sound is reaching the other device, across the gaps in it. Starting takes real
/// sound, confirmed from the far side. Carrying on is more forgiving, as silence on the output is
/// normal: it's what's between tracks. But silence can also mean the mute itself stopped the sound,
/// with nobody hearing anything. See the constants above for how the two are told apart.
#[derive(Default)]
struct SoundHold {
    /// When this run of muting began.
    since: Option<Instant>,
    last_confirmed: Option<Instant>,
    vanished: u32,
    last_vanished: Option<Instant>,
    barred_until: Option<Instant>,
    next_bar: Option<Duration>,
}

impl SoundHold {
    fn end_run(&mut self, started: Instant, last: Instant, proven: bool, now: Instant) {
        self.since = None;
        if proven || last.duration_since(started) >= SETTLED_AFTER {
            self.vanished = 0;
            self.next_bar = None;
            return;
        }
        if self.last_vanished.is_none_or(|at| now.duration_since(at) >= VANISHED_WITHIN) {
            self.vanished = 0;
        }
        self.vanished += 1;
        self.last_vanished = Some(now);
        if self.vanished >= VANISHED_LIMIT {
            let bar = self.next_bar.unwrap_or(FIRST_BAR);
            self.barred_until = Some(now + bar);
            self.next_bar = Some((bar * 2).min(LONGEST_BAR));
            // One more failure is enough once the bar lifts.
            self.vanished = VANISHED_LIMIT - 1;
            self.last_vanished = self.barred_until;
        }
    }

    /// `requested`: the person wants it and the link is up. `confirmed`: real sound is going out
    /// and the far side says it's coming out of an output. `connected`: the capture is running and
    /// the far side has an output open, sound or no sound.
    /// `proven`: that this capture carries on under a mute has been measured, so sound stopping can
    /// only be sound stopping.
    fn tick(&mut self, requested: bool, confirmed: bool, connected: bool, proven: bool, now: Instant) -> bool {
        if !requested {
            *self = Self::default();
            return false;
        }
        if self.barred_until.is_some_and(|until| now < until) {
            return false;
        }
        self.barred_until = None;
        let Some(started) = self.since else {
            if confirmed {
                self.since = Some(now);
                self.last_confirmed = Some(now);
            }
            return confirmed;
        };
        if confirmed {
            self.last_confirmed = Some(now);
        }
        if !connected {
            self.since = None;
            return false;
        }
        let last = self.last_confirmed.unwrap_or(started);
        let settled = last.duration_since(started) >= SETTLED_AFTER;
        let hold = if settled { SILENCE_HOLD } else { UNSETTLED_HOLD };
        if now.duration_since(last) >= hold {
            self.end_run(started, last, proven, now);
            return false;
        }
        true
    }

    fn barred(&self, now: Instant) -> bool {
        self.barred_until.is_some_and(|until| now < until)
    }
}

/// What the rest of the service says, at one moment.
#[derive(Debug, Clone, Copy)]
struct Readings {
    setting: bool,
    output_chosen: bool,
    capture_running: bool,
    /// Muting the output won't silence that capture.
    capture_survives_mute: bool,
    /// And that's been measured on real hardware, so a silence after muting says nothing about it.
    capture_proven_under_mute: bool,
    /// Packets sent with real sound in them, however long ago.
    sound_packets: u64,
    peer_online: bool,
    /// The far side has an output open.
    peer_listening: bool,
    /// The far side says real sound is coming out of that output.
    peer_hearing: bool,
    /// Sound from the far side is playing here, on the output that would be muted.
    playing_here: bool,
}

#[derive(Default)]
struct LiveSignals {
    sound_packets: u64,
    tx: Recent,
    rx: Recent,
    hold: SoundHold,
}

impl LiveSignals {
    fn read(&mut self, now: Instant) -> MuteInputs {
        let in_use = devices::in_use();
        // With Play off, or playing elsewhere, muting this output silences none of it.
        let playing_here = !AUDIO_RX_MUTED.load(Ordering::Relaxed)
            && AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed) != 0
            && (in_use.play == in_use.output || (in_use.play.is_some() && in_use.play == in_use.also_mute));
        let survives = AUDIO_CAPTURE_SURVIVES_MUTE.load(Ordering::Relaxed);
        let inputs = self.apply(
            Readings {
                setting: AUDIO_PLAY_ONLY_ON_OTHER.load(Ordering::Relaxed),
                output_chosen: devices::choice().output != devices::NONE,
                capture_running: AUDIO_OUTPUT_CAPTURE_LIVE.load(Ordering::Relaxed),
                capture_survives_mute: survives,
                capture_proven_under_mute: survives && cfg!(target_os = "windows"),
                sound_packets: AUDIO_TX_SOUND_PACKETS.load(Ordering::Relaxed),
                peer_online: AUDIO_PEER_ONLINE.load(Ordering::Relaxed),
                peer_listening: peer_listening(now, RECENT_AUDIO),
                peer_hearing: reaching_peer(now, RECENT_AUDIO),
                playing_here,
            },
            now,
        );
        BARRED.store(self.hold.barred(now), Ordering::Relaxed);
        inputs
    }

    fn apply(&mut self, r: Readings, now: Instant) -> MuteInputs {
        let sending_sound = self.tx.note(r.sound_packets != self.sound_packets, now);
        self.sound_packets = r.sound_packets;
        let confirmed = r.capture_running && sending_sound && r.peer_hearing;
        let connected = r.capture_running && r.peer_listening;
        let requested = r.setting && r.output_chosen && r.peer_online && r.capture_survives_mute;
        MuteInputs {
            setting: r.setting,
            send_on: r.output_chosen,
            tx_active: r.capture_running,
            peer_online: r.peer_online,
            reaching_peer: self.hold.tick(requested, confirmed, connected, r.capture_proven_under_mute, now),
            receiving_audio: self.rx.note(r.playing_here, now),
        }
    }
}

fn flag() -> MuteFlag {
    MuteFlag::new(crate::settings::config_dir().join(FLAG_FILE))
}

/// Points the controller at the output being sent, whichever device that is now. It works out the
/// same answer the capture does, so the two can't end up on different devices.
struct ChosenOutput<B: MuteBackend>(B);

impl<B: MuteBackend> MuteBackend for ChosenOutput<B> {
    fn default_device(&mut self) -> Option<String> {
        devices::in_use().output.or_else(|| self.0.default_device())
    }

    fn targets(&mut self) -> Option<Vec<String>> {
        let mut targets = vec![self.default_device()?];
        targets.extend(devices::in_use().also_mute.filter(|device| !targets.contains(device)));
        Some(targets)
    }

    fn is_muted(&mut self, device: &str) -> Option<bool> {
        self.0.is_muted(device)
    }

    fn set_muted(&mut self, device: &str, muted: bool) -> bool {
        self.0.set_muted(device, muted)
    }
}

/// Starts the polling thread. Only the service may call this, once it holds its port: a second
/// copy must not act on the running service's record.
pub fn start() {
    STARTED.store(true, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("mesh-kvm-mute".to_string()).spawn(|| {
        let mut controller = MuteController::new(ChosenOutput(platform::backend()), flag());
        let mut signals = LiveSignals::default();
        let mut reported = false;
        while !STOP.load(Ordering::SeqCst) {
            let now = Instant::now();
            let inputs = signals.read(now);
            if !SLEEPING.load(Ordering::SeqCst) {
                controller.tick(inputs, now);
            } else if !RELEASED_FOR_SLEEP.load(Ordering::SeqCst) {
                controller.release();
                RELEASED_FOR_SLEEP.store(true, Ordering::SeqCst);
            }
            if controller.failing() != reported {
                reported = controller.failing();
                if reported {
                    crate::settings::log_note("Couldn't read or change this device's mute. Retrying quietly.");
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        controller.release();
        STOPPED.store(true, Ordering::SeqCst);
    });
    if let Err(e) = spawned {
        STARTED.store(false, Ordering::SeqCst);
        crate::settings::log_note(&format!("Couldn't start the mute thread: {e}"));
    }
}

fn wait_until(done: &AtomicBool, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Stops the thread and gives the speakers back. Safe to call more than once, or before `start`.
/// It gives up after a while so that a stuck sound server can't hold up an exit; the record then
/// clears the mute at the next start.
pub fn shutdown() {
    if STARTED.load(Ordering::SeqCst) {
        STOP.store(true, Ordering::SeqCst);
        wait_until(&STOPPED, SHUTDOWN_WAIT);
    }
}

/// Gives the speakers back before the machine sleeps, as the link will be gone when it wakes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn before_sleep() {
    if STARTED.load(Ordering::SeqCst) {
        RELEASED_FOR_SLEEP.store(false, Ordering::SeqCst);
        SLEEPING.store(true, Ordering::SeqCst);
        wait_until(&RELEASED_FOR_SLEEP, SLEEP_WAIT);
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn after_sleep() {
    SLEEPING.store(false, Ordering::SeqCst);
}

/// Tells the tray when the service has been silent for long enough to clean up after it.
#[derive(Default)]
struct ServiceWatch {
    silent_since: Option<Instant>,
}

impl ServiceWatch {
    /// True once per stretch of silence, so a device that can't be restored yet is retried.
    fn should_recover(&mut self, answered: bool, now: Instant) -> bool {
        if answered {
            self.silent_since = None;
            return false;
        }
        let since = *self.silent_since.get_or_insert(now);
        let due = now.duration_since(since) >= SERVICE_SILENCE;
        if due {
            self.silent_since = Some(now);
        }
        due
    }
}

/// Runs in the tray, which outlives a crashed or killed service: otherwise the speakers would
/// stay silent until the service next starts, which on the laptop means the next login.
pub fn start_service_watch() {
    let spawned = std::thread::Builder::new().name("mesh-kvm-mute-watch".to_string()).spawn(|| {
        let mut watch = ServiceWatch::default();
        loop {
            let answered = crate::webui::http_call("127.0.0.1", "GET", "/api/local", "").is_ok();
            if watch.should_recover(answered, Instant::now()) && !flag().read().is_empty() {
                MuteController::new(platform::backend(), flag()).release();
            }
            std::thread::sleep(WATCH_INTERVAL);
        }
    });
    if let Err(e) = spawned {
        crate::settings::log_note(&format!("Couldn't start the mute watch: {e}"));
    }
}

#[cfg(target_os = "linux")]
pub mod platform {
    use super::MuteBackend;
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
    const WPCTL_DEADLINE: Duration = Duration::from_secs(2);

    /// Names a sink by `node.name`, as its numeric ID changes when it comes back. A sink's monitor
    /// keeps delivering audio while the sink is muted, so sharing carries on.
    pub struct Wpctl;

    pub fn backend() -> Wpctl {
        Wpctl
    }

    /// Runs a command, killing it at the deadline: a stuck sound server or desktop must not stop
    /// the thread calling this. Its output is read as it comes, so a long listing can't fill the
    /// pipe and stall it, and something else holding the pipe open can't stall this.
    pub fn run_with_deadline(mut command: Command, deadline: Duration) -> Option<Output> {
        let mut child =
            command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let (done, read) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut text);
            let _ = done.send(text);
        });
        let end = Instant::now() + deadline;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        let left = end.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
        let stdout = read.recv_timeout(left).ok()?;
        Some(Output { status, stdout, stderr: Vec::new() })
    }

    fn wpctl(args: &[&str]) -> Option<String> {
        let mut command = Command::new("wpctl");
        command.args(args);
        let output = run_with_deadline(command, WPCTL_DEADLINE)?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `wpctl inspect`'s `node.name`.
    pub(super) fn parse_node_name(inspect: &str) -> Option<String> {
        inspect.lines().find_map(|line| {
            let value = line.trim_start_matches([' ', '*']).strip_prefix("node.name = ")?;
            Some(value.trim().trim_matches('"').to_string()).filter(|name| !name.is_empty())
        })
    }

    /// The numeric ID of a sink in `wpctl status --name`, from a line such as
    /// ` │  *   57. alsa_output.pci-0000 [vol: 1.00]`.
    pub(super) fn parse_sink_id(status: &str, name: &str) -> Option<String> {
        let mut lines = status.lines().skip_while(|line| !line.trim_end().ends_with("Sinks:"));
        lines.next()?;
        lines.take_while(|line| !line.contains('─')).find_map(|line| {
            let mut words = line.split_whitespace().filter(|word| !matches!(*word, "│" | "*"));
            let id = words.next()?.strip_suffix('.')?;
            (words.next()? == name && id.parse::<u32>().is_ok()).then(|| id.to_string())
        })
    }

    /// `wpctl get-volume` says a node is missing with a zero exit, so the volume must be there.
    pub(super) fn parse_muted(get_volume: &str) -> Option<bool> {
        get_volume.contains("Volume:").then(|| get_volume.contains("[MUTED]"))
    }

    fn sink_id(name: &str) -> Option<String> {
        parse_sink_id(&wpctl(&["status", "--name"])?, name)
    }

    impl MuteBackend for Wpctl {
        fn default_device(&mut self) -> Option<String> {
            parse_node_name(&wpctl(&["inspect", DEFAULT_SINK])?)
        }

        fn is_muted(&mut self, device: &str) -> Option<bool> {
            parse_muted(&wpctl(&["get-volume", &sink_id(device)?])?)
        }

        fn set_muted(&mut self, device: &str, muted: bool) -> bool {
            sink_id(device).is_some_and(|id| {
                wpctl(&["set-mute", &id, if muted { "1" } else { "0" }]).is_some()
            })
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::MuteBackend;
    use windows::core::{Result, PCWSTR};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    /// Names an endpoint by its `IMMDevice` ID.
    pub struct EndpointMute;

    /// Runs on the thread that will use it, as that is the one that needs COM.
    pub fn backend() -> EndpointMute {
        // A failure here shows up as a failure of the first call below.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        EndpointMute
    }

    fn devices() -> Result<IMMDeviceEnumerator> {
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
    }

    fn default_id() -> Result<String> {
        unsafe {
            let id = devices()?.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?;
            let text = id.to_string();
            CoTaskMemFree(Some(id.0 as *const _));
            Ok(text?)
        }
    }

    fn endpoint(device: &str) -> Result<IAudioEndpointVolume> {
        let id: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { devices()?.GetDevice(PCWSTR(id.as_ptr()))?.Activate(CLSCTX_ALL, None) }
    }

    impl MuteBackend for EndpointMute {
        fn default_device(&mut self) -> Option<String> {
            default_id().ok()
        }

        fn is_muted(&mut self, device: &str) -> Option<bool> {
            let muted = endpoint(device).and_then(|volume| unsafe { volume.GetMute() });
            muted.ok().map(|muted| muted.as_bool())
        }

        fn set_muted(&mut self, device: &str, muted: bool) -> bool {
            endpoint(device)
                .and_then(|volume| unsafe { volume.SetMute(muted, std::ptr::null()) })
                .is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SPEAKERS: &str = "speakers";
    const HEADSET: &str = "headset";

    /// Any number of devices, one of them the default.
    struct Outputs {
        default: String,
        muted: HashMap<String, bool>,
        absent: Vec<String>,
        broken: bool,
        /// `set_muted` reports failure, whether or not the change happened.
        mute_reports_failure: bool,
        mute_takes_effect: bool,
        calls: u32,
        writes: Vec<(String, bool)>,
    }

    impl Default for Outputs {
        fn default() -> Self {
            Self {
                default: SPEAKERS.into(),
                muted: HashMap::new(),
                absent: Vec::new(),
                broken: false,
                mute_reports_failure: false,
                mute_takes_effect: true,
                calls: 0,
                writes: Vec::new(),
            }
        }
    }

    impl Outputs {
        fn with_muted(device: &str) -> Self {
            let mut outputs = Self::default();
            outputs.muted.insert(device.into(), true);
            outputs
        }

        fn is(&self, device: &str) -> bool {
            self.muted.get(device).copied().unwrap_or(false)
        }

        fn reachable(&self, device: &str) -> bool {
            !self.broken && !self.absent.iter().any(|d| d == device)
        }
    }

    impl MuteBackend for &mut Outputs {
        fn default_device(&mut self) -> Option<String> {
            self.calls += 1;
            (!self.broken).then(|| self.default.clone())
        }

        fn is_muted(&mut self, device: &str) -> Option<bool> {
            self.calls += 1;
            self.reachable(device).then(|| self.is(device))
        }

        fn set_muted(&mut self, device: &str, muted: bool) -> bool {
            self.calls += 1;
            if !self.reachable(device) {
                return false;
            }
            if !muted || self.mute_takes_effect {
                self.writes.push((device.into(), muted));
                self.muted.insert(device.into(), muted);
            }
            !muted || (self.mute_takes_effect && !self.mute_reports_failure)
        }
    }

    /// A flag file in a fresh directory, away from the real one.
    fn flag(name: &str) -> MuteFlag {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-mute-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MuteFlag::new(dir.join("nested").join("mute-state.json"))
    }

    const SENDING: MuteInputs = MuteInputs {
        setting: true,
        send_on: true,
        tx_active: true,
        peer_online: true,
        reaching_peer: true,
        receiving_audio: false,
    };
    const STOPPED: MuteInputs = MuteInputs { send_on: false, ..SENDING };
    const LINK_DOWN: MuteInputs = MuteInputs { peer_online: false, ..SENDING };
    const STEP: Duration = WATCH_INTERVAL;

    #[test]
    fn mutes_only_while_every_condition_holds() {
        for bits in 0..32u8 {
            let [a, b, c, d, e] = [1, 2, 4, 8, 16].map(|bit| bits & bit != 0);
            assert_eq!(should_mute(a, b, c, d, e), bits == 15, "{a} {b} {c} {d} {e}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wpctl_output_is_understood() {
        use platform::{parse_muted, parse_node_name, parse_sink_id};
        assert_eq!(parse_muted("Volume: 0.40 [MUTED]\n"), Some(true));
        assert_eq!(parse_muted("Volume: 1.00\n"), Some(false));
        assert_eq!(parse_muted("Node '99999' not found\n"), None, "wpctl exits 0 for this");

        let inspect = "id 57, type PipeWire:Interface:Node\n    media.class = \"Audio/Sink\"\n  * node.description = \"USB\"\n  * node.name = \"alsa_output.usb.Speaker__sink\"\n    node.nick = \"x\"\n";
        assert_eq!(parse_node_name(inspect).as_deref(), Some("alsa_output.usb.Speaker__sink"));
        assert_eq!(parse_node_name("Object '@DEFAULT_AUDIO_SINK@' not found\n"), None);

        let status = "Audio\n ├─ Devices:\n │      48. alsa_output.other         [alsa]\n │  \n ├─ Sinks:\n │      56. alsa_output.usb.Headphones__sink [vol: 1.00]\n │  *   57. alsa_output.usb.Speaker__sink [vol: 1.00 MUTED]\n │  \n ├─ Sources:\n │      58. alsa_output.source [vol: 1.00]\n";
        assert_eq!(parse_sink_id(status, "alsa_output.usb.Speaker__sink").as_deref(), Some("57"));
        assert_eq!(parse_sink_id(status, "alsa_output.usb.Headphones__sink").as_deref(), Some("56"));
        assert_eq!(parse_sink_id(status, "alsa_output.source"), None, "not a sink");
        assert_eq!(parse_sink_id(status, "alsa_output.other"), None, "not a sink");
        assert_eq!(parse_sink_id("nothing here\n", "x"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stuck_command_is_killed_at_the_deadline() {
        use std::process::Command;
        let mut sleeper = Command::new("sleep");
        sleeper.arg("5");
        let started = Instant::now();
        assert!(platform::run_with_deadline(sleeper, Duration::from_millis(50)).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));

        let mut echo = Command::new("echo");
        echo.arg("hello");
        let output = platform::run_with_deadline(echo, Duration::from_secs(2)).unwrap();
        assert_eq!(output.stdout, b"hello\n");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_listing_longer_than_the_pipe_still_comes_back_whole() {
        use std::process::Command;
        let mut long = Command::new("head");
        long.args(["-c", "300000", "/dev/zero"]);
        let output = platform::run_with_deadline(long, Duration::from_secs(5)).unwrap();
        assert_eq!(output.stdout.len(), 300_000);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn something_left_holding_the_pipe_does_not_stall_the_caller() {
        use std::process::Command;
        let mut leaves_a_holder = Command::new("sh");
        leaves_a_holder.args(["-c", "sleep 3 & exit 0"]);
        let started = Instant::now();
        assert!(platform::run_with_deadline(leaves_a_holder, Duration::from_millis(150)).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn mutes_when_sending_starts_and_restores_when_it_stops() {
        let (mut outputs, flag) = (Outputs::default(), flag("start-stop"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(STOPPED, t);
        assert!(!path.exists());
        c.tick(SENDING, t + STEP);
        assert!(std::fs::read_to_string(&path).unwrap().contains(SPEAKERS), "the record names it");
        c.tick(SENDING, t + STEP * 2);
        c.tick(STOPPED, t + STEP * 3);
        assert!(!path.exists());
        c.tick(STOPPED, t + STEP * 4);
        drop(c);
        assert_eq!(outputs.writes, vec![(SPEAKERS.into(), true), (SPEAKERS.into(), false)]);
    }

    #[test]
    fn an_idle_controller_leaves_the_backend_alone() {
        let (mut outputs, flag) = (Outputs::default(), flag("idle"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        for i in 0..10 {
            c.tick(STOPPED, t + STEP * i);
        }
        drop(c);
        assert_eq!(outputs.calls, 0);
    }

    #[test]
    fn the_fast_loop_only_looks_at_the_speakers_once_a_second() {
        let (mut outputs, flag) = (Outputs::default(), flag("cadence"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        let after_muting = c.backend.calls;
        for i in 1..4 {
            c.tick(SENDING, t + POLL_INTERVAL * i);
        }
        assert_eq!(c.backend.calls, after_muting);
        c.tick(SENDING, t + WATCH_INTERVAL);
        assert!(c.backend.calls > after_muting);
    }

    #[test]
    fn stopping_is_acted_on_at_once_not_at_the_next_look() {
        let (mut outputs, flag) = (Outputs::default(), flag("prompt"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.tick(LINK_DOWN, t + POLL_INTERVAL);
        assert!(!c.backend.is(SPEAKERS));
        c.tick(SENDING, t + POLL_INTERVAL * 2);
        assert!(c.backend.is(SPEAKERS));
    }

    #[test]
    fn losing_the_link_unmutes_and_its_return_mutes_again() {
        let (mut outputs, flag) = (Outputs::default(), flag("link"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.tick(LINK_DOWN, t + STEP);
        c.tick(LINK_DOWN, t + STEP * 2);
        c.tick(SENDING, t + STEP * 3);
        drop(c);
        let muted: Vec<bool> = outputs.writes.iter().map(|w| w.1).collect();
        assert_eq!(muted, vec![true, false, true]);
    }

    #[test]
    fn anything_that_ends_the_need_unmutes() {
        for off in [
            MuteInputs { setting: false, ..SENDING },
            MuteInputs { reaching_peer: false, ..SENDING },
            MuteInputs { tx_active: false, ..SENDING },
            MuteInputs { receiving_audio: true, ..SENDING },
        ] {
            let (mut outputs, flag) = (Outputs::default(), flag("off"));
            let mut c = MuteController::new(&mut outputs, flag);
            let t = Instant::now();
            c.tick(SENDING, t);
            assert!(c.backend.is(SPEAKERS));
            c.tick(off, t + STEP);
            assert!(!c.backend.is(SPEAKERS), "{off:?}");
        }
    }

    #[test]
    fn sound_the_other_device_isn_t_playing_is_no_reason_to_mute() {
        let (mut outputs, flag) = (Outputs::default(), flag("not-reaching"));
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(MuteInputs { reaching_peer: false, ..SENDING }, Instant::now());
        drop(c);
        assert!(outputs.writes.is_empty());
    }

    #[test]
    fn a_silenced_other_device_gives_the_speakers_back_and_returning_mutes_again() {
        let (mut outputs, flag) = (Outputs::default(), flag("reach-back"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.tick(MuteInputs { reaching_peer: false, ..SENDING }, t + STEP);
        assert!(!c.backend.is(SPEAKERS));
        c.tick(SENDING, t + STEP * 2);
        assert!(c.backend.is(SPEAKERS));
    }

    #[test]
    fn the_chosen_output_is_what_gets_muted_not_the_default() {
        // `ChosenOutput` reads this from the audio engine; in a test nothing is in use yet.
        let mut outputs = Outputs::default();
        let mut chosen = ChosenOutput(&mut outputs);
        devices::set_in_use(devices::InUse { output: Some(HEADSET.into()), ..Default::default() });
        assert_eq!(chosen.default_device().as_deref(), Some(HEADSET));
        devices::set_in_use(devices::InUse::default());
        assert_eq!(chosen.default_device().as_deref(), Some(SPEAKERS), "nothing chosen: the old default");
    }

    const QUIET_LINK: Readings = Readings {
        setting: true,
        output_chosen: true,
        capture_running: true,
        capture_survives_mute: true,
        capture_proven_under_mute: false,
        sound_packets: 0,
        peer_online: true,
        peer_listening: true,
        peer_hearing: false,
        playing_here: false,
    };

    /// A second of live signals, feeding the controller as the service does.
    fn run(c: &mut MuteController<&mut Outputs>, signals: &mut LiveSignals, r: Readings, at: Instant) {
        c.tick(signals.apply(r, at), at);
    }

    /// Sound flowing, as the two ends report it: packets going out and heard from the far side.
    fn sounding(packets: u64) -> Readings {
        Readings { sound_packets: packets, peer_hearing: true, ..QUIET_LINK }
    }

    #[test]
    fn a_capture_that_never_delivers_does_not_mute() {
        // Word that the stream is alive goes out, and the far side has an output open for it, but
        // nothing is being heard.
        let (mut outputs, flag) = (Outputs::default(), flag("no-capture"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..60 {
            run(&mut c, &mut signals, QUIET_LINK, t + STEP * second);
        }
        drop(c);
        assert!(outputs.writes.is_empty());
    }

    #[test]
    fn sound_the_far_side_doesn_t_confirm_does_not_mute() {
        let (mut outputs, flag) = (Outputs::default(), flag("unconfirmed"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..10 {
            let r = Readings { sound_packets: second + 1, ..QUIET_LINK };
            run(&mut c, &mut signals, r, t + STEP * second as u32);
        }
        drop(c);
        assert!(outputs.writes.is_empty());
    }

    #[test]
    fn confirmed_sound_mutes_and_a_pause_between_tracks_does_not_flap() {
        let (mut outputs, flag) = (Outputs::default(), flag("no-flap"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..10 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
        }
        assert!(c.backend.is(SPEAKERS));
        // Twenty seconds with nothing playing, and on with the next track.
        for second in 10..30 {
            run(&mut c, &mut signals, Readings { sound_packets: 10, ..QUIET_LINK }, t + STEP * second);
        }
        for second in 30..40 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
        }
        drop(c);
        assert_eq!(outputs.writes.len(), 1, "muted once, and left: {:?}", outputs.writes);
    }

    #[test]
    fn a_long_silence_gives_the_output_back_once_and_doesn_t_flap() {
        let (mut outputs, flag) = (Outputs::default(), flag("long-silence"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..10 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
        }
        for second in 10..300 {
            run(&mut c, &mut signals, Readings { sound_packets: 10, ..QUIET_LINK }, t + STEP * second);
        }
        drop(c);
        assert_eq!(outputs.writes.len(), 2, "{:?}", outputs.writes);
        assert!(!outputs.is(SPEAKERS));
    }

    #[test]
    fn a_mute_that_silences_the_capture_can_t_lock_itself_on() {
        // A Windows output that gives silence once it's muted. Nobody hears a thing, so this must
        // let go, and stop trying for a while, and try again later.
        let (mut outputs, flag) = (Outputs::default(), flag("self-silencing"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        let (mut packets, mut longest_muted, mut muted_for) = (0, 0, 0);
        let mut mutes = Vec::new();
        for second in 0..600u32 {
            let muted = c.backend.is(SPEAKERS);
            let r = if muted {
                // Sound was going out a moment ago and the far side's report lags a little.
                Readings { sound_packets: packets, ..QUIET_LINK }
            } else {
                packets += 1;
                sounding(packets)
            };
            run(&mut c, &mut signals, r, t + STEP * second);
            muted_for = if c.backend.is(SPEAKERS) { muted_for + 1 } else { 0 };
            longest_muted = longest_muted.max(muted_for);
            if !muted && c.backend.is(SPEAKERS) {
                mutes.push(second);
            }
        }
        assert!(longest_muted <= 6, "muted for {longest_muted} seconds with nobody hearing");
        assert!(mutes.len() >= 3, "it does try again: {mutes:?}");
        assert!(mutes.len() <= 12, "but it backs off: {mutes:?}");
        assert!(signals.hold.next_bar.is_some());
    }

    #[test]
    fn a_healthy_capture_keeps_its_mute_for_as_long_as_the_sound_lasts() {
        let (mut outputs, flag) = (Outputs::default(), flag("healthy"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..600 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
            assert!(c.backend.is(SPEAKERS) && !signals.hold.barred(t + STEP * second as u32), "{second}");
        }
        drop(c);
        assert_eq!(outputs.writes.len(), 1);
    }

    #[test]
    fn short_sounds_now_and_then_are_not_taken_for_a_mute_stopping_the_sound() {
        // A notification every minute: each one mutes and lets go, which is fine.
        let (mut outputs, flag) = (Outputs::default(), flag("dings"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        let mut packets = 0;
        for second in 0..600u32 {
            if second % 60 == 0 {
                packets += 1;
            }
            let hearing = second % 60 < 2;
            let r = Readings { sound_packets: packets, peer_hearing: hearing, ..QUIET_LINK };
            let now = t + STEP * second;
            run(&mut c, &mut signals, r, now);
            assert!(!signals.hold.barred(now), "{second}");
        }
    }

    #[test]
    fn short_sounds_close_together_never_bar_a_capture_proven_under_a_mute() {
        // Windows exclusion: sound stopping is just sound stopping, however often it happens.
        let (mut outputs, flag) = (Outputs::default(), flag("proven"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        let (mut packets, mut mutes, mut was_muted) = (0, 0, false);
        for second in 0..120u32 {
            let hearing = second % 8 < 2;
            packets += u64::from(hearing);
            let r = Readings {
                sound_packets: packets,
                peer_hearing: hearing,
                capture_proven_under_mute: true,
                ..QUIET_LINK
            };
            let now = t + STEP * second;
            run(&mut c, &mut signals, r, now);
            assert!(!signals.hold.barred(now), "{second}");
            let muted = c.backend.is(SPEAKERS);
            mutes += u32::from(muted && !was_muted);
            was_muted = muted;
        }
        assert!(mutes >= 10, "it goes on muting for each one: {mutes}");
    }

    #[test]
    fn the_same_short_sounds_do_bar_a_capture_not_proven() {
        let (mut outputs, flag) = (Outputs::default(), flag("not-proven"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        let (mut packets, mut barred) = (0, false);
        for second in 0..60u32 {
            let hearing = second % 8 < 2;
            packets += u64::from(hearing);
            let now = t + STEP * second;
            let r = Readings { sound_packets: packets, peer_hearing: hearing, ..QUIET_LINK };
            run(&mut c, &mut signals, r, now);
            barred |= signals.hold.barred(now);
        }
        assert!(barred);
    }

    #[test]
    fn the_far_side_going_away_gives_the_output_back_at_once_even_in_a_pause() {
        let (mut outputs, flag) = (Outputs::default(), flag("gone-in-pause"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..10 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
        }
        run(&mut c, &mut signals, Readings { sound_packets: 10, ..QUIET_LINK }, t + STEP * 10);
        assert!(c.backend.is(SPEAKERS));
        let gone = Readings { sound_packets: 10, peer_listening: false, ..QUIET_LINK };
        run(&mut c, &mut signals, gone, t + STEP * 11);
        assert!(!c.backend.is(SPEAKERS));
    }

    #[test]
    fn a_capture_the_mute_would_silence_is_never_muted_for() {
        // Ordinary loopback on Windows: the sound is confirmed, and muting would cut it off.
        let (mut outputs, flag) = (Outputs::default(), flag("no-exclusion"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..30 {
            let r = Readings { capture_survives_mute: false, ..sounding(second + 1) };
            run(&mut c, &mut signals, r, t + STEP * second as u32);
        }
        drop(c);
        assert!(outputs.writes.is_empty(), "{:?}", outputs.writes);
    }

    #[test]
    fn exclusion_failing_gives_the_output_back_at_once() {
        let (mut outputs, flag) = (Outputs::default(), flag("exclusion-fails"));
        let mut c = MuteController::new(&mut outputs, flag);
        let mut signals = LiveSignals::default();
        let t = Instant::now();
        for second in 0..10 {
            run(&mut c, &mut signals, sounding(second + 1), t + STEP * second as u32);
        }
        assert!(c.backend.is(SPEAKERS));
        let fell_back = Readings { capture_survives_mute: false, ..sounding(11) };
        run(&mut c, &mut signals, fell_back, t + STEP * 10);
        assert!(!c.backend.is(SPEAKERS), "the capture is ordinary now, and a mute would silence it");
    }

    /// Two outputs to mute: what's chosen, and the one Windows is really playing on.
    struct TwoOutputs<'a>(&'a mut Outputs);

    impl MuteBackend for TwoOutputs<'_> {
        fn default_device(&mut self) -> Option<String> {
            (&mut *self.0).default_device()
        }

        fn targets(&mut self) -> Option<Vec<String>> {
            let chosen = (&mut *self.0).default_device()?;
            Some(vec![chosen, HEADSET.to_string()])
        }

        fn is_muted(&mut self, device: &str) -> Option<bool> {
            (&mut *self.0).is_muted(device)
        }

        fn set_muted(&mut self, device: &str, muted: bool) -> bool {
            (&mut *self.0).set_muted(device, muted)
        }
    }

    #[test]
    fn both_outputs_are_muted_on_one_record_and_both_come_back() {
        let (mut outputs, flag) = (Outputs::default(), flag("two-outputs"));
        let path = flag.path.clone();
        let mut c = MuteController::new(TwoOutputs(&mut outputs), flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.tick(SENDING, t + STEP);
        assert!(c.backend.0.is(SPEAKERS) && c.backend.0.is(HEADSET));
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(SPEAKERS) && record.contains(HEADSET), "{record}");
        c.tick(STOPPED, t + STEP * 2);
        assert!(!c.backend.0.is(SPEAKERS) && !c.backend.0.is(HEADSET));
        assert!(!path.exists());
        drop(c);
        assert_eq!(outputs.writes.len(), 4, "muted once each, and left alone: {:?}", outputs.writes);
    }

    #[test]
    fn a_crash_with_two_outputs_muted_restores_both_from_the_record() {
        let (mut outputs, flag) = (Outputs::default(), flag("two-crash"));
        let path = flag.path.clone();
        let mut c = MuteController::new(TwoOutputs(&mut outputs), flag);
        c.tick(SENDING, Instant::now());
        drop(c);
        let mut c = MuteController::new(TwoOutputs(&mut outputs), MuteFlag::new(path.clone()));
        c.release();
        drop(c);
        assert!(!outputs.is(SPEAKERS) && !outputs.is(HEADSET));
        assert!(!path.exists());
    }

    #[test]
    fn a_manual_unmute_of_one_output_leaves_the_other_muted() {
        let (mut outputs, flag) = (Outputs::default(), flag("two-manual"));
        let mut c = MuteController::new(TwoOutputs(&mut outputs), flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.0.muted.insert(HEADSET.into(), false);
        c.tick(SENDING, t + STEP);
        c.tick(SENDING, t + STEP * 2);
        assert!(c.backend.0.is(SPEAKERS));
        assert!(!c.backend.0.is(HEADSET), "not fought");
    }

    #[test]
    fn an_output_that_stops_being_a_target_comes_back_and_the_other_stays_muted() {
        let (mut outputs, flag) = (Outputs::default(), flag("two-then-one"));
        let path = flag.path.clone();
        let mut c = MuteController::new(TwoOutputs(&mut outputs), flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        drop(c);
        let mut c = MuteController::new(&mut outputs, MuteFlag::new(path));
        c.tick(SENDING, t + STEP);
        assert!(c.backend.is(SPEAKERS), "still the target");
        assert!(!c.backend.is(HEADSET), "no longer one");
    }

    #[test]
    fn the_chosen_output_and_the_windows_default_are_both_targets_once_each() {
        let mut outputs = Outputs::default();
        let mut chosen = ChosenOutput(&mut outputs);
        let two = devices::InUse { output: Some(HEADSET.into()), also_mute: Some(SPEAKERS.into()), ..Default::default() };
        devices::set_in_use(two);
        assert_eq!(chosen.targets().unwrap(), [HEADSET, SPEAKERS]);
        devices::set_in_use(devices::InUse::default());
    }

    #[test]
    fn a_new_request_forgets_any_bar() {
        let mut hold = SoundHold::default();
        let t = Instant::now();
        hold.barred_until = Some(t + LONGEST_BAR);
        assert!(!hold.tick(true, true, true, false, t));
        assert!(hold.barred(t));
        hold.tick(false, false, false, false, t);
        assert!(hold.tick(true, true, true, false, t));
    }

    #[test]
    fn speakers_the_user_had_muted_stay_muted() {
        let mut outputs = Outputs::with_muted(SPEAKERS);
        let flag = flag("already");
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        assert!(!path.exists(), "mesh-kvm didn't mute them");
        c.tick(LINK_DOWN, t + STEP);
        c.tick(STOPPED, t + STEP * 2);
        c.release();
        drop(c);
        assert!(outputs.is(SPEAKERS));
        assert!(outputs.writes.is_empty());
    }

    #[test]
    fn a_manual_unmute_is_respected_until_sending_next_starts() {
        let (mut outputs, flag) = (Outputs::default(), flag("manual"));
        let path = flag.path.clone();
        let t = Instant::now();
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(SENDING, t);
        c.backend.muted.insert(SPEAKERS.into(), false);
        c.tick(SENDING, t + STEP);
        assert!(!path.exists(), "the speakers are the user's again");
        c.tick(SENDING, t + STEP * 2);
        assert!(!c.backend.is(SPEAKERS), "not fought");
        // Neither a dropped link nor a quiet moment is a new start.
        c.tick(LINK_DOWN, t + STEP * 3);
        c.tick(SENDING, t + STEP * 4);
        c.tick(MuteInputs { tx_active: false, ..SENDING }, t + STEP * 5);
        c.tick(SENDING, t + STEP * 6);
        assert!(!c.backend.is(SPEAKERS), "not fought after a blip either");
        c.tick(STOPPED, t + STEP * 7);
        c.tick(SENDING, t + STEP * 8);
        assert!(c.backend.is(SPEAKERS), "sending started again");
    }

    #[test]
    fn a_user_mute_after_a_manual_unmute_is_not_undone() {
        let (mut outputs, flag) = (Outputs::default(), flag("remute"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.muted.insert(SPEAKERS.into(), false);
        c.tick(SENDING, t + STEP);
        c.backend.muted.insert(SPEAKERS.into(), true);
        c.tick(STOPPED, t + STEP * 2);
        c.release();
        assert!(c.backend.is(SPEAKERS));
    }

    #[test]
    fn a_changed_default_restores_the_old_device_then_mutes_the_new_one() {
        let (mut outputs, flag) = (Outputs::default(), flag("moved"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.default = HEADSET.into();
        c.tick(SENDING, t + STEP);
        assert!(!c.backend.is(SPEAKERS), "not taken for a manual unmute");
        assert!(c.backend.is(HEADSET));
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(HEADSET) && !record.contains(SPEAKERS), "{record}");
        c.tick(STOPPED, t + STEP * 2);
        assert!(!c.backend.is(HEADSET));
        assert!(!path.exists());
        drop(c);
        let order: Vec<(&str, bool)> = outputs.writes.iter().map(|(d, m)| (d.as_str(), *m)).collect();
        assert_eq!(
            order,
            vec![(SPEAKERS, true), (SPEAKERS, false), (HEADSET, true), (HEADSET, false)]
        );
    }

    #[test]
    fn stopping_restores_the_muted_device_not_the_new_default() {
        let mut outputs = Outputs::with_muted(HEADSET);
        let flag = flag("restore-right-one");
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        // The default moves to a headset the user muted, and sending stops before the next look.
        c.backend.default = HEADSET.into();
        c.tick(STOPPED, t + POLL_INTERVAL);
        drop(c);
        assert!(!outputs.is(SPEAKERS));
        assert!(outputs.is(HEADSET), "the user's mute");
    }

    #[test]
    fn a_new_default_the_user_had_muted_is_left_alone() {
        let mut outputs = Outputs::with_muted(HEADSET);
        let flag = flag("new-already");
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.default = HEADSET.into();
        c.tick(SENDING, t + STEP);
        c.tick(STOPPED, t + STEP * 2);
        drop(c);
        assert!(!outputs.is(SPEAKERS));
        assert!(outputs.is(HEADSET));
    }

    #[test]
    fn hands_off_covers_one_device_so_a_new_default_is_still_muted() {
        let (mut outputs, flag) = (Outputs::default(), flag("hands-off-device"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.muted.insert(SPEAKERS.into(), false);
        c.tick(SENDING, t + STEP);
        c.backend.default = HEADSET.into();
        c.tick(SENDING, t + STEP * 2);
        assert!(c.backend.is(HEADSET));
        c.backend.default = SPEAKERS.into();
        c.tick(SENDING, t + STEP * 3);
        assert!(!c.backend.is(SPEAKERS), "still the user's");
        assert!(!c.backend.is(HEADSET));
    }

    #[test]
    fn an_unplugged_device_stays_on_record_until_it_is_seen_again() {
        let (mut outputs, flag) = (Outputs::default(), flag("unplugged"));
        let path = flag.path.clone();
        outputs.default = HEADSET.into();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.absent.push(HEADSET.into());
        c.backend.default = SPEAKERS.into();
        c.tick(SENDING, t + STEP);
        assert!(c.backend.is(SPEAKERS), "the new default is muted meanwhile");
        let record = std::fs::read_to_string(&path).unwrap();
        assert!(record.contains(HEADSET) && record.contains(SPEAKERS), "{record}");

        // Sending stops: the speakers come back at once, the record of the headset stays.
        c.tick(STOPPED, t + STEP * 2);
        assert!(!c.backend.is(SPEAKERS));
        assert!(path.exists());
        c.tick(STOPPED, t + STEP * 2 + RETRY_INTERVAL);
        assert!(path.exists());
        assert!(c.backend.is(HEADSET));

        c.backend.absent.clear();
        c.tick(STOPPED, t + STEP * 2 + RETRY_INTERVAL * 2);
        assert!(!c.backend.is(HEADSET));
        assert!(!path.exists());
        assert!(!c.failing());
    }

    #[test]
    fn an_unplugged_device_does_not_hold_up_the_one_in_use() {
        let (mut outputs, flag) = (Outputs::default(), flag("not-held-up"));
        outputs.default = HEADSET.into();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.absent.push(HEADSET.into());
        c.backend.default = SPEAKERS.into();
        c.tick(SENDING, t + STEP);
        // Inside the unplugged headset's retry gap, the manual unmute is still seen.
        c.backend.muted.insert(SPEAKERS.into(), false);
        c.tick(SENDING, t + STEP * 2);
        c.tick(SENDING, t + STEP * 3);
        assert!(!c.backend.is(SPEAKERS));
    }

    #[test]
    fn a_device_still_gone_at_the_next_start_is_kept_and_found_later() {
        let flag = flag("gone-at-start");
        let path = flag.path.clone();
        assert!(flag.write(&[HEADSET.to_string()]));
        let mut outputs = Outputs::with_muted(HEADSET);
        outputs.absent.push(HEADSET.into());
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(STOPPED, t);
        assert!(path.exists());
        c.backend.absent.clear();
        c.tick(STOPPED, t + RETRY_INTERVAL);
        assert!(!c.backend.is(HEADSET));
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_read_is_not_a_manual_unmute() {
        let (mut outputs, flag) = (Outputs::default(), flag("read-fail"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.broken = true;
        c.tick(SENDING, t + STEP);
        assert!(path.exists());
        c.backend.broken = false;
        c.tick(STOPPED, t + STEP * 2);
        assert!(!c.backend.is(SPEAKERS), "still ours to restore");
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_backend_is_retried_every_five_seconds() {
        let mut outputs = Outputs { broken: true, ..Default::default() };
        let flag = flag("retry");
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        let mut at = t;
        while at < t + RETRY_INTERVAL {
            c.tick(SENDING, at);
            at += POLL_INTERVAL;
        }
        assert_eq!(c.backend.calls, 1);
        assert!(c.failing());
        c.tick(SENDING, t + RETRY_INTERVAL);
        assert_eq!(c.backend.calls, 2);
        c.backend.broken = false;
        c.tick(SENDING, t + RETRY_INTERVAL * 2 - Duration::from_millis(1));
        assert_eq!(c.backend.calls, 2);
        c.tick(SENDING, t + RETRY_INTERVAL * 2);
        assert!(c.backend.is(SPEAKERS));
        assert!(!c.failing());
    }

    #[test]
    fn a_failed_unmute_keeps_trying_and_keeps_the_record() {
        let (mut outputs, flag) = (Outputs::default(), flag("unmute-fail"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.broken = true;
        c.tick(LINK_DOWN, t + STEP);
        assert!(path.exists());
        assert!(c.failing());
        c.backend.broken = false;
        c.tick(LINK_DOWN, t + STEP * 2);
        assert!(c.backend.is(SPEAKERS), "waits out the retry gap");
        c.tick(LINK_DOWN, t + STEP + RETRY_INTERVAL);
        assert!(!c.backend.is(SPEAKERS));
        assert!(!path.exists());
    }

    #[test]
    fn a_retry_gap_does_not_delay_the_unmute_when_muting_stops_being_wanted() {
        let (mut outputs, flag) = (Outputs::default(), flag("no-delay"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.backend.broken = true;
        c.tick(SENDING, t + STEP);
        c.backend.broken = false;
        c.tick(LINK_DOWN, t + STEP + POLL_INTERVAL);
        assert!(!c.backend.is(SPEAKERS));
    }

    #[test]
    fn a_failed_mute_that_did_nothing_leaves_no_record() {
        let mut outputs = Outputs { mute_reports_failure: true, mute_takes_effect: false, ..Default::default() };
        let flag = flag("mute-fail");
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(SENDING, Instant::now());
        assert!(!path.exists());
        assert!(c.failing());
    }

    #[test]
    fn a_failed_mute_that_took_effect_stays_ours() {
        let mut outputs = Outputs { mute_reports_failure: true, ..Default::default() };
        let flag = flag("mute-half-fail");
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        assert!(c.failing());
        assert!(path.exists());
        c.tick(STOPPED, t + STEP);
        assert!(!c.backend.is(SPEAKERS));
        assert!(!path.exists());
    }

    #[test]
    fn no_record_means_no_mute() {
        let (mut outputs, flag) = (Outputs::default(), flag("no-record"));
        // A file where the directory should be: the record can't be written.
        let parent = flag.path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, "").unwrap();
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(SENDING, Instant::now());
        assert!(c.failing());
        drop(c);
        assert!(!outputs.is(SPEAKERS), "a crash could leave it muted with nothing to say so");
    }

    #[test]
    fn a_stale_record_is_cleared_at_start_on_the_device_it_names() {
        let mut outputs = Outputs::with_muted(HEADSET);
        outputs.muted.insert(SPEAKERS.into(), true);
        let flag = flag("stale");
        let path = flag.path.clone();
        assert!(flag.write(&[HEADSET.to_string()]));
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(STOPPED, Instant::now());
        drop(c);
        assert!(!outputs.is(HEADSET));
        assert!(outputs.is(SPEAKERS), "the default wasn't ours");
        assert!(!path.exists());
    }

    #[test]
    fn a_stale_record_survives_until_the_backend_is_ready() {
        // At login the sound server may not be up yet.
        let mut outputs = Outputs { broken: true, ..Outputs::with_muted(SPEAKERS) };
        let flag = flag("stale-early");
        let path = flag.path.clone();
        assert!(flag.write(&[SPEAKERS.to_string()]));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(STOPPED, t);
        assert!(path.exists());
        c.backend.broken = false;
        c.tick(STOPPED, t + RETRY_INTERVAL);
        assert!(!c.backend.is(SPEAKERS));
        assert!(!path.exists());
    }

    #[test]
    fn an_unreadable_record_unmutes_the_default() {
        let flag = flag("corrupt");
        std::fs::create_dir_all(flag.path.parent().unwrap()).unwrap();
        std::fs::write(&flag.path, r#"{"muted_by_mesh":false}"#).unwrap();
        assert!(flag.read().is_empty());
        for text in ["{nope", r#"{"muted_by_mesh":true}"#] {
            std::fs::write(&flag.path, text).unwrap();
            let path = flag.path.clone();
            let mut outputs = Outputs::with_muted(SPEAKERS);
            let mut c = MuteController::new(&mut outputs, MuteFlag::new(path.clone()));
            c.tick(STOPPED, Instant::now());
            assert!(!c.backend.is(SPEAKERS), "{text}");
            assert!(!path.exists());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn release_unmutes_at_once_and_clears_the_record() {
        let (mut outputs, flag) = (Outputs::default(), flag("release"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        // Even inside a retry gap: there may be no later tick.
        c.backend.broken = true;
        c.tick(SENDING, t + STEP);
        c.backend.broken = false;
        c.release();
        drop(c);
        assert!(!outputs.is(SPEAKERS));
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_release_keeps_the_record_for_the_next_start() {
        let (mut outputs, flag) = (Outputs::default(), flag("release-fail"));
        let path = flag.path.clone();
        let mut c = MuteController::new(&mut outputs, flag);
        c.tick(SENDING, Instant::now());
        c.backend.broken = true;
        c.release();
        assert!(path.exists());
    }

    #[test]
    fn muting_resumes_after_a_release_for_sleep() {
        let (mut outputs, flag) = (Outputs::default(), flag("sleep"));
        let mut c = MuteController::new(&mut outputs, flag);
        let t = Instant::now();
        c.tick(SENDING, t);
        c.release();
        assert!(!c.backend.is(SPEAKERS));
        c.tick(SENDING, t + STEP);
        assert!(c.backend.is(SPEAKERS));
    }

    #[test]
    fn recent_means_within_two_seconds() {
        let (mut recent, t) = (Recent::default(), Instant::now());
        assert!(!recent.note(false, t), "nothing yet");
        assert!(recent.note(true, t));
        assert!(recent.note(false, t + RECENT_AUDIO - Duration::from_millis(1)));
        assert!(!recent.note(false, t + RECENT_AUDIO));
    }

    #[test]
    fn the_tray_acts_after_five_silent_seconds_and_then_every_five() {
        let (mut watch, t) = (ServiceWatch::default(), Instant::now());
        assert!(!watch.should_recover(true, t));
        assert!(!watch.should_recover(false, t));
        assert!(!watch.should_recover(false, t + SERVICE_SILENCE - Duration::from_millis(1)));
        assert!(watch.should_recover(false, t + SERVICE_SILENCE));
        assert!(!watch.should_recover(false, t + SERVICE_SILENCE + Duration::from_secs(1)));
        assert!(watch.should_recover(false, t + SERVICE_SILENCE * 2));
    }

    #[test]
    fn an_answer_restarts_the_tray_s_count() {
        let (mut watch, t) = (ServiceWatch::default(), Instant::now());
        watch.should_recover(false, t);
        assert!(!watch.should_recover(true, t + Duration::from_secs(4)));
        assert!(!watch.should_recover(false, t + SERVICE_SILENCE));
    }
}
