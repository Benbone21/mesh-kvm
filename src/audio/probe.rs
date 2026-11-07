//! A one-off measurement: which capture of the default output survives its speakers being silenced.
//!
//! It runs instead of everything else when the exe's name contains `audio-probe`, or with
//! `--audio-probe`. This whole file can go once the answer is known, along with the one call in
//! `main`, `mute::holds_mute`, and the `pub(super)`s in `wasapi`.
//!
//! The speakers' mute and volume change while it runs, so they're written to a record first and put
//! back after every case. Release builds abort on panic, so that `Restoring` only covers errors there:
//! a panic hook and a time limit restore from the record, and the next run repairs anything left.

use serde::{Deserialize, Serialize};

/// Below this, nothing is playing.
const NO_SOUND: f32 = 0.001;
/// How close a volume read back must be to what was set.
const SAME_VOLUME: f32 = 0.005;

const NO_SOUND_MESSAGE: &str = "No sound was playing, so there was nothing to measure. Start some music on the default output and try again.";

/// The state of the speakers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Speakers {
    pub muted: bool,
    pub scalar: f32,
}

/// What's written down before anything changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub device: String,
    pub speakers: Speakers,
}

/// The endpoint's volume controls.
pub trait Volume {
    fn read(&mut self) -> Result<Speakers, String>;
    fn set_muted(&mut self, muted: bool) -> Result<(), String>;
    fn set_scalar(&mut self, scalar: f32) -> Result<(), String>;
    fn level_db(&mut self) -> Option<f32>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Loopback,
    Excluding,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    AsFound,
    Muted,
    Scalar(f32),
}

pub struct Case {
    pub number: u32,
    pub capture: Capture,
    pub step: Step,
}

pub const CASES: [Case; 8] = [
    Case { number: 1, capture: Capture::Loopback, step: Step::AsFound },
    Case { number: 2, capture: Capture::Loopback, step: Step::Muted },
    Case { number: 3, capture: Capture::Loopback, step: Step::Scalar(0.01) },
    Case { number: 4, capture: Capture::Loopback, step: Step::Scalar(0.0) },
    Case { number: 5, capture: Capture::Excluding, step: Step::AsFound },
    Case { number: 6, capture: Capture::Excluding, step: Step::Muted },
    Case { number: 7, capture: Capture::Excluding, step: Step::Scalar(0.01) },
    Case { number: 8, capture: Capture::Excluding, step: Step::Scalar(0.0) },
];

/// What came out of a capture.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Levels {
    pub peak: f32,
    squares: f64,
    samples: u64,
    pub frames: u64,
    /// Frames Windows flagged as silent.
    pub silent_frames: u64,
}

impl Levels {
    pub fn add(&mut self, sample: f32) {
        self.peak = self.peak.max(sample.abs());
        self.squares += f64::from(sample) * f64::from(sample);
        self.samples += 1;
    }

    pub fn rms(&self) -> f64 {
        if self.samples == 0 {
            return 0.0;
        }
        (self.squares / self.samples as f64).sqrt()
    }
}

/// Runs one capture. `apply` sets the speakers once the capture is up, as they would be silenced
/// under a capture already running.
pub trait Meter {
    fn measure(&mut self, capture: Capture, apply: &mut dyn FnMut() -> Result<(), String>) -> Result<Levels, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Measured(Levels),
    Failed(String),
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseResult {
    pub number: u32,
    pub capture: Capture,
    pub step: Step,
    pub volume_db: Option<f32>,
    pub outcome: Outcome,
}

#[derive(Debug, Default, PartialEq)]
pub struct Run {
    pub cases: Vec<CaseResult>,
    pub refused: Option<String>,
    /// Why the speakers couldn't be put back, if they couldn't.
    pub not_restored: Option<String>,
}

/// Puts the speakers back when it's told to, and whatever happens if it never is.
struct Restoring<'a, V: Volume> {
    volume: &'a mut V,
    original: Speakers,
    finished: bool,
}

impl<V: Volume> Restoring<'_, V> {
    /// Volume first, so that unmuting can't let out a moment at the wrong level. Only what changed
    /// is written: a control that refuses writes has nothing to put back if nothing moved.
    fn put_back(&mut self) -> Result<(), String> {
        let now = self.volume.read()?;
        if (now.scalar - self.original.scalar).abs() > SAME_VOLUME {
            self.volume.set_scalar(self.original.scalar)?;
        }
        if now.muted != self.original.muted {
            self.volume.set_muted(self.original.muted)?;
        }
        let now = self.volume.read()?;
        if now.muted != self.original.muted || (now.scalar - self.original.scalar).abs() > SAME_VOLUME {
            return Err(format!("read back as {now:?}, wanted {:?}", self.original));
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<(), String> {
        self.put_back().or_else(|_| self.put_back())
    }

    fn apply(&mut self, step: Step) -> Result<(), String> {
        match step {
            Step::AsFound => Ok(()),
            Step::Muted => self.volume.set_muted(true),
            Step::Scalar(scalar) => self.volume.set_scalar(scalar),
        }
    }
}

impl<V: Volume> Drop for Restoring<'_, V> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.restore();
        }
    }
}

