# Performance measurements

Release builds on a Ryzen 7 8845HS, using Rust 1.99.1 and LLVM-MinGW UCRT.
These are local comparisons under ordinary desktop load, without CPU affinity or
frequency locking. They describe this build and machine, not universal limits.

## DSP benchmark

Each case warms up for two simulated audio seconds, then measures 100 seconds in
10 ms chunks. Setup and coefficient construction are outside the timing window.
The benchmark checks that processing did not fall into an underrun/silence path.
Reported time is elapsed processing time, including scheduler interference.

| Workload | Processing ms/audio second |
| --- | ---: |
| 48 kHz mono | 1.5613 |
| 48 kHz stereo | 1.9111 |
| 44.1 → 48 kHz stereo | 2.0599 |
| 48 → 44.1 kHz stereo | 2.0074 |
| 48 kHz, eight channels | 2.1964 |
| 48 kHz, eight channels → stereo | 2.6216 |
| 48 kHz stereo, 1000 ms delay | 1.8249 |

At zero delay, the stereo ring uses 256 KiB and the eight-channel ring uses 1 MiB. Maximum-delay
rings remain large enough for the requested delay and recovery headroom.
The 48-tap kernel occupies 49,344 bytes and is shared across matching rate pairs;
each stream has a small private coefficient scratch buffer.

Raw results: [current DSP measurements](benchmarks/current.csv).
Repeat with `sas-cli bench`. Compare repeated runs on the same machine and workload.

## Live process measurements

A 30-second pilot mirrored the Senary Audio default output to Acer/AMD HDMI, at
48 kHz stereo with a 1 ms safety margin. Samples after the first three seconds:

| Metric | Measured value |
| --- | ---: |
| Process CPU time per elapsed second | 6.23 ms |
| Average private memory | 2.46 MB |
| Average working set | 11.43 MB |

CPU time sums the process's threads; 10 ms per second is 1% of one logical core.
Working set includes Windows libraries and varies with memory pressure. These
values are decimal MB, distinct from the binary MiB ring sizes above.

An idle check with only the direct default output selected recorded zero audio
wakeups and no recurring control wakeups. No audio stream was opened by that
engine. Device changes can legitimately wake the control thread.

## Latency acceptance

The default 1 ms safety margin reached approximately 23 minutes with zero
underruns and resyncs before further testing was stopped at the user's request.
Smaller margins did not pass acceptance. Windows volume/mute following was added
afterward and built without further runtime tests, as requested. Digital loopback
measurements do not include acoustic or wireless transport delay.

The implementation also has coverage for shared kernels, ring wraparound,
specialized/generic numerical agreement, idle shutdown, notification filtering,
and the existing signal-fidelity, delay, settings, and native-dialog cases.
