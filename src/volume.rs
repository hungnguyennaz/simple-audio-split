//! Follow the source endpoint's master controls without polling or changing target volumes.
use crate::audio::{wide, Event};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use windows::{
    core::{implement, Result, PCWSTR},
    Win32::{
        Media::Audio::{Endpoints::*, *},
        System::Com::CLSCTX_ALL,
    },
};

#[implement(IAudioEndpointVolumeCallback)]
struct Notification {
    dirty: Arc<AtomicBool>,
    wake: Arc<Event>,
}
impl IAudioEndpointVolumeCallback_Impl for Notification_Impl {
    fn OnNotify(&self, _: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        // Never call back into the volume API from its notification thread.
        self.dirty.store(true, Ordering::Release);
        self.wake.signal();
        Ok(())
    }
}
pub struct SourceVolume {
    endpoint: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
    dirty: Arc<AtomicBool>,
    gain: Arc<AtomicU32>,
}
impl SourceVolume {
    pub fn open(
        en: &IMMDeviceEnumerator,
        id: &str,
        wake: Arc<Event>,
        gain: Arc<AtomicU32>,
    ) -> Result<Self> {
        unsafe {
            let id = wide(id);
            let device = en.GetDevice(PCWSTR(id.as_ptr()))?;
            let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
            let dirty = Arc::new(AtomicBool::new(true));
            let callback: IAudioEndpointVolumeCallback = Notification {
                dirty: dirty.clone(),
                wake,
            }
            .into();
            endpoint.RegisterControlChangeNotify(&callback)?;
            let volume = Self {
                endpoint,
                callback,
                dirty,
                gain,
            };
            volume.refresh()?;
            Ok(volume)
        }
    }
    pub fn refresh_if_changed(&self) -> Result<()> {
        if self.dirty.load(Ordering::Acquire) {
            self.refresh()?;
        }
        Ok(())
    }
    fn refresh(&self) -> Result<()> {
        unsafe {
            self.dirty.store(false, Ordering::Release);
            let gain = if self.endpoint.GetMute()?.as_bool()
                || self.endpoint.GetMasterVolumeLevelScalar()? <= 0.0
            {
                0.0
            } else {
                // Endpoint scalar percentages are audio-tapered, not linear sample gains.
                10.0f32.powf(self.endpoint.GetMasterVolumeLevel()? / 20.0)
            };
            self.gain.store(
                if gain.is_finite() {
                    gain.max(0.0).to_bits()
                } else {
                    0.0f32.to_bits()
                },
                Ordering::Release,
            );
            Ok(())
        }
    }
}
impl Drop for SourceVolume {
    fn drop(&mut self) {
        unsafe {
            let _ = self.endpoint.UnregisterControlChangeNotify(&self.callback);
        }
    }
}

pub struct Gain {
    current: f32,
    target: f32,
    step: f32,
    remaining: usize,
}
impl Default for Gain {
    fn default() -> Self {
        Self {
            current: 0.0,
            target: 0.0,
            step: 0.0,
            remaining: 0,
        }
    }
}
impl Gain {
    pub fn apply(&mut self, out: &mut [f32], channels: usize, rate: f64, target: f32) {
        if target <= 0.0 {
            out.fill(0.0);
            self.current = 0.0;
            self.target = 0.0;
            self.remaining = 0;
            return;
        }
        if target != self.target {
            self.target = target;
            self.remaining = (rate * 0.005).round().max(1.0) as usize;
            self.step = (target - self.current) / self.remaining as f32;
        }
        let ramp = (out.len() / channels).min(self.remaining);
        for frame in out[..ramp * channels].chunks_exact_mut(channels) {
            self.current += self.step;
            for value in frame {
                *value *= self.current;
            }
            self.remaining -= 1;
        }
        if self.remaining == 0 {
            self.current = self.target;
        }
        if self.current != 1.0 {
            for value in &mut out[ramp * channels..] {
                *value *= self.current;
            }
        }
    }
}