/// Runs every case, stopping for anything that makes going on pointless or unsafe. `may_go_on` says
/// whether there's time, and whether the service has started muting meanwhile.
pub fn run_cases<V: Volume, M: Meter>(
    volume: &mut V,
    meter: &mut M,
    original: Speakers,
    may_go_on: &mut dyn FnMut() -> Result<bool, String>,
) -> Run {
    let mut run = Run::default();
    let mut guard = Restoring { volume, original, finished: false };
    let mut exclusion_error = None;
    for case in &CASES {
        let skip = |reason: String| CaseResult {
            number: case.number,
            capture: case.capture,
            step: case.step,
            volume_db: None,
            outcome: Outcome::Skipped(reason),
        };
        match may_go_on() {
            Ok(true) => {}
            Ok(false) => {
                run.cases.push(skip("out of time".to_string()));
                continue;
            }
            Err(reason) => {
                run.refused = Some(reason);
                break;
            }
        }
        if case.capture == Capture::Excluding {
            if let Some(first) = exclusion_error {
                run.cases.push(skip(format!("as case {first}")));
                continue;
            }
        }
        let mut volume_db = None;
        let measured = meter.measure(case.capture, &mut || {
            guard.apply(case.step)?;
            volume_db = guard.volume.level_db();
            Ok(())
        });
        let restored = guard.restore();
        if measured.is_err() && case.capture == Capture::Excluding {
            exclusion_error = Some(case.number);
        }
        let no_sound = case.number == 1 && measured.as_ref().is_ok_and(|levels| levels.peak < NO_SOUND);
        let outcome = measured.map_or_else(Outcome::Failed, Outcome::Measured);
        run.cases.push(CaseResult { number: case.number, capture: case.capture, step: case.step, volume_db, outcome });
        if let Err(reason) = restored {
            run.not_restored = Some(reason);
            guard.finished = true;
            return run;
        }
        if no_sound {
            run.refused = Some(NO_SOUND_MESSAGE.to_string());
            break;
        }
    }
    guard.finished = true;
    run
}

/// What's known about the output.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Info {
    pub windows: String,
    pub endpoint: String,
    pub mix_format: String,
    /// Least, most and step, in dB.
    pub range: Option<(f32, f32, f32)>,
    pub found: Option<Speakers>,
    pub found_db: Option<f32>,
    pub hardware_support: Option<u32>,
    pub others: Vec<String>,
    pub notes: Vec<String>,
}

fn decibels(ratio: f64) -> f64 {
    20.0 * ratio.max(1e-12).log10()
}

fn support_text(mask: u32) -> String {
    let names = [(1, "volume"), (2, "mute"), (4, "meter")];
    let set: Vec<&str> = names.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, name)| *name).collect();
    format!("0x{mask:08X} ({})", if set.is_empty() { "software only".to_string() } else { set.join(", ") })
}

fn step_text(step: Step) -> String {
    match step {
        Step::AsFound => "as found".to_string(),
        Step::Muted => "muted".to_string(),
        Step::Scalar(scalar) => format!("volume {scalar:.2}"),
    }
}

fn levels_of(cases: &[CaseResult], number: u32) -> Option<(Levels, Option<f32>)> {
    cases.iter().find(|case| case.number == number).and_then(|case| match case.outcome {
        Outcome::Measured(levels) => Some((levels, case.volume_db)),
        _ => None,
    })
}

/// How a case compares with its baseline, for the two the gain could make up for.
fn against_baseline(cases: &[CaseResult], case: &CaseResult, levels: &Levels) -> String {
    let baseline = match case.number {
        3 => 1,
        7 => 5,
        _ => return String::new(),
    };
    let Some((base, base_db)) = levels_of(cases, baseline) else {
        return format!(" | no case {baseline} to compare");
    };
    if levels.rms() == 0.0 || base.rms() == 0.0 {
        return format!(" | no level to compare with case {baseline}");
    }
    let captured = decibels(levels.rms() / base.rms());
    let moved = match (case.volume_db, base_db) {
        (Some(now), Some(before)) => format!(", volume moved {:+.1} dB", now - before),
        _ => String::new(),
    };
    format!(" | {captured:+.1} dB vs case {baseline}{moved}")
}

fn case_line(cases: &[CaseResult], case: &CaseResult) -> String {
    let capture = match case.capture {
        Capture::Loopback => "Loopback",
        Capture::Excluding => "Exclusion",
    };
    let volume = case.volume_db.map_or("vol ?".to_string(), |db| format!("vol {db:.1} dB"));
    let start = format!("{} | {capture} | {} | ", case.number, step_text(case.step));
    match &case.outcome {
        Outcome::Measured(levels) => {
            let silent = levels.silent_frames * 100 / levels.frames.max(1);
            format!(
                "{start}{volume} | peak {:.4} | RMS {:.1} dB | {} frames, {silent}% flagged silent{}",
                levels.peak,
                decibels(levels.rms()),
                levels.frames,
                against_baseline(cases, case, levels)
            )
        }
        Outcome::Failed(reason) => format!("{start}failed: {reason}"),
        Outcome::Skipped(reason) => format!("{start}skipped: {reason}"),
    }
}

