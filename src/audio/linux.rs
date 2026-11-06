//! PipeWire, through its command line tools: `pw-record`, `pw-play`, `pw-link` and `pw-dump`.
//!
//! Output sound is not taken from a sink's monitor, which would carry what this device plays and
//! start an echo when both devices send. The capture node is left unconnected, and its inputs are
//! linked to each app playing on the chosen sink, leaving out our own player. Nothing is added to
//! the graph but those links and our own streams, and they go when the capture stops. The sink's
//! volume and mute don't touch such a capture, so a muted sink still sends.

use super::devices::{self, CaptureHealth, Device, InUse, Lists, Resolved};
use super::jitter::RxStream;
use super::mix::{Signal, Source};
use super::{drain_received, finish_received, AUDIO_OUTPUT_CAPTURE_LIVE, AUDIO_PLAYER_LIVE, AUDIO_RX_MUTED, AUDIO_RX_PEAK_PCT, CHANNELS, HUB};
use crate::mute::platform::run_with_deadline;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `--raw` is required: without it pw-play rejects stdin as an unknown file format and exits,
/// and pw-record wraps the stream in a file header.
const PW_FORMAT_ARGS: [&str; 5] = ["--raw", "--rate=48000", "--channels=2", "--format=f32", "--latency=10ms"];
const BYTES_PER_FRAME: usize = CHANNELS * 4;
const PREFIX: &str = "mesh-kvm-";
const PLAY_NODE: &str = "mesh-kvm-play";
const CHIME_NODE: &str = "mesh-kvm-chime";
const CAPTURE_NODE: &str = "mesh-kvm-capture";
const MIC_NODE: &str = "mesh-kvm-mic";
const TOOL_DEADLINE: Duration = Duration::from_secs(2);
/// How often the devices are looked at when nothing has said they changed.
const LOOK_EVERY: Duration = Duration::from_secs(5);
/// Changes come in bursts, so this long is left for the rest of a burst before looking.
const SETTLE: Duration = Duration::from_millis(40);
/// However many changes there are, the graph is read at most this often.
const SHORTEST_LOOK_GAP: Duration = Duration::from_millis(250);
/// How long a capture ending by itself stays in the notes.
const FAILURE_SHOWS_FOR: Duration = Duration::from_secs(6);
/// A helper that won't stay up isn't restarted more often than this.
const RESPAWN_GAP: Duration = Duration::from_secs(1);
/// pw-play reads a whole period at a time in the middle of the audio graph, and blocks there if the
/// pipe runs dry. So the pipe is kept one period and this much ahead.
const PLAYER_MARGIN_BYTES: usize = 240 * BYTES_PER_FRAME;
/// The period asked for with `--latency`, until pw-play shows what it really reads.
const PLAYER_USUAL_PERIOD_BYTES: usize = 480 * BYTES_PER_FRAME;
/// Room for the longest period there's any sense in keeping up with, 2048 frames, and the margin.
const PLAYER_PIPE_BYTES: usize = 32768;
/// How long a longer period is remembered for.
const READ_SIZE_WINDOW: Duration = Duration::from_secs(3);
/// How often the queue is topped up.
const PLAYER_TICK: Duration = Duration::from_millis(1);
/// pw-play holds up the audio graph while it waits for input, so it only runs during a stream.
const PLAYER_IDLE_CLOSE: Duration = Duration::from_secs(2);
const PLAYER_IDLE_TICK: Duration = Duration::from_millis(20);
const PLAYER_OFF_TICK: Duration = Duration::from_millis(5);

/// Wakes the engine when the devices or the choices change.
static WAKE: LazyLock<Signal> = LazyLock::new(Signal::default);

/// What pw-play should play to, with `None` meaning the system's default.
static PLAY_TARGET: Mutex<Option<String>> = Mutex::new(None);

pub fn choices_changed() {
    WAKE.notify();
}

/// A tool with its C library buffering turned off. pw-record and pw-play go through 4 KB buffers,
/// which holds audio back and delivers it in pieces that don't line up with anything.
fn without_stdio_buffering(program: &str, flag: &str) -> Command {
    static HAVE_STDBUF: LazyLock<bool> = LazyLock::new(|| {
        let mut command = Command::new("stdbuf");
        command.arg("--version");
        run_with_deadline(command, TOOL_DEADLINE).is_some_and(|output| output.status.success())
    });
    if !*HAVE_STDBUF {
        return Command::new(program);
    }
    let mut command = Command::new("stdbuf");
    command.args([flag, program]);
    command
}

