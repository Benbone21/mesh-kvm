//! WASAPI: device lists and process-exclusion loopback directly, everything else through `cpal`.
//!
//! An output's sound comes from loopback capture of its endpoint, which is exact but includes
//! everything playing there. When this device also plays received audio on that same endpoint, and
//! both directions are in use, that would start an echo. Then capture changes to process-exclusion
//! loopback, which takes all sound except this process's. That API can't name an endpoint, so it's
//! kept for that one case, and anything going wrong with it falls back to ordinary loopback and
//! says so in the status.

use super::devices::{self, CaptureHealth, Device, InUse, Lists, Resolved};
use super::jitter::RxStream;
use super::mix::{Signal, Source};
use super::resample::Resampler;
use super::{drain_received, finish_received, AUDIO_OUTPUT_CAPTURE_LIVE, AUDIO_PLAYER_LIVE, AUDIO_RX_MUTED, AUDIO_RX_PEAK_PCT, CHANNELS, HUB, RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, Result as WinResult, BSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, EDataFlow, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    DEVICE_STATE_ACTIVE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IAgileObject, IAgileObject_Impl, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};

/// How often the devices are looked at, so that plugging and unplugging is picked up.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// A stream that won't stay up isn't restarted more often than this.
const RESPAWN_GAP: Duration = Duration::from_secs(1);
const STREAM_TICK: Duration = Duration::from_millis(50);
const PLAYER_TICK: Duration = Duration::from_millis(20);
const PLAYER_OFF_TICK: Duration = Duration::from_millis(20);
/// The output is let go once nothing has arrived for this long.
const PLAYER_IDLE_CLOSE: Duration = Duration::from_secs(2);
/// The longest process loopback is given to come up.
const ACTIVATE_WAIT: Duration = Duration::from_secs(3);
/// How much the shared engine holds for process loopback, in 100 ns units: 20 ms.
const PROCESS_BUFFER: i64 = 200_000;
const PROCESS_WAIT_MS: u32 = 100;
const BITS: u16 = 16;
/// How long a stream failing stays in the notes.
const FAILURE_SHOWS_FOR: Duration = Duration::from_secs(4);
/// What to try when a microphone gives nothing. Windows can block one for every app.
const MIC_HELP: &str = "Check that Microphone access is on in Windows privacy settings.";

/// Wakes the engine when the choices change.
static WAKE: LazyLock<Signal> = LazyLock::new(Signal::default);
/// Audio is arriving, so both directions may be in use.
static RECEIVING: AtomicBool = AtomicBool::new(false);
/// Process loopback failed here, so it isn't tried again until the service restarts.
static EXCLUSION_FAILED: AtomicBool = AtomicBool::new(false);
/// The friendly name of the output to play on, with `None` meaning the default.
static PLAY_DEVICE: Mutex<Option<String>> = Mutex::new(None);

pub fn choices_changed() {
    WAKE.notify();
}

// ============================================================================================
// Device lists
// ============================================================================================