/// The whole result, as it goes in the message box and the file.
pub fn format_report(info: &Info, run: &Run) -> String {
    let mut lines = vec!["mesh-kvm audio probe".to_string()];
    lines.extend(info.notes.iter().cloned());
    if let Some(reason) = &run.refused {
        lines.push(format!("Stopped: {reason}"));
    }
    if let Some(reason) = &run.not_restored {
        lines.push(format!(
            "The speakers couldn't be put back: {reason}. Run this again to repair them, or set them by hand."
        ));
    }
    lines.push(format!("Windows: {}", info.windows));
    if !info.endpoint.is_empty() {
        lines.push(format!("Endpoint: {}", info.endpoint));
        lines.push(format!("Mix format: {}", info.mix_format));
    }
    if let Some((least, most, step)) = info.range {
        lines.push(format!("Volume range: {least:.2} to {most:.2} dB, step {step:.2} dB"));
    }
    if let Some(found) = info.found {
        let level = info.found_db.map_or(String::new(), |db| format!(", {db:.1} dB"));
        lines.push(format!("Found: {}, scalar {:.3}{level}", if found.muted { "muted" } else { "not muted" }, found.scalar));
    }
    if let Some(mask) = info.hardware_support {
        lines.push(format!("Hardware support: {}", support_text(mask)));
    }
    if !info.endpoint.is_empty() {
        let others = if info.others.is_empty() { "none".to_string() } else { info.others.join("; ") };
        lines.push(format!("Other outputs: {others}"));
    }
    if !run.cases.is_empty() {
        lines.push(String::new());
        lines.extend(run.cases.iter().map(|case| case_line(&run.cases, case)));
    }
    lines.join("\r\n")
}

/// True for a program name or arguments that ask for this.
pub fn asked_for(exe_name: &str, mut args: impl Iterator<Item = String>) -> bool {
    exe_name.to_lowercase().contains("audio-probe") || args.any(|arg| arg == "--audio-probe")
}

#[cfg(target_os = "windows")]
pub use live::{requested, run};

