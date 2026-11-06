# Simple Audio Split

Plays your PC's sound on several Windows output devices at once. Written in Rust,
using native Windows controls and WASAPI. No virtual cable, driver, admin rights,
or separately installed runtime is required to run the release executable.

## Using it

1. Run `SimpleAudioSplit.exe` and check the outputs that should receive a copy.
2. Select a device and use **Play test sound** to identify it.
3. To align copies, select a checked output, enter **Extra delay (ms)** from
   **0 to 1000**, and choose **Apply delay**. Each output remembers its own delay.
   Zero adds no intentional delay. Changing one output's delay re-buffers only it.

The Windows default output always plays directly: its audio is the source being
copied, so this app cannot delay it. Align mirrored devices by delaying faster
copies. Controlling the timing of every physical output would require routing
through a separate virtual audio device.

Windows master volume and mute on the default output also control the mirrored
audio. Each secondary device retains its own Windows/device volume control.
Changes are applied during rendering, so a configured sync delay does not leave
old buffered audio playing after mute.

**Close** hides the window and keeps audio running in the notification area.
Click the tray icon or launch the app again to reopen it; **Exit** stops it.
**Start with Windows** starts silently in the tray at sign-in.

## Latency

There is no fixed 40-41 ms delay target. The engine requests the output's minimum
supported shared-mode period through `IAudioClient3`, keeps one rendering period
queued, and sizes its pipeline from actual capture packet durations. It starts
with a 1 ms safety margin, increases that margin after a pipeline underrun, and
slowly reduces it after a long stable interval. Drivers that reject the low-period
API fall back to ordinary shared-mode WASAPI; diagnostics identify that fallback.

This cannot guarantee zero latency or a universal millisecond limit. Loopback
capture, drivers, independent device clocks, and wireless transport impose limits.
The window shows **estimated** latency, including intentional delay; wireless and
acoustic transport are excluded. The default output is not delayed by this app.

During local validation, a Senary Audio default output mirrored to an Acer/AMD
HDMI output measured **32 ms** through WASAPI loopback with zero added delay and
**82 ms** with 50 ms added. These short checks had no pipeline underruns or resyncs.
Those are results for that device pair, not a hardware-independent guarantee.

## Performance

The engine uses block copies for matching channel layouts, shared resampler
coefficients, specialized mono/stereo/multichannel processing, and bounded rings
sized for the selected delay. Changing a delay prepares its replacement buffer
on the control thread and retains the learned clock correction.

Normal operation waits on audio events. Timed polling remains a compatibility
fallback for drivers without capture events or configurations exceeding Windows'
wait-handle limit. With no mirrored outputs, capture and audio polling stop.
The hidden window suspends its refresh timer; device notifications still handle
reconnection and renamed devices.

In local release benchmarks, stereo DSP processing fell from **4.69 to 1.91 ms
per audio second**. A zero-delay 48 kHz stereo ring fell from **1 MiB to 256 KiB**.
Maximum sync delays reserve more memory as needed. See
[measurement details](docs/performance.md) and the benchmark CSVs for the tested
workloads and limitations.

## How it works

A control thread opens and recovers streams. A separate MMCSS audio thread drains
loopback capture, converts channels, and feeds a sinc resampler for each output.
A drift controller continuously adjusts the resampling ratio for independent
hardware clocks. Audio buffers are allocated when streams are configured.
A silent source stream keeps loopback capture active during silence.

Device notifications and retries handle unplugged devices, default-output changes,
format changes, and streams interrupted by exclusive-mode applications. Opening a
device happens outside the audio thread. A saved device can be recognized by its
name after moving USB ports when the match is unambiguous; identical names are
never merged automatically.

## Building

The standard build requires Rust **1.88 or later** with the MSVC toolchain and
Visual Studio 2022 or later's **Desktop development with C++** workload (linker and
Windows SDK/resource compiler; the application itself contains no C++).

```bat
build.bat
```

