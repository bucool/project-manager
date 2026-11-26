//! App-owned Media Foundation playback surface. No browser or external player.
use anyhow::{Context as _, Result, ensure};
use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, rc::Rc, sync::mpsc};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Gdi::{BeginPaint, ClientToScreen, EndPaint, PAINTSTRUCT},
        Media::MediaFoundation::*,
        System::{
            Com::{
                COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
                StructuredStorage::{PROPVARIANT, PropVariantToInt64},
            },
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::{HSTRING, implement, w},
};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
struct Foundation;
impl Drop for Foundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}
struct Surface {
    state: Box<Rc<SurfaceState>>,
}
struct SurfaceState {
    window: HWND,
    owner: HWND,
    rectangle: Cell<(i32, i32, i32, i32)>,
    visible: Cell<bool>,
}
impl SurfaceState {
    fn sync(&self) {
        unsafe {
            let (x, y, width, height) = self.rectangle.get();
            if !self.visible.get()
                || width <= 0
                || height <= 0
                || !IsWindowVisible(self.owner).as_bool()
                || IsIconic(self.owner).as_bool()
            {
                let _ = ShowWindow(self.window, SW_HIDE);
                return;
            }
            let mut origin = POINT { x, y };
            if ClientToScreen(self.owner, &mut origin).as_bool() {
                let _ = SetWindowPos(
                    self.window,
                    None,
                    origin.x,
                    origin.y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                );
            }
        }
    }
}
unsafe extern "system" fn owner_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    unsafe {
        // DefSubclassProc can dispatch callbacks that close or replace the
        // player. Keep the state alive across that reentrant dispatch.
        let state = (data as *const Rc<SurfaceState>).as_ref().cloned();
        let result = DefSubclassProc(hwnd, message, wparam, lparam);
        if matches!(
            message,
            WM_WINDOWPOSCHANGED | WM_MOVE | WM_SIZE | WM_SHOWWINDOW | WM_DPICHANGED
        ) {
            if let Some(state) = state {
                state.sync();
            }
        }
        result
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(
                self.state.owner,
                Some(owner_proc),
                self.state.window.0 as usize,
            );
            let _ = DestroyWindow(self.state.window);
        }
    }
}

#[implement(IMFPMediaPlayerCallback)]
struct Callback(mpsc::Sender<Option<String>>);
impl IMFPMediaPlayerCallback_Impl for Callback_Impl {
    fn OnMediaPlayerEvent(&self, header: *const MFP_EVENT_HEADER) {
        if let Some(header) = unsafe { header.as_ref() } {
            if let Err(error) = header.hrEvent.ok() {
                let _ = self.0.send(Some(error.to_string()));
            } else if header.eEventType == MFP_EVENT_TYPE_PLAYBACK_ENDED {
                let _ = self.0.send(None);
            }
        }
    }
}

