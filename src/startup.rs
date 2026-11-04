//! Per-user Windows login startup. Read and write the OS registration directly.
#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
                REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
                RegQueryValueExW, RegSetValueExW,
            },
        },
        core::w,
    };

    const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const VALUE_NAME: *const u16 = w!("ProjectManager");

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn check(status: u32) -> Result<()> {
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32).into());
        }
        Ok(())
    }

    pub fn enabled() -> Result<bool> {
        unsafe {
            let mut handle = ptr::null_mut();
            let status = RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_QUERY_VALUE, &mut handle);
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(false);
            }
            check(status)?;
            let key = Key(handle);
            let mut kind = 0;
            let mut bytes = 0;
            let status = RegQueryValueExW(
                key.0,
                VALUE_NAME,
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut bytes,
            );
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(false);
            }
            check(status)?;
            Ok(kind == REG_SZ && bytes > 2)
        }
    }

    pub fn set_enabled(enabled: bool) -> Result<()> {
        unsafe {
            let mut handle = ptr::null_mut();
            if !enabled {
                // Removing an absent entry succeeds without creating any keys.
                let status =
                    RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE, &mut handle);
                if status == ERROR_FILE_NOT_FOUND {
                    return Ok(());
                }
                check(status)?;
                let key = Key(handle);
                let status = RegDeleteValueW(key.0, VALUE_NAME);
                return if status == ERROR_FILE_NOT_FOUND {
                    Ok(())
                } else {
                    check(status)
                };
            }
            let executable =
                std::env::current_exe().context("Could not locate the application executable")?;
            // Quote the full executable path, including paths containing spaces
            // or non-ASCII characters. No shell, helper script or working directory.
            let command: Vec<u16> = std::iter::once('"' as u16)
                .chain(executable.as_os_str().encode_wide())
                .chain(['"' as u16, 0])
                .collect();
            let bytes = u32::try_from(command.len() * size_of::<u16>())?;
            check(RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut handle,
                ptr::null_mut(),
            ))?;
            let key = Key(handle);
            check(RegSetValueExW(
                key.0,
                VALUE_NAME,
                0,
                REG_SZ,
                command.as_ptr().cast(),
                bytes,
            ))
        }
    }
}

#[cfg(windows)]
pub use platform::{enabled, set_enabled};

#[cfg(not(windows))]
pub fn enabled() -> anyhow::Result<bool> {
    Ok(false)
}

#[cfg(not(windows))]
pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    anyhow::ensure!(!enabled, "Login startup is only supported on Windows");
    Ok(())
}