This runs the release build and tests, then copies the standalone binaries to
`build\SimpleAudioSplit.exe` and `build\sas-cli.exe`. The MSVC runtime is linked
statically. Equivalent development checks:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --release --locked
cargo build --release --locked
```

Alternatively, with a portable LLVM-MinGW UCRT toolchain installed in
`C:\tools\llvm-mingw`, PowerShell can build without Visual Studio:

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnullvm --profile minimal
$env:PATH = "C:\tools\llvm-mingw\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER = "x86_64-w64-mingw32-clang.exe"
$env:WINDRES = "llvm-windres.exe"
cargo +stable-x86_64-pc-windows-gnullvm test --release --locked
cargo +stable-x86_64-pc-windows-gnullvm build --release --locked
```

Both configurations link their runtime statically. Binaries are in `target\release`.
CI builds and tests on Windows; pushing a `v*` tag retains the GitHub release flow.
`python tools\make_icon.py` regenerates the icons.

## Diagnostics

```text
sas-cli list
sas-cli bench
sas-cli run --target 1 --seconds 60
sas-cli run --target 1 --target 2 --delay 1=25 --delay 2=80
sas-cli verify --source default --target 1 --seconds 8 --level -20
sas-cli verify --source default --target 1 --delay 1=50
sas-cli churn --target 1 --target 2 --seconds 10
sas-cli tone 1
```

Device selectors are indexes from `list`, `default`, or full endpoint IDs. Delays
must be whole milliseconds from 0 to 1000 and refer to selected targets. `run`
also accepts `--source D` and `--margin MS` (0-100 ms).

`run` reports the negotiated period, fallback state, estimated pipeline and queue
latency, requested delay, learned clock drift, pipeline underruns, and resyncs.
`verify` plays quiet filtered noise, records both endpoints, anchors them to their
first WASAPI packet timestamps, and filters to 100 Hz before measuring correlation
and offset at 1 ms resolution. It
exits unsuccessfully if correlation is below 0.90. The noise drive level defaults
to -20 dBFS before filtering; very low levels such as -50 can produce unreliable
correlation on devices with noise or signal processing. Close other mirroring apps
and adjust endpoint volumes when measuring. This measures the digital
loopback paths, not sound reaching your ears. Other playing audio can affect it.
`churn` repeatedly removes and adds the second output while the first keeps playing.
Set `SAS_TRACE=1` to log stream formats, negotiated periods, fallback decisions,
and opening times.

`bench` runs deterministic DSP workloads without opening audio streams. It reports
processing milliseconds per audio second, 95th/99th-percentile chunk times, ring
bytes, and kernel bytes. Kernels are shared when multiple outputs use the same
sample-rate pair. `run` also reports audio/control wake counts.

For live stability and process-memory measurements, run
`tools/perf-check.ps1 -TargetDevice 1 -Seconds 1800 -Verify` in PowerShell.
It tests 0, 0.5, and 1 ms safety margins, stops a candidate after an underrun or
resync, and writes logs and JSON into `target/perf`. It temporarily closes running
Simple Audio Split windows to isolate playback and restores them in the tray.

## Source and tests

- `rust/audio.rs`: device enumeration, WASAPI streams, sample conversion, test chime.
- `rust/dsp.rs`: channel mapping, sinc FIFO, drift control, per-output delay/recovery.
- `rust/bench.rs`: deterministic DSP timing and allocation benchmarks.
- `rust/engine.rs`: control/audio threads, notifications, recovery, status.
- `rust/app.rs`: native dialog, tray, autostart controls and delay controls.
- `rust/settings.rs`: compatible INI persistence and startup registry entry.
- `rust/cli.rs`: diagnostics, timestamp-aligned signal verification.

Tests cover resampling fidelity, format conversion, channel layouts, simulated
clock drift and scheduling jitter, delay offsets and changes, underrun recovery,
settings migration, ambiguous reconnects, CLI validation/correlation, and actual
native delay-control behavior using isolated test settings.

## Privacy and uninstalling

The application never connects to the internet. Choices remain in
`%APPDATA%\SimpleAudioSplit\settings.ini`; existing settings load with zero extra
delay. New files retain UTF-16 encoding and add `DelayMsN` per saved target.
Startup uses `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\SimpleAudioSplit`.

To uninstall, disable **Start with Windows**, choose **Exit**, then delete the
executable and settings folder.

## License

[MIT](LICENSE)
