//! Deterministic offline workload; does not open devices or play sound.
use crate::dsp::{Format, Pipeline};
use std::{hint::black_box, time::Instant};
pub fn run() {
    println!("src_hz,dst_hz,src_ch,dst_ch,delay_ms,ms_per_audio_second,p95_chunk_us,p99_chunk_us,ring_bytes,kernel_bytes");
    for (sr, dr, sc, dc, delay) in [
        (48000., 48000., 1, 1, 0),
        (48000., 48000., 2, 2, 0),
        (44100., 48000., 2, 2, 0),
        (48000., 44100., 2, 2, 0),
        (48000., 48000., 8, 8, 0),
        (48000., 48000., 8, 2, 0),
        (48000., 48000., 2, 2, 1000),
    ] {
        let src = Format {
            rate: sr,
            channels: sc,
            mask: 0,
        };
        let dst = Format {
            rate: dr,
            channels: dc,
            mask: 0,
        };
        let mut pipe = Pipeline::new(src, dst, 0.01, 0.001, delay);
        let packet = (sr / 100.) as usize;
        let chunk = (dr / 100.) as usize;
        let input: Vec<f32> = (0..packet * sc)
            .map(|i| (0.1 * (i as f64 * 0.05).sin()) as f32)
            .collect();
        let mut output = vec![0.; chunk * dc];
        let mut times = Vec::with_capacity(10000);
        let mut total = 0u128;
        for i in 0..10200 {
            let time = i as f64 * 0.01;
            let start = Instant::now();
            pipe.push(black_box(&input), time);
            pipe.pull(black_box(&mut output), time);
            black_box(&output);
            let ns = start.elapsed().as_nanos();
            if i >= 200 {
                times.push(ns);
                total += ns;
            }
        }
        assert_eq!(
            pipe.underruns, 0,
            "benchmark must not measure a silent recovery path"
        );
        times.sort_unstable();
        let (ring, kernel) = pipe.allocation_bytes();
        println!(
            "{sr:.0},{dr:.0},{sc},{dc},{delay},{:.4},{:.3},{:.3},{ring},{kernel}",
            total as f64 / 1e6 / 100.0,
            times[9500] as f64 / 1000.,
            times[9900] as f64 / 1000.
        );
    }
}
