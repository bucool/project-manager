//! Placement belongs to the click's monitor, including when reusing a viewer.
use gpui::{App, Bounds, DisplayId, Pixels, Window, point, px, size};

pub(super) struct Placement {
    pub bounds: Bounds<Pixels>,
    pub display_id: Option<DisplayId>,
    #[cfg(windows)]
    monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    #[cfg(windows)]
    work: windows::Win32::Foundation::RECT,
    #[cfg(windows)]
    scale: f32,
}

impl Placement {
    pub fn new(source: &Window, cx: &App) -> Self {
        let display_id = source.display(cx).map(|display| display.id());
        let bounds = Bounds::centered(display_id, size(px(1040.), px(780.)), cx);
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use windows::Win32::{
                Foundation::{HWND, POINT},
                Graphics::Gdi::*,
                UI::{
                    HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
                    WindowsAndMessaging::GetCursorPos,
                },
            };
            unsafe {
                let mut cursor = POINT::default();
                let mut monitor = if GetCursorPos(&mut cursor).is_ok() {
                    MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST)
                } else {
                    HMONITOR::default()
                };
                if monitor.is_invalid() {
                    if let Ok(handle) = HasWindowHandle::window_handle(source) {
                        if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                            monitor = MonitorFromWindow(
                                HWND(handle.hwnd.get() as *mut _),
                                MONITOR_DEFAULTTONEAREST,
                            );
                        }
                    }
                }
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(monitor, &mut info).as_bool() {
                    let mut x = 96;
                    let mut y = 96;
                    let scale =
                        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() {
                            x as f32 / 96.
                        } else {
                            source.scale_factor()
                        };
                    let rect = info.rcMonitor;
                    let display_id = cx
                        .displays()
                        .into_iter()
                        .find(|display| {
                            let bounds = display.bounds();
                            (f32::from(bounds.origin.x) - rect.left as f32 / scale).abs() < 2.
                                && (f32::from(bounds.origin.y) - rect.top as f32 / scale).abs() < 2.
                                && (f32::from(bounds.size.width)
                                    - (rect.right - rect.left) as f32 / scale)
                                    .abs()
                                    < 2.
                                && (f32::from(bounds.size.height)
                                    - (rect.bottom - rect.top) as f32 / scale)
                                    .abs()
                                    < 2.
                        })
                        .map(|display| display.id())
                        .or(display_id);
                    let work = info.rcWork;
                    let width = (1040. * scale).min((work.right - work.left - 24).max(1) as f32);
                    let height = (780. * scale).min((work.bottom - work.top - 24).max(1) as f32);
                    let left = work.left as f32 + ((work.right - work.left) as f32 - width) / 2.;
                    let top = work.top as f32 + ((work.bottom - work.top) as f32 - height) / 2.;
                    return Self {
                        bounds: Bounds::new(
                            point(px(left / scale), px(top / scale)),
                            size(px(width / scale), px(height / scale)),
                        ),
                        display_id,
                        monitor,
                        work,
                        scale,
                    };
                }
                return Self {
                    bounds,
                    display_id,
                    monitor,
                    work: Default::default(),
                    scale: source.scale_factor(),
                };
            }
        }
        #[cfg(not(windows))]
        Self { bounds, display_id }
    }

    pub fn apply(&self, window: &Window, force: bool) {
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            use windows::Win32::{
                Foundation::HWND,
                Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
                UI::WindowsAndMessaging::*,
            };
            unsafe {
                let Ok(handle) = HasWindowHandle::window_handle(window) else {
                    return;
                };
                let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                    return;
                };
                let hwnd = HWND(handle.hwnd.get() as *mut _);
                if self.work.right <= self.work.left {
                    return;
                }
                if !force && MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) == self.monitor {
                    return;
                }
                if IsIconic(hwnd).as_bool() || IsZoomed(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    (f32::from(self.bounds.origin.x) * self.scale).round() as i32,
                    (f32::from(self.bounds.origin.y) * self.scale).round() as i32,
                    (f32::from(self.bounds.size.width) * self.scale).round() as i32,
                    (f32::from(self.bounds.size.height) * self.scale).round() as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
    }
}