fn enumerator() -> WinResult<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn take_id(id: PWSTR) -> Option<String> {
    unsafe {
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

fn describe(device: &IMMDevice) -> Option<Device> {
    unsafe {
        let id = take_id(device.GetId().ok()?)?;
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = BSTR::try_from(&value).ok()?.to_string();
        Some(Device { id, name })
    }
}

fn endpoints(all: &IMMDeviceEnumerator, flow: EDataFlow) -> Vec<Device> {
    let Ok(collection) = (unsafe { all.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }) else {
        return Vec::new();
    };
    let count = unsafe { collection.GetCount() }.unwrap_or(0);
    let mut devices: Vec<Device> =
        (0..count).filter_map(|index| describe(&unsafe { collection.Item(index) }.ok()?)).collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    devices
}

fn default_endpoint(all: &IMMDeviceEnumerator, flow: EDataFlow) -> Option<String> {
    unsafe { take_id(all.GetDefaultAudioEndpoint(flow, eConsole).ok()?.GetId().ok()?) }
}

fn read_lists() -> Option<Lists> {
    let all = enumerator().ok()?;
    Some(Lists {
        inputs: endpoints(&all, eCapture),
        outputs: endpoints(&all, eRender),
        default_input: default_endpoint(&all, eCapture),
        default_output: default_endpoint(&all, eRender),
    })
}

/// `cpal` finds devices by name, which is all it shows of them.
fn name_of(id: &str, devices: &[Device]) -> Option<String> {
    devices.iter().find(|device| device.id == id).map(|device| device.name.clone())
}

// ============================================================================================
// Running streams
// ============================================================================================

/// A stream on a thread of its own, as `cpal` streams can't move between threads.
struct Worker<K> {
    key: Option<K>,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    last_start: Option<Instant>,
    /// When it last ended by itself.
    failed_at: Option<Instant>,
}

impl<K: PartialEq> Default for Worker<K> {
    fn default() -> Self {
        Self {
            key: None,
            stop: Arc::default(),
            finished: Arc::new(AtomicBool::new(true)),
            last_start: None,
            failed_at: None,
        }
    }
}

impl<K: PartialEq> Worker<K> {
    fn alive(&self) -> bool {
        self.key.is_some() && !self.finished.load(Ordering::SeqCst)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.key = None;
    }

    /// How a capture on this worker is doing.
    fn health(&self, source: &Source, expected: bool, microphone: bool, now: Instant) -> CaptureHealth {
        let up_for = self.last_start.filter(|_| expected && self.alive()).map(|at| now.duration_since(at));
        CaptureHealth {
            failed: self.failed_at.is_some_and(|at| now.duration_since(at) < FAILURE_SHOWS_FOR),
            expected_for: up_for,
            activity: source.activity(),
            microphone,
        }
    }

    /// Brings the worker in line with `wanted`, starting `run` if that takes a new stream. `run`
    /// returns when it's told to stop, or when the device goes away.
    fn ensure<F>(&mut self, wanted: Option<K>, now: Instant, run: F)
    where
        F: FnOnce(&K, Arc<AtomicBool>) + Send + 'static,
        K: Clone + Send + 'static,
    {
        if self.key.is_some() && !self.alive() {
            self.failed_at = Some(now);
        }
        if self.key != wanted || !self.alive() {
            self.stop();
        }
        let Some(key) = wanted else { return };
        if self.key.is_some() || self.last_start.is_some_and(|at| now.duration_since(at) < RESPAWN_GAP) {
            return;
        }
        self.last_start = Some(now);
        let (stop, finished) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        self.stop = Arc::clone(&stop);
        self.finished = Arc::clone(&finished);
        self.key = Some(key.clone());
        let spawned = std::thread::Builder::new().name("mesh-kvm-audio-stream".to_string()).spawn(move || {
            run(&key, stop);
            finished.store(true, Ordering::SeqCst);
        });
        if spawned.is_err() {
            self.key = None;
        }
    }
}

/// Keeps a built stream going until told to stop or until it reports an error.
fn hold(stream: cpal::Stream, stop: &AtomicBool, failed: &AtomicBool) {
    if stream.play().is_ok() {
        while !stop.load(Ordering::SeqCst) && !failed.load(Ordering::SeqCst) {
            std::thread::sleep(STREAM_TICK);
        }
    }
}

// ============================================================================================
// Capture
// ============================================================================================

/// Turns whatever a device delivers into 48 kHz stereo and hands it on.
struct Intake {
    source: &'static Source,
    channels: usize,
    resampler: Option<Resampler>,
    stereo: Vec<f32>,
    out: Vec<f32>,
}

impl Intake {
    fn new(source: &'static Source, channels: usize, rate: u32) -> Self {
        Self {
            source,
            channels: channels.max(1),
            resampler: (rate != RATE).then(|| Resampler::new(rate, RATE)),
            stereo: Vec::new(),
            out: Vec::new(),
        }
    }

    fn take<T: Copy>(&mut self, data: &[T], to_f32: impl Fn(T) -> f32) {
        self.stereo.clear();
        for frame in data.chunks_exact(self.channels) {
            let left = to_f32(frame[0]);
            let right = frame.get(1).map_or(left, |right| to_f32(*right));
            self.stereo.extend_from_slice(&[left, right]);
        }
        let samples = match &mut self.resampler {
            Some(resampler) => {
                self.out.clear();
                resampler.process(&self.stereo, &mut self.out);
                &mut self.out
            }
            None => &mut self.stereo,
        };
        HUB.capture(self.source, samples);
    }
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    source: &'static Source,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut intake = Intake::new(source, config.channels as usize, config.sample_rate.0);
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| intake.take(data, |sample| sample.to_sample::<f32>()),
        move |e| {
            eprintln!("[Audio] Capture stopped: {e}");
            failed.store(true, Ordering::SeqCst);
        },
        None,
    )
}

/// Captures one device until told to stop or until it goes away. On WASAPI, an input stream on an
/// output device is loopback capture of that output.
fn capture_device(device: Option<cpal::Device>, loopback: bool, source: &'static Source, stop: &AtomicBool) {
    let Some(device) = device else { return };
    let supported = if loopback { device.default_output_config() } else { device.default_input_config() };
    let Ok(supported) = supported else { return };
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let failed = Arc::new(AtomicBool::new(false));
    let stream = match format {
        cpal::SampleFormat::F32 => build_input::<f32>(&device, &config, source, Arc::clone(&failed)),
        cpal::SampleFormat::I16 => build_input::<i16>(&device, &config, source, Arc::clone(&failed)),
        cpal::SampleFormat::I32 => build_input::<i32>(&device, &config, source, Arc::clone(&failed)),
        other => {
            eprintln!("[Audio] Unsupported sample format: {other:?}");
            return;
        }
    };
    match stream {
        Ok(stream) => hold(stream, stop, &failed),
        Err(e) => eprintln!("[Audio] Couldn't open the capture: {e}"),
    }
    source.clear();
}

fn find_input(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    match name {
        Some(name) => host.input_devices().ok()?.find(|device| device.name().is_ok_and(|n| n == name)),
        None => host.default_input_device(),
    }
}

fn find_output(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    match name {
        Some(name) => host.output_devices().ok()?.find(|device| device.name().is_ok_and(|n| n == name)),
        None => host.default_output_device(),
    }
}

/// How an output is being captured.
#[derive(Clone, PartialEq, Eq)]
enum OutputCapture {
    /// Loopback of one endpoint, or the default with `None`.
    Endpoint(Option<String>),
    /// All sound on this device except this process's.
    Excluding,
}

/// Tells the waiting thread that activation is over.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated {
    done: std::sync::mpsc::Sender<()>,
}

