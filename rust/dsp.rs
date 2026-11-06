//! Allocation-free processing after stream configuration. Times are seconds.
use std::{
    collections::HashMap,
    f64::consts::PI,
    sync::{Arc, Weak},
};

#[derive(Clone, Copy, Debug)]
pub struct Format {
    pub rate: f64,
    pub channels: usize,
    pub mask: u32,
}

fn positions(ch: usize, mask: u32) -> Vec<u32> {
    let mask = if mask != 0 {
        mask
    } else {
        match ch {
            1 => 4,
            2 => 3,
            3 => 7,
            4 => 51,
            5 => 55,
            6 => 63,
            7 => 319,
            8 => 1599,
            _ => 0,
        }
    };
    let mut p: Vec<_> = (0..32)
        .filter_map(|b| (mask & (1 << b) != 0).then_some(1 << b))
        .take(ch)
        .collect();
    p.resize(ch, 0);
    p
}
fn route(pos: u32, gain: f32, dst: &[u32], src: u32, col: &mut [f32], depth: usize) {
    if depth > 3 {
        return;
    }
    if let Some(i) = dst.iter().position(|&p| p == pos) {
        col[i] += gain;
        return;
    }
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let mut add = |p, g| route(p, g, dst, src, col, depth + 1);
    match pos {
        1 | 2 => add(4, gain * h),
        4 | 0x800 | 0x2000 => {
            if dst.contains(&4) {
                add(4, gain * h)
            } else {
                add(1, gain * h);
                add(2, gain * h)
            }
        }
        0x40 | 0x1000 => add(1, gain),
        0x80 | 0x4000 => add(2, gain),
        0x200 | 0x8000 => {
            if dst.contains(&16) {
                add(16, gain * if src & 16 != 0 { h } else { 1.0 })
            } else {
                add(1, gain * h)
            }
        }
        0x400 | 0x20000 => {
            if dst.contains(&32) {
                add(32, gain * if src & 32 != 0 { h } else { 1.0 })
            } else {
                add(2, gain * h)
            }
        }
        16 => {
            if dst.contains(&0x200) {
                add(0x200, gain * if src & 0x200 != 0 { h } else { 1.0 })
            } else {
                add(1, gain * h)
            }
        }
        32 => {
            if dst.contains(&0x400) {
                add(0x400, gain * if src & 0x400 != 0 { h } else { 1.0 })
            } else {
                add(2, gain * h)
            }
        }
        0x100 | 0x10000 => {
            let (l, r, g) = if dst.contains(&16) && dst.contains(&32) {
                (16, 32, gain * h)
            } else if dst.contains(&512) && dst.contains(&1024) {
                (512, 1024, gain * h)
            } else {
                (1, 2, gain * 0.5)
            };
            add(l, g);
            add(r, g);
        }
        _ => {}
    }
}
pub fn matrix(src: Format, dst: Format) -> Vec<f32> {
    let mut m = vec![0.0; src.channels * dst.channels];
    if dst.channels == 1 && src.channels > 1 {
        let st = matrix(
            src,
            Format {
                channels: 2,
                mask: 3,
                ..dst
            },
        );
        for s in 0..src.channels {
            m[s] = (st[s] + st[src.channels + s]) * 0.5;
        }
        return m;
    }
    let sp = positions(src.channels, src.mask);
    let dp = positions(dst.channels, dst.mask);
    if src.channels == 1 {
        if let (Some(l), Some(r)) = (
            dp.iter().position(|&p| p == 1),
            dp.iter().position(|&p| p == 2),
        ) {
            m[l] = 1.0;
            m[r] = 1.0;
        } else {
            let c = dp.iter().position(|&p| p == 4).unwrap_or(0);
            m[c] = 1.0;
        }
        return m;
    }
    let mut col = vec![0.0; dst.channels];
    for s in 0..src.channels {
        col.fill(0.0);
        if sp[s] == 0 {
            if s < dst.channels {
                col[s] = 1.0;
            }
        } else {
            route(
                sp[s],
                1.0,
                &dp,
                sp.iter().fold(0, |a, b| a | b),
                &mut col,
                0,
            );
        }
        for d in 0..dst.channels {
            m[d * src.channels + s] = col[d];
        }
    }
    m
}
fn i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..40 {
        term *= x * x / (4.0 * (k * k) as f64);
        sum += term;
        if term < sum * 1e-15 {
            break;
        }
    }
    sum
}

