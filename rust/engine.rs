use crate::{
    audio::{self, Com, Device, Event, Stream},
    dsp::{Format, KernelCache, Pipeline},
    settings::SavedDevice,
    volume::{Gain, SourceVolume},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{implement, Result, PCWSTR},
    Win32::{Media::Audio::*, System::Threading::*, UI::Shell::PropertiesSystem::PROPERTYKEY},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub enabled: bool,
    pub source_id: String,
    pub targets: Vec<SavedDevice>,
    pub margin_ms: f64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            source_id: String::new(),
            targets: Vec::new(),
            margin_ms: 1.0,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct TargetStatus {
    pub id: String,
    pub generation: u64,
    pub state: String,
    pub error: String,
    pub pipeline_ms: f64,
    pub queue_ms: f64,
    pub delay_ms: u32,
    pub margin_ms: f64,
    pub period_ms: f64,
    pub fallback: bool,
    pub drift_ppm: f64,
    pub underruns: u32,
    pub resyncs: u32,
}
impl TargetStatus {
    pub fn latency_ms(&self) -> f64 {
        self.pipeline_ms + self.queue_ms
    }
}
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub devices: Vec<Device>,
    pub source_id: String,
    pub source_error: String,
    pub capturing: bool,
    pub captured_frames: u64,
    pub targets: HashMap<String, TargetStatus>,
}
#[derive(Default)]
struct WakeCounts {
    audio: AtomicU64,
    control: AtomicU64,
}
pub struct Engine {
    wakes: Arc<WakeCounts>,
    tx: Sender<Config>,
    control_wake: Option<Arc<Event>>,
    status: Arc<Mutex<Status>>,
    join: Option<JoinHandle<()>>,
}
impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
impl Engine {
    pub fn new() -> Self {
        Self::with_device_notifier(|| {})
    }
    pub fn with_device_notifier(notify: impl Fn() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let wakes = Arc::new(WakeCounts::default());
        let status = Arc::new(Mutex::new(Status::default()));
        let control_wake = match Event::new() {
            Ok(e) => Arc::new(e),
            Err(e) => {
                status.lock().unwrap().source_error = e.to_string();
                return Self {
                    tx,
                    status,
                    join: None,
                    control_wake: None,
                    wakes,
                };
            }
        };
        let shared = status.clone();
        let thread_wake = control_wake.clone();
        let thread_counts = wakes.clone();
        let join = thread::spawn(move || {
            control(rx, shared, thread_wake, thread_counts, Box::new(notify))
        });
        Self {
            tx,
            status,
            join: Some(join),
            control_wake: Some(control_wake),
            wakes,
        }
    }
    pub fn configure(&self, config: Config) {
        let _ = self.tx.send(config);
        if let Some(wake) = &self.control_wake {
            wake.signal();
        }
    }
    pub fn wake_counts(&self) -> (u64, u64) {
        (
            self.wakes.audio.load(Ordering::Relaxed),
            self.wakes.control.load(Ordering::Relaxed),
        )
    }
    pub fn device_present(&self, id: &str) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .devices
            .iter()
            .any(|d| d.id == id)
    }
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let (old, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, old));
        if let Some(wake) = &self.control_wake {
            wake.signal();
        }
        if let Some(t) = self.join.take() {
            let _ = t.join();
        }
    }
}