impl IAgileObject_Impl for Activated_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Option<&IActivateAudioInterfaceAsyncOperation>) -> WinResult<()> {
        let _ = self.done.send(());
        Ok(())
    }
}

/// A `PROPVARIANT` holding a blob, which is all `ActivateAudioInterfaceAsync` asks of one.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

const VT_BLOB: u16 = 65;

fn activate_excluding_this_process() -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: unsafe { GetCurrentProcessId() },
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = BlobVariant {
        vt: VT_BLOB,
        reserved: [0; 3],
        size: std::mem::size_of_val(&params) as u32,
        data: std::ptr::from_ref(&params).cast(),
    };
    let (done, wait) = std::sync::mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated { done }.into();
    unsafe {
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(std::ptr::from_ref(&blob).cast()),
            &handler,
        )
        .map_err(|e| format!("activation: {e}"))?;
        wait.recv_timeout(ACTIVATE_WAIT).map_err(|_| "activation timed out".to_string())?;
        let (mut result, mut interface) = (windows::core::HRESULT(0), None);
        operation.GetActivateResult(&mut result, &mut interface).map_err(|e| format!("result: {e}"))?;
        result.ok().map_err(|e| format!("activated: {e}"))?;
        interface.ok_or("no client came back")?.cast().map_err(|e| format!("client: {e}"))
    }
}

/// Takes everything waiting in the capture client. False means it has stopped working.
fn read_packets(capture: &IAudioCaptureClient, intake: &mut Intake) -> bool {
    loop {
        match unsafe { capture.GetNextPacketSize() } {
            Ok(0) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
        let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
        if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err() {
            return false;
        }
        let count = frames as usize * CHANNELS;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            intake.take(&vec![0i16; count], |_| 0.0);
        } else {
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<i16>(), count) };
            intake.take(samples, |sample| f32::from(sample) / 32768.0);
        }
        if unsafe { capture.ReleaseBuffer(frames) }.is_err() {
            return false;
        }
    }
}