fn tell_kernel_to_end_it_with_us(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // A killed service must not leave helpers behind. The helpers are started on threads that last
    // as long as the service, which is what this signal follows.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

// ============================================================================================
// The audio graph
// ============================================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    id: u32,
    name: String,
    description: String,
    class: String,
    /// Moving audio right now, not just connected.
    running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Port {
    id: u32,
    node: u32,
    output: bool,
    channel: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    id: u32,
    from_node: u32,
    from_port: u32,
    to_node: u32,
    to_port: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Graph {
    nodes: Vec<Node>,
    ports: Vec<Port>,
    links: Vec<Link>,
    default_sink: Option<String>,
    default_source: Option<String>,
}

fn as_id(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|id| u32::try_from(id).ok())
}

/// The `name` in a metadata value, which `pw-dump` gives parsed or as text.
fn metadata_name(value: &Value) -> Option<String> {
    let parsed;
    let object = match value {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        other => other,
    };
    object.get("name")?.as_str().map(String::from)
}

fn parse_graph(dump: &str) -> Option<Graph> {
    let objects: Vec<Value> = serde_json::from_str(dump).ok()?;
    let mut graph = Graph::default();
    for object in &objects {
        let id = object.get("id").and_then(as_id);
        let info = object.get("info");
        let props = info.and_then(|info| info.get("props"));
        let text = |key: &str| props.and_then(|p| p.get(key)).and_then(Value::as_str).map(String::from);
        match (object.get("type").and_then(Value::as_str).unwrap_or(""), id) {
            ("PipeWire:Interface:Node", Some(id)) => {
                let name = text("node.name").unwrap_or_default();
                let description = text("node.description").or_else(|| text("node.nick")).unwrap_or_else(|| name.clone());
                let running = info.and_then(|i| i.get("state")).and_then(Value::as_str) == Some("running");
                graph.nodes.push(Node { id, name, description, class: text("media.class").unwrap_or_default(), running });
            }
            ("PipeWire:Interface:Port", Some(id)) => {
                let node = props.and_then(|p| p.get("node.id")).and_then(as_id);
                let direction = info.and_then(|i| i.get("direction")).and_then(Value::as_str);
                if let (Some(node), Some(direction)) = (node, direction) {
                    let channel = text("audio.channel").unwrap_or_default();
                    graph.ports.push(Port { id, node, output: direction == "output", channel });
                }
            }
            ("PipeWire:Interface:Link", Some(id)) => {
                let field = |key: &str| info.and_then(|i| i.get(key)).and_then(as_id);
                if let (Some(from_node), Some(from_port), Some(to_node), Some(to_port)) =
                    (field("output-node-id"), field("output-port-id"), field("input-node-id"), field("input-port-id"))
                {
                    graph.links.push(Link { id, from_node, from_port, to_node, to_port });
                }
            }
            ("PipeWire:Interface:Metadata", _) if text_of(object, "metadata.name") == Some("default") => {
                for entry in object.get("metadata").and_then(Value::as_array).into_iter().flatten() {
                    let value = entry.get("value");
                    match entry.get("key").and_then(Value::as_str) {
                        Some("default.audio.sink") => graph.default_sink = value.and_then(metadata_name),
                        Some("default.audio.source") => graph.default_source = value.and_then(metadata_name),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(graph)
}

/// Metadata keeps its properties beside `info`, not in it.
fn text_of<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object.get("props")?.get(key)?.as_str()
}

fn is_ours(node: &Node) -> bool {
    node.name.starts_with(PREFIX)
}

impl Graph {
    fn lists(&self) -> Lists {
        let listed = |classes: &[&str]| {
            let mut devices: Vec<Device> = self
                .nodes
                .iter()
                .filter(|node| classes.contains(&node.class.as_str()) && !is_ours(node))
                .map(|node| Device { id: node.name.clone(), name: node.description.clone() })
                .collect();
            devices.sort_by(|a, b| a.name.cmp(&b.name));
            devices
        };
        Lists {
            inputs: listed(&["Audio/Source", "Audio/Source/Virtual", "Audio/Duplex"]),
            outputs: listed(&["Audio/Sink", "Audio/Duplex"]),
            default_input: self.default_source.clone(),
            default_output: self.default_sink.clone(),
        }
    }

    fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// Whether audio from this node can have come from our player, through any number of steps:
    /// an effects sink between the player and the output still carries it.
    fn carries_our_audio(&self, node: u32) -> bool {
        let mut seen = HashSet::new();
        let mut waiting = vec![node];
        while let Some(at) = waiting.pop() {
            if !seen.insert(at) {
                continue;
            }
            if self.node(at).is_some_and(|node| node.name == PLAY_NODE || node.name == CHIME_NODE) {
                return true;
            }
            waiting.extend(self.links.iter().filter(|link| link.to_node == at).map(|link| link.from_node));
        }
        false
    }

    /// Whether anything that should be captured from a sink is really playing, not just connected.
    fn apps_running_on(&self, sink: &str) -> bool {
        let Some(sink) = self.nodes.iter().find(|node| node.name == sink) else { return false };
        self.apps_on(sink.id).into_iter().any(|app| self.node(app).is_some_and(|node| node.running))
    }

    /// The apps playing on a sink, leaving out anything that carries our own audio.
    fn apps_on(&self, sink: u32) -> HashSet<u32> {
        self.links
            .iter()
            .filter(|link| link.to_node == sink)
            .map(|link| link.from_node)
            .filter(|&from| {
                self.node(from).is_some_and(|node| node.class.starts_with("Stream/Output/Audio") && !is_ours(node))
                    && !self.carries_our_audio(from)
            })
            .collect()
    }
}

/// Which capture inputs an app's channel belongs on. What has no place in stereo is left out,
/// apart from the centre channel, which is most of what a surround film has to say.
fn stereo_places(channel: &str) -> &'static [&'static str] {
    match channel {
        "FL" => &["FL"],
        "FR" => &["FR"],
        "MONO" | "FC" => &["FL", "FR"],
        _ => &[],
    }
}

/// The links to make and the ones to take away, to feed `capture` from the apps on `sink`.
#[derive(Debug, Default, PartialEq, Eq)]
struct LinkPlan {
    add: Vec<(u32, u32)>,
    remove: Vec<u32>,
}

fn plan_links(graph: &Graph, sink: Option<&str>) -> LinkPlan {
    let mut plan = LinkPlan::default();
    let Some(capture) = graph.nodes.iter().find(|node| node.name == CAPTURE_NODE) else {
        return plan;
    };
    let sink = sink.and_then(|name| graph.nodes.iter().find(|node| node.name == name));
    let apps = sink.map(|sink| graph.apps_on(sink.id)).unwrap_or_default();
    let inputs: HashMap<&str, u32> = graph
        .ports
        .iter()
        .filter(|port| port.node == capture.id && !port.output)
        .map(|port| (port.channel.as_str(), port.id))
        .collect();
    let existing: Vec<&Link> = graph.links.iter().filter(|link| link.to_node == capture.id).collect();
    for port in graph.ports.iter().filter(|port| port.output && apps.contains(&port.node)) {
        for place in stereo_places(&port.channel) {
            if let Some(&input) = inputs.get(place) {
                if !existing.iter().any(|link| link.from_port == port.id && link.to_port == input) {
                    plan.add.push((port.id, input));
                }
            }
        }
    }
    plan.remove = existing.iter().filter(|link| !apps.contains(&link.from_node)).map(|link| link.id).collect();
    plan
}

fn read_graph() -> Option<Graph> {
    let mut command = Command::new("pw-dump");
    command.arg("-N");
    let output = run_with_deadline(command, TOOL_DEADLINE)?;
    parse_graph(&String::from_utf8_lossy(&output.stdout))
}

fn run_pw_link(args: &[String]) {
    let mut command = Command::new("pw-link");
    command.args(args);
    run_with_deadline(command, TOOL_DEADLINE);
}

fn apply_links(plan: &LinkPlan) {
    for (from, to) in &plan.add {
        run_pw_link(&[from.to_string(), to.to_string()]);
    }
    for link in &plan.remove {
        run_pw_link(&["-d".to_string(), link.to_string()]);
    }
}

/// Tells which of `pw-dump --monitor`'s changes matter. Clients don't: every `pw-dump` and `pw-link`
/// this service runs is one coming and going, and waking on those would never stop. A removal gives
/// only an id, so this keeps the ids it would care about.
#[derive(Default)]
struct GraphChanges {
    relevant: HashSet<u64>,
}

impl GraphChanges {
    fn matters(&mut self, object: &Value) -> bool {
        let Some(id) = object.get("id").and_then(Value::as_u64) else { return false };
        match object.get("type").and_then(Value::as_str) {
            Some(
                "PipeWire:Interface:Node"
                | "PipeWire:Interface:Port"
                | "PipeWire:Interface:Link"
                | "PipeWire:Interface:Metadata",
            ) => {
                self.relevant.insert(id);
                true
            }
            Some(_) => false,
            None => self.relevant.remove(&id),
        }
    }
}

/// Keeps `pw-dump --monitor` running, and wakes the engine when something that matters changes. The
/// changes themselves come in pieces, and looking at the whole graph takes 10 ms.
fn watch_graph() {
    loop {
        let mut command = Command::new("pw-dump");
        command.args(["-m", "-N"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        tell_kernel_to_end_it_with_us(&mut command);
        if let Ok(mut child) = command.spawn() {
            if let Some(stdout) = child.stdout.take() {
                let mut changes = GraphChanges::default();
                let reader = std::io::BufReader::new(stdout);
                for batch in serde_json::Deserializer::from_reader(reader).into_iter::<Vec<Value>>() {
                    let Ok(batch) = batch else { break };
                    // Not `any`, which would stop at the first and miss ids further on.
                    if batch.iter().filter(|object| changes.matters(object)).count() > 0 {
                        WAKE.notify();
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        std::thread::sleep(RESPAWN_GAP * 3);
    }
}

// ============================================================================================
// Capture
// ============================================================================================

/// One running `pw-record`, feeding a source.
struct Capture {
    node: &'static str,
    source: &'static Source,
    child: Option<Child>,
    /// What the running capture was started with.
    running: Option<Vec<String>>,
    last_spawn: Option<Instant>,
    /// When the capture last ended by itself.
    failed_at: Option<Instant>,
    /// Since when it has been up, with something to deliver.
    expected_since: Option<Instant>,
}

/// Reads until the capture ends. Only whole frames go on.
fn pump(mut stdout: std::process::ChildStdout, source: &'static Source) {
    use std::io::Read;
    let mut bytes = [0u8; 8192];
    let mut held = 0;
    let mut samples = Vec::with_capacity(2048);
    while let Ok(read) = stdout.read(&mut bytes[held..]) {
        if read == 0 {
            break;
        }
        held += read;
        let whole = held / BYTES_PER_FRAME * BYTES_PER_FRAME;
        samples.clear();
        samples.extend(bytes[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        HUB.capture(source, &mut samples);
        bytes.copy_within(whole..held, 0);
        held -= whole;
    }
}

impl Capture {
    fn new(node: &'static str, source: &'static Source) -> Self {
        Self { node, source, child: None, running: None, last_spawn: None, failed_at: None, expected_since: None }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.running = None;
        self.expected_since = None;
        self.source.clear();
    }

    /// True while it's running.
    fn alive(&mut self) -> bool {
        let ended = self.child.as_mut().is_some_and(|child| !matches!(child.try_wait(), Ok(None)));
        if ended {
            self.failed_at = Some(Instant::now());
            self.stop();
        }
        self.child.is_some()
    }

    /// How it's doing, given whether anything should be coming out of it now.
    fn health(&mut self, expected: bool, microphone: bool, now: Instant) -> CaptureHealth {
        let running = self.alive();
        if !(running && expected) {
            self.expected_since = None;
        } else if self.expected_since.is_none() {
            self.expected_since = Some(now);
        }
        CaptureHealth {
            failed: self.failed_at.is_some_and(|at| now.duration_since(at) < FAILURE_SHOWS_FOR),
            expected_for: self.expected_since.map(|since| now.duration_since(since)),
            activity: self.source.activity(),
            microphone,
        }
    }

    /// Brings the capture in line with `wanted`, the arguments it should run with, if any.
    fn ensure(&mut self, wanted: Option<Vec<String>>, now: Instant) {
        if !self.alive() || self.running != wanted {
            self.stop();
        }
        let Some(args) = wanted else { return };
        if self.child.is_some() || self.last_spawn.is_some_and(|at| now.duration_since(at) < RESPAWN_GAP) {
            return;
        }
        self.last_spawn = Some(now);
        let mut command = without_stdio_buffering("pw-record", "-o0");
        command
            .args(PW_FORMAT_ARGS)
            .args(["-P", &format!("node.name={}", self.node)])
            .args(&args)
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        tell_kernel_to_end_it_with_us(&mut command);
        match command.spawn() {
            Ok(mut child) => {
                if let Some(stdout) = child.stdout.take() {
                    let source = self.source;
                    let _ = std::thread::Builder::new()
                        .name("mesh-kvm-audio-capture".to_string())
                        .spawn(move || pump(stdout, source));
                }
                self.child = Some(child);
                self.running = Some(args);
            }
            Err(e) => {
                self.failed_at = Some(now);
                crate::settings::log_note(&format!("Couldn't start pw-record: {e}"));
            }
        }
    }
}

/// The arguments for capturing from a microphone.
fn mic_args(target: Option<String>) -> Vec<String> {
    target.map(|name| vec!["--target".to_string(), name]).unwrap_or_default()
}

/// The arguments for a capture that connects to nothing by itself.
fn unconnected_args() -> Vec<String> {
    ["--target", "0", "-P", "node.autoconnect=false"].map(String::from).to_vec()
}

/// How long to wait before looking at the graph.
fn look_wait(last_look: Option<Instant>, now: Instant) -> Duration {
    let until_allowed = last_look.map_or(Duration::ZERO, |at| (at + SHORTEST_LOOK_GAP).saturating_duration_since(now));
    until_allowed.max(SETTLE)
}

fn engine() {
    let mut mic = Capture::new(MIC_NODE, &HUB.mic);
    let mut output = Capture::new(CAPTURE_NODE, &HUB.output);
    let mut last_look = None;
    loop {
        WAKE.wait(LOOK_EVERY);
        std::thread::sleep(look_wait(last_look, Instant::now()));
        let now = Instant::now();
        last_look = Some(now);
        let Some(graph) = read_graph() else {
            // No sound server yet. The captures stay as they are: it may just be restarting.
            AUDIO_OUTPUT_CAPTURE_LIVE.store(false, Ordering::Relaxed);
            continue;
        };
        let lists = graph.lists();
        let choice = devices::choice();
        let input = devices::resolve(&choice.input, &lists.inputs);
        let sent = devices::resolve(&choice.output, &lists.outputs);
        let play = match devices::resolve(&choice.play, &lists.outputs) {
            Resolved::Off => Resolved::Default,
            other => other,
        };

        mic.ensure((input != Resolved::Off).then(|| mic_args(input.target())), now);
        output.ensure((sent != Resolved::Off).then(unconnected_args), now);
        *PLAY_TARGET.lock().unwrap_or_else(PoisonError::into_inner) = play.target();

        let sink = sent.id(lists.default_output.as_ref());
        if output.alive() {
            apply_links(&plan_links(&graph, sink.as_deref()));
        }
        AUDIO_OUTPUT_CAPTURE_LIVE.store(output.alive(), Ordering::Relaxed);

        let mic_health = mic.health(input != Resolved::Off, true, now);
        let apps_running = sink.as_deref().is_some_and(|sink| graph.apps_running_on(sink));
        let output_health = output.health(sent != Resolved::Off && apps_running, false, now);
        let notes = [
            input.missing_note("microphone or line-in"),
            sent.missing_note("output to send"),
            play.missing_note("output to play on"),
            (input != Resolved::Off).then(|| mic_health.note("microphone or line-in", "", now)).flatten(),
            (sent != Resolved::Off).then(|| output_health.note("output to send", "", now)).flatten(),
        ];
        devices::set_in_use(InUse {
            output: sink,
            play: play.id(lists.default_output.as_ref()),
            echo_safe: true,
            notes: notes.into_iter().flatten().collect(),
        });
        devices::set_lists(lists);
    }
}

pub fn start_engine() {
    let watcher = std::thread::Builder::new().name("mesh-kvm-audio-watch".to_string()).spawn(watch_graph);
    let engine = std::thread::Builder::new().name("mesh-kvm-audio-engine".to_string()).spawn(engine);
    if let Err(e) = watcher.and(engine) {
        crate::settings::log_note(&format!("Couldn't start audio devices: {e}"));
    }
    WAKE.notify();
}

// ============================================================================================
// Playback
// ============================================================================================

/// Feeds audio to `pw-play` through a small non-blocking pipe.
struct Player<F: Fn(Option<&str>) -> Command> {
    command: F,
    child: Option<Child>,
    /// What the running player was started with.
    target: Option<String>,
    last_spawn: Option<Instant>,
}

impl<F: Fn(Option<&str>) -> Command> Player<F> {
    fn new(command: F) -> Self {
        Self { command, child: None, target: None, last_spawn: None }
    }

    fn stdin(&self) -> Option<&ChildStdin> {
        self.child.as_ref()?.stdin.as_ref()
    }

    /// The bytes waiting in the pipe.
    fn queued(&self) -> Option<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.stdin()?.as_raw_fd();
        let mut bytes: libc::c_int = 0;
        let read = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) };
        (read == 0).then_some(bytes as usize)
    }

    fn spawn(&mut self, target: Option<String>) {
        use std::os::fd::AsRawFd;
        let mut command = (self.command)(target.as_deref());
        command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        tell_kernel_to_end_it_with_us(&mut command);
        self.child = command.spawn().ok();
        self.target = target;
        if let Some(stdin) = self.stdin() {
            let fd = stdin.as_raw_fd();
            unsafe {
                libc::fcntl(fd, libc::F_SETPIPE_SZ, PLAYER_PIPE_BYTES as libc::c_int);
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    fn close(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Has a player running for `target`, unless one failed a moment ago. Returns whether it has.
    fn ready(&mut self, target: Option<String>, now: Instant) -> bool {
        let ended = self.child.as_mut().is_some_and(|child| !matches!(child.try_wait(), Ok(None)));
        if ended || (self.child.is_some() && self.target != target) {
            self.close();
        }
        if self.child.is_none() {
            if self.last_spawn.is_some_and(|at| now.duration_since(at) < RESPAWN_GAP) {
                return false;
            }
            self.last_spawn = Some(now);
            self.spawn(target);
        }
        self.child.is_some()
    }

    /// Writes as much as fits, returning false if the player is gone.
    fn write(&mut self, bytes: &[u8]) -> bool {
        use std::io::{ErrorKind, Write};
        let Some(stdin) = self.child.as_mut().and_then(|child| child.stdin.as_mut()) else {
            return false;
        };
        match stdin.write(bytes) {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => true,
            Err(_) => {
                self.close();
                false
            }
        }
    }
}

fn pw_play_command(target: Option<&str>) -> Command {
    let mut command = without_stdio_buffering("pw-play", "-i0");
    command.args(PW_FORMAT_ARGS).args(["-P", &format!("node.name={PLAY_NODE}")]);
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    command.arg("-");
    command
}

/// Learns how much pw-play takes at a time. The audio graph can run at a longer period than was
/// asked for, when something else insists on it, and the pipe has to cover whatever it is.
struct ReadSize {
    period: usize,
    /// The level after the last top-up.
    level: usize,
    /// When the biggest read lately was seen, so that a period that's come back down is noticed.
    since: Option<Instant>,
    biggest: usize,
}

impl Default for ReadSize {
    fn default() -> Self {
        Self { period: PLAYER_USUAL_PERIOD_BYTES, level: 0, since: None, biggest: 0 }
    }
}

impl ReadSize {
    /// Notes the level before a top-up, and returns the level to fill to.
    fn note(&mut self, queued: usize, now: Instant) -> usize {
        let taken = self.level.saturating_sub(queued);
        self.biggest = self.biggest.max(taken);
        self.period = self.period.max(taken);
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) >= READ_SIZE_WINDOW {
            self.period = self.biggest.max(PLAYER_USUAL_PERIOD_BYTES);
            self.biggest = 0;
            self.since = Some(now);
        }
        self.period + PLAYER_MARGIN_BYTES
    }

    fn filled_to(&mut self, level: usize) {
        self.level = level;
    }
}

/// Keeps pw-play topped up. Its pipe is what sets the pace: audio is only asked for from the
/// stream as pw-play takes it, so the device's clock drives everything back to the jitter buffer.
fn playout<F: Fn(Option<&str>) -> Command>(mut player: Player<F>, stream: &Mutex<RxStream>) {
    let mut samples = vec![0.0f32; 120 * CHANNELS];
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    let mut last_alive = None;
    let mut read_size = ReadSize::default();
    loop {
        let now = Instant::now();
        let alive = stream.lock().unwrap_or_else(PoisonError::into_inner).alive(now);
        if alive {
            last_alive = Some(now);
        }
        let muted = AUDIO_RX_MUTED.load(Ordering::Relaxed);
        let idle = last_alive.is_none_or(|at| now.duration_since(at) >= PLAYER_IDLE_CLOSE);
        if muted || idle {
            player.close();
            AUDIO_PLAYER_LIVE.store(false, Ordering::Relaxed);
            let peak = AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
            AUDIO_RX_PEAK_PCT.store(peak / 2, Ordering::Relaxed);
            if !muted || !alive {
                std::thread::sleep(PLAYER_IDLE_TICK);
                continue;
            }
            drain_received(stream, PLAYER_OFF_TICK, now);
            std::thread::sleep(PLAYER_OFF_TICK);
            continue;
        }
        let target = PLAY_TARGET.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if !player.ready(target, now) {
            AUDIO_PLAYER_LIVE.store(false, Ordering::Relaxed);
            std::thread::sleep(PLAYER_IDLE_TICK);
            continue;
        }
        AUDIO_PLAYER_LIVE.store(true, Ordering::Relaxed);
        let fill_to = read_size.note(player.queued().unwrap_or(0), now);
        while player.queued().is_some_and(|queued| queued < fill_to) {
            stream.lock().unwrap_or_else(PoisonError::into_inner).fill(&mut samples, Instant::now());
            finish_received(&mut samples);
            bytes.clear();
            bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
            if !player.write(&bytes) {
                break;
            }
        }
        read_size.filled_to(player.queued().unwrap_or(0));
        std::thread::sleep(PLAYER_TICK);
    }
}

pub fn start_playback(stream: Arc<Mutex<RxStream>>) {
    let spawned = std::thread::Builder::new()
        .name("mesh-kvm-audio-play".to_string())
        .spawn(move || playout(Player::new(pw_play_command), &stream));
    if let Err(e) = spawned {
        crate::settings::log_note(&format!("Couldn't start audio playback: {e}"));
    }
}

/// Plays the test chime here, on the output chosen for received audio.
pub fn play_chime(pcm: &[f32]) {
    use std::io::Write;
    let mut command = Command::new("pw-play");
    command.args(PW_FORMAT_ARGS).args(["-P", &format!("node.name={CHIME_NODE}")]);
    if let Some(target) = PLAY_TARGET.lock().unwrap_or_else(PoisonError::into_inner).clone() {
        command.args(["--target", &target]);
    }
    command.arg("-").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
            let _ = stdin.write_all(&bytes);
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[
      {"id":0,"type":"PipeWire:Interface:Core","info":{}},
      {"id":35,"type":"PipeWire:Interface:Metadata","props":{"metadata.name":"default"},
       "metadata":[{"subject":0,"key":"default.audio.sink","type":"Spa:String:JSON","value":{"name":"speakers"}},
                   {"subject":0,"key":"default.audio.source","type":"Spa:String:JSON","value":"{\"name\":\"mic\"}"}]},
      {"id":36,"type":"PipeWire:Interface:Metadata","props":{"metadata.name":"settings"},
       "metadata":[{"subject":0,"key":"default.audio.sink","value":{"name":"not-this"}}]},
      {"id":10,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"speakers","node.description":"USB speakers","media.class":"Audio/Sink"}}},
      {"id":11,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"headset","node.nick":"Headset","media.class":"Audio/Sink"}}},
      {"id":12,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mic","node.description":"USB microphone","media.class":"Audio/Source"}}},
      {"id":13,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"webcam","media.class":"Video/Source"}}},
      {"id":20,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":"Firefox","media.class":"Stream/Output/Audio"}}},
      {"id":21,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mesh-kvm-play","media.class":"Stream/Output/Audio"}}},
      {"id":22,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mesh-kvm-capture","media.class":"Stream/Input/Audio"}}},
      {"id":23,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mono-app","media.class":"Stream/Output/Audio"}}},
      {"id":24,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"other-sink-app","media.class":"Stream/Output/Audio"}}},
      {"id":100,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":10,"audio.channel":"FL"}}},
      {"id":101,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":10,"audio.channel":"FR"}}},
      {"id":110,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":11,"audio.channel":"FL"}}},
      {"id":200,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":20,"audio.channel":"FL"}}},
      {"id":201,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":20,"audio.channel":"FR"}}},
      {"id":210,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":21,"audio.channel":"FL"}}},
      {"id":211,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":21,"audio.channel":"FR"}}},
      {"id":220,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":22,"audio.channel":"FL"}}},
      {"id":221,"type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":22,"audio.channel":"FR"}}},
      {"id":230,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":23,"audio.channel":"MONO"}}},
      {"id":240,"type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":24,"audio.channel":"FL"}}},
      {"id":300,"type":"PipeWire:Interface:Link","info":{"output-node-id":20,"output-port-id":200,"input-node-id":10,"input-port-id":100}},
      {"id":301,"type":"PipeWire:Interface:Link","info":{"output-node-id":20,"output-port-id":201,"input-node-id":10,"input-port-id":101}},
      {"id":302,"type":"PipeWire:Interface:Link","info":{"output-node-id":21,"output-port-id":210,"input-node-id":10,"input-port-id":100}},
      {"id":303,"type":"PipeWire:Interface:Link","info":{"output-node-id":21,"output-port-id":211,"input-node-id":10,"input-port-id":101}},
      {"id":304,"type":"PipeWire:Interface:Link","info":{"output-node-id":23,"output-port-id":230,"input-node-id":10,"input-port-id":100}},
      {"id":305,"type":"PipeWire:Interface:Link","info":{"output-node-id":24,"output-port-id":240,"input-node-id":11,"input-port-id":110}}
    ]"#;

    fn graph() -> Graph {
        parse_graph(DUMP).unwrap()
    }

    #[test]
    fn the_lists_show_real_devices_by_name_and_leave_out_the_rest() {
        let lists = graph().lists();
        let outputs: Vec<(&str, &str)> = lists.outputs.iter().map(|d| (d.id.as_str(), d.name.as_str())).collect();
        assert_eq!(outputs, [("headset", "Headset"), ("speakers", "USB speakers")]);
        let inputs: Vec<&str> = lists.inputs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(inputs, ["USB microphone"]);
        assert_eq!(lists.default_output.as_deref(), Some("speakers"));
        assert_eq!(lists.default_input.as_deref(), Some("mic"), "written as text or as an object");
    }

    #[test]
    fn the_service_s_own_clients_coming_and_going_change_nothing() {
        let mut changes = GraphChanges::default();
        let client = serde_json::json!({"id": 51, "type": "PipeWire:Interface:Client", "info": {"props": {}}});
        assert!(!changes.matters(&client), "a pw-dump starting");
        assert!(!changes.matters(&client), "and updating");
        assert!(!changes.matters(&serde_json::json!({"id": 51, "info": null})), "and leaving");
        assert!(!changes.matters(&serde_json::json!({"id": 7, "type": "PipeWire:Interface:Module"})));
    }

    #[test]
    fn nodes_ports_links_and_metadata_matter_and_so_does_their_going() {
        let mut changes = GraphChanges::default();
        for (id, kind) in [(10, "Node"), (11, "Port"), (12, "Link"), (13, "Metadata")] {
            let object = serde_json::json!({"id": id, "type": format!("PipeWire:Interface:{kind}"), "info": {}});
            assert!(changes.matters(&object), "{kind}");
        }
        for id in [10, 11, 12, 13] {
            assert!(changes.matters(&serde_json::json!({"id": id, "info": null})));
            assert!(!changes.matters(&serde_json::json!({"id": id, "info": null})), "it only goes once");
        }
    }

    #[test]
    fn the_graph_is_not_read_more_often_than_the_limit_however_often_it_changes() {
        let t = Instant::now();
        assert_eq!(look_wait(None, t), SETTLE);
        assert_eq!(look_wait(Some(t), t), SHORTEST_LOOK_GAP);
        assert_eq!(look_wait(Some(t), t + SHORTEST_LOOK_GAP), SETTLE);
    }

    #[test]
    fn a_broken_dump_is_not_a_graph() {
        assert_eq!(parse_graph("not json"), None);
        assert_eq!(parse_graph(""), None);
    }

    #[test]
    fn the_capture_is_fed_by_the_apps_on_the_sink_and_never_our_player() {
        let plan = plan_links(&graph(), Some("speakers"));
        assert_eq!(plan.add, vec![(200, 220), (201, 221), (230, 220), (230, 221)]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn links_already_there_are_left_alone() {
        let mut graph = graph();
        graph.links.push(Link { id: 400, from_node: 20, from_port: 200, to_node: 22, to_port: 220 });
        let plan = plan_links(&graph, Some("speakers"));
        assert!(!plan.add.contains(&(200, 220)));
        assert!(plan.add.contains(&(201, 221)));
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn an_app_that_moved_to_another_output_is_taken_off_the_capture() {
        let mut graph = graph();
        graph.links.push(Link { id: 400, from_node: 24, from_port: 240, to_node: 22, to_port: 220 });
        assert_eq!(plan_links(&graph, Some("speakers")).remove, vec![400]);
        assert!(plan_links(&graph, Some("headset")).remove.is_empty(), "the one it moved to keeps it");
    }

    #[test]
    fn a_paused_app_is_connected_but_isn_t_expected_to_deliver() {
        assert!(graph().apps_running_on("speakers"));
        assert!(!graph().apps_running_on("headset"), "it's linked there, but idle");
        assert!(!graph().apps_running_on("gone"));
    }

    #[test]
    fn choosing_another_sink_changes_which_apps_are_heard() {
        let plan = plan_links(&graph(), Some("headset"));
        assert_eq!(plan.add, vec![(240, 220)]);
    }

    #[test]
    fn a_sink_that_has_gone_leaves_the_capture_with_no_apps() {
        let mut graph = graph();
        graph.links.push(Link { id: 400, from_node: 20, from_port: 200, to_node: 22, to_port: 220 });
        let plan = plan_links(&graph, Some("gone"));
        assert!(plan.add.is_empty());
        assert_eq!(plan.remove, vec![400]);
    }

    #[test]
    fn our_player_is_left_out_even_through_an_effects_sink() {
        let mut graph = graph();
        // pw-play -> effects sink -> effects output stream -> speakers
        graph.nodes.push(Node { id: 30, name: "fx".into(), description: "fx".into(), class: "Audio/Sink".into(), running: true });
        graph.nodes.push(Node { id: 31, name: "fx-out".into(), description: "fx".into(), class: "Stream/Output/Audio".into(), running: true });
        graph.ports.push(Port { id: 310, node: 31, output: true, channel: "FL".into() });
        graph.links.retain(|link| link.from_node != 21);
        graph.links.push(Link { id: 401, from_node: 21, from_port: 210, to_node: 30, to_port: 0 });
        graph.links.push(Link { id: 402, from_node: 30, from_port: 0, to_node: 31, to_port: 0 });
        graph.links.push(Link { id: 403, from_node: 31, from_port: 310, to_node: 10, to_port: 100 });
        assert!(!plan_links(&graph, Some("speakers")).add.iter().any(|(from, _)| *from == 310));
    }

    #[test]
    fn no_capture_running_means_nothing_to_link() {
        let mut graph = graph();
        graph.nodes.retain(|node| node.name != CAPTURE_NODE);
        assert_eq!(plan_links(&graph, Some("speakers")), LinkPlan::default());
    }

    #[test]
    fn surround_channels_fold_to_stereo_sensibly() {
        assert_eq!(stereo_places("FL"), ["FL"]);
        assert_eq!(stereo_places("FC"), ["FL", "FR"]);
        assert!(stereo_places("LFE").is_empty());
    }

    #[test]
    fn pipewire_tools_are_told_the_stream_is_raw() {
        assert!(PW_FORMAT_ARGS.contains(&"--raw"));
    }

    #[test]
    fn the_pipe_keeps_a_period_and_a_margin_in_it() {
        let (mut size, t) = (ReadSize::default(), Instant::now());
        assert_eq!(size.note(0, t), PLAYER_USUAL_PERIOD_BYTES + PLAYER_MARGIN_BYTES);
    }

    #[test]
    fn a_longer_period_is_covered_at_once_and_let_go_later() {
        let (mut size, t) = (ReadSize::default(), Instant::now());
        size.note(0, t);
        size.filled_to(10_000);
        // 1024 frames gone in one read.
        let level = size.note(10_000 - 8192, t + Duration::from_millis(20));
        assert_eq!(level, 8192 + PLAYER_MARGIN_BYTES);
        size.filled_to(10_000);
        size.note(10_000 - 3840, t + READ_SIZE_WINDOW);
        size.filled_to(10_000);
        let level = size.note(10_000 - 3840, t + READ_SIZE_WINDOW * 2);
        assert_eq!(level, PLAYER_USUAL_PERIOD_BYTES + PLAYER_MARGIN_BYTES);
    }

    fn sleeper(_: Option<&str>) -> Command {
        let mut command = Command::new("sleep");
        command.arg("5");
        command
    }

    #[test]
    fn a_stalled_player_can_only_hold_what_fits_the_pipe() {
        let mut player = Player::new(sleeper);
        assert!(player.ready(None, Instant::now()));
        let chunk = [0u8; 960];
        for _ in 0..100 {
            player.write(&chunk);
        }
        assert!(player.queued().unwrap() <= 65536);
        player.close();
    }

    #[test]
    fn the_queue_level_is_read_from_the_pipe() {
        let mut player = Player::new(sleeper);
        assert_eq!(player.queued(), None, "no player yet");
        assert!(player.ready(None, Instant::now()));
        for packets in 1..=3 {
            assert!(player.write(&[0; 960]));
            assert_eq!(player.queued(), Some(packets * 960));
        }
        player.close();
    }

    #[test]
    fn a_player_that_keeps_failing_starts_once_a_second_at_most() {
        let spawns = std::cell::Cell::new(0);
        let mut player = Player::new(|_| {
            spawns.set(spawns.get() + 1);
            Command::new("/nonexistent/pw-play")
        });
        let start = Instant::now();
        for ms in (0..1000).step_by(5) {
            assert!(!player.ready(None, start + Duration::from_millis(ms)));
        }
        assert_eq!(spawns.get(), 1);
        player.ready(None, start + RESPAWN_GAP);
        assert_eq!(spawns.get(), 2);
    }

    #[test]
    fn a_new_output_gets_a_new_player() {
        let targets = std::cell::RefCell::new(Vec::new());
        let mut player = Player::new(|target: Option<&str>| {
            targets.borrow_mut().push(target.map(String::from));
            sleeper(None)
        });
        let start = Instant::now();
        assert!(player.ready(None, start));
        assert!(player.ready(None, start + RESPAWN_GAP));
        assert_eq!(targets.borrow().len(), 1, "same output, same player");
        assert!(player.ready(Some("headset".into()), start + RESPAWN_GAP * 2));
        assert_eq!(targets.borrow().last().unwrap().as_deref(), Some("headset"));
        player.close();
    }

    #[test]
    fn a_dead_player_is_replaced() {
        let mut player = Player::new(|_| Command::new("cat"));
        let now = Instant::now();
        assert!(player.ready(None, now));
        let first = player.child.as_ref().map(Child::id);
        player.child.as_mut().unwrap().kill().unwrap();
        player.child.as_mut().unwrap().wait().unwrap();
        assert!(player.ready(None, now + RESPAWN_GAP));
        assert_ne!(player.child.as_ref().map(Child::id), first);
        player.close();
    }
}
