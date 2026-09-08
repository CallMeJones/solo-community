// SPDX-License-Identifier: Apache-2.0

//! Process-level single-instance guard for solo-tray.

use anyhow::Result;

#[cfg(windows)]
pub struct InstanceGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl InstanceGuard {
    pub fn acquire() -> Result<Option<Self>> {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        // Match Unix's per-data-directory guard. An explicitly isolated library
        // must not steal or close the user's already-running default app.
        let data_dir = crate::tray::resolve_data_dir();
        std::fs::create_dir_all(&data_dir)?;
        let default_dir =
            std::env::var_os("USERPROFILE").map(|p| std::path::PathBuf::from(p).join(".solo"));
        let canonical = std::fs::canonicalize(&data_dir)?;
        let is_default = default_dir
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| p == canonical);
        let identity = canonical.to_string_lossy().to_lowercase();
        let hash = identity.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        let name = if is_default {
            "Local\\SoloTray".to_string()
        } else {
            format!("Local\\SoloTray-{hash:016x}")
        };
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }

        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Ok(None);
        }

        Ok(Some(Self { handle }))
    }
}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
pub struct InstanceGuard {
    _file: std::fs::File,
}

#[cfg(not(windows))]
impl InstanceGuard {
    pub fn acquire() -> Result<Option<Self>> {
        use fs2::FileExt;
        use std::fs::OpenOptions;
        use std::io::ErrorKind;

        let data_dir = crate::settings::settings_path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        std::fs::create_dir_all(&data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_dir.join("solo-tray.lock"))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