fn run_exclusion(client: &IAudioClient, event: HANDLE, stop: &AtomicBool) -> Result<(), String> {
    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * u32::from(BITS) / 8,
        nBlockAlign: CHANNELS as u16 * BITS / 8,
        wBitsPerSample: BITS,
        cbSize: 0,
    };
    unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                PROCESS_BUFFER,
                0,
                &format,
                None,
            )
            .map_err(|e| format!("initialise: {e}"))?;
        client.SetEventHandle(event).map_err(|e| format!("event: {e}"))?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("service: {e}"))?;
        client.Start().map_err(|e| format!("start: {e}"))?;
        let mut intake = Intake::new(&HUB.output, CHANNELS, RATE);
        while !stop.load(Ordering::SeqCst) {
            if WaitForSingleObject(event, PROCESS_WAIT_MS) == WAIT_OBJECT_0 && !read_packets(&capture, &mut intake) {
                let _ = client.Stop();
                return Err("capture stopped".to_string());
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

/// Captures all sound but this process's, until told to stop.
fn capture_excluding(stop: &AtomicBool) -> Result<(), String> {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let client = activate_excluding_this_process()?;
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| format!("event: {e}"))?;
    let result = run_exclusion(&client, event, stop);
    let _ = unsafe { CloseHandle(event) };
    result
}

fn run_output_capture(how: &OutputCapture, stop: Arc<AtomicBool>) {
    match how {
        OutputCapture::Endpoint(name) => capture_device(find_output(name.as_deref()), true, &HUB.output, &stop),
        OutputCapture::Excluding => {
            if let Err(e) = capture_excluding(&stop) {
                crate::settings::log_note(&format!(
                    "Couldn't leave this device's own sound out of what it sends ({e}). Using ordinary capture instead."
                ));
                EXCLUSION_FAILED.store(true, Ordering::SeqCst);
                WAKE.notify();
            }
            HUB.output.clear();
        }
    }
}

/// Whether to capture with process exclusion. An echo needs received audio playing on the same
/// endpoint that's being captured, so for anything else, loopback of the endpoint is the better
/// capture: it's what the dropdown asks for, and it can't echo.
fn needs_exclusion(sent: Option<&str>, play: Option<&str>, receiving: bool, failed: bool) -> bool {
    sent.is_some() && sent == play && receiving && !failed
}

/// Whether what's sent can be trusted to leave out what's played, now or as soon as it matters.
fn echo_safe(sent: Option<&str>, play: Option<&str>, failed: bool) -> bool {
    sent != play || !failed
}

fn engine() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut mic = Worker::<Option<String>>::default();
    let mut output = Worker::<OutputCapture>::default();
    loop {
        WAKE.wait(LOOK_EVERY);
        let now = Instant::now();
        let Some(lists) = read_lists() else { continue };
        let choice = devices::choice();
        let input = devices::resolve(&choice.input, &lists.inputs);
        let sent = devices::resolve(&choice.output, &lists.outputs);
        let play = match devices::resolve(&choice.play, &lists.outputs) {
            Resolved::Off => Resolved::Default,
            other => other,
        };
        let sent_id = sent.id(lists.default_output.as_ref());
        let play_id = play.id(lists.default_output.as_ref());
        let failed = EXCLUSION_FAILED.load(Ordering::SeqCst);
        let receiving = RECEIVING.load(Ordering::SeqCst);
        let excluding = needs_exclusion(sent_id.as_deref(), play_id.as_deref(), receiving, failed);

        let mic_name = input.target().and_then(|id| name_of(&id, &lists.inputs));
        mic.ensure((input != Resolved::Off).then_some(mic_name), now, |name, stop| {
            capture_device(find_input(name.as_deref()), false, &HUB.mic, &stop);
        });

        // By name even for the default, so that a change of default restarts the capture.
        let sent_name = sent_id.as_deref().and_then(|id| name_of(id, &lists.outputs));
        let how = if excluding { OutputCapture::Excluding } else { OutputCapture::Endpoint(sent_name) };
        output.ensure((sent != Resolved::Off).then_some(how), now, run_output_capture);
        AUDIO_OUTPUT_CAPTURE_LIVE.store(output.alive(), Ordering::Relaxed);

        *PLAY_DEVICE.lock().unwrap_or_else(PoisonError::into_inner) =
            play_id.as_deref().and_then(|id| name_of(id, &lists.outputs));

        let whole_device = excluding && choice.output != devices::DEFAULT;
        let notes = [
            input.missing_note("microphone or line-in"),
            sent.missing_note("output to send"),
            play.missing_note("output to play on"),
            whole_device.then(|| "While both devices are sending, all of this device's sound is sent, not just this output's.".to_string()),
            (input != Resolved::Off)
                .then(|| mic.health(&HUB.mic, true, true, now).note("microphone or line-in", MIC_HELP, now))
                .flatten(),
            // Loopback delivers nothing while nothing plays, so only a failure says anything here.
            (sent != Resolved::Off)
                .then(|| output.health(&HUB.output, false, false, now).note("output to send", "", now))
                .flatten(),
        ];
        devices::set_in_use(InUse {
            output: sent_id.clone(),
            play: play_id.clone(),
            echo_safe: echo_safe(sent_id.as_deref(), play_id.as_deref(), failed),
            notes: notes.into_iter().flatten().collect(),
        });
        devices::set_lists(lists);
    }
}

