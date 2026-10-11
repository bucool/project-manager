//! Windows tray integration, using GPUI's existing message loop.
use crate::i18n::tr;
use anyhow::{Context, Result, anyhow, bail};
use gpui::{App, Global, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, mem::size_of};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{HSTRING, PCWSTR, w},
};

pub(crate) const CLASS_NAME: PCWSTR = w!("ProjectManagerTray");
const CALLBACK_MESSAGE: u32 = WM_APP + 1;

enum Action {
    Show,
    Exit,
}

struct Tray {
    events: async_channel::Sender<Action>,
    native: Option<NativeTray>,
}
impl Global for Tray {}

pub fn init(window: &Window, cx: &mut App) {
    let (events, receiver) = async_channel::unbounded();
    // Keep a hidden native window available for secondary launches to identify
    // this process. A tray icon is still added only when the user hides the app.
    let native = NativeTray::new(events.clone()).ok();
    cx.set_global(Tray { events, native });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        while let Ok(action) = receiver.recv().await {
            match action {
                Action::Show => {
                    let _ = handle.update(cx, |_, window, cx| {
                        show(window, cx);
                    });
                }
                Action::Exit => {
                    let _ = cx.update(|cx| {
                        shutdown(cx);
                        cx.quit();
                    });
                    break;
                }
            }
        }
    })
    .detach();
}

pub fn show(window: &mut Window, cx: &mut App) {
    if let Ok(hwnd) = window_handle(window) {
        unsafe {
            // SW_SHOW preserves a maximized window; only restore minimized ones.
            let mode = if IsIconic(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(hwnd, mode);
            let _ = SetForegroundWindow(hwnd);
        }
        if let Some(native) = &cx.global::<Tray>().native {
            native.state.remove();
        }
        window.refresh();
    }
}

fn window_handle(window: &Window) -> Result<HWND> {
    let handle =
        HasWindowHandle::window_handle(window).map_err(|_| anyhow!("无法隐藏窗口到系统托盘"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        _ => bail!("无法隐藏窗口到系统托盘"),
    }
}

pub fn hide(window: &Window, cx: &mut App) -> Result<()> {
    let hwnd = window_handle(window)?;
    let tray = cx.global_mut::<Tray>();
    if tray.native.is_none() {
        tray.native = Some(NativeTray::new(tray.events.clone()).context("无法创建系统托盘图标")?);
    }
    let native = tray.native.as_ref().unwrap();
    // Never hide the only window unless the user can recover it from a tray icon.
    native.state.add()?;
    if !unsafe { ShowWindowAsync(hwnd, SW_HIDE) }.as_bool() {
        native.state.remove();
        bail!("无法隐藏窗口到系统托盘");
    }
    Ok(())
}

pub fn shutdown(cx: &mut App) {
    if cx.try_global::<Tray>().is_some() {
        cx.global_mut::<Tray>().native.take();
    }
}

struct TrayState {
    data: NOTIFYICONDATAW,
    visible: Cell<bool>,
    taskbar_created: u32,
    events: async_channel::Sender<Action>,
}

impl TrayState {
    fn add(&self) -> Result<()> {
        unsafe {
            if self.visible.get() && Shell_NotifyIconW(NIM_MODIFY, &self.data).as_bool() {
                return Ok(());
            }
            if !Shell_NotifyIconW(NIM_ADD, &self.data).as_bool() {
                bail!("无法创建系统托盘图标");
            }
            self.visible.set(true);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &self.data);
        }
        Ok(())
    }

    fn remove(&self) {
        if self.visible.replace(false) {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            }
        }
    }
}

struct NativeTray {
    hwnd: HWND,
    instance: HINSTANCE,
    state: Box<TrayState>,
}

impl NativeTray {
    fn new(events: async_channel::Sender<Action>) -> Result<Self> {
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None)?.into();
            // Shared icon from the EXE's embedded resource; no external icon file is needed.
            let icon = LoadIconW(Some(instance), PCWSTR(1usize as *const u16))?;
            let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
            if taskbar_created == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread().into());
            }
            // Hidden top-level window receives Explorer restart broadcasts as well as tray events.
            let hwnd = match CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("Project Manager"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let _ = UnregisterClassW(CLASS_NAME, Some(instance));
                    return Err(error.into());
                }
            };
            let mut data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
                uCallbackMessage: CALLBACK_MESSAGE,
                hIcon: icon,
                ..Default::default()
            };
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            for (slot, value) in data.szTip.iter_mut().zip("Project Manager".encode_utf16()) {
                *slot = value;
            }
            let state = Box::new(TrayState {
                data,
                visible: Cell::new(false),
                taskbar_created,
                events,
            });
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                state.as_ref() as *const TrayState as isize,
            );
            Ok(Self {
                hwnd,
                instance,
                state,
            })
        }
    }
}

impl Drop for NativeTray {
    fn drop(&mut self) {
        self.state.remove();
        unsafe {
            // Detach the borrowed state pointer before freeing its owner.
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(CLASS_NAME, Some(self.instance));
        }
    }
}

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

fn show_menu(hwnd: HWND) -> Result<Option<Action>> {
    unsafe {
        let menu = Menu(CreatePopupMenu()?);
        AppendMenuW(menu.0, MF_STRING, 1, &HSTRING::from(tr("显示窗口")))?;
        AppendMenuW(menu.0, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(menu.0, MF_STRING, 2, &HSTRING::from(tr("退出")))?;
        let mut position = POINT::default();
        GetCursorPos(&mut position)?;
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            position.x,
            position.y,
            None,
            hwnd,
            None,
        )
        .0;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        Ok(match command {
            1 => Some(Action::Show),
            2 => Some(Action::Exit),
            _ => None,
        })
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayState).as_ref();
        if let Some(state) = state {
            if message == state.taskbar_created && state.visible.get() {
                // Restore the icon after Explorer restarts; show the app if recovery fails.
                if state.add().is_err() {
                    let _ = state.events.try_send(Action::Show);
                }
                return LRESULT(0);
            }
            if message == CALLBACK_MESSAGE {
                // Clone before TrackPopupMenu enters a nested Windows message loop.
                let events = state.events.clone();
                match lparam.0 as u32 & 0xffff {
                    NIN_SELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                        let _ = events.try_send(Action::Show);
                    }
                    value if value == (NIN_SELECT | 1) => {
                        let _ = events.try_send(Action::Show);
                    }
                    WM_CONTEXTMENU | WM_RBUTTONUP => {
                        if let Ok(Some(action)) = show_menu(hwnd) {
                            let _ = events.try_send(action);
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
