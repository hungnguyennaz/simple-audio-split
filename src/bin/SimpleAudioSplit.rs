#![windows_subsystem = "windows"]
use simple_audio_split::{
    audio::{self, wide, Device},
    engine::{Config, Engine},
    settings::{self, SavedDevice, Settings},
};
use std::{cell::RefCell, mem::size_of, thread};
use windows::{
    core::{w, Result, PCWSTR, PWSTR},
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{
            Controls::*, Input::KeyboardAndMouse::EnableWindow, Shell::*, WindowsAndMessaging::*,
        },
    },
};
const DEVICES: i32 = 1001;
const TEST: i32 = 1002;
const ENABLED: i32 = 1003;
const STARTUP: i32 = 1004;
const STATUS: i32 = 1005;
const DELAY: i32 = 1007;
const APPLY: i32 = 1008;
const DELAY_HINT: i32 = 1009;
const OPEN: i32 = 40001;
const TOGGLE: i32 = 40002;
const EXIT: i32 = 40003;
const TRAY: u32 = WM_APP + 2;
const TEST_DONE: u32 = WM_APP + 3;
thread_local! {static APP:RefCell<Option<App>>=const{RefCell::new(None)};}
struct App {
    hwnd: HWND,
    list: HWND,
    engine: Engine,
    settings: Settings,
    rows: Vec<Device>,
    populating: bool,
    testing: bool,
    icon: HICON,
    off_icon: HICON,
    taskbar: u32,
    show: u32,
    tray: bool,
    row_status: Vec<String>,
    status_text: String,
}
unsafe fn text(hwnd: HWND, id: i32, s: &str) {
    let s = wide(s);
    let _ = SetDlgItemTextW(hwnd, id, PCWSTR(s.as_ptr()));
}
unsafe fn message(hwnd: HWND, s: &str) {
    let s = wide(s);
    MessageBoxW(
        hwnd,
        PCWSTR(s.as_ptr()),
        w!("Simple Audio Split"),
        MB_OK | MB_ICONWARNING,
    );
}
unsafe fn selected(list: HWND) -> Option<usize> {
    let n = SendMessageW(
        list,
        LVM_GETNEXTITEM,
        WPARAM(usize::MAX),
        LPARAM(LVNI_SELECTED as isize),
    )
    .0;
    usize::try_from(n).ok()
}
unsafe fn layout_columns(list: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(list, &mut rect).is_ok() {
        let width = (rect.right - rect.left).max(0);
        let first = width * 45 / 100;
        SendMessageW(list, LVM_SETCOLUMNWIDTH, WPARAM(0), LPARAM(first as isize));
        SendMessageW(
            list,
            LVM_SETCOLUMNWIDTH,
            WPARAM(1),
            LPARAM((width - first) as isize),
        );
    }
}
impl App {
    fn configure(&self) {
        self.engine.configure(Config {
            enabled: self.settings.enabled,
            targets: self.settings.targets.clone(),
            ..Config::default()
        });
    }
    unsafe fn save(&self) {
        if let Err(e) = settings::save(&self.settings) {
            message(self.hwnd, &format!("Couldn't save your choices: {e}"));
        }
    }
    unsafe fn tray(&mut self, add: bool) {
        let mut n = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: TRAY,
            hIcon: if self.settings.enabled {
                self.icon
            } else {
                self.off_icon
            },
            ..Default::default()
        };
        let tip = wide(if self.settings.enabled {
            "Simple Audio Split — mirroring on"
        } else {
            "Simple Audio Split — mirroring off"
        });
        n.szTip[..tip.len()].copy_from_slice(&tip);
        self.tray = Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &n).as_bool();
    }
    unsafe fn delay_controls(&self) {
        let device = selected(self.list).and_then(|i| self.rows.get(i));
        let saved = device.and_then(|d| self.settings.targets.iter().find(|t| t.id == d.id));
        let enabled = device.is_some_and(|d| !d.is_default) && saved.is_some();
        for id in [DELAY, APPLY] {
            if let Ok(h) = GetDlgItem(self.hwnd, id) {
                let _ = EnableWindow(h, enabled);
            }
        }
        text(
            self.hwnd,
            DELAY,
            &saved.map(|d| d.extra_delay_ms).unwrap_or(0).to_string(),
        );
        text(
            self.hwnd,
            DELAY_HINT,
            if device.is_some_and(|d| d.is_default) {
                "The default output plays directly; its timing cannot be changed."
            } else if saved.is_none() {
                "Select a checked output to set its extra delay (0–1000 ms)."
            } else {
                "Delay the faster copies to align devices. 0 adds no intentional delay."
            },
        );
        if let Ok(h) = GetDlgItem(self.hwnd, TEST) {
            let _ = EnableWindow(
                h,
                device.is_some_and(|d| self.engine.device_present(&d.id)) && !self.testing,
            );
        }
    }
    unsafe fn set_cell(&self, row: usize, col: i32, s: &str) {
        let mut s = wide(s);
        let item = LVITEMW {
            iSubItem: col,
            pszText: PWSTR(s.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(
            self.list,
            LVM_SETITEMTEXTW,
            WPARAM(row),
            LPARAM(&item as *const _ as isize),
        );
    }
    unsafe fn refresh(&mut self) {
        let st = self.engine.status();
        let mut rows = st.devices.clone();
        if settings::reconnect_devices(&mut self.settings, &rows) {
            self.save();
            self.configure();
        }
        for saved in &self.settings.targets {
            if !rows.iter().any(|d| d.id == saved.id) {
                rows.push(Device {
                    id: saved.id.clone(),
                    name: saved.name.clone(),
                    is_default: false,
                });
            }
        }
        if rows != self.rows {
            let old = selected(self.list)
                .and_then(|i| self.rows.get(i))
                .map(|d| d.id.clone());
            self.populating = true;
            SendMessageW(self.list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
            self.rows = rows;
            for (i, row) in self.rows.iter().enumerate() {
                let mut name = wide(&row.name);
                let mut item = LVITEMW {
                    mask: LVIF_TEXT | LVIF_STATE,
                    iItem: i as i32,
                    pszText: PWSTR(name.as_mut_ptr()),
                    stateMask: LVIS_STATEIMAGEMASK,
                    state: if self.settings.targets.iter().any(|d| d.id == row.id) {
                        LIST_VIEW_ITEM_STATE_FLAGS(2 << 12)
                    } else {
                        LIST_VIEW_ITEM_STATE_FLAGS(1 << 12)
                    },
                    ..Default::default()
                };
                if old.as_ref() == Some(&row.id) {
                    item.stateMask.0 |= LVIS_SELECTED.0;
                    item.state.0 |= LVIS_SELECTED.0;
                }
                SendMessageW(
                    self.list,
                    LVM_INSERTITEMW,
                    WPARAM(0),
                    LPARAM(&item as *const _ as isize),
                );
            }
            self.populating = false;
            self.row_status.clear();
            self.row_status.resize(self.rows.len(), String::new());
            layout_columns(self.list);
            self.delay_controls();
        }
        for (i, row) in self.rows.iter().enumerate() {
            let state = if row.is_default {
                "Default output (direct)".to_string()
            } else if let Some(s) = st.targets.get(&row.id) {
                if s.state == "Playing" {
                    format!("Playing · ≈{:.0} ms (+{} ms)", s.latency_ms(), s.delay_ms)
                } else if !s.error.is_empty() {
                    format!("{}: {}", s.state, s.error)
                } else {
                    s.state.clone()
                }
            } else {
                "Not selected".into()
            };
            if self.row_status.get(i) != Some(&state) {
                self.set_cell(i, 1, &state);
                self.row_status[i] = state;
            }
        }
        let status_text = if !self.settings.enabled {
            "Mirroring is off."
        } else if !st.source_error.is_empty() {
            &st.source_error
        } else if st.capturing {
            "Copying audio ? latency estimates exclude wireless transport."
        } else {
            "Select a connected output to copy audio."
        };
        if status_text != self.status_text {
            text(self.hwnd, STATUS, status_text);
            self.status_text.clear();
            self.status_text.push_str(status_text);
        }
    }
    unsafe fn show(&mut self) {
        SetTimer(self.hwnd, 1, 500, None);
        let _ = ShowWindow(self.hwnd, SW_RESTORE);
        let _ = ShowWindow(self.hwnd, SW_SHOW);
        let _ = SetForegroundWindow(self.hwnd);
        self.refresh();
    }
    unsafe fn hide(&mut self) {
        let _ = KillTimer(self.hwnd, 1);
        let _ = ShowWindow(self.hwnd, SW_HIDE);
        if !self.settings.tray_hint_shown {
            let mut n = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                uFlags: NIF_INFO,
                dwInfoFlags: NIIF_INFO,
                ..Default::default()
            };
            let title = wide("Still running");
            n.szInfoTitle[..title.len()].copy_from_slice(&title);
            let body =
                wide("Audio keeps playing. Click the tray icon to reopen; use Exit to stop.");
            n.szInfo[..body.len()].copy_from_slice(&body);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
            self.settings.tray_hint_shown = true;
            self.save();
        }
    }
    unsafe fn toggle(&mut self) {
        self.settings.enabled = !self.settings.enabled;
        let _ = CheckDlgButton(
            self.hwnd,
            ENABLED,
            if self.settings.enabled {
                BST_CHECKED
            } else {
                BST_UNCHECKED
            },
        );
        self.save();
        self.configure();
        self.tray(false);
        self.refresh();
    }
    unsafe fn command(&mut self, id: i32) {
        match id {
            TEST => {
                if self.testing {
                    return;
                }
                if let Some(row) = selected(self.list).and_then(|i| self.rows.get(i)) {
                    self.testing = true;
                    let id = row.id.clone();
                    let hwnd = self.hwnd.0 as usize;
                    self.delay_controls();
                    thread::spawn(move || {
                        let hr = audio::tone(&id).err().map(|e| e.code().0).unwrap_or(0);
                        let _ = PostMessageW(
                            HWND(hwnd as *mut _),
                            TEST_DONE,
                            WPARAM(hr as usize),
                            LPARAM(0),
                        );
                    });
                }
            }
            ENABLED => self.toggle(),
            STARTUP => {
                let want = IsDlgButtonChecked(self.hwnd, STARTUP) == BST_CHECKED.0;
                if let Err(e) = settings::set_autostart(want) {
                    message(self.hwnd, &format!("Couldn't change startup: {e}"));
                    let _ = CheckDlgButton(
                        self.hwnd,
                        STARTUP,
                        if settings::autostart() {
                            BST_CHECKED
                        } else {
                            BST_UNCHECKED
                        },
                    );
                }
            }
            APPLY => {
                let mut buf = [0u16; 32];
                let n = GetDlgItemTextW(self.hwnd, DELAY, &mut buf) as usize;
                let value = String::from_utf16_lossy(&buf[..n]).parse::<u32>();
                match value {
                    Ok(ms) if ms <= 1000 => {
                        if let Some(row) = selected(self.list).and_then(|i| self.rows.get(i)) {
                            if let Some(saved) =
                                self.settings.targets.iter_mut().find(|d| d.id == row.id)
                            {
                                saved.extra_delay_ms = ms;
                                self.save();
                                self.configure();
                            }
                        }
                    }
                    _ => message(
                        self.hwnd,
                        "Enter a whole number from 0 to 1000 milliseconds.",
                    ),
                }
            }
            OPEN => self.show(),
            TOGGLE => self.toggle(),
            EXIT => {
                let _ = DestroyWindow(self.hwnd);
            }
            2 => self.hide(),
            _ => {}
        }
    }
    unsafe fn notify(&mut self, l: LPARAM) {
        let hdr = &*(l.0 as *const NMHDR);
        if hdr.idFrom != DEVICES as usize || hdr.code != LVN_ITEMCHANGED || self.populating {
            return;
        }
        let n = &*(l.0 as *const NMLISTVIEW);
        let old = n.uOldState & LVIS_STATEIMAGEMASK.0;
        let new = n.uNewState & LVIS_STATEIMAGEMASK.0;
        if old != new && old != 0 && new != 0 {
            if let Some(row) = self.rows.get(n.iItem as usize) {
                if new == 2 << 12 {
                    if !self.settings.targets.iter().any(|d| d.id == row.id) {
                        self.settings.targets.push(SavedDevice {
                            id: row.id.clone(),
                            name: row.name.clone(),
                            extra_delay_ms: 0,
                        });
                    }
                } else {
                    self.settings.targets.retain(|d| d.id != row.id);
                }
                self.save();
                self.configure();
            }
        }
        if old != new || (n.uOldState ^ n.uNewState) & LVIS_SELECTED.0 != 0 {
            self.delay_controls();
        }
    }
}
unsafe extern "system" fn dialog(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    // DestroyWindow sends this synchronously while the command handler still borrows APP.
    if msg == WM_DESTROY {
        let _ = KillTimer(hwnd, 1);
        let n = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &n);
        PostQuitMessage(0);
        return 1;
    }
    APP.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return 0;
        };
        if msg == WM_INITDIALOG {
            let inst = GetModuleHandleW(None).unwrap_or_default();
            let list = GetDlgItem(hwnd, DEVICES).unwrap_or_default();
            let icon = LoadIconW(inst, PCWSTR(101usize as *const u16)).unwrap_or_default();
            let off_icon = LoadIconW(inst, PCWSTR(102usize as *const u16)).unwrap_or(icon);
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(icon.0 as isize),
            );
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(icon.0 as isize),
            );
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                WPARAM(0),
                LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize),
            );
            for (i, label) in ["Device", "Status / estimated latency"].iter().enumerate() {
                let mut s = wide(label);
                let col = LVCOLUMNW {
                    mask: LVCF_TEXT | LVCF_WIDTH,
                    cx: if i == 0 { 190 } else { 370 },
                    pszText: PWSTR(s.as_mut_ptr()),
                    ..Default::default()
                };
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    WPARAM(i),
                    LPARAM(&col as *const _ as isize),
                );
            }
            layout_columns(list);
            SendMessageW(
                GetDlgItem(hwnd, DELAY).unwrap_or_default(),
                EM_SETLIMITTEXT,
                WPARAM(4),
                LPARAM(0),
            );
            let settings = match settings::load() {
                Ok(s) => s,
                Err(e) => {
                    message(hwnd, &format!("Couldn't load settings: {e}"));
                    Settings::default()
                }
            };
            let mut app = App {
                hwnd,
                list,
                engine: Engine::with_device_notifier({
                    let target = hwnd.0 as usize;
                    move || {
                        let _ =
                            PostMessageW(HWND(target as *mut _), WM_APP + 1, WPARAM(0), LPARAM(0));
                    }
                }),
                settings,
                rows: Vec::new(),
                populating: false,
                testing: false,
                icon,
                off_icon,
                taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
                show: RegisterWindowMessageW(w!("SimpleAudioSplit.Show")),
                tray: false,
                row_status: Vec::new(),
                status_text: String::new(),
            };
            let _ = CheckDlgButton(
                hwnd,
                ENABLED,
                if app.settings.enabled {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                },
            );
            let _ = CheckDlgButton(
                hwnd,
                STARTUP,
                if settings::autostart() {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                },
            );
            app.configure();
            app.tray(true);
            app.delay_controls();

            *slot = Some(app);
            return 1;
        }
        let Some(app) = slot.as_mut() else {
            return 0;
        };
        if msg == app.show && msg != 0 {
            app.show();
            return 1;
        }
        if msg == app.taskbar && msg != 0 {
            app.tray(true);
            return 1;
        }
        match msg {
            m if m == WM_APP + 1 => {
                app.refresh();
                1
            }
            WM_SIZE => {
                layout_columns(app.list);
                1
            }
            WM_DPICHANGED => {
                let _ = PostMessageW(hwnd, WM_APP + 4, WPARAM(0), LPARAM(0));
                0
            }
            m if m == WM_APP + 4 => {
                layout_columns(app.list);
                1
            }
            WM_COMMAND => {
                app.command((wparam.0 & 0xffff) as i32);
                1
            }
            WM_NOTIFY => {
                app.notify(lparam);
                0
            }
            WM_TIMER => {
                app.refresh();
                1
            }
            WM_CLOSE => {
                app.hide();
                1
            }
            TEST_DONE => {
                app.testing = false;
                app.delay_controls();
                if (wparam.0 as i32) < 0 {
                    message(
                        hwnd,
                        &format!(
                            "Couldn't play test sound: {}",
                            windows::core::Error::from_hresult(windows::core::HRESULT(
                                wparam.0 as i32
                            ))
                        ),
                    );
                }
                1
            }
            TRAY => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => app.show(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        if let Ok(menu) = CreatePopupMenu() {
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                OPEN as usize,
                                w!("Open Simple Audio Split"),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING
                                    | if app.settings.enabled {
                                        MF_CHECKED
                                    } else {
                                        MF_UNCHECKED
                                    },
                                TOGGLE as usize,
                                w!("Mirroring on"),
                            );
                            let _ = AppendMenuW(menu, MF_STRING, EXIT as usize, w!("Exit"));
                            let mut p = POINT::default();
                            let _ = GetCursorPos(&mut p);
                            let _ = SetForegroundWindow(hwnd);
                            let id = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                p.x,
                                p.y,
                                0,
                                hwnd,
                                None,
                            );
                            let _ =
                                PostMessageW(hwnd, WM_COMMAND, WPARAM(id.0 as usize), LPARAM(0));
                            let _ = DestroyMenu(menu);
                        }
                    }
                    _ => {}
                }
                1
            }
            _ => 0,
        }
    })
}
fn run() -> Result<()> {
    unsafe {
        let mutex = CreateMutexW(None, false, w!("Local\\SimpleAudioSplit.Instance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(hwnd) = FindWindowW(w!("#32770"), w!("Simple Audio Split")) {
                let _ = PostMessageW(
                    hwnd,
                    RegisterWindowMessageW(w!("SimpleAudioSplit.Show")),
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            let _ = CloseHandle(mutex);
            return Ok(());
        }
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&controls);
        let hwnd = CreateDialogParamW(
            GetModuleHandleW(None)?,
            PCWSTR(201usize as *const u16),
            None,
            Some(dialog),
            LPARAM(0),
        )?;
        if !std::env::args().any(|a| a == "--tray") {
            let _ = SendMessageW(
                hwnd,
                RegisterWindowMessageW(w!("SimpleAudioSplit.Show")),
                WPARAM(0),
                LPARAM(0),
            );
        }
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0).0;
            if result <= 0 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        APP.with(|c| {
            c.borrow_mut().take();
        });
        let _ = CloseHandle(mutex);
        Ok(())
    }
}
fn main() {
    if let Err(e) = run() {
        unsafe {
            message(HWND::default(), &format!("Couldn't start: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    #[test]
    fn native_delay_controls_save_and_disable_for_direct_output() {
        // Exercise the real dialog and notifications with isolated settings, without playing audio.
        let folder = std::env::temp_dir().join(format!("sas-dialog-test-{}", std::process::id()));
        let previous = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", &folder);
        let s = Settings {
            enabled: false,
            targets: vec![SavedDevice {
                id: "offline-test-device".into(),
                name: "Test output".into(),
                extra_delay_ms: 37,
            }],
            ..Settings::default()
        };
        settings::save(&s).unwrap();
        unsafe {
            let controls = INITCOMMONCONTROLSEX {
                dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
            };
            assert!(InitCommonControlsEx(&controls).as_bool());
            let hwnd = CreateDialogParamW(
                GetModuleHandleW(None).unwrap(),
                PCWSTR(201usize as *const u16),
                None,
                Some(dialog),
                LPARAM(0),
            )
            .unwrap();
            SendMessageW(hwnd, WM_TIMER, WPARAM(1), LPARAM(0));
            let (list, row) = APP.with(|c| {
                let slot = c.borrow();
                let app = slot.as_ref().unwrap();
                (
                    app.list,
                    app.rows
                        .iter()
                        .position(|d| d.id == "offline-test-device")
                        .unwrap(),
                )
            });
            let mut bounds = RECT::default();
            GetClientRect(list, &mut bounds).unwrap();
            let columns = SendMessageW(list, LVM_GETCOLUMNWIDTH, WPARAM(0), LPARAM(0)).0
                + SendMessageW(list, LVM_GETCOLUMNWIDTH, WPARAM(1), LPARAM(0)).0;
            assert_eq!(columns, (bounds.right - bounds.left) as isize);
            let item = LVITEMW {
                stateMask: LVIS_SELECTED,
                state: LVIS_SELECTED,
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_SETITEMSTATE,
                WPARAM(row),
                LPARAM(&item as *const _ as isize),
            );
            assert!(IsWindowEnabled(GetDlgItem(hwnd, DELAY).unwrap()).as_bool());
            let mut value = [0u16; 16];
            let n = GetDlgItemTextW(hwnd, DELAY, &mut value) as usize;
            assert_eq!(String::from_utf16_lossy(&value[..n]), "37");
            SetDlgItemTextW(hwnd, DELAY, w!("75")).unwrap();
            SendMessageW(hwnd, WM_COMMAND, WPARAM(APPLY as usize), LPARAM(0));
            assert_eq!(settings::load().unwrap().targets[0].extra_delay_ms, 75);
            APP.with(|c| {
                let mut slot = c.borrow_mut();
                let app = slot.as_mut().unwrap();
                app.rows[row].is_default = true;
                app.delay_controls();
            });
            assert!(!IsWindowEnabled(GetDlgItem(hwnd, DELAY).unwrap()).as_bool());
            assert!(!IsWindowEnabled(GetDlgItem(hwnd, APPLY).unwrap()).as_bool());
            SendMessageW(hwnd, WM_COMMAND, WPARAM(EXIT as usize), LPARAM(0));
            assert!(!IsWindow(hwnd).as_bool());
            let mut quit = MSG::default();
            assert!(PeekMessageW(&mut quit, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool());
            APP.with(|c| {
                c.borrow_mut().take();
            });
        }
        if let Some(value) = previous {
            std::env::set_var("APPDATA", value);
        } else {
            std::env::remove_var("APPDATA");
        }
        // The path is a unique directory created above, never a user settings directory.
        std::fs::remove_file(folder.join("SimpleAudioSplit/settings.ini")).unwrap();
        std::fs::remove_dir(folder.join("SimpleAudioSplit")).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