pub fn start_engine() {
    let spawned = std::thread::Builder::new().name("mesh-kvm-audio-engine".to_string()).spawn(engine);
    if let Err(e) = spawned {
        crate::settings::log_note(&format!("Couldn't start audio devices: {e}"));
    }
    WAKE.notify();
}

// ============================================================================================
// Playback
// ============================================================================================

/// Pulls from the stream at the device's pace and turns it into the device's rate and channels.
struct Outlet {
    stream: Arc<Mutex<RxStream>>,
    channels: usize,
    resampler: Option<Resampler>,
    ready: VecDeque<f32>,
    pulled: Vec<f32>,
    resampled: Vec<f32>,
}

impl Outlet {
    fn new(stream: Arc<Mutex<RxStream>>, channels: usize, rate: u32) -> Self {
        Self {
            stream,
            channels: channels.max(1),
            resampler: (rate != RATE).then(|| Resampler::new(RATE, rate)),
            ready: VecDeque::new(),
            pulled: vec![0.0; 120 * CHANNELS],
            resampled: Vec::new(),
        }
    }

    fn refill(&mut self, needed: usize) {
        let now = Instant::now();
        while self.ready.len() < needed {
            self.stream.lock().unwrap_or_else(PoisonError::into_inner).fill(&mut self.pulled, now);
            finish_received(&mut self.pulled);
            match &mut self.resampler {
                Some(resampler) => {
                    self.resampled.clear();
                    resampler.process(&self.pulled, &mut self.resampled);
                    self.ready.extend(&self.resampled);
                }
                None => self.ready.extend(&self.pulled),
            }
        }
    }

    fn give<T: SizedSample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let frames = out.len() / self.channels;
        self.refill(frames * CHANNELS);
        for frame in out.chunks_exact_mut(self.channels) {
            let (left, right) = (self.ready.pop_front().unwrap_or(0.0), self.ready.pop_front().unwrap_or(0.0));
            match frame {
                [only] => *only = T::from_sample((left + right) / 2.0),
                [first, second, rest @ ..] => {
                    *first = T::from_sample(left);
                    *second = T::from_sample(right);
                    rest.iter_mut().for_each(|sample| *sample = T::from_sample(0.0));
                }
                [] => {}
            }
        }
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    stream: Arc<Mutex<RxStream>>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let mut outlet = Outlet::new(stream, config.channels as usize, config.sample_rate.0);
    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| outlet.give(out),
        move |e| {
            eprintln!("[Audio] Playback stopped: {e}");
            failed.store(true, Ordering::SeqCst);
        },
        None,
    )
}

fn play_on(name: Option<&str>, stream: Arc<Mutex<RxStream>>, stop: &AtomicBool) {
    let Some(device) = find_output(name) else { return };
    let Ok(supported) = device.default_output_config() else { return };
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let failed = Arc::new(AtomicBool::new(false));
    let built = match format {
        cpal::SampleFormat::F32 => build_output::<f32>(&device, &config, stream, Arc::clone(&failed)),
        cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, stream, Arc::clone(&failed)),
        cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, stream, Arc::clone(&failed)),
        other => {
            eprintln!("[Audio] Unsupported sample format: {other:?}");
            return;
        }
    };
    match built {
        Ok(stream) => {
            AUDIO_PLAYER_LIVE.store(true, Ordering::Relaxed);
            hold(stream, stop, &failed);
        }
        Err(e) => eprintln!("[Audio] Couldn't open the output: {e}"),
    }
    AUDIO_PLAYER_LIVE.store(false, Ordering::Relaxed);
}

