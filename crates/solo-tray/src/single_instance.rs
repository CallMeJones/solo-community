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

        let name: Vec<u16> = "Local\\SoloTray".encode_utf16().chain(Some(0)).collect();
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
