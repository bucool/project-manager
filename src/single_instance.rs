//! One workbench per Windows session, with event-driven activation of an existing window.
use anyhow::{Context, Result};
use gpui::{App, Global, Window};
use std::{
    ffi::c_void,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
        System::Threading::{
            CreateEventW, CreateMutexW, INFINITE, RegisterWaitForSingleObject, SetEvent,
            UnregisterWaitEx, WT_EXECUTEDEFAULT,
        },
        UI::WindowsAndMessaging::{
            AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId,
        },
    },
    core::w,
};

// Stable across versions, build profiles and installation paths. Local scopes the
// objects to this login session; no lock files or user data are written.
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\ProjectManager.Instance");
const ACTIVATE_NAME: windows::core::PCWSTR = w!("Local\\ProjectManager.Activate");

pub struct SingleInstance {
    _mutex: OwnedHandle,
    activate: OwnedHandle,
    listener: Option<ActivationWait>,
}
impl Global for SingleInstance {}

impl SingleInstance {
    /// A secondary launch signals the primary and exits before starting any services.
    pub fn acquire() -> Result<Option<Self>> {
        unsafe {
            // Create the event first so a launch arriving during initialization is
            // remembered until the primary has attached its window listener.
            let activate = OwnedHandle::from_raw_handle(
                CreateEventW(None, false, false, ACTIVATE_NAME)
                    .context("Unable to create the window activation event")?
                    .0,
            );
            let mutex = CreateMutexW(None, false, MUTEX_NAME)
                .context("Unable to acquire the application instance lock")?;
            let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
            let mutex = OwnedHandle::from_raw_handle(mutex.0);
            if already_running {
                // The process opened by Explorer can pass its foreground permission
                // to the primary before that process restores the GPUI window.
                if let Ok(hwnd) = FindWindowW(crate::tray::CLASS_NAME, None) {
                    let mut pid = 0;
                    GetWindowThreadProcessId(hwnd, Some(&mut pid));
                    if pid != 0 {
                        let _ = AllowSetForegroundWindow(pid);
                    }
                }
                SetEvent(HANDLE(activate.as_raw_handle()))
                    .context("Unable to activate the existing application")?;
                return Ok(None);
            }
            Ok(Some(Self {
                _mutex: mutex,
                activate,
                listener: None,
            }))
        }
    }

    pub fn connect(window: &Window, cx: &mut App) -> Result<()> {
        let (sender, receiver) = async_channel::bounded(1);
        let instance = cx.global_mut::<Self>();
        instance.listener = Some(ActivationWait::new(&instance.activate, sender)?);
        let handle = window.window_handle();
        cx.spawn(async move |cx| {
            while receiver.recv().await.is_ok() {
                if handle
                    .update(cx, |_, window, cx| crate::tray::show(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Ok(())
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Stop callbacks before closing the activation event or releasing the lock.
        self.listener.take();
    }
}

struct ActivationWait {
    handle: HANDLE,
    // The native callback borrows this allocation, which remains stable until
    // UnregisterWaitEx has waited for every outstanding callback to finish.
    _sender: Box<async_channel::Sender<()>>,
}

impl ActivationWait {
    fn new(event: &OwnedHandle, sender: async_channel::Sender<()>) -> Result<Self> {
        let sender = Box::new(sender);
        let mut handle = HANDLE::default();
        unsafe {
            RegisterWaitForSingleObject(
                &mut handle,
                HANDLE(event.as_raw_handle()),
                Some(activate_window),
                Some(sender.as_ref() as *const _ as *const c_void),
                INFINITE,
                WT_EXECUTEDEFAULT,
            )
            .context("Unable to listen for application activation")?;
        }
        Ok(Self {
            handle,
            _sender: sender,
        })
    }
}

impl Drop for ActivationWait {
    fn drop(&mut self) {
        unsafe {
            // INVALID_HANDLE_VALUE makes removal synchronous. The callback only
            // queues a message, so it never waits for the UI during shutdown.
            let _ = UnregisterWaitEx(self.handle, Some(INVALID_HANDLE_VALUE));
        }
    }
}

unsafe extern "system" fn activate_window(context: *mut c_void, _: bool) {
    if let Some(sender) = unsafe { (context as *const async_channel::Sender<()>).as_ref() } {
        // Coalesce bursts of launches; never block a Windows thread-pool callback.
        let _ = sender.try_send(());
    }
}