unsafe extern "system" fn surface_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let player = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut IMFPMediaPlayer;
        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                if let Some(player) = player.as_ref().cloned() {
                    let _ = player.UpdateVideo();
                }
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_SIZE => {
                if let Some(player) = player.as_ref().cloned() {
                    let _ = player.UpdateVideo();
                }
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if !player.is_null() {
                    drop(Box::from_raw(player));
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub struct Playback {
    pub position: f64,
    pub duration: f64,
    pub playing: bool,
    pub ended: bool,
    pub error: Option<String>,
}

pub struct Player {
    player: Option<IMFPMediaPlayer>,
    surface: Surface,
    receiver: mpsc::Receiver<Option<String>>,
    _callback: IMFPMediaPlayerCallback,
    _foundation: Foundation,
    _apartment: Apartment,
}
impl Player {
    pub fn new(path: &str, window: &Window) -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            let apartment = Apartment;
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let foundation = Foundation;
            let parent = match HasWindowHandle::window_handle(window)
                .map_err(|error| anyhow::anyhow!("Cannot access media window: {error:?}"))?
                .as_raw()
            {
                RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as *mut _),
                _ => anyhow::bail!("Not a Windows window"),
            };
            let instance = GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                lpfnWndProc: Some(surface_proc),
                hInstance: instance,
                lpszClassName: w!("ProjectManagerMediaSurface"),
                ..Default::default()
            };
            // Registration is process-wide; subsequent viewer sessions reuse it.
            let _ = RegisterClassW(&class);
            // GPUI creates a DirectComposition target *above* its HWND children.
            // An owned popup stays above that visual tree without appearing as a
            // separate application window or replacing GPUI's rendering settings.
            let surface_window = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                class.lpszClassName,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                Some(instance),
                None,
            )?;
            let surface = Surface {
                state: Box::new(Rc::new(SurfaceState {
                    window: surface_window,
                    owner: parent,
                    rectangle: Cell::new((0, 0, 0, 0)),
                    visible: Cell::new(true),
                })),
            };
            ensure!(
                SetWindowSubclass(
                    parent,
                    Some(owner_proc),
                    surface_window.0 as usize,
                    (&*surface.state as *const Rc<SurfaceState>) as usize
                )
                .as_bool(),
                "Cannot attach video surface to viewer"
            );
            let (sender, receiver) = mpsc::channel();
            let callback: IMFPMediaPlayerCallback = Callback(sender).into();
            let mut player = None;
            // URL loading and decoding happen in Media Foundation's pipeline.
            MFPCreateMediaPlayer(
                &HSTRING::from(path),
                true,
                MFP_OPTION_FREE_THREADED_CALLBACK,
                &callback,
                Some(surface_window),
                Some(&mut player),
            )?;
            let player = player.context("Media Foundation returned no player")?;
            let _ = player.SetAspectRatioMode(MFVideoARMode_PreservePicture.0 as u32);
            SetWindowLongPtrW(
                surface_window,
                GWLP_USERDATA,
                Box::into_raw(Box::new(player.clone())) as isize,
            );
            Ok(Self {
                player: Some(player),
                surface,
                receiver,
                _callback: callback,
                _foundation: foundation,
                _apartment: apartment,
            })
        }
    }
    pub fn layout(&self, bounds: Bounds<Pixels>, scale: f32) {
        let rectangle = (
            (f32::from(bounds.origin.x) * scale).round() as i32,
            (f32::from(bounds.origin.y) * scale).round() as i32,
            (f32::from(bounds.size.width) * scale).round().max(1.) as i32,
            (f32::from(bounds.size.height) * scale).round().max(1.) as i32,
        );
        if self.surface.state.rectangle.replace(rectangle) != rectangle {
            self.surface.state.sync();
        }
    }
    pub fn toggle(&self) -> Result<()> {
        let player = self.player.as_ref().context("Player is closed")?;
        unsafe {
            if player.GetState()? == MFP_MEDIAPLAYER_STATE_PLAYING {
                player.Pause()?;
            } else {
                player.Play()?;
            }
        }
        Ok(())
    }
    pub fn shutdown(&self) {
        if let Some(player) = &self.player {
            unsafe {
                let _ = player.Shutdown();
            }
        }
        unsafe {
            self.surface.state.visible.set(false);
            let _ = ShowWindow(self.surface.state.window, SW_HIDE);
        }
    }
    pub fn seek(&self, seconds: f64) -> Result<()> {
        ensure!(
            seconds.is_finite() && seconds >= 0.,
            "Invalid seek position"
        );
        let value = PROPVARIANT::from((seconds * 10_000_000.) as i64);
        unsafe {
            self.player
                .as_ref()
                .context("Player is closed")?
                .SetPosition(&MFP_POSITIONTYPE_100NS, &value)?;
        }
        Ok(())
    }
    pub fn poll(&self) -> Playback {
        let mut ended = false;
        let mut error = None;
        while let Ok(event) = self.receiver.try_recv() {
            if let Some(message) = event {
                error = Some(message);
            } else {
                ended = true;
            }
        }
        let mut position = 0.;
        let mut duration = 0.;
        let mut playing = false;
        if let Some(player) = &self.player {
            unsafe {
                if let Ok(value) = player.GetPosition(&MFP_POSITIONTYPE_100NS) {
                    position = PropVariantToInt64(&value).unwrap_or(0) as f64 / 10_000_000.;
                }
                if let Ok(value) = player.GetDuration(&MFP_POSITIONTYPE_100NS) {
                    duration = PropVariantToInt64(&value).unwrap_or(0) as f64 / 10_000_000.;
                }
                playing = player
                    .GetState()
                    .is_ok_and(|state| state == MFP_MEDIAPLAYER_STATE_PLAYING);
            }
        }
        Playback {
            position,
            duration,
            playing,
            ended,
            error,
        }
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        if let Some(player) = self.player.take() {
            unsafe {
                let _ = player.Shutdown();
            }
        }
        // Surface drops next, releasing its extra COM reference before MFShutdown.
    }
}