#[cfg(target_os = "windows")]
mod live {
    use super::super::wasapi::{self, activate_excluding_this_process, enumerator, open_exclusion, take_id};
    use super::*;
    use crate::settings::{config_dir, write_atomically};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDOK, MB_ICONINFORMATION, MB_OK, MB_OKCANCEL};

    const SETTLE: Duration = Duration::from_secs(1);
    const MEASURE: Duration = Duration::from_secs(3);
    /// No case starts unless it will finish inside this, counted from the first change.
    const CASES_BUDGET: Duration = Duration::from_secs(36);
    const RECORD_FILE: &str = "audio-probe-restore.json";
    const REPORT_FILE: &str = "audio-probe.txt";
    /// Everything has to be over by this long after the first change, however it went.
    const HARD_LIMIT: Duration = Duration::from_secs(38);
    /// How much loopback buffers, in 100 ns units: 100 ms.
    const LOOPBACK_BUFFER: i64 = 1_000_000;
    const POLL: Duration = Duration::from_millis(5);
    const FORMAT_FLOAT: u32 = 3;
    const TAG_FLOAT: u16 = 3;
    const TAG_PCM: u16 = 1;
    const TAG_EXTENSIBLE: u16 = 0xFFFE;

    const WHAT_HAPPENS: &str = "This measures how your speakers' sound can be captured.\n\nYour speakers will go quiet for about 30 seconds. Keep the music playing.\n\nYour volume and mute are put back afterwards.";

    static FINISHED: AtomicBool = AtomicBool::new(false);

    pub fn requested() -> bool {
        let name = std::env::current_exe().ok().and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()));
        asked_for(&name.unwrap_or_default(), std::env::args().skip(1))
    }

    fn record_path() -> PathBuf {
        config_dir().join(RECORD_FILE)
    }

    fn show(text: &str, style: u32) -> i32 {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        unsafe { MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("mesh-kvm audio probe").as_ptr(), style | MB_ICONINFORMATION) }
    }

    /// A `--no-box` run is for testing, where nobody is there to press OK.
    fn boxes_wanted() -> bool {
        !std::env::args().any(|arg| arg == "--no-box")
    }

    fn finish(text: &str, code: i32) -> i32 {
        let _ = std::fs::create_dir_all(config_dir());
        let _ = std::fs::write(config_dir().join(REPORT_FILE), text);
        if boxes_wanted() {
            show(text, MB_OK);
        }
        code
    }

    fn refuse(info: &Info, reason: &str) -> i32 {
        finish(&format_report(info, &Run { refused: Some(reason.to_string()), ..Default::default() }), 1)
    }

    fn volume_of(device: &IMMDevice) -> Result<IAudioEndpointVolume, String> {
        unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| format!("endpoint volume: {e}"))
    }

    struct LiveVolume(IAudioEndpointVolume);

    impl Volume for LiveVolume {
        fn read(&mut self) -> Result<Speakers, String> {
            unsafe {
                Ok(Speakers {
                    muted: self.0.GetMute().map_err(|e| format!("GetMute: {e}"))?.as_bool(),
                    scalar: self.0.GetMasterVolumeLevelScalar().map_err(|e| format!("GetMasterVolumeLevelScalar: {e}"))?,
                })
            }
        }

        fn set_muted(&mut self, muted: bool) -> Result<(), String> {
            unsafe { self.0.SetMute(muted, std::ptr::null()) }.map_err(|e| format!("SetMute: {e}"))
        }

        fn set_scalar(&mut self, scalar: f32) -> Result<(), String> {
            unsafe { self.0.SetMasterVolumeLevelScalar(scalar, std::ptr::null()) }.map_err(|e| format!("SetMasterVolumeLevelScalar: {e}"))
        }

        fn level_db(&mut self) -> Option<f32> {
            unsafe { self.0.GetMasterVolumeLevel() }.ok()
        }
    }

    /// Puts back what the record says, if there is one. Returns whether there was.
    fn restore_from_record() -> Result<bool, String> {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let Ok(text) = std::fs::read_to_string(record_path()) else { return Ok(false) };
        let record: Record = serde_json::from_str(&text)
            .map_err(|e| format!("{} can't be read, so check your volume and delete it: {e}", record_path().display()))?;
        let id: Vec<u16> = record.device.encode_utf16().chain(std::iter::once(0)).collect();
        let all = enumerator().map_err(|e| format!("devices: {e}"))?;
        let device = unsafe { all.GetDevice(PCWSTR(id.as_ptr())) }.map_err(|e| format!("the output isn't there: {e}"))?;
        let mut volume = LiveVolume(volume_of(&device)?);
        let now = volume.read()?;
        if (now.scalar - record.speakers.scalar).abs() > SAME_VOLUME {
            volume.set_scalar(record.speakers.scalar)?;
        }
        if now.muted != record.speakers.muted {
            volume.set_muted(record.speakers.muted)?;
        }
        let _ = std::fs::remove_file(record_path());
        Ok(true)
    }

    fn write_record(record: &Record) -> Result<(), String> {
        let _ = std::fs::create_dir_all(config_dir());
        let text = serde_json::to_string(record).map_err(|e| e.to_string())?;
        write_atomically(&record_path(), &text).map_err(|e| e.to_string())
    }

    /// In a release build a panic aborts, so nothing unwinds: only this runs.
    fn restore_on_panic() {
        let before = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            let _ = restore_from_record();
            before(panic);
        }));
    }

    /// Ends the run if something hangs, and leaves the speakers as they were.
    fn start_time_limit() {
        std::thread::spawn(|| {
            std::thread::sleep(HARD_LIMIT);
            if !FINISHED.load(Ordering::SeqCst) {
                let restored = restore_from_record();
                let text = format!("mesh-kvm audio probe\r\nStopped: it ran out of time. Speakers put back: {restored:?}");
                let _ = std::fs::write(config_dir().join(REPORT_FILE), text);
                std::process::exit(3);
            }
        });
    }

    #[repr(C)]
    struct OsVersion {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersion) -> i32;
    }

    fn windows_version() -> String {
        let mut info = OsVersion {
            size: std::mem::size_of::<OsVersion>() as u32,
            major: 0,
            minor: 0,
            build: 0,
            platform: 0,
            service_pack: [0; 128],
        };
        if unsafe { RtlGetVersion(&mut info) } != 0 {
            return "unknown".to_string();
        }
        format!("{}.{}, build {}", info.major, info.minor, info.build)
    }

    /// How samples are laid out in a loopback stream.
    #[derive(Clone, Copy)]
    enum Samples {
        Float32,
        Int16,
        Int32,
    }

    struct MixFormat {
        channels: usize,
        samples: Option<Samples>,
        text: String,
    }

    fn read_format(format: *const WAVEFORMATEX) -> MixFormat {
        let (tag, channels, rate, bits) = unsafe {
            let f = format.read_unaligned();
            (f.wFormatTag, f.nChannels, f.nSamplesPerSec, f.wBitsPerSample)
        };
        let float = match tag {
            TAG_FLOAT => true,
            TAG_EXTENSIBLE => unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned().SubFormat.data1 == FORMAT_FLOAT },
            _ => false,
        };
        let samples = match (float, bits, tag) {
            (true, 32, _) => Some(Samples::Float32),
            (false, 16, TAG_PCM | TAG_EXTENSIBLE) => Some(Samples::Int16),
            (false, 32, TAG_PCM | TAG_EXTENSIBLE) => Some(Samples::Int32),
            _ => None,
        };
        let kind = if float { "float" } else { "integer" };
        MixFormat { channels: channels as usize, samples, text: format!("{rate} Hz, {channels} ch, {bits}-bit {kind}, tag 0x{tag:04X}") }
    }

    /// One running capture, stopped when it's dropped.
    struct Stream {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        channels: usize,
        samples: Samples,
        event: Option<HANDLE>,
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
                if let Some(event) = self.event {
                    let _ = CloseHandle(event);
                }
            }
        }
    }

    impl Stream {
        fn loopback(device: &IMMDevice) -> Result<Self, String> {
            unsafe {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| format!("activate: {e}"))?;
                let format = client.GetMixFormat().map_err(|e| format!("GetMixFormat: {e}"))?;
                let mix = read_format(format);
                let opened = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, LOOPBACK_BUFFER, 0, format, None);
                CoTaskMemFree(Some(format as *const _));
                opened.map_err(|e| format!("Initialize: {e}"))?;
                let samples = mix.samples.ok_or_else(|| format!("can't read {}", mix.text))?;
                let capture = client.GetService().map_err(|e| format!("GetService: {e}"))?;
                client.Start().map_err(|e| format!("Start: {e}"))?;
                Ok(Self { client, capture, channels: mix.channels, samples, event: None })
            }
        }

        fn excluding() -> Result<Self, String> {
            let client = activate_excluding_this_process().map_err(|e| format!("{e:?}"))?;
            unsafe {
                let event = CreateEventW(None, false, false, None).map_err(|e| format!("event: {e}"))?;
                let opened = open_exclusion(&client, event);
                let capture = match opened {
                    Ok(capture) => capture,
                    Err(e) => {
                        let _ = CloseHandle(event);
                        return Err(e);
                    }
                };
                let stream = Self { client, capture, channels: super::super::CHANNELS, samples: Samples::Int16, event: Some(event) };
                stream.client.Start().map_err(|e| format!("start: {e}"))?;
                Ok(stream)
            }
        }

        /// Takes all that's waiting, into `levels` if there's anywhere for it to go.
        fn drain(&self, mut levels: Option<&mut Levels>) -> Result<(), String> {
            loop {
                let waiting = unsafe { self.capture.GetNextPacketSize() }.map_err(|e| format!("GetNextPacketSize: {e}"))?;
                if waiting == 0 {
                    return Ok(());
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .map_err(|e| format!("GetBuffer: {e}"))?;
                if let Some(levels) = levels.as_deref_mut() {
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    levels.frames += u64::from(frames);
                    if silent || data.is_null() {
                        levels.silent_frames += u64::from(frames);
                        (0..frames as usize * self.channels).for_each(|_| levels.add(0.0));
                    } else {
                        self.count(data, frames as usize * self.channels, levels);
                    }
                }
                unsafe { self.capture.ReleaseBuffer(frames) }.map_err(|e| format!("ReleaseBuffer: {e}"))?;
            }
        }

        fn count(&self, data: *const u8, count: usize, levels: &mut Levels) {
            unsafe {
                match self.samples {
                    Samples::Float32 => {
                        std::slice::from_raw_parts(data.cast::<f32>(), count).iter().for_each(|s| levels.add(*s));
                    }
                    Samples::Int16 => {
                        std::slice::from_raw_parts(data.cast::<i16>(), count).iter().for_each(|s| levels.add(f32::from(*s) / 32768.0));
                    }
                    Samples::Int32 => {
                        std::slice::from_raw_parts(data.cast::<i32>(), count)
                            .iter()
                            .for_each(|s| levels.add(*s as f32 / 2_147_483_648.0));
                    }
                }
            }
        }
    }

    struct LiveMeter<'a> {
        device: &'a IMMDevice,
    }

    impl Meter for LiveMeter<'_> {
        fn measure(&mut self, capture: Capture, apply: &mut dyn FnMut() -> Result<(), String>) -> Result<Levels, String> {
            let stream = match capture {
                Capture::Loopback => Stream::loopback(self.device)?,
                Capture::Excluding => Stream::excluding()?,
            };
            apply()?;
            let started = Instant::now();
            while started.elapsed() < SETTLE {
                stream.drain(None)?;
                std::thread::sleep(POLL);
            }
            let mut levels = Levels::default();
            while started.elapsed() < SETTLE + MEASURE {
                stream.drain(Some(&mut levels))?;
                std::thread::sleep(POLL);
            }
            Ok(levels)
        }
    }

    fn describe_output(device: &IMMDevice, id: &str, volume: &IAudioEndpointVolume, info: &mut Info) {
        info.endpoint = wasapi::describe(device).map_or_else(|| id.to_string(), |d| d.name);
        unsafe {
            let (mut least, mut most, mut step) = (0.0, 0.0, 0.0);
            if volume.GetVolumeRange(&mut least, &mut most, &mut step).is_ok() {
                info.range = Some((least, most, step));
            }
            info.hardware_support = volume.QueryHardwareSupport().ok();
            info.found_db = volume.GetMasterVolumeLevel().ok();
            let client: windows::core::Result<IAudioClient> = device.Activate(CLSCTX_ALL, None);
            match client.and_then(|client| client.GetMixFormat()) {
                Ok(format) => {
                    info.mix_format = read_format(format).text;
                    CoTaskMemFree(Some(format as *const _));
                }
                Err(e) => info.mix_format = format!("unknown: {e}"),
            }
        }
        if let Some(found) = wasapi::read_lists() {
            info.others = found.lists.outputs.into_iter().filter(|d| d.id != id).map(|d| d.name).collect();
        }
    }

    pub fn run() -> i32 {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut info = Info { windows: windows_version(), ..Default::default() };
        match restore_from_record() {
            Ok(true) => info.notes.push("An earlier run didn't finish. Its volume and mute were put back first.".to_string()),
            Ok(false) => {}
            Err(reason) => return refuse(&info, &format!("An earlier run left the speakers changed, and they can't be put back yet: {reason}")),
        }
        if crate::mute::holds_mute() {
            return refuse(&info, "mesh-kvm is muting an output right now. Stop sending, or turn off Play only on the other device, and try again.");
        }
        let device = match enumerator().and_then(|all| unsafe { all.GetDefaultAudioEndpoint(eRender, eConsole) }) {
            Ok(device) => device,
            Err(e) => return refuse(&info, &format!("There's no default output: {e}")),
        };
        let id = unsafe { device.GetId() }.ok().and_then(|id| take_id(id));
        let (Some(id), Ok(endpoint)) = (id, volume_of(&device)) else {
            return refuse(&info, "The default output's volume can't be reached.");
        };
        describe_output(&device, &id, &endpoint, &mut info);
        let mut volume = LiveVolume(endpoint);
        let original = match volume.read() {
            Ok(original) => original,
            Err(reason) => return refuse(&info, &reason),
        };
        info.found = Some(original);
        if original.muted {
            return refuse(&info, "The speakers are muted, so there's nothing to compare with. Unmute them and try again.");
        }
        if boxes_wanted() && show(WHAT_HAPPENS, MB_OKCANCEL) != IDOK {
            return 0;
        }
        if let Err(reason) = write_record(&Record { device: id, speakers: original }) {
            return refuse(&info, &format!("The volume can't be written down first: {reason}"));
        }
        restore_on_panic();
        start_time_limit();
        let started = Instant::now();
        let mut may_go_on = || {
            if crate::mute::holds_mute() {
                return Err("mesh-kvm started muting an output, so this stopped.".to_string());
            }
            Ok(started.elapsed() + SETTLE + MEASURE <= CASES_BUDGET)
        };
        let result = run_cases(&mut volume, &mut LiveMeter { device: &device }, original, &mut may_go_on);
        FINISHED.store(true, Ordering::SeqCst);
        if result.not_restored.is_none() {
            let _ = std::fs::remove_file(record_path());
        }
        let failed = result.refused.is_some() || result.not_restored.is_some();
        finish(&format_report(&info, &result), i32::from(failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const FOUND: Speakers = Speakers { muted: false, scalar: 0.5 };

    #[derive(Default)]
    struct Log {
        now: Option<Speakers>,
        /// The speakers at each moment a case's capture was running.
        during: Vec<Speakers>,
        /// The speakers just before each case.
        before: Vec<Speakers>,
    }

    #[derive(Clone)]
    struct FakeVolume {
        log: Rc<RefCell<Log>>,
        /// Fails to put the speakers back.
        stuck: Rc<RefCell<bool>>,
        /// Refuses any change of volume.
        fixed_volume: Rc<RefCell<bool>>,
    }

    fn fake() -> FakeVolume {
        let log = Log { now: Some(FOUND), ..Default::default() };
        FakeVolume { log: Rc::new(RefCell::new(log)), stuck: Rc::default(), fixed_volume: Rc::default() }
    }

    impl FakeVolume {
        fn now(&self) -> Speakers {
            self.log.borrow().now.unwrap()
        }
    }

    impl Volume for FakeVolume {
        fn read(&mut self) -> Result<Speakers, String> {
            Ok(self.now())
        }

        fn set_muted(&mut self, muted: bool) -> Result<(), String> {
            if *self.stuck.borrow() && !muted {
                return Err("no".into());
            }
            self.log.borrow_mut().now.as_mut().unwrap().muted = muted;
            Ok(())
        }

        fn set_scalar(&mut self, scalar: f32) -> Result<(), String> {
            if *self.fixed_volume.borrow() {
                return Err("SetMasterVolumeLevelScalar: Not implemented.".into());
            }
            self.log.borrow_mut().now.as_mut().unwrap().scalar = scalar;
            Ok(())
        }

        fn level_db(&mut self) -> Option<f32> {
            Some(20.0 * self.now().scalar.max(0.0001).log10())
        }
    }

    fn levels(sample: f32) -> Levels {
        let mut levels = Levels { frames: 100, ..Default::default() };
        (0..200).for_each(|_| levels.add(sample));
        levels
    }

    /// A capture that follows the speakers, like Windows does on the real laptop.
    struct FakeMeter {
        volume: FakeVolume,
        exclusion_error: Option<String>,
        fail_case: Option<u32>,
        panic_on: Option<u32>,
        silent: bool,
        count: u32,
    }

    fn meter(volume: &FakeVolume) -> FakeMeter {
        FakeMeter { volume: volume.clone(), exclusion_error: None, fail_case: None, panic_on: None, silent: false, count: 0 }
    }

    impl Meter for FakeMeter {
        fn measure(&mut self, capture: Capture, apply: &mut dyn FnMut() -> Result<(), String>) -> Result<Levels, String> {
            self.count += 1;
            let before = self.volume.now();
            self.volume.log.borrow_mut().before.push(before);
            if capture == Capture::Excluding {
                if let Some(error) = &self.exclusion_error {
                    return Err(error.clone());
                }
            }
            apply()?;
            let during = self.volume.now();
            self.volume.log.borrow_mut().during.push(during);
            if self.panic_on == Some(self.count) {
                panic!("the capture fell over");
            }
            if self.fail_case == Some(self.count) {
                return Err("GetBuffer: device gone".into());
            }
            let sample = if self.silent || during.muted { 0.0 } else { 0.4 * during.scalar };
            Ok(levels(sample))
        }
    }

    fn go_on() -> impl FnMut() -> Result<bool, String> {
        || Ok(true)
    }

    #[test]
    fn every_case_runs_in_the_right_state_and_the_speakers_come_back_each_time() {
        let mut volume = fake();
        let mut m = meter(&volume);
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.cases.len(), 8);
        assert_eq!(run.refused, None);
        let log = volume.log.borrow();
        assert!(log.before.iter().all(|s| *s == FOUND), "put back after each one: {:?}", log.before);
        let during = |i: usize| log.during[i];
        assert_eq!(during(0), FOUND);
        assert!(during(1).muted && during(5).muted);
        assert_eq!((during(2).scalar, during(6).scalar), (0.01, 0.01));
        assert!(!during(3).muted && during(3).scalar == 0.0 && during(7).scalar == 0.0);
        assert_eq!(log.now, Some(FOUND));
    }

    #[test]
    fn a_failed_capture_still_puts_the_speakers_back_and_the_rest_carry_on() {
        let mut volume = fake();
        let mut m = meter(&volume);
        m.fail_case = Some(2);
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.cases[1].outcome, Outcome::Failed("GetBuffer: device gone".into()));
        assert!(matches!(run.cases[2].outcome, Outcome::Measured(_)));
        assert_eq!(volume.now(), FOUND);
    }

    #[test]
    fn a_panicking_capture_still_puts_the_speakers_back() {
        let mut volume = fake();
        let mut m = meter(&volume);
        m.panic_on = Some(2);
        let mut for_run = volume.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_cases(&mut for_run, &mut m, FOUND, &mut go_on());
        }));
        assert!(result.is_err());
        assert_eq!(volume.now(), FOUND);
        let _ = &mut volume;
    }

    #[test]
    fn exclusion_failing_is_reported_exactly_and_tried_only_once() {
        let mut volume = fake();
        let mut m = meter(&volume);
        let error = "Failed(\"activated: HRESULT(0x8007000E)\")".to_string();
        m.exclusion_error = Some(error.clone());
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.cases[4].outcome, Outcome::Failed(error));
        for case in &run.cases[5..] {
            assert_eq!(case.outcome, Outcome::Skipped("as case 5".into()));
        }
        assert_eq!(m.count, 5);
        assert_eq!(volume.now(), FOUND);
    }

    #[test]
    fn no_sound_in_the_baseline_stops_the_run_before_anything_changes_much() {
        let mut volume = fake();
        let mut m = meter(&volume);
        m.silent = true;
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.cases.len(), 1);
        assert_eq!(run.refused.as_deref(), Some(NO_SOUND_MESSAGE));
        assert_eq!(volume.now(), FOUND);
        assert_eq!(volume.log.borrow().during.len(), 1, "no muting happened");
    }

    #[test]
    fn a_volume_that_can_t_be_set_fails_its_cases_and_nothing_else() {
        let mut volume = fake();
        *volume.fixed_volume.borrow_mut() = true;
        let mut m = meter(&volume);
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.not_restored, None, "nothing moved, so there's nothing to put back");
        assert_eq!(run.cases.len(), 8);
        assert!(matches!(&run.cases[2].outcome, Outcome::Failed(e) if e.contains("Not implemented")));
        assert!(matches!(run.cases[1].outcome, Outcome::Measured(_)));
    }

    #[test]
    fn speakers_that_won_t_come_back_stop_everything_and_say_so() {
        let mut volume = fake();
        let mut m = meter(&volume);
        *volume.stuck.borrow_mut() = true;
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        assert_eq!(run.cases.len(), 2, "no more changes once one can't be undone");
        assert!(run.not_restored.is_some());
        assert!(format_report(&Info::default(), &run).contains("couldn't be put back"));
    }

    #[test]
    fn the_service_starting_to_mute_stops_the_run() {
        let mut volume = fake();
        let mut m = meter(&volume);
        let mut asked = 0;
        let mut go = || {
            asked += 1;
            if asked == 3 {
                Err("mesh-kvm started muting".to_string())
            } else {
                Ok(true)
            }
        };
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go);
        assert_eq!(run.cases.len(), 2);
        assert_eq!(run.refused.as_deref(), Some("mesh-kvm started muting"));
        assert_eq!(volume.now(), FOUND);
    }

    #[test]
    fn running_out_of_time_skips_what_is_left_and_finishes_cleanly() {
        let mut volume = fake();
        let mut m = meter(&volume);
        let mut left = 3;
        let mut go = || {
            left -= 1;
            Ok(left >= 0)
        };
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go);
        assert_eq!(m.count, 3);
        assert_eq!(run.cases[7].outcome, Outcome::Skipped("out of time".into()));
    }

    fn sample_run() -> (Info, Run) {
        let mut volume = fake();
        let mut m = meter(&volume);
        let run = run_cases(&mut volume, &mut m, FOUND, &mut go_on());
        let info = Info {
            windows: "10.0, build 26100".into(),
            endpoint: "Speakers (Realtek(R) Audio)".into(),
            mix_format: "48000 Hz, 2 ch, 32-bit float, tag 0xFFFE".into(),
            range: Some((-65.25, 0.0, 0.03)),
            found: Some(FOUND),
            found_db: Some(-10.2),
            hardware_support: Some(3),
            others: vec!["Steam Streaming Speakers".into()],
            notes: vec![],
        };
        (info, run)
    }

    #[test]
    fn the_report_has_everything_asked_for() {
        let (info, run) = sample_run();
        let text = format_report(&info, &run);
        for wanted in [
            "Windows: 10.0, build 26100",
            "Endpoint: Speakers (Realtek(R) Audio)",
            "Mix format: 48000 Hz, 2 ch, 32-bit float",
            "Volume range: -65.25 to 0.00 dB, step 0.03 dB",
            "Found: not muted, scalar 0.500, -10.2 dB",
            "Hardware support: 0x00000003 (volume, mute)",
            "Other outputs: Steam Streaming Speakers",
            "1 | Loopback | as found | vol -6.0 dB | peak 0.2000 | RMS -14.0 dB | 100 frames, 0% flagged silent",
            "2 | Loopback | muted | vol -6.0 dB | peak 0.0000",
            "8 | Exclusion | volume 0.00",
        ] {
            assert!(text.contains(wanted), "{wanted}\n{text}");
        }
    }

    #[test]
    fn the_two_quiet_cases_say_how_far_below_their_baseline_they_are() {
        let (info, run) = sample_run();
        let text = format_report(&info, &run);
        // 0.5 to 0.01 is 34 dB, on the volume and in the capture.
        let line = |n: &str| text.lines().find(|l| l.starts_with(n)).unwrap().to_string();
        assert!(line("3 |").contains("-34.0 dB vs case 1, volume moved -34.0 dB"), "{}", line("3 |"));
        assert!(line("7 |").contains("-34.0 dB vs case 5"), "{}", line("7 |"));
        assert!(!line("2 |").contains("vs case"));
    }

    #[test]
    fn a_failed_baseline_or_silent_capture_has_nothing_to_compare_with() {
        let (info, mut run) = sample_run();
        run.cases[4].outcome = Outcome::Failed("no".into());
        run.cases[0].outcome = Outcome::Measured(levels(0.0));
        let text = format_report(&info, &run);
        assert!(text.contains("no level to compare with case 1"), "{text}");
        assert!(text.contains("no case 5 to compare"), "{text}");
    }

    #[test]
    fn a_refusal_comes_first_and_shows_what_is_known() {
        let run = Run { refused: Some("Nothing playing.".into()), ..Default::default() };
        let text = format_report(&Info { windows: "10.0".into(), ..Default::default() }, &run);
        assert!(text.starts_with("mesh-kvm audio probe\r\nStopped: Nothing playing."));
        assert!(text.contains("Windows: 10.0"));
        assert!(!text.contains("Endpoint:"), "nothing was found out about it yet");
    }

    #[test]
    fn the_hardware_mask_is_spelled_out() {
        assert_eq!(support_text(0), "0x00000000 (software only)");
        assert_eq!(support_text(7), "0x00000007 (volume, mute, meter)");
    }

    #[test]
    fn levels_add_up() {
        let mut l = Levels::default();
        for s in [0.5, -0.5, 0.5, -0.5] {
            l.add(s);
        }
        assert_eq!(l.peak, 0.5);
        assert!((l.rms() - 0.5).abs() < 1e-9);
        assert_eq!(Levels::default().rms(), 0.0);
    }

    #[test]
    fn the_record_survives_being_written_down() {
        let record = Record { device: "{0.0.0.00000000}.{abc}".into(), speakers: FOUND };
        let text = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&text).unwrap(), record);
    }

    #[test]
    fn the_name_or_the_flag_asks_for_the_probe() {
        let none = || std::iter::empty::<String>();
        assert!(asked_for("mesh-kvm-audio-probe.exe", none()));
        assert!(asked_for("Mesh-KVM-Audio-Probe (1).exe", none()));
        assert!(!asked_for("mesh-kvm.exe", none()));
        assert!(asked_for("mesh-kvm.exe", ["--audio-probe".to_string()].into_iter()));
        assert!(!asked_for("mesh-kvm.exe", ["--app".to_string()].into_iter()));
    }
}