fn supervise_playback(stream: Arc<Mutex<RxStream>>) {
    let mut player = Worker::<Option<String>>::default();
    let mut last_alive = None;
    loop {
        let now = Instant::now();
        let alive = stream.lock().unwrap_or_else(PoisonError::into_inner).alive(now);
        if alive {
            last_alive = Some(now);
        }
        if RECEIVING.swap(alive, Ordering::SeqCst) != alive {
            WAKE.notify();
        }
        let muted = AUDIO_RX_MUTED.load(Ordering::Relaxed);
        let idle = last_alive.is_none_or(|at| now.duration_since(at) >= PLAYER_IDLE_CLOSE);
        let wanted = (!muted && !idle).then(|| PLAY_DEVICE.lock().unwrap_or_else(PoisonError::into_inner).clone());
        let for_player = Arc::clone(&stream);
        player.ensure(wanted, now, move |name, stop| play_on(name.as_deref(), for_player, &stop));
        if muted && alive {
            drain_received(&stream, PLAYER_OFF_TICK, now);
        } else if muted || idle {
            let peak = AUDIO_RX_PEAK_PCT.load(Ordering::Relaxed);
            AUDIO_RX_PEAK_PCT.store(peak / 2, Ordering::Relaxed);
        }
        std::thread::sleep(if muted { PLAYER_OFF_TICK } else { PLAYER_TICK });
    }
}

pub fn start_playback(stream: Arc<Mutex<RxStream>>) {
    let spawned = std::thread::Builder::new()
        .name("mesh-kvm-audio-play".to_string())
        .spawn(move || supervise_playback(stream));
    if let Err(e) = spawned {
        crate::settings::log_note(&format!("Couldn't start audio playback: {e}"));
    }
}

/// Windows has no way to play the test sound on its own speakers from here.
pub fn play_chime(_: &[f32]) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_is_only_used_when_an_echo_could_really_happen() {
        let (a, b) = (Some("speakers"), Some("headset"));
        assert!(needs_exclusion(a, a, true, false));
        assert!(!needs_exclusion(a, b, true, false), "playing elsewhere: plain loopback can't echo");
        assert!(!needs_exclusion(a, a, false, false), "nothing arriving: nothing to echo");
        assert!(!needs_exclusion(a, a, true, true), "it failed here, so it's not tried again");
        assert!(!needs_exclusion(None, None, true, false), "nothing being sent");
    }

    #[test]
    fn a_failure_is_only_an_echo_risk_when_the_two_outputs_are_one() {
        let (a, b) = (Some("speakers"), Some("headset"));
        assert!(echo_safe(a, b, true));
        assert!(echo_safe(a, a, false));
        assert!(!echo_safe(a, a, true));
    }

    #[test]
    fn a_device_is_found_by_its_id_and_a_stranger_is_not() {
        let list = vec![Device { id: "{1}".into(), name: "Speakers".into() }];
        assert_eq!(name_of("{1}", &list).as_deref(), Some("Speakers"));
        assert_eq!(name_of("{2}", &list), None);
    }

    #[test]
    fn the_blob_is_laid_out_as_a_propvariant() {
        assert_eq!(std::mem::size_of::<BlobVariant>(), 24);
        assert_eq!(std::mem::offset_of!(BlobVariant, size), 8);
        assert_eq!(std::mem::offset_of!(BlobVariant, data), 16);
    }

    #[test]
    fn a_worker_restarts_only_when_it_has_to() {
        let starts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut worker = Worker::<u32>::default();
        let t = Instant::now();
        let run = |starts: &Arc<std::sync::atomic::AtomicU32>| {
            let starts = Arc::clone(starts);
            move |_: &u32, stop: Arc<AtomicBool>| {
                starts.fetch_add(1, Ordering::SeqCst);
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };
        worker.ensure(Some(1), t, run(&starts));
        worker.ensure(Some(1), t + RESPAWN_GAP, run(&starts));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(starts.load(Ordering::SeqCst), 1, "same key, same stream");
        worker.ensure(Some(2), t + RESPAWN_GAP, run(&starts));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        worker.ensure(None, t + RESPAWN_GAP * 2, run(&starts));
        assert!(!worker.alive());
    }

    #[test]
    fn a_failing_stream_is_not_restarted_more_than_once_a_second() {
        let starts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut worker = Worker::<u32>::default();
        let t = Instant::now();
        for ms in (0..1000).step_by(100) {
            let starts = Arc::clone(&starts);
            worker.ensure(Some(1), t + Duration::from_millis(ms), move |_, _| {
                starts.fetch_add(1, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mono_and_surround_devices_map_to_stereo() {
        let mut intake = Intake::new(&HUB.mic, 1, RATE);
        intake.take(&[0.5f32, 0.25], |s| s);
        assert_eq!(intake.stereo, [0.5, 0.5, 0.25, 0.25]);
        let mut intake = Intake::new(&HUB.mic, 6, RATE);
        intake.take(&[0.1f32, 0.2, 9.0, 9.0, 9.0, 9.0], |s| s);
        assert_eq!(intake.stereo, [0.1, 0.2]);
        HUB.mic.clear();
    }
}
