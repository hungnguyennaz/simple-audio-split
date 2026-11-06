use simple_audio_split::{
    audio::{self, Com, Stream},
    engine::{Config, Engine},
    settings::SavedDevice,
};
use std::{
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const HELP:&str="sas-cli list\nsas-cli bench\nsas-cli run --target D [--target D] [--source D] [--seconds 60] [--margin MS] [--delay D=MS]\nsas-cli verify --source D --target D [--seconds 8] [--level -20] [--delay D=MS]\nsas-cli churn --target D --target D [--seconds 10]\nsas-cli tone D\nD: index from list, default, or endpoint ID. Delay: whole milliseconds, 0–1000.";
#[derive(Default)]
struct Options {
    targets: Vec<String>,
    source: Option<String>,
    seconds: Option<f64>,
    margin: Option<f64>,
    level: Option<f64>,
    delays: Vec<(String, u32)>,
}
fn parse(args: &[String]) -> Result<Options> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let key = &args[i];
        let value = args.get(i + 1).ok_or("Missing option value")?;
        match key.as_str() {
            "--target" => o.targets.push(value.clone()),
            "--source" => o.source = Some(value.clone()),
            "--seconds" => {
                let n = value.parse::<f64>()?;
                if !n.is_finite() || n <= 0.0 || n > 86400.0 {
                    return Err("Seconds must be between 0 and 86400".into());
                }
                o.seconds = Some(n);
            }
            "--margin" => {
                let n = value.parse::<f64>()?;
                if !n.is_finite() || !(0.0..=100.0).contains(&n) {
                    return Err("Margin must be 0–100 ms".into());
                }
                o.margin = Some(n);
            }
            "--level" => {
                let n = value.parse::<f64>()?;
                if !n.is_finite() || !(-80.0..=-6.0).contains(&n) {
                    return Err("Level must be -80 to -6 dBFS".into());
                }
                o.level = Some(n);
            }
            "--delay" => {
                let (d, v) = value.rsplit_once('=').ok_or("Delay must be DEVICE=MS")?;
                let ms = v.parse::<u32>()?;
                if ms > 1000 || d.is_empty() {
                    return Err("Delay must be 0–1000 ms".into());
                }
                o.delays.push((d.into(), ms));
            }
            _ => return Err(format!("Unknown option: {key}").into()),
        }
        i += 2;
    }
    Ok(o)
}
fn report(engine: &Engine) {
    let s = engine.status();
    if !s.source_error.is_empty() {
        eprintln!("Source: {}", s.source_error);
    }
    for t in s.targets.values() {
        println!("{}: {} · estimated {:.2} ms (pipeline {:.2}, queue {:.2}, includes +{} intentional) · period {:.2} ms{} · margin {:.1} ms · drift {:.1} ppm · underruns {} · resyncs {} {}",t.id,t.state,t.latency_ms(),t.pipeline_ms,t.queue_ms,t.delay_ms,t.period_ms,if t.fallback{" [shared-mode fallback]"}else{""},t.margin_ms,t.drift_ppm,t.underruns,t.resyncs,t.error);
    }
    let (audio, control) = engine.wake_counts();
    println!("Wake counts: audio={audio}, control={control}");
}
fn correlation(source: &[f32], target: &[f32], max_lag: usize) -> Option<(isize, f64)> {
    let mut best = None;
    for lag in -(max_lag as isize)..=max_lag as isize {
        let (si, ti) = if lag < 0 {
            ((-lag) as usize, 0)
        } else {
            (0, lag as usize)
        };
        let n = source
            .len()
            .saturating_sub(si)
            .min(target.len().saturating_sub(ti));
        if n < source.len().min(target.len()) / 2 || n < 100 {
            continue;
        }
        let mut xy = 0.0;
        let mut xx = 0.0;
        let mut yy = 0.0;
        for i in 0..n {
            let x = source[si + i] as f64;
            let y = target[ti + i] as f64;
            xy += x * y;
            xx += x * x;
            yy += y * y;
        }
        if xx < 1e-12 || yy < 1e-12 {
            continue;
        }
        let c = xy / (xx * yy).sqrt();
        if best.is_none_or(|(_, b)| c > b) {
            best = Some((lag, c));
        }
    }
    best
}
struct Recording {
    values: Vec<f32>,
    first_bin: Option<u64>,
    first_qpc: u64,
    frame_position: u64,
    bin: u64,
    sum: f64,
    count: usize,
    rate: f64,
    invalid_timestamps: bool,
    lowpass: [f64; 2],
}
impl Recording {
    fn new(rate: f64) -> Self {
        Self {
            values: Vec::new(),
            first_bin: None,
            first_qpc: 0,
            frame_position: 0,
            bin: 0,
            sum: 0.,
            count: 0,
            rate,
            invalid_timestamps: false,
            lowpass: [0.0; 2],
        }
    }
    fn push(&mut self, samples: &[f32], ch: usize, qpc: u64) {
        if qpc == 0 {
            self.invalid_timestamps = true;
            return;
        }
        if self.first_qpc == 0 {
            self.first_qpc = qpc;
        }
        // Anchor once, then follow sample counts: individual packet QPC timestamps can jitter.
        for f in samples.chunks_exact(ch) {
            let bin = (self.first_qpc as f64 / 10000.0
                + self.frame_position as f64 * 1000.0 / self.rate)
                .floor() as u64;
            self.frame_position += 1;
            if self.first_bin.is_none() {
                self.first_bin = Some(bin);
                self.bin = bin;
            }
            if bin > self.bin {
                if bin - self.bin > 10000 {
                    self.invalid_timestamps = true;
                    return;
                }
                self.values.push(if self.count == 0 {
                    0.0
                } else {
                    (self.sum / self.count as f64) as f32
                });
                self.values.extend(std::iter::repeat_n(
                    0.0,
                    (bin - self.bin - 1).min(10000) as usize,
                ));
                self.sum = 0.0;
                self.count = 0;
                self.bin = bin;
            }
            // Reject out-of-band content before reducing to 1 kHz, including other playing audio.
            let x = f.iter().copied().sum::<f32>() as f64 / ch as f64;
            let alpha = 1.0 - (-std::f64::consts::TAU * 100.0 / self.rate).exp();
            self.lowpass[0] += alpha * (x - self.lowpass[0]);
            self.lowpass[1] += alpha * (self.lowpass[0] - self.lowpass[1]);
            self.sum += self.lowpass[1];
            self.count += 1;
        }
    }
}
fn verify(config: Config, seconds: f64, level: f64) -> Result<()> {
    let _com = Com::init()?;
    let en = audio::enumerator()?;
    let source_id = if config.source_id.is_empty() {
        audio::default_id(&en)?
    } else {
        config.source_id.clone()
    };
    let target = config.targets.first().ok_or("Select one target")?;
    if source_id == target.id {
        return Err("Source and target must differ".into());
    }
    let mut generator = Stream::open(&en, &source_id, false)?;
    let mut source = Stream::open(&en, &source_id, true)?;
    let mut target_capture = Stream::open(&en, &target.id, true)?;
    let mut sr = Recording::new(source.format.dsp.rate);
    let mut tr = Recording::new(target_capture.format.dsp.rate);
    let sc = source.format.dsp.channels;
    let tc = target_capture.format.dsp.channels;
    let engine = Engine::new();
    engine.configure(config);
    let rate = generator.format.dsp.rate;
    let ch = generator.format.dsp.channels;
    let amplitude = 10f64.powf(level / 20.);
    let mut rng = 0x12345678u32;
    let mut filtered = 0.;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < seconds {
        generator.render_with(|out| {
            for f in out.chunks_exact_mut(ch) {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                let x = (rng as f64 / u32::MAX as f64 * 2. - 1.) * amplitude;
                filtered += (1. - (-std::f64::consts::TAU * 100. / rate).exp()) * (x - filtered);
                f.fill(filtered as f32);
            }
        })?;
        source.drain_timed(|s, discontinuity, qpc| {
            if discontinuity && sr.frame_position > 0 {
                sr.invalid_timestamps = true;
            }
            sr.push(s, sc, qpc)
        })?;
        target_capture.drain_timed(|s, discontinuity, qpc| {
            if discontinuity && tr.frame_position > 0 {
                tr.invalid_timestamps = true;
            }
            tr.push(s, tc, qpc)
        })?;
        thread::sleep(Duration::from_millis(1));
    }
    report(&engine);
    if sr.invalid_timestamps || tr.invalid_timestamps {
        return Err("Capture timestamps were unavailable or discontinuous; offset measurement is unreliable".into());
    }
    let start = sr
        .first_bin
        .ok_or("No source packets")?
        .max(tr.first_bin.ok_or("No target packets")?);
    let si = (start - sr.first_bin.unwrap()) as usize;
    let ti = (start - tr.first_bin.unwrap()) as usize;
    let trim = 2000
        .min(sr.values.len().saturating_sub(si) / 3)
        .min(tr.values.len().saturating_sub(ti) / 3);
    let source_samples = sr
        .values
        .get(si + trim..)
        .ok_or("Source and target capture timelines do not overlap")?;
    let target_samples = tr
        .values
        .get(ti + trim..)
        .ok_or("Source and target capture timelines do not overlap")?;
    let (lag, c) =
        correlation(source_samples, target_samples, 1500).ok_or("No usable audio detected")?;
    println!("Loopback correlation: {c:.5}; target offset: {lag} ms. This excludes acoustic/wireless transport delay.");
    if c < 0.90 {
        for start in (0..source_samples
            .len()
            .min(target_samples.len())
            .saturating_sub(1000))
            .step_by(1000)
        {
            let s = &source_samples[start..start + 1000];
            let t = &target_samples[start..start + 1000];
            if let Some((offset, corr)) = correlation(s, t, 300) {
                let rms = |x: &[f32]| {
                    20.0 * ((x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64)
                        .sqrt()
                        .max(1e-20))
                    .log10()
                };
                eprintln!("Window {} s: offset {offset} ms, correlation {corr:.3}, source {:.1} dBFS, target {:.1} dBFS",start/1000,rms(s),rms(t));
            }
        }
        return Err("Signal correlation is below 0.90; check endpoint volumes and other audio sources, or increase --level (maximum -6). Very quiet signals can fall below device noise/processing floors.".into());
    }
    Ok(())
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        println!("{HELP}");
        return Ok(());
    };
    if ["help", "--help", "-h"].contains(&command.as_str()) {
        println!("{HELP}");
        return Ok(());
    }
    if command == "bench" {
        simple_audio_split::bench::run();
        return Ok(());
    }
    let _com = Com::init()?;
    let en = audio::enumerator()?;
    if command == "list" {
        for (i, d) in audio::devices(&en)?.iter().enumerate() {
            let format = audio::device_format(&en, &d.id)
                .map(|f| {
                    format!(
                        "{:.0} Hz, {} channels, {:?}",
                        f.dsp.rate, f.dsp.channels, f.sample
                    )
                })
                .unwrap_or_else(|e| e.to_string());
            println!(
                "{i}: {}{}\n   {}\n   {format}",
                d.name,
                if d.is_default { " [default]" } else { "" },
                d.id
            );
        }
        return Ok(());
    }
    if command == "tone" {
        let id = audio::resolve(&en, args.get(1).ok_or("Specify a device")?)?;
        audio::tone(&id)?;
        return Ok(());
    }
    if !["run", "verify", "churn"].contains(&command.as_str()) {
        return Err(format!("Unknown command\n{HELP}").into());
    }
    let options = parse(&args[1..])?;
    if options.targets.is_empty() {
        return Err("Select at least one --target".into());
    }
    let mut config = Config {
        source_id: options
            .source
            .as_deref()
            .map(|s| audio::resolve(&en, s))
            .transpose()?
            .unwrap_or_default(),
        margin_ms: options.margin.unwrap_or(1.0),
        ..Config::default()
    };
    for selector in &options.targets {
        let id = audio::resolve(&en, selector)?;
        if config.targets.iter().any(|t| t.id == id) {
            return Err("Duplicate target".into());
        }
        config.targets.push(SavedDevice {
            id,
            name: String::new(),
            extra_delay_ms: 0,
        });
    }
    for (selector, ms) in options.delays {
        let id = audio::resolve(&en, &selector)?;
        config
            .targets
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or("Delay device must also be selected with --target")?
            .extra_delay_ms = ms;
    }
    if command == "verify" {
        if config.targets.len() != 1 {
            return Err("Verify requires exactly one target".into());
        }
        return verify(
            config,
            options.seconds.unwrap_or(8.),
            options.level.unwrap_or(-20.),
        );
    }
    if command == "churn" && config.targets.len() < 2 {
        return Err("Churn requires two targets".into());
    }
    let engine = Engine::new();
    engine.configure(config.clone());
    let start = Instant::now();
    let seconds = options
        .seconds
        .unwrap_or(if command == "churn" { 10. } else { 60. });
    let mut last = 0;
    while start.elapsed().as_secs_f64() < seconds {
        thread::sleep(Duration::from_secs_f64(
            (seconds - start.elapsed().as_secs_f64()).clamp(0.0, 1.0),
        ));
        let second = start.elapsed().as_secs();
        if second != last {
            last = second;
            report(&engine);
            if command == "churn" {
                let mut c = config.clone();
                if second % 2 == 1 {
                    c.targets.truncate(1);
                }
                engine.configure(c);
            }
        }
    }
    let status = engine.status();
    if (!status.capturing && status.targets.values().any(|s| s.state != "Default output"))
        || status.targets.values().any(|s| !s.error.is_empty())
    {
        return Err("One or more audio streams failed".into());
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn options() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse(&a(&["--delay", "0=-1"])).is_err());
        assert!(parse(&a(&["--seconds", "NaN"])).is_err());
        assert_eq!(
            parse(&a(&["--target", "1", "--delay", "1=1000"]))
                .unwrap()
                .delays[0]
                .1,
            1000
        );
    }
    #[test]
    fn measured_delay() {
        let s: Vec<f32> = (0..500)
            .map(|i| ((i * i % 79) as f32 - 39.) / 39.)
            .collect();
        let mut t = vec![0.; 37];
        t.extend_from_slice(&s);
        let (lag, c) = correlation(&s, &t, 100).unwrap();
        assert_eq!(lag, 37);
        assert!(c > 0.99999);
    }
    #[test]
    fn packet_timestamps_align_recordings() {
        let mut a = Recording::new(48000.);
        let mut b = Recording::new(48000.);
        a.push(&vec![0.25; 480], 1, 1_000_000);
        b.push(&vec![0.25; 480], 1, 1_050_000);
        assert_eq!(b.first_bin.unwrap() - a.first_bin.unwrap(), 5);
        assert!(!a.invalid_timestamps);
        a.push(&[0.25; 480], 1, 1_110_000);
        assert_eq!(a.values.len(), 19); // 20 ms of continuous samples despite 1 ms timestamp jitter.
        b.push(&[0.0; 48], 1, 0);
        assert!(b.invalid_timestamps);
    }
    #[test]
    fn recording_filters_before_downsampling() {
        let mut r = Recording::new(48000.);
        let x: Vec<f32> = (0..48000)
            .map(|i| (0.5 * (std::f64::consts::TAU * 2500.0 * i as f64 / 48000.0).sin()) as f32)
            .collect();
        r.push(&x, 1, 1_000_000);
        let rms = (r.values[100..]
            .iter()
            .map(|&v| (v as f64).powi(2))
            .sum::<f64>()
            / (r.values.len() - 100) as f64)
            .sqrt();
        assert!(rms < 0.0002, "aliased signal RMS: {rms}");
    }
}
