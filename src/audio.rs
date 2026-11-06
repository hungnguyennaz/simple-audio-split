//! WASAPI ownership stays inside MTA threads; no COM pointers reach the UI.
use crate::dsp::Format;
use std::{
    ptr, slice,
    time::{Duration, Instant},
};
use windows::{
    core::{Error, Interface, Result, HRESULT, PCWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{CloseHandle, HANDLE},
        Media::Audio::*,
        System::{Com::StructuredStorage::PropVariantToStringAlloc, Com::*, Threading::*},
    },
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub struct Com;
impl Com {
    pub fn init() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
pub fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}
unsafe fn take_string(p: windows::core::PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0.cast()));
    s
}
pub fn default_id(en: &IMMDeviceEnumerator) -> Result<String> {
    unsafe {
        Ok(take_string(
            en.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?,
        ))
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}
pub fn devices(en: &IMMDeviceEnumerator) -> Result<Vec<Device>> {
    unsafe {
        let default = default_id(en).unwrap_or_default();
        let collection = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut list = Vec::new();
        for i in 0..collection.GetCount()? {
            let d = collection.Item(i)?;
            let id = take_string(d.GetId()?);
            let name = d
                .OpenPropertyStore(STGM_READ)
                .and_then(|p| p.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|v| PropVariantToStringAlloc(&v))
                .map(|p| take_string(p))
                .unwrap_or_else(|_| id.clone());
            list.push(Device {
                is_default: id == default,
                id,
                name,
            });
        }
        Ok(list)
    }
}
pub fn resolve(en: &IMMDeviceEnumerator, selector: &str) -> Result<String> {
    if selector == "default" {
        return default_id(en);
    }
    let list = devices(en)?;
    if let Ok(i) = selector.parse::<usize>() {
        return list
            .get(i)
            .map(|d| d.id.clone())
            .ok_or_else(|| Error::new(HRESULT(0x80070057u32 as i32), "Unknown device index"));
    }
    if list.iter().any(|d| d.id == selector) {
        Ok(selector.into())
    } else {
        Err(Error::new(HRESULT(0x80070057u32 as i32), "Unknown device"))
    }
}
pub fn device_format(en: &IMMDeviceEnumerator, id: &str) -> Result<AudioFormat> {
    unsafe {
        let id = wide(id);
        let device = en.GetDevice(PCWSTR(id.as_ptr()))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = Mix(client.GetMixFormat()?);
        AudioFormat::parse(mix.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sample {
    Float,
    Int16,
    Int24,
    Int32,
}
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub dsp: Format,
    pub sample: Sample,
    pub align: usize,
    pub valid_bits: u16,
}
impl AudioFormat {
    unsafe fn parse(p: *const WAVEFORMATEX) -> Result<Self> {
        let wf = ptr::read_unaligned(p);
        let mut tag = wf.wFormatTag;
        let mut mask = 0;
        let mut valid = wf.wBitsPerSample;
        if tag == 0xfffe && wf.cbSize >= 22 {
            let ext = ptr::read_unaligned(p.cast::<WAVEFORMATEXTENSIBLE>());
            let subtype = ext.SubFormat;
            tag = if subtype
                == windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71)
            {
                1
            } else if subtype
                == windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71)
            {
                3
            } else {
                return Err(Error::from_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT));
            };
            mask = ext.dwChannelMask;
            valid = ext.Samples.wValidBitsPerSample;
        }
        let sample = match (tag, wf.wBitsPerSample) {
            (3, 32) => Sample::Float,
            (1, 16) => Sample::Int16,
            (1, 24) => Sample::Int24,
            (1, 32) => Sample::Int32,
            _ => return Err(Error::from_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT)),
        };
        let bytes = match sample {
            Sample::Int16 => 2,
            Sample::Int24 => 3,
            _ => 4,
        };
        if wf.nChannels == 0
            || wf.nChannels > 32
            || wf.nSamplesPerSec < 8000
            || wf.nSamplesPerSec > 384000
            || wf.nBlockAlign as usize != wf.nChannels as usize * bytes
            || valid == 0
            || valid > wf.wBitsPerSample
        {
            return Err(Error::from_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT));
        }
        Ok(Self {
            dsp: Format {
                rate: wf.nSamplesPerSec as f64,
                channels: wf.nChannels as usize,
                mask,
            },
            sample,
            align: wf.nBlockAlign as usize,
            valid_bits: valid,
        })
    }
    pub fn decode(&self, bytes: &[u8], out: &mut [f32]) {
        let size = self.align / self.dsp.channels;
        for (b, o) in bytes.chunks_exact(size).zip(out) {
            *o = match self.sample {
                Sample::Float => f32::from_le_bytes(b.try_into().unwrap()),
                Sample::Int16 => i16::from_le_bytes(b.try_into().unwrap()) as f32 / 32768.0,
                Sample::Int24 => {
                    ((u32::from(b[0]) << 8 | u32::from(b[1]) << 16 | u32::from(b[2]) << 24) as i32
                        >> 8) as f32
                        / 8388608.0
                }
                Sample::Int32 => {
                    (i32::from_le_bytes(b.try_into().unwrap()) as f64 / 2147483648.0) as f32
                }
            };
        }
    }
    pub fn encode(&self, input: &[f32], bytes: &mut [u8]) {
        let size = self.align / self.dsp.channels;
        for (&v, b) in input.iter().zip(bytes.chunks_exact_mut(size)) {
            let v = if v.is_finite() { v as f64 } else { 0.0 };
            match self.sample {
                Sample::Float => b.copy_from_slice(&(v as f32).to_le_bytes()),
                Sample::Int16 => b.copy_from_slice(
                    &((v * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                ),
                Sample::Int24 => b.copy_from_slice(
                    &((v * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32).to_le_bytes()
                        [..3],
                ),
                Sample::Int32 => {
                    let shift = 32 - self.valid_bits;
                    let q = ((v * 2147483648.0)
                        .round()
                        .clamp(-2147483648.0, 2147483647.0) as i32
                        >> shift)
                        << shift;
                    b.copy_from_slice(&q.to_le_bytes());
                }
            }
        }
    }
}
struct Mix(*mut WAVEFORMATEX);
impl Drop for Mix {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}
pub struct Event(pub HANDLE);
// Windows event handles may be signaled/waited from any thread; this wrapper owns closure.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}
impl Event {
    pub fn new() -> Result<Self> {
        unsafe { Ok(Self(CreateEventW(None, false, false, None)?)) }
    }
    pub fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Stream {
    pub client: IAudioClient,
    pub render: Option<IAudioRenderClient>,
    pub capture: Option<IAudioCaptureClient>,
    pub event: Event,
    pub format: AudioFormat,
    pub capacity: u32,
    pub period: u32,
    pub fallback: bool,
    pub events: bool,
    pub samples: Vec<f32>,
}
// Streams are transferred between COM MTA threads with exclusive ownership, never shared.
unsafe impl Send for Stream {}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
impl Stream {
    pub fn open(en: &IMMDeviceEnumerator, id: &str, capture: bool) -> Result<Self> {
        let started = Instant::now();
        unsafe {
            let id = wide(id);
            let dev = en.GetDevice(PCWSTR(id.as_ptr()))?;
            let mut client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
            let mix = Mix(client.GetMixFormat()?);
            let format = AudioFormat::parse(mix.0)?;
            let event = Event::new()?;
            let mut period = 0;
            let mut fallback = true;
            let mut events = true;
            if !capture {
                if let Ok(c3) = client.cast::<IAudioClient3>() {
                    let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
                    if c3
                        .GetSharedModeEnginePeriod(
                            mix.0,
                            &mut default,
                            &mut fundamental,
                            &mut min,
                            &mut max,
                        )
                        .is_ok()
                        && min > 0
                        && c3
                            .InitializeSharedAudioStream(
                                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                                min,
                                mix.0,
                                None,
                            )
                            .is_ok()
                    {
                        period = min;
                        fallback = false;
                    }
                }
            }
            if fallback {
                // Re-activate after a failed initialization; a partially initialized client is not reusable.
                client = dev.Activate(CLSCTX_ALL, None)?;
                let flags = if capture {
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                } else {
                    0
                } | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
                if let Err(e) = client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    if capture { 2_000_000 } else { 0 },
                    0,
                    mix.0,
                    None,
                ) {
                    if !capture {
                        return Err(e);
                    }
                    events = false;
                    client = dev.Activate(CLSCTX_ALL, None)?;
                    client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        2_000_000,
                        0,
                        mix.0,
                        None,
                    )?;
                }
                let mut hns = 0;
                client.GetDevicePeriod(Some(&mut hns), None)?;
                period = ((format.dsp.rate * hns as f64 / 1e7).round() as u32).max(1);
            }
            if events {
                client.SetEventHandle(event.0)?;
            }
            let capacity = client.GetBufferSize()?;
            let render = if capture {
                None
            } else {
                Some(client.GetService::<IAudioRenderClient>()?)
            };
            let capture_client = if capture {
                Some(client.GetService::<IAudioCaptureClient>()?)
            } else {
                None
            };
            if let Some(r) = &render {
                let n = period.min(capacity);
                let _ = r.GetBuffer(n)?;
                r.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            client.Start()?;
            if std::env::var_os("SAS_TRACE").is_some() {
                eprintln!("Opened {}: {:.0} Hz, {} channels, {:?}, period {:.2} ms, fallback {}, events {} in {:.1} ms",if capture {"loopback"}else{"render"},format.dsp.rate,format.dsp.channels,format.sample,period as f64/format.dsp.rate*1000.0,fallback,events,started.elapsed().as_secs_f64()*1000.0);
            }
            Ok(Self {
                client,
                render,
                capture: capture_client,
                event,
                format,
                capacity,
                period,
                fallback,
                events,
                samples: vec![0.0; capacity as usize * format.dsp.channels],
            })
        }
    }
    pub fn silence(&mut self) -> Result<()> {
        unsafe {
            let r = self.render.as_ref().unwrap();
            let padding = self.client.GetCurrentPadding()?;
            let n = self.capacity.saturating_sub(padding);
            if n > 0 {
                r.GetBuffer(n)?;
                r.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            Ok(())
        }
    }
    pub fn drain(&mut self, mut feed: impl FnMut(&[f32], bool)) -> Result<()> {
        self.drain_timed(|samples, discontinuity, _| feed(samples, discontinuity))
    }
    /// Packet timestamp is the first frame's QPC time in 100 ns units, or zero if invalid.
    pub fn drain_timed(&mut self, mut feed: impl FnMut(&[f32], bool, u64)) -> Result<()> {
        unsafe {
            let c = self.capture.as_ref().unwrap();
            while c.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (ptr::null_mut(), 0, 0);
                let mut qpc = 0;
                c.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                if frames == 0 {
                    break;
                }
                let n = frames as usize * self.format.dsp.channels;
                if n > self.samples.len() {
                    c.ReleaseBuffer(frames)?;
                    return Err(Error::from_hresult(AUDCLNT_E_BUFFER_SIZE_ERROR));
                }
                let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                let qpc = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                    0
                } else {
                    qpc
                };
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0
                    && self.format.sample == Sample::Float
                    && !data.is_null()
                    && data.cast::<f32>().is_aligned()
                {
                    // WASAPI owns this aligned slice until ReleaseBuffer; consumers cannot retain it.
                    feed(
                        slice::from_raw_parts(data.cast::<f32>(), n),
                        discontinuity,
                        qpc,
                    );
                    c.ReleaseBuffer(frames)?;
                    continue;
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    self.samples[..n].fill(0.0);
                } else {
                    if data.is_null() {
                        c.ReleaseBuffer(frames)?;
                        return Err(Error::from_hresult(AUDCLNT_E_BUFFER_ERROR));
                    }
                    self.format.decode(
                        slice::from_raw_parts(data, frames as usize * self.format.align),
                        &mut self.samples[..n],
                    );
                }
                feed(&self.samples[..n], discontinuity, qpc);
                c.ReleaseBuffer(frames)?;
            }
            Ok(())
        }
    }
    pub fn render_with(&mut self, mut fill: impl FnMut(&mut [f32])) -> Result<()> {
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let n = self.period.min(self.capacity).saturating_sub(padding);
            if n == 0 {
                return Ok(());
            }
            let count = n as usize * self.format.dsp.channels;
            let r = self.render.as_ref().unwrap();
            let data = r.GetBuffer(n)?;
            if self.format.sample == Sample::Float
                && !data.is_null()
                && data.cast::<f32>().is_aligned()
            {
                let output = slice::from_raw_parts_mut(data.cast::<f32>(), count);
                fill(output);
                for value in output {
                    if !value.is_finite() {
                        *value = 0.0;
                    }
                }
            } else {
                fill(&mut self.samples[..count]);
                self.format.encode(
                    &self.samples[..count],
                    slice::from_raw_parts_mut(data, n as usize * self.format.align),
                );
            }
            r.ReleaseBuffer(n, 0)
        }
    }
}
pub fn tone(id: &str) -> Result<()> {
    let _com = Com::init()?;
    let en = enumerator()?;
    let mut s = Stream::open(&en, id, false)?;
    let rate = s.format.dsp.rate;
    let channels = s.format.dsp.channels;
    let mut position = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(850) {
        s.render_with(|out| {
            for frame in out.chunks_exact_mut(channels) {
                let t = position as f64 / rate;
                let envelope = (1.0 - t / 0.75).max(0.0) * (t / 0.01).min(1.0);
                let hz = if t < 0.25 {
                    523.25
                } else if t < 0.5 {
                    659.25
                } else {
                    783.99
                };
                frame.fill((0.1 * envelope * (t * hz * std::f64::consts::TAU).sin()) as f32);
                position += 1;
            }
        })?;
        unsafe {
            WaitForSingleObject(s.event.0, 10);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversion() {
        for (sample, size, bits) in [
            (Sample::Float, 4, 32),
            (Sample::Int16, 2, 16),
            (Sample::Int24, 3, 24),
            (Sample::Int32, 4, 32),
        ] {
            let f = AudioFormat {
                dsp: Format {
                    rate: 48000.,
                    channels: 1,
                    mask: 4,
                },
                sample,
                align: size,
                valid_bits: bits,
            };
            let input = [-1., -0.5, 0., 0.5];
            let mut bytes = vec![0; size * input.len()];
            let mut output = [0.; 4];
            f.encode(&input, &mut bytes);
            f.decode(&bytes, &mut output);
            assert_eq!(input, output);
        }
    }
}