#[implement(IMMNotificationClient)]
struct Watcher {
    changed: Arc<AtomicBool>,
    wake: Arc<Event>,
}
impl IMMNotificationClient_Impl for Watcher_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        self.changed.store(true, Ordering::Release);
        self.wake.signal();
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        self.changed.store(true, Ordering::Release);
        self.wake.signal();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        self.changed.store(true, Ordering::Release);
        self.wake.signal();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eConsole {
            self.changed.store(true, Ordering::Release);
            self.wake.signal();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, key: &PROPERTYKEY) -> Result<()> {
        // Renames affect the list; format invalidations are recovered through stream failures.
        let name = windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
        if key.fmtid == name.fmtid && key.pid == name.pid {
            self.changed.store(true, Ordering::Release);
            self.wake.signal();
        }
        Ok(())
    }
}
struct Source {
    stream: Stream,
    keepalive: Option<Stream>,
}
struct Target {
    id: String,
    generation: u64,
    stream: Stream,
    pipe: Pipeline,
    gain: Gain,
}
// Keep transfers inline so retiring streams never needs a new Box on the audio thread.
#[allow(clippy::large_enum_variant)]
enum Command {
    Source(Option<Source>),
    Add(Target),
    Remove(String),
    Pipeline(String, u64, Pipeline),
    Quit,
}
#[allow(clippy::large_enum_variant)]
enum Retired {
    Source(Source),
    Target(Target),
    Pipeline(Pipeline),
}
struct Failure {
    source: bool,
    id: String,
    generation: u64,
    error: String,
}
fn submit(tx: &Sender<Command>, wake: &Event, c: Command) {
    let _ = tx.send(c);
    wake.signal();
}
fn control(
    rx: Receiver<Config>,
    shared: Arc<Mutex<Status>>,
    control_wake: Arc<Event>,
    wakes: Arc<WakeCounts>,
    notify: Box<dyn Fn() + Send>,
) {
    let _com = match Com::init() {
        Ok(c) => c,
        Err(e) => {
            shared.lock().unwrap().source_error = e.to_string();
            return;
        }
    };
    let en = match audio::enumerator() {
        Ok(e) => e,
        Err(e) => {
            shared.lock().unwrap().source_error = e.to_string();
            return;
        }
    };
    let changed = Arc::new(AtomicBool::new(true));
    let watcher: IMMNotificationClient = Watcher {
        changed: changed.clone(),
        wake: control_wake.clone(),
    }
    .into();
    let notifications = unsafe { en.RegisterEndpointNotificationCallback(&watcher).is_ok() };
    let wake = match Event::new() {
        Ok(e) => Arc::new(e),
        Err(e) => {
            shared.lock().unwrap().source_error = e.to_string();
            return;
        }
    };
    let (tx, commands) = mpsc::channel();
    let (retire, retired) = mpsc::channel();
    let (fail, failures) = mpsc::channel();
    let rt_wake = wake.clone();
    let rt_status = shared.clone();
    let retire_wake = control_wake.clone();
    let audio_counts = wakes.clone();
    let master_gain = Arc::new(AtomicU32::new(0.0f32.to_bits()));
    let render_gain = master_gain.clone();
    let audio = thread::spawn(move || {
        audio_loop(
            commands,
            retire,
            fail,
            rt_wake,
            rt_status,
            retire_wake,
            audio_counts,
            render_gain,
        )
    });
    let mut cfg = Config {
        enabled: false,
        ..Config::default()
    };
    let mut source = String::new();
    let mut volume: Option<SourceVolume> = None;
    let mut format = None;
    let mut active: HashMap<String, (u64, u32, Format, f64, f64)> = HashMap::new();
    let mut kernels = KernelCache::default();
    let mut packet_bound = 0.2;
    let mut generation = 0;
    let mut next_retry = Some(Instant::now());
    'control: loop {
        let mut dirty = changed.swap(false, Ordering::AcqRel);
        loop {
            match rx.try_recv() {
                Ok(c) => {
                    cfg = c;
                    dirty = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => break 'control,
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if let Some(v) = &volume {
            if let Err(e) = v.refresh_if_changed() {
                master_gain.store(0.0f32.to_bits(), Ordering::Release);
                shared.lock().unwrap().source_error = format!("Volume control: {e}");
            }
        }
        for item in retired.try_iter() {
            match item {
                Retired::Source(s) => drop(s),
                Retired::Target(t) => drop(t),
                Retired::Pipeline(p) => drop(p),
            }
        }
        for failure in failures.try_iter() {
            if failure.source {
                source.clear();
                volume = None;
                master_gain.store(0.0f32.to_bits(), Ordering::Release);
                format = None;
                active.clear();
                let mut st = shared.lock().unwrap();
                st.source_error = failure.error;
                st.capturing = false;
            } else if active
                .get(&failure.id)
                .is_some_and(|v| v.0 == failure.generation)
            {
                active.remove(&failure.id);
                if let Some(st) = shared.lock().unwrap().targets.get_mut(&failure.id) {
                    st.state = "Error".into();
                    st.error = failure.error;
                }
            }
            dirty = true;
        }
        if next_retry.is_some_and(|t| Instant::now() >= t) {
            dirty = true;
        }
        if !dirty {
            let timeout = next_retry
                .map(|t| {
                    t.saturating_duration_since(Instant::now())
                        .as_millis()
                        .saturating_add(1)
                        .min(u32::MAX as u128 - 1) as u32
                })
                .unwrap_or(INFINITE);
            unsafe {
                WaitForSingleObject(control_wake.0, timeout);
                wakes.control.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }

        let list = audio::devices(&en).unwrap_or_default();
        let resolved_source = if cfg.enabled {
            if cfg.source_id.is_empty() {
                audio::default_id(&en).unwrap_or_default()
            } else {
                cfg.source_id.clone()
            }
        } else {
            String::new()
        };
        let desired = if cfg
            .targets
            .iter()
            .any(|d| d.id != resolved_source && list.iter().any(|a| a.id == d.id))
        {
            resolved_source.clone()
        } else {
            String::new()
        };
        let device_changed = {
            let mut st = shared.lock().unwrap();
            let changed = st.devices != list;
            if desired.is_empty() {
                st.source_error.clear();
                st.capturing = false;
                st.source_id.clear();
            }
            st.devices = list.clone();
            st.targets
                .retain(|id, _| cfg.targets.iter().any(|t| &t.id == id));
            changed
        };
        if device_changed {
            notify();
        }
        let ids: HashSet<_> = cfg.targets.iter().map(|d| d.id.as_str()).collect();
        let removed: Vec<_> = active
            .keys()
            .filter(|id| {
                !ids.contains(id.as_str())
                    || desired != source
                    || !cfg.enabled
                    || !list.iter().any(|d| d.id == **id)
            })
            .cloned()
            .collect();
        for id in removed {
            active.remove(&id);
            submit(&tx, &wake, Command::Remove(id));
        }
        // Format changes invalidate the active client and are recovered through its failure path.
        if desired != source {
            for id in active.keys() {
                submit(&tx, &wake, Command::Remove(id.clone()));
            }
            active.clear();
            submit(&tx, &wake, Command::Source(None));
            source.clear();
            volume = None;
            master_gain.store(0.0f32.to_bits(), Ordering::Release);
            format = None;
        }
        if !desired.is_empty() && source.is_empty() {
            match SourceVolume::open(&en, &desired, control_wake.clone(), master_gain.clone())
                .and_then(|v| Stream::open(&en, &desired, true).map(|stream| (stream, v)))
            {
                Ok((stream, v)) => {
                    volume = Some(v);
                    format = Some(stream.format.dsp);
                    packet_bound = stream.capacity as f64 / stream.format.dsp.rate;
                    let keepalive = Stream::open(&en, &desired, false).ok();
                    source = desired.clone();
                    submit(
                        &tx,
                        &wake,
                        Command::Source(Some(Source { stream, keepalive })),
                    );
                    let mut st = shared.lock().unwrap();
                    st.source_error.clear();
                    st.source_id = source.clone();
                }
                Err(e) => {
                    shared.lock().unwrap().source_error = e.to_string();
                }
            }
        }
        if !cfg.enabled {
            let mut st = shared.lock().unwrap();
            st.capturing = false;
            st.source_id.clear();
        }
        for device in &cfg.targets {
            if let Some((generation, delay, dst, chunk, margin)) = active.get_mut(&device.id) {
                let next_margin = cfg.margin_ms.clamp(0.0, 100.0) / 1000.0;
                if *delay != device.extra_delay_ms || *margin != next_margin {
                    if let Some(src) = format {
                        let pipe = Pipeline::new_cached(
                            src,
                            *dst,
                            *chunk,
                            next_margin,
                            device.extra_delay_ms,
                            packet_bound,
                            &mut kernels,
                        );
                        submit(
                            &tx,
                            &wake,
                            Command::Pipeline(device.id.clone(), *generation, pipe),
                        );
                        *delay = device.extra_delay_ms;
                        *margin = next_margin;
                    }
                }
                continue;
            }
            let mut state = TargetStatus {
                id: device.id.clone(),
                delay_ms: device.extra_delay_ms,
                ..TargetStatus::default()
            };
            if !cfg.enabled {
                state.state = "Off".into();
            } else if device.id == resolved_source {
                state.state = "Default output".into();
            } else if !list.iter().any(|d| d.id == device.id) {
                state.state = "Unavailable".into();
            } else if let Some(src) = format {
                match Stream::open(&en, &device.id, false) {
                    Ok(stream) => {
                        generation += 1;
                        state.generation = generation;
                        state.state = String::with_capacity(32);
                        state.state.push_str("Starting");
                        state.period_ms = stream.period as f64 / stream.format.dsp.rate * 1000.0;
                        state.fallback = stream.fallback;
                        let pipe = Pipeline::new_cached(
                            src,
                            stream.format.dsp,
                            state.period_ms / 1000.0,
                            cfg.margin_ms.clamp(0.0, 100.0) / 1000.0,
                            device.extra_delay_ms,
                            packet_bound,
                            &mut kernels,
                        );
                        let dst = stream.format.dsp;
                        let chunk = state.period_ms / 1000.0;
                        let target = Target {
                            id: device.id.clone(),
                            generation,
                            stream,
                            pipe,
                            gain: Gain::default(),
                        };
                        active.insert(
                            device.id.clone(),
                            (
                                generation,
                                device.extra_delay_ms,
                                dst,
                                chunk,
                                cfg.margin_ms.clamp(0.0, 100.0) / 1000.0,
                            ),
                        );
                        shared
                            .lock()
                            .unwrap()
                            .targets
                            .insert(device.id.clone(), state);
                        submit(&tx, &wake, Command::Add(target));
                        continue;
                    }
                    Err(e) => {
                        state.state = "Error".into();
                        state.error = e.to_string();
                    }
                }
            } else {
                state.state = "Waiting for source".into();
            }
            shared
                .lock()
                .unwrap()
                .targets
                .insert(device.id.clone(), state);
        }
        let retry_needed = !notifications
            || (!desired.is_empty() && source.is_empty())
            || (cfg.enabled
                && cfg.targets.iter().any(|d| {
                    d.id != resolved_source
                        && list.iter().any(|a| a.id == d.id)
                        && !active.contains_key(&d.id)
                }));
        next_retry = retry_needed.then(|| Instant::now() + Duration::from_secs(2));
    }
    unsafe {
        let _ = en.UnregisterEndpointNotificationCallback(&watcher);
    }
    submit(&tx, &wake, Command::Quit);
    let _ = audio.join();
    for item in retired.try_iter() {
        match item {
            Retired::Source(s) => drop(s),
            Retired::Target(t) => drop(t),
            Retired::Pipeline(p) => drop(p),
        }
    }
}
fn audio_loop(
    commands: Receiver<Command>,
    retire: Sender<Retired>,
    fail: Sender<Failure>,
    wake: Arc<Event>,
    shared: Arc<Mutex<Status>>,
    control_wake: Arc<Event>,
    wakes: Arc<WakeCounts>,
    master_gain: Arc<AtomicU32>,
) {
    let _com = match Com::init() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut index = 0;
    let mmcss =
        unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut index).ok() };
    let start = Instant::now();
    let mut last_status = Instant::now();
    let mut source: Option<Source> = None;
    let mut targets: Vec<Target> = Vec::with_capacity(64);
    let mut captured = 0;
    let mut handles = Vec::with_capacity(64);
    'running: loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Source(s) => {
                    if let Some(old) = source.take() {
                        retire_item(&retire, &control_wake, Retired::Source(old));
                    }
                    source = s;
                    captured = 0;
                }
                Command::Add(t) => targets.push(t),
                Command::Remove(id) => {
                    if let Some(i) = targets.iter().position(|t| t.id == id) {
                        retire_item(
                            &retire,
                            &control_wake,
                            Retired::Target(targets.swap_remove(i)),
                        );
                    }
                }
                Command::Pipeline(id, generation, mut pipe) => {
                    if let Some(t) = targets
                        .iter_mut()
                        .find(|t| t.id == id && t.generation == generation)
                    {
                        pipe.inherit_clock(&t.pipe);
                        std::mem::swap(&mut t.pipe, &mut pipe);
                    }
                    retire_item(&retire, &control_wake, Retired::Pipeline(pipe));
                }
                Command::Quit => break 'running,
            }
        }
        let now = start.elapsed().as_secs_f64();
        if let Some(s) = source.as_mut() {
            if let Some(k) = s.keepalive.as_mut() {
                let _ = k.silence();
            }
            let channels = s.stream.format.dsp.channels as u64;
            if let Err(e) = s.stream.drain(|samples, discontinuity| {
                captured += samples.len() as u64 / channels;
                for t in &mut targets {
                    if discontinuity {
                        t.pipe.priming = true;
                    }
                    t.pipe.push(samples, now);
                }
            }) {
                let _ = fail.send(Failure {
                    source: true,
                    id: String::new(),
                    generation: 0,
                    error: e.to_string(),
                });
                if let Some(s) = source.take() {
                    retire_item(&retire, &control_wake, Retired::Source(s));
                }
                for t in targets.drain(..) {
                    retire_item(&retire, &control_wake, Retired::Target(t));
                }
            }
        }
        let mut i = 0;
        while i < targets.len() {
            let t = &mut targets[i];
            let channels = t.stream.format.dsp.channels;
            let rate = t.stream.format.dsp.rate;
            let gain = f32::from_bits(master_gain.load(Ordering::Acquire));
            if let Err(e) = t.stream.render_with(|out| {
                t.pipe.pull(out, now);
                t.gain.apply(out, channels, rate, gain);
            }) {
                let t = targets.swap_remove(i);
                let _ = fail.send(Failure {
                    source: false,
                    id: t.id.clone(),
                    generation: t.generation,
                    error: e.to_string(),
                });
                retire_item(&retire, &control_wake, Retired::Target(t));
            } else {
                i += 1;
            }
        }
        if last_status.elapsed() >= Duration::from_millis(250) {
            if let Ok(mut status) = shared.try_lock() {
                status.capturing = source.is_some();
                status.captured_frames = captured;
                for t in &targets {
                    if let Some(st) = status.targets.get_mut(&t.id) {
                        if st.generation != t.generation {
                            continue;
                        }
                        st.state.clear();
                        st.state.push_str(if t.pipe.priming {
                            "Buffering"
                        } else {
                            "Playing"
                        });
                        st.delay_ms = t.pipe.delay_ms;
                        st.pipeline_ms = t.pipe.drift.smoothed * 1000.0;
                        st.queue_ms = unsafe { t.stream.client.GetCurrentPadding().unwrap_or(0) }
                            as f64
                            / t.stream.format.dsp.rate
                            * 1000.0;
                        st.margin_ms = t.pipe.margin * 1000.0;
                        st.drift_ppm = t.pipe.drift.integral * 1e6;
                        st.underruns = t.pipe.underruns;
                        st.resyncs = t.pipe.resyncs;
                    }
                }
            }
            last_status = Instant::now();
        }
        handles.clear();
        handles.push(wake.0);
        let mut polling = false;
        if let Some(s) = &source {
            if s.stream.events {
                handles.push(s.stream.event.0);
            } else {
                polling = true;
            }
            if let Some(k) = &s.keepalive {
                handles.push(k.event.0);
            }
        }
        for t in &targets {
            if handles.len() < 64 {
                handles.push(t.stream.event.0);
            } else {
                polling = true;
            }
        }
        let timeout = if polling {
            targets
                .iter()
                .map(|t| (t.stream.period as f64 / t.stream.format.dsp.rate * 1000.0).ceil() as u32)
                .min()
                .unwrap_or(10)
                .clamp(1, 10)
        } else {
            INFINITE
        };
        unsafe {
            WaitForMultipleObjects(&handles, false, timeout);
            wakes.audio.fetch_add(1, Ordering::Relaxed);
        }
    }
    for t in targets {
        retire_item(&retire, &control_wake, Retired::Target(t));
    }
    if let Some(s) = source {
        retire_item(&retire, &control_wake, Retired::Source(s));
    }
    if let Some(mmcss) = mmcss {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(mmcss);
        }
    }
}

