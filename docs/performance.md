# Performance measurements

Release builds on a Ryzen 7 8845HS, using Rust 1.99.1 and LLVM-MinGW UCRT.
These are local comparisons under ordinary desktop load, without CPU affinity or
frequency locking. They describe this build and machine, not universal limits.
The baseline is the Rust rewrite before this optimization pass.

## DSP benchmark

Each case warms up for two simulated audio seconds, then measures 100 seconds in
10 ms chunks. Setup and coefficient construction are outside the timing window.
The benchmark checks that processing did not fall into an underrun/silence path.
Reported time is elapsed processing time, including scheduler interference.

| Workload | Before, ms/audio second | After | Reduction |
| --- | ---: | ---: | ---: |
| 48 kHz mono | 2.3483 | 1.5613 | 34% |
| 48 kHz stereo | 4.6939 | 1.9111 | 59% |
| 44.1 → 48 kHz stereo | 5.2717 | 2.0599 | 61% |
| 48 → 44.1 kHz stereo | 4.9211 | 2.0074 | 59% |
| 48 kHz, eight channels | 17.6920 | 2.1964 | 88% |
| 48 kHz, eight channels → stereo | 4.9482 | 2.6216 | 47% |
| 48 kHz stereo, 1000 ms delay | 5.1000 | 1.8249 | 64% |

At zero delay, the stereo ring uses 262,144 bytes instead of 1,048,576 bytes,
a 75% reduction. The eight-channel ring drops from 4 MiB to 1 MiB. Maximum-delay
rings remain large enough for the requested delay and recovery headroom.
The 48-tap kernel occupies 49,344 bytes and is shared across matching rate pairs;
each stream has a small private coefficient scratch buffer.

Raw results: [baseline](benchmarks/baseline.csv), [optimized](benchmarks/optimized.csv).
Repeat with `sas-cli bench`. Compare repeated runs on the same machine and workload.

## Live process measurements

A 30-second pilot mirrored the Senary Audio default output to Acer/AMD HDMI, at
48 kHz stereo with a 1 ms safety margin. Samples after the first three seconds:

| Metric | Before | After |
| --- | ---: | ---: |
| Process CPU time per elapsed second | 18.18 ms | 6.23 ms |
| Average private memory | 3.23 MB | 2.46 MB |
| Average working set | 12.20 MB | 11.43 MB |

CPU time sums the process's threads; 10 ms per second is 1% of one logical core.
Working set includes Windows libraries and varies with memory pressure. These
values are decimal MB, distinct from the binary MiB ring sizes above.

An idle check with only the direct default output selected recorded zero audio
wakeups and no recurring control wakeups. No audio stream was opened by that
engine. Device changes can legitimately wake the control thread.

## Latency acceptance

The long-run margin comparison is recorded by `tools/perf-check.ps1`; its logs and
JSON include requested/elapsed duration, pass/fail, CPU time, private memory,
working set, and digital signal verification results. A candidate is rejected
after any pipeline underrun or resync. The test does not infer acoustic or wireless
transport delay from WASAPI loopback timestamps.

The zero-margin pilot produced underruns within five seconds. The 0.5 ms pilot
had zero underruns but required two resyncs after about 607 seconds, so it also
failed acceptance. The existing 1 ms default remains; audio quality thresholds
and drift correction are unchanged.

The 1 ms run reached approximately 23 minutes with zero underruns and resyncs
before further testing was stopped at the user's request. It did not complete
the planned 30-minute acceptance run. Windows volume/mute following was added
afterward and built without further runtime tests, as requested.

The implementation also has coverage for shared kernels, ring wraparound,
specialized/generic numerical agreement, idle shutdown, notification filtering,
and the existing signal-fidelity, delay, settings, and native-dialog cases.