struct Kernel {
    phases: Vec<f32>,
    half: usize,
}
#[derive(Default)]
pub struct KernelCache {
    entries: HashMap<(u32, u32), Weak<Kernel>>,
}
impl KernelCache {
    fn get(&mut self, src: f64, dst: f64) -> Arc<Kernel> {
        let key = (src as u32, dst as u32);
        if let Some(kernel) = self.entries.get(&key).and_then(Weak::upgrade) {
            return kernel;
        }
        self.entries.retain(|_, v| v.strong_count() > 0);
        let half = (24.0 * (src / dst).max(1.0)).ceil() as usize;
        let taps = 2 * half;
        let mut phases = vec![0.0; 257 * taps];
        let cutoff = 0.95 * (dst / src).min(1.0);
        let window_norm = i0(8.0);
        for p in 0..=256 {
            let mut sum = 0.0;
            for k in 0..taps {
                let t = k as f64 - half as f64 + 1.0 - p as f64 / 256.0;
                let x = t / half as f64;
                if x.abs() >= 1.0 {
                    continue;
                }
                let a = PI * cutoff * t;
                let sinc = if a.abs() < 1e-12 { 1.0 } else { a.sin() / a };
                let h = sinc * i0(8.0 * (1.0 - x * x).sqrt()) / window_norm;
                phases[p * taps + k] = h as f32;
                sum += h;
            }
            for k in 0..taps {
                phases[p * taps + k] /= sum as f32;
            }
        }
        let kernel = Arc::new(Kernel { phases, half });
        self.entries.insert(key, Arc::downgrade(&kernel));
        kernel
    }
}
pub struct Fifo {
    data: Vec<f32>,
    kernel: Arc<Kernel>,
    coeffs: Vec<f32>,
    cap: usize,
    ch: usize,
    half: usize,
    write: i64,
    read: i64,
    frac: f64,
    step: f64,
}
impl Fifo {
    pub fn new(ch: usize, src: f64, dst: f64) -> Self {
        Self::configured(ch, src, dst, 0.2, 0.01, 0, &mut KernelCache::default())
    }
    fn configured(
        ch: usize,
        src: f64,
        dst: f64,
        packet_bound: f64,
        chunk: f64,
        delay: u32,
        cache: &mut KernelCache,
    ) -> Self {
        let kernel = cache.get(src, dst);
        Self::with_kernel(ch, src, dst, packet_bound, chunk, delay, kernel)
    }
    fn with_kernel(
        ch: usize,
        src: f64,
        dst: f64,
        packet_bound: f64,
        chunk: f64,
        delay: u32,
        kernel: Arc<Kernel>,
    ) -> Self {
        let half = kernel.half;
        // Maximum adaptive margin + recovery backlog + largest capture packet, independent of user delay.
        let cap = (((delay.min(1000) as f64 / 1000.0 + packet_bound + chunk + 0.2) * src).ceil()
            as usize
            + 4 * half)
            .next_power_of_two();
        Self {
            data: vec![0.0; cap * ch],
            coeffs: vec![0.0; half * 2],
            kernel,
            cap,
            ch,
            half,
            write: 0,
            read: 0,
            frac: 0.0,
            step: src / dst,
        }
    }
    fn fill(&self) -> f64 {
        (self.write - self.read) as f64 - self.frac
    }
    fn seek(&mut self, fill: f64) {
        let pos = self.write as f64 - fill.clamp(0.0, (self.cap - 2 * self.half - 1) as f64);
        self.read = pos.floor() as i64;
        self.frac = pos - pos.floor();
    }
    #[cfg(test)]
    fn push_frame(&mut self, frame: &[f32]) {
        self.push(frame);
    }
    fn push(&mut self, input: &[f32]) {
        let frames = input.len() / self.ch;
        let mut offset = 0;
        while offset < frames {
            let slot = self.write as usize & (self.cap - 1);
            let n = (frames - offset).min(self.cap - slot);
            self.data[slot * self.ch..(slot + n) * self.ch]
                .copy_from_slice(&input[offset * self.ch..(offset + n) * self.ch]);
            self.write += n as i64;
            offset += n;
        }
    }
    pub fn read(&mut self, out: &mut [f32], adjust: f64) -> usize {
        match self.ch {
            1 => self.read_frames::<1>(out, adjust),
            2 => self.read_frames::<2>(out, adjust),
            6 => self.read_frames::<6>(out, adjust),
            8 => self.read_frames::<8>(out, adjust),
            _ => self.read_frames::<0>(out, adjust),
        }
    }
    #[allow(clippy::chunks_exact_to_as_chunks)] // CH=0 selects the generic branch; as_chunks::<0> is invalid.
    fn read_frames<const CH: usize>(&mut self, out: &mut [f32], adjust: f64) -> usize {
        let ch = if CH == 0 { self.ch } else { CH };
        let frames = out.len() / ch;
        let taps = 2 * self.half;
        let mut n = 0;
        while n < frames && self.read + (self.half as i64) < self.write {
            let pf = self.frac * 256.0;
            let p = pf as usize;
            let a = (pf - p as f64) as f32;
            let row0 = &self.kernel.phases[p * taps..(p + 1) * taps];
            let row1 = &self.kernel.phases[(p + 1) * taps..(p + 2) * taps];
            for ((w, &v0), &v1) in self.coeffs.iter_mut().zip(row0).zip(row1) {
                *w = v0 + a * (v1 - v0);
            }
            let first = (self.read - self.half as i64 + 1) as usize & (self.cap - 1);
            let end = taps.min(self.cap - first);
            let pieces = [
                (
                    &self.data[first * ch..(first + end) * ch],
                    &self.coeffs[..end],
                ),
                (&self.data[..(taps - end) * ch], &self.coeffs[end..]),
            ];
            let dst = &mut out[n * ch..(n + 1) * ch];
            if CH > 0 {
                let mut acc = [0.0; CH];
                for (samples, weights) in pieces {
                    for (frame, &w) in samples.chunks_exact(CH).zip(weights) {
                        for (c, sum) in acc.iter_mut().enumerate() {
                            *sum += w * frame[c];
                        }
                    }
                }
                dst.copy_from_slice(&acc);
            } else {
                dst.fill(0.0);
                for (samples, weights) in pieces {
                    for (frame, &w) in samples.chunks_exact(ch).zip(weights) {
                        for (sum, &v) in dst.iter_mut().zip(frame) {
                            *sum += w * v;
                        }
                    }
                }
            }
            self.frac += self.step * (1.0 + adjust);
            let whole = self.frac.floor();
            self.read += whole as i64;
            self.frac -= whole;
            n += 1;
        }
        out[n * ch..].fill(0.0);
        n
    }
}
#[derive(Clone, Copy, Default)]
pub struct Drift {
    pub integral: f64,
    pub adjust: f64,
    pub smoothed: f64,
    elapsed: f64,
    primed: bool,
}
impl Drift {
    fn resync(&mut self, fill: f64) {
        self.smoothed = fill;
        self.primed = true;
    }
    fn update(&mut self, fill: f64, target: f64, dt: f64) -> f64 {
        if !self.primed {
            self.resync(fill);
        }
        let dt = dt.clamp(0.0, 0.1);
        self.smoothed += (1.0 - (-dt / 1.5).exp()) * (fill - self.smoothed);
        let (kp, ki) = if self.elapsed < 20.0 {
            (0.3, 0.0225)
        } else {
            (0.05, 0.0006)
        };
        self.elapsed += dt;
        let err = self.smoothed - target;
        self.integral = (self.integral + ki * err * dt).clamp(-0.005, 0.005);
        self.adjust = (kp * err + self.integral).clamp(-0.005, 0.005);
        self.adjust
    }
}
pub struct Pipeline {
    src: Format,
    dst: Format,
    matrix: Vec<f32>,
    identity: bool,
    packet_bound: f64,
    frame: Vec<f32>,
    fifo: Fifo,
    pub drift: Drift,
    packet: f64,
    chunk: f64,
    pub margin: f64,
    base_margin: f64,
    pub delay_ms: u32,
    last_arrival: f64,
    last_update: f64,
    have_input: bool,
    pub priming: bool,
    fade: usize,
    calm: f64,
    pub underruns: u32,
    pub resyncs: u32,
}
impl Pipeline {
    pub fn allocation_bytes(&self) -> (usize, usize) {
        (
            self.fifo.data.capacity() * std::mem::size_of::<f32>(),
            self.fifo.kernel.phases.capacity() * std::mem::size_of::<f32>(),
        )
    }
    pub fn new(src: Format, dst: Format, chunk: f64, margin: f64, delay_ms: u32) -> Self {
        Self::new_cached(
            src,
            dst,
            chunk,
            margin,
            delay_ms,
            0.2,
            &mut KernelCache::default(),
        )
    }
    pub fn new_cached(
        src: Format,
        dst: Format,
        chunk: f64,
        margin: f64,
        delay_ms: u32,
        packet_bound: f64,
        cache: &mut KernelCache,
    ) -> Self {
        let matrix = matrix(src, dst);
        let identity = src.channels == dst.channels
            && matrix.iter().enumerate().all(|(i, &v)| {
                v == if i / src.channels == i % src.channels {
                    1.0
                } else {
                    0.0
                }
            });
        Self {
            src,
            dst,
            matrix,
            identity,
            packet_bound,
            frame: vec![0.0; dst.channels],
            fifo: Fifo::configured(
                dst.channels,
                src.rate,
                dst.rate,
                packet_bound,
                chunk,
                delay_ms,
                cache,
            ),
            drift: Drift::default(),
            packet: 0.0,
            chunk,
            margin,
            base_margin: margin,
            delay_ms: delay_ms.min(1000),
            last_arrival: 0.0,
            last_update: 0.0,
            have_input: false,
            priming: true,
            fade: 0,
            calm: 0.0,
            underruns: 0,
            resyncs: 0,
        }
    }
    pub fn inherit_clock(&mut self, old: &Self) {
        self.drift = old.drift;
        self.margin = self.margin.max(old.margin);
        self.underruns = old.underruns;
        self.resyncs = old.resyncs;
    }
    pub fn target(&self) -> f64 {
        self.packet
            + self.chunk
            + self.margin
            + self.fifo.half as f64 / self.src.rate
            + self.delay_ms as f64 / 1000.0
    }
    pub fn set_delay(&mut self, ms: u32) {
        if self.delay_ms != ms {
            self.delay_ms = ms.min(1000);
            // This convenience API may allocate; the engine instead prepares replacement pipelines on its control thread.
            self.fifo = Fifo::with_kernel(
                self.dst.channels,
                self.src.rate,
                self.dst.rate,
                self.packet_bound,
                self.chunk,
                self.delay_ms,
                self.fifo.kernel.clone(),
            );
            self.priming = true;
            self.have_input = false;
            self.fifo.seek(0.0);
        }
    }
    fn inflight(&self, now: f64) -> f64 {
        (now - self.last_arrival).clamp(0.0, 2.0 * self.packet)
    }
    fn virtual_fill(&self, now: f64) -> f64 {
        self.fifo.fill() / self.src.rate + self.inflight(now)
    }
    pub fn push(&mut self, input: &[f32], now: f64) {
        let frames = input.len() / self.src.channels;
        if frames == 0 {
            return;
        }
        // Track packet duration without a permanent 10 ms floor.
        self.packet = (frames as f64 / self.src.rate).max(self.packet * 0.999);
        if self.identity {
            self.fifo.push(input);
        } else {
            for samples in input.chunks_exact(self.src.channels) {
                for d in 0..self.dst.channels {
                    self.frame[d] = samples
                        .iter()
                        .enumerate()
                        .map(|(s, v)| v * self.matrix[d * self.src.channels + s])
                        .sum();
                }
                self.fifo.push(&self.frame);
            }
        }
        self.last_arrival = now;
        self.have_input = true;
        if self.fifo.fill() > (self.target() + 0.1) * self.src.rate {
            self.fifo.seek(self.target() * self.src.rate);
            self.drift.resync(self.target());
            self.resyncs += 1;
        }
    }
    pub fn pull(&mut self, out: &mut [f32], now: f64) -> bool {
        let target = self.target();
        if self.priming {
            if !self.have_input || self.virtual_fill(now) < target {
                out.fill(0.0);
                return false;
            }
            self.fifo
                .seek((target - self.inflight(now)) * self.src.rate);
            self.drift.resync(target);
            self.last_update = now;
            self.priming = false;
            self.fade = (self.dst.rate * 0.005) as usize;
        }
        let dt = (now - self.last_update).max(0.0);
        self.last_update = now;
        self.calm += dt;
        if self.calm > 600.0 && self.margin > self.base_margin {
            self.margin = (self.margin - 0.001).max(self.base_margin);
            self.calm = 0.0;
        }
        let adjust = self.drift.update(self.virtual_fill(now), self.target(), dt);
        if self.fifo.read(out, adjust) < out.len() / self.dst.channels {
            self.underruns += 1;
            self.margin = (self.margin + 0.001).min(0.1);
            self.calm = 0.0;
            self.priming = true;
            out.fill(0.0);
            return false;
        }
        if (self.drift.smoothed - self.target()).abs() > 0.05 {
            self.fifo
                .seek((self.target() - self.inflight(now)) * self.src.rate);
            self.drift.resync(self.target());
            self.resyncs += 1;
            self.fade = (self.dst.rate * 0.005) as usize;
        }
        let total = (self.dst.rate * 0.005) as usize;
        for frame in out.chunks_exact_mut(self.dst.channels) {
            if self.fade == 0 {
                break;
            }
            let gain = (total - self.fade) as f32 / total as f32;
            for v in frame {
                *v *= gain;
            }
            self.fade -= 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sized_rings_and_shared_kernels() {
        let f = stereo();
        let mut cache = KernelCache::default();
        let mut a = Pipeline::new_cached(f, f, 0.01, 0.001, 0, 0.2, &mut cache);
        let b = Pipeline::new_cached(
            f,
            Format {
                channels: 1,
                mask: 4,
                ..f
            },
            0.01,
            0.001,
            1000,
            0.2,
            &mut cache,
        );
        assert!(Arc::ptr_eq(&a.fifo.kernel, &b.fifo.kernel));
        assert!(a.allocation_bytes().0 <= 1048576 / 4);
        assert!(b.fifo.cap as f64 / f.rate > 1.4);
        assert!(a.identity);
        assert!(!b.identity);
        a.drift.integral = 0.000123;
        a.margin = 0.005;
        a.underruns = 7;
        a.resyncs = 2;
        let mut replacement = Pipeline::new_cached(f, f, 0.01, 0.001, 1000, 0.2, &mut cache);
        replacement.inherit_clock(&a);
        assert!(replacement.priming);
        assert_eq!(replacement.delay_ms, 1000);
        assert_eq!(replacement.drift.integral, 0.000123);
        assert_eq!(replacement.margin, 0.005);
        assert_eq!((replacement.underruns, replacement.resyncs), (7, 2));
        assert_eq!(replacement.fifo.fill(), 0.0);
        replacement.set_delay(0);
        assert!(replacement.allocation_bytes().0 <= 1048576 / 4);
        assert_eq!(replacement.drift.integral, 0.000123);
        drop(a);
        drop(b);
        drop(replacement);
        assert!(cache
            .entries
            .values()
            .all(|entry| entry.strong_count() == 0));
        let _new = cache.get(44100.0, 48000.0);
        assert_eq!(cache.entries.len(), 1);
    }
    #[test]
    fn specialized_and_generic_paths_match_across_ring_wrap() {
        for ch in [1, 2, 3, 6, 8] {
            let mut a = Fifo::new(ch, 48000., 48000.);
            let mut b = Fifo::new(ch, 48000., 48000.);
            let packet: Vec<f32> = (0..240 * ch)
                .map(|i| (i as f32 * 0.03).sin() * 0.1)
                .collect();
            let mut x = vec![0.; 240 * ch];
            let mut y = x.clone();
            for _ in 0..(a.cap * 3 / 240) {
                a.push(&packet);
                b.push(&packet);
                if a.fill() > 1000. {
                    assert_eq!(a.read(&mut x, 0.0001), b.read_frames::<0>(&mut y, 0.0001));
                    for (&u, &v) in x.iter().zip(&y) {
                        assert!((u - v).abs() < 1e-6);
                    }
                }
            }
        }
    }
    fn stereo() -> Format {
        Format {
            rate: 48000.0,
            channels: 2,
            mask: 3,
        }
    }
    #[test]
    fn channel_mapping() {
        let f = stereo();
        assert_eq!(matrix(f, f), vec![1., 0., 0., 1.]);
        let mono = Format {
            channels: 1,
            mask: 4,
            ..f
        };
        assert_eq!(matrix(mono, f), vec![1., 1.]);
        let surround = Format {
            channels: 8,
            mask: 1599,
            ..f
        };
        let m = matrix(surround, f);
        assert_eq!(m[3], 0.0);
        assert!(m[2] > 0.7);
    }
    #[test]
    fn sinc_dc_and_rates() {
        for rate in [44100.0, 48000.0, 96000.0] {
            let mut fifo = Fifo::new(2, rate, 48000.0);
            for _ in 0..(rate as usize / 10) {
                fifo.push_frame(&[0.25, -0.25]);
            }
            fifo.seek(rate * 0.08);
            let mut out = vec![0.0; 2048];
            assert_eq!(fifo.read(&mut out, 0.0001), 1024);
            for s in out.as_chunks::<2>().0 {
                assert!((s[0] - 0.25).abs() < 1e-5);
                assert!((s[1] + 0.25).abs() < 1e-5);
            }
        }
    }
    #[test]
    fn delay_and_drift() {
        for delay in [0, 50, 1000] {
            let f = stereo();
            let mut p = Pipeline::new(f, f, 0.003, 0.001, delay);
            let input = vec![0.25; 288];
            let mut out = vec![0.0; 288];
            let mut first = None;
            for i in 0..4000 {
                let now = i as f64 * 0.003;
                p.push(&input, now);
                if p.pull(&mut out, now) && first.is_none() {
                    first = Some(now);
                }
            }
            let first = first.unwrap();
            assert!((first - delay as f64 / 1000.0) < 0.02);
            assert!(first >= delay as f64 / 1000.0);
            assert_eq!(p.underruns, 0);
            p.set_delay(10);
            assert!(p.priming);
        }
    }
    #[test]
    fn clock_tracking() {
        let mut d = Drift::default();
        let mut fill = 0.02;
        for _ in 0..60000 {
            let a = d.update(fill, 0.02, 0.01);
            fill += (0.0003 - a) * 0.01;
        }
        assert!((d.integral - 0.0003).abs() < 0.00002);
        assert!((fill - 0.02).abs() < 0.001);
    }
    #[test]
    fn resampler_accuracy() {
        for (src, dst, hz, min_snr) in [
            (48000., 48000., 1000., 85.),
            (48000., 48000., 10000., 80.),
            (48000., 48000., 18000., 70.),
            (44100., 48000., 1000., 85.),
            (44100., 48000., 15000., 70.),
            (48000., 44100., 1000., 85.),
            (48000., 44100., 15000., 70.),
            (96000., 48000., 1000., 85.),
        ] {
            let mut fifo = Fifo::new(1, src, dst);
            let count = 20000;
            for i in 0..count {
                fifo.push_frame(&[(0.5 * (2. * PI * hz * i as f64 / src).sin()) as f32]);
            }
            let start = 1000.37;
            fifo.seek(count as f64 - start);
            let n = ((count as f64 - start - 200.) / (src / dst)) as usize;
            let mut out = vec![0.; n];
            assert_eq!(fifo.read(&mut out, 0.), n);
            let (mut signal, mut error) = (0., 0.);
            for (i, &v) in out.iter().enumerate() {
                let expected = 0.5 * (2. * PI * hz * (start + i as f64 * src / dst) / src).sin();
                signal += expected * expected;
                error += (v as f64 - expected).powi(2);
            }
            let snr = 10. * (signal / error.max(1e-30)).log10();
            assert!(snr > min_snr, "{src}->{dst}, {hz} Hz: {snr} dB");
        }
    }
    #[test]
    fn independent_delay_offsets_and_recovery() {
        let f = Format {
            rate: 48000.,
            channels: 1,
            mask: 4,
        };
        let mut a = Pipeline::new(f, f, 0.001, 0.001, 0);
        let mut b = Pipeline::new(f, f, 0.001, 0.001, 50);
        let (mut peak_a, mut peak_b) = (None, None);
        let mut x = [0.; 48];
        let mut oa = [0.; 48];
        let mut ob = [0.; 48];
        for i in 0..300 {
            x.fill(0.);
            if i == 100 {
                x[0] = 0.5;
            }
            let now = i as f64 * 0.001;
            a.push(&x, now);
            b.push(&x, now);
            a.pull(&mut oa, now);
            b.pull(&mut ob, now);
            if oa.iter().any(|v| v.abs() > 0.2) {
                peak_a = Some(i);
            }
            if ob.iter().any(|v| v.abs() > 0.2) {
                peak_b = Some(i);
            }
        }
        assert_eq!(peak_b.unwrap() - peak_a.unwrap(), 50);
        b.set_delay(1000);
        assert!(b.priming);
        assert!(!a.priming);
        let mut out = [0.; 48];
        for i in 0..40 {
            a.pull(&mut out, 2. + i as f64 * 0.001);
        }
        assert!(a.resyncs > 0 || a.priming);
        let input = vec![0.1; 48];
        for i in 0..1000 {
            a.push(&input, 2.1 + i as f64 * 0.001);
        }
        assert!(a.resyncs > 0);
        assert!(a.fifo.fill() < (a.target() + 0.101) * f.rate);
    }
    #[test]
    fn simulated_device_clocks() {
        for (src_rate, dst_rate, ppm, jitter_ms) in [
            (48000., 48000., 100., 2.),
            (48000., 48000., -300., 2.),
            (44100., 48000., 50., 2.),
            (48000., 44100., -80., 2.),
            (48000., 48000., 0., 4.),
            (48000., 48000., 1000., 2.),
        ] {
            let src = Format {
                rate: src_rate,
                channels: 1,
                mask: 4,
            };
            let dst = Format {
                rate: dst_rate,
                ..src
            };
            let mut p = Pipeline::new(src, dst, 0.01, 0.005, 0);
            let packet = (src_rate / 100.) as usize;
            let chunk = (dst_rate / 100.) as usize;
            let mut input = vec![0.; packet];
            let mut out = vec![0.; chunk];
            let src_period = packet as f64 / (src_rate * (1. + ppm * 1e-6));
            let dst_period = chunk as f64 / dst_rate;
            let (mut ns, mut nd) = (1, 1);
            let mut rng = 1234u32;
            let mut jitter = || {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                rng as f64 / u32::MAX as f64 * jitter_ms / 1000.
            };
            let (mut ts, mut td) = (src_period + jitter(), dst_period + jitter());
            let mut index = 0;
            let mut max_error: f64 = 0.;
            let (mut prev1, mut prev2, mut curvature) = (0.0f64, 0.0f64, 0.0f64);
            while ts.min(td) < 300. {
                if ts <= td {
                    for v in &mut input {
                        *v = (0.5 * (2. * PI * 440. * index as f64 / src_rate).sin()) as f32;
                        index += 1;
                    }
                    p.push(&input, ts);
                    ns += 1;
                    ts = ns as f64 * src_period + jitter();
                } else {
                    p.pull(&mut out, td);
                    for &sample in &out {
                        let v = sample as f64;
                        if td > 5.0 {
                            curvature = curvature.max((v - 2.0 * prev1 + prev2).abs());
                        }
                        prev2 = prev1;
                        prev1 = v;
                    }
                    if td > 120. {
                        max_error = max_error.max((p.drift.smoothed - p.target()).abs());
                    }
                    nd += 1;
                    td = nd as f64 * dst_period + jitter();
                }
            }
            assert_eq!(p.underruns, 0, "{src_rate}->{dst_rate} {ppm} ppm");
            assert_eq!(p.resyncs, 0);
            assert!(
                max_error < 0.002,
                "{src_rate}->{dst_rate}, {ppm} ppm: level error {max_error}"
            );
            assert!(
                (p.drift.integral * 1e6 - ppm).abs() < 10.,
                "learned {} vs {ppm}",
                p.drift.integral * 1e6
            );
            let clean = 0.5 * (2.0 * PI * 440.0 / dst_rate).powi(2) * 1.5;
            assert!(
                curvature < clean,
                "output discontinuity: curvature {curvature} > {clean}"
            );
        }
    }
}
