//! App-owned Media Foundation playback surface. No browser or external player.
use anyhow::{Context as _, Result, ensure};
use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::Cell, sync::mpsc};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT},
        Media::MediaFoundation::*,
        System::{
            Com::{
                COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
                StructuredStorage::{PROPVARIANT, PropVariantToInt64},
            },
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::*,
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
struct Child(HWND);
impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
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
                let _ = EndPaint(hwnd, &paint);
                if let Some(player) = player.as_ref() {
                    let _ = player.UpdateVideo();
                }
                return LRESULT(0);
            }
            WM_SIZE => {
                if let Some(player) = player.as_ref() {
                    let _ = player.UpdateVideo();
                }
            }
            WM_ERASEBKGND => return LRESULT(1),
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
    child: Child,
    rectangle: Cell<(i32, i32, i32, i32)>,
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
            let child = Child(CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WS_DISABLED,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                Some(instance),
                None,
            )?);
            let (sender, receiver) = mpsc::channel();
            let callback: IMFPMediaPlayerCallback = Callback(sender).into();
            let mut player = None;
            // URL loading and decoding happen in Media Foundation's pipeline.
            MFPCreateMediaPlayer(
                &HSTRING::from(path),
                true,
                MFP_OPTION_FREE_THREADED_CALLBACK,
                &callback,
                Some(child.0),
                Some(&mut player),
            )?;
            let player = player.context("Media Foundation returned no player")?;
            let _ = player.SetAspectRatioMode(MFVideoARMode_PreservePicture.0 as u32);
            SetWindowLongPtrW(
                child.0,
                GWLP_USERDATA,
                Box::into_raw(Box::new(player.clone())) as isize,
            );
            Ok(Self {
                player: Some(player),
                child,
                rectangle: Cell::new((0, 0, 0, 0)),
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
        if self.rectangle.replace(rectangle) != rectangle {
            unsafe {
                let _ = SetWindowPos(
                    self.child.0,
                    None,
                    rectangle.0,
                    rectangle.1,
                    rectangle.2,
                    rectangle.3,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                );
            }
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
            let _ = ShowWindow(self.child.0, SW_HIDE);
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
        // Child drops next, releasing its extra COM reference before MFShutdown.
    }
}