fn retire_item(tx: &Sender<Retired>, wake: &Event, item: Retired) {
    let _ = tx.send(item);
    wake.signal();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_ignore_unrelated_properties_and_capture_defaults() {
        let changed = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Event::new().unwrap());
        let client: IMMNotificationClient = Watcher {
            changed: changed.clone(),
            wake: wake.clone(),
        }
        .into();
        unsafe {
            client
                .OnPropertyValueChanged(PCWSTR::null(), PROPERTYKEY::default())
                .unwrap();
            client
                .OnDefaultDeviceChanged(eCapture, eConsole, PCWSTR::null())
                .unwrap();
            assert!(!changed.load(Ordering::Acquire));
            assert_eq!(
                WaitForSingleObject(wake.0, 0),
                windows::Win32::Foundation::WAIT_TIMEOUT
            );
            client
                .OnPropertyValueChanged(
                    PCWSTR::null(),
                    windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
                )
                .unwrap();
            assert!(changed.load(Ordering::Acquire));
            assert_eq!(
                WaitForSingleObject(wake.0, 0),
                windows::Win32::Foundation::WAIT_OBJECT_0
            );
        }
    }
    #[test]
    fn idle_engine_has_no_audio_polling_and_stops_promptly() {
        let engine = Engine::new();
        engine.configure(Config::default());
        thread::sleep(Duration::from_millis(300));
        let before = engine.wake_counts();
        thread::sleep(Duration::from_millis(300));
        let after = engine.wake_counts();
        assert_eq!(before.0, 0);
        assert_eq!(after.0, 0);
        assert!(!engine.status().capturing);
        let start = Instant::now();
        drop(engine);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
