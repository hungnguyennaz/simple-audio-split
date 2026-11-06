use crate::audio::wide;
use std::{collections::HashMap, fs, io, path::PathBuf};
use windows::{
    core::{w, PCWSTR},
    Win32::System::Registry::*,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedDevice {
    pub id: String,
    pub name: String,
    pub extra_delay_ms: u32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub tray_hint_shown: bool,
    pub targets: Vec<SavedDevice>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            tray_hint_shown: false,
            targets: Vec::new(),
        }
    }
}
/// Rebind only an unambiguous saved device; identical headset names never merge.
pub fn reconnect_devices(settings: &mut Settings, devices: &[crate::audio::Device]) -> bool {
    let original = settings.targets.clone();
    let mut changed = false;
    for target in &mut settings.targets {
        if let Some(device) = devices.iter().find(|d| d.id == target.id) {
            if target.name != device.name {
                target.name = device.name.clone();
                changed = true;
            }
            continue;
        }
        if original.iter().filter(|d| d.name == target.name).count() != 1 {
            continue;
        }
        let mut matches = devices
            .iter()
            .filter(|d| d.name == target.name && !original.iter().any(|s| s.id == d.id));
        if let (Some(device), None) = (matches.next(), matches.next()) {
            target.id = device.id.clone();
            changed = true;
        }
    }
    changed
}
pub fn path() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("SimpleAudioSplit/settings.ini"))
        .ok_or_else(|| io::Error::other("APPDATA is unavailable"))
}
pub fn parse(text: &str) -> Settings {
    let mut values = HashMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_lowercase();
        } else if let Some((k, v)) = line.split_once('=') {
            values.insert(
                (section.clone(), k.trim().to_lowercase()),
                v.trim().to_string(),
            );
        }
    }
    let get = |s: &str, k: &str| {
        values
            .get(&(s.into(), k.to_lowercase()))
            .cloned()
            .unwrap_or_default()
    };
    let mut settings = Settings {
        enabled: get("general", "Enabled") != "0",
        tray_hint_shown: get("general", "TrayHintShown") == "1",
        ..Settings::default()
    };
    let count = get("targets", "Count")
        .parse::<usize>()
        .unwrap_or(0)
        .min(64);
    for i in 0..count {
        let id = get("targets", &format!("Id{i}"));
        if !id.is_empty() && !settings.targets.iter().any(|d| d.id == id) {
            let delay = get("targets", &format!("DelayMs{i}"))
                .parse::<u32>()
                .ok()
                .filter(|&v| v <= 1000)
                .unwrap_or(0);
            settings.targets.push(SavedDevice {
                id,
                name: get("targets", &format!("Name{i}")),
                extra_delay_ms: delay,
            });
        }
    }
    settings
}
pub fn load() -> io::Result<Settings> {
    let path = path()?;
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(e),
    };
    let text = if bytes.starts_with(&[255, 254]) {
        String::from_utf16_lossy(
            &bytes[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(&bytes)
            .trim_start_matches('\u{feff}')
            .into()
    };
    Ok(parse(&text))
}
pub fn serialize(s: &Settings) -> String {
    let mut text = format!(
        "[General]\r\nEnabled={}\r\nTrayHintShown={}\r\n[Targets]\r\nCount={}\r\n",
        u8::from(s.enabled),
        u8::from(s.tray_hint_shown),
        s.targets.len()
    );
    for (i, d) in s.targets.iter().enumerate() {
        let clean = |v: &str| v.replace(['\r', '\n'], " ");
        text.push_str(&format!(
            "Id{i}={}\r\nName{i}={}\r\nDelayMs{i}={}\r\n",
            clean(&d.id),
            clean(&d.name),
            d.extra_delay_ms.min(1000)
        ));
    }
    text
}
pub fn save(s: &Settings) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let path = path()?;
    fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension("ini.tmp");
    let bytes: Vec<u8> = std::iter::once(0xfeffu16)
        .chain(serialize(s).encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    fs::write(&temp, bytes)?;
    let from = wide(&temp.to_string_lossy());
    let to = wide(&path.to_string_lossy());
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::other)
    }
}
const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
fn startup_command() -> io::Result<String> {
    Ok(format!("\"{}\" --tray", std::env::current_exe()?.display()))
}
pub fn autostart() -> bool {
    unsafe {
        let mut data = vec![0u16; 32768];
        let mut bytes = (data.len() * 2) as u32;
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            RUN,
            w!("SimpleAudioSplit"),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut bytes),
        );
        if rc.is_err() {
            return false;
        }
        let n = data.iter().position(|&c| c == 0).unwrap_or(data.len());
        startup_command()
            .is_ok_and(|s| String::from_utf16_lossy(&data[..n]).eq_ignore_ascii_case(&s))
    }
}
pub fn set_autostart(enable: bool) -> io::Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(io::Error::other)?;
        let result = if enable {
            let data = wide(&startup_command()?);
            RegSetValueExW(
                key,
                w!("SimpleAudioSplit"),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    data.as_ptr().cast(),
                    data.len() * 2,
                )),
            )
        } else {
            RegDeleteValueW(key, w!("SimpleAudioSplit"))
        };
        let _ = RegCloseKey(key);
        if !enable && result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        result.ok().map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_keeps_delay_and_avoids_ambiguous_names() {
        let mut s = Settings::default();
        s.targets.push(SavedDevice {
            id: "old".into(),
            name: "Headset".into(),
            extra_delay_ms: 73,
        });
        let d = crate::audio::Device {
            id: "new".into(),
            name: "Headset".into(),
            is_default: false,
        };
        assert!(reconnect_devices(&mut s, std::slice::from_ref(&d)));
        assert_eq!(s.targets[0].id, "new");
        assert_eq!(s.targets[0].extra_delay_ms, 73);
        s.targets[0].id = "old".into();
        s.targets.push(SavedDevice {
            id: "another".into(),
            name: "Headset".into(),
            extra_delay_ms: 12,
        });
        assert!(!reconnect_devices(&mut s, &[d]));
        assert_eq!(s.targets.len(), 2);
    }
    #[test]
    fn migration_and_roundtrip() {
        let old = "[General]\nEnabled=0\n[Targets]\nCount=1\nId0=abc\nName0=耳机\n";
        let mut s = parse(old);
        assert!(!s.enabled);
        assert_eq!(s.targets[0].extra_delay_ms, 0);
        s.targets[0].extra_delay_ms = 1000;
        assert_eq!(parse(&serialize(&s)), s);
        for v in ["-1", "1001", "nan"] {
            assert_eq!(
                parse(&format!("{old}DelayMs0={v}\n")).targets[0].extra_delay_ms,
                0
            );
        }
    }
}
