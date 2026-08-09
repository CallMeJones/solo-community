// SPDX-License-Identifier: Apache-2.0

//! Autostart-on-login integration.
//!
//! Windows uses the per-user
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` registry key.
//! Linux uses the freedesktop XDG autostart directory. macOS
//! LaunchAgent support remains scoped for follow-up.
//!
//! Solo Controls writes a `Solo Controls` value under the Run key pointing at
//! the current binary's absolute path. Windows runs that on every user
//! login. Removing the value disables autostart.

use anyhow::Result;

/// Backend-agnostic enable/disable.
#[allow(unused_variables)]
pub fn set_enabled(enabled: bool) -> Result<()> {
    #[cfg(windows)]
    {
        windows::set_enabled(enabled)
    }
    #[cfg(target_os = "linux")]
    {
        linux::set_enabled(enabled)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        anyhow::bail!(
            "autostart is not yet implemented for this platform; set it up manually via launchd"
        )
    }
}

/// Best-effort read of the current state. Surface used by future
/// menu-state code that wants to render a checkmark next to the
/// "Toggle autostart on login" item; not consumed yet but kept public
/// so the eventual UI work doesn't have to come back and add it.
#[allow(dead_code)]
pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        windows::is_enabled().unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        linux::is_enabled().unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, bail};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    const DESKTOP_FILE: &str = "solo.desktop";

    pub fn set_enabled(enabled: bool) -> Result<()> {
        let path = desktop_file_path()?;
        if enabled {
            install_desktop_entry(&path)
        } else {
            remove_desktop_entry(&path)
        }
    }

    pub fn is_enabled() -> Result<bool> {
        Ok(desktop_file_path()?.is_file())
    }

    fn desktop_file_path() -> Result<PathBuf> {
        let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => {
                let home = std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .context("HOME is unavailable; cannot resolve XDG autostart directory")?;
                PathBuf::from(home).join(".config")
            }
        };
        if !config_home.is_absolute() {
            bail!(
                "XDG_CONFIG_HOME must be an absolute path: {}",
                config_home.display()
            );
        }
        Ok(config_home.join("autostart").join(DESKTOP_FILE))
    }

    fn install_desktop_entry(path: &Path) -> Result<()> {
        let exe = std::env::current_exe().context("resolve current_exe for autostart")?;
        let parent = path.parent().context("autostart path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create XDG autostart directory {}", parent.display()))?;

        let entry = desktop_entry(&exe);
        let temp = parent.join(format!(".{DESKTOP_FILE}.tmp-{}", std::process::id()));
        let write_result = (|| -> Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temp)
                .with_context(|| format!("create temporary autostart entry {}", temp.display()))?;
            file.write_all(entry.as_bytes())
                .with_context(|| format!("write temporary autostart entry {}", temp.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temporary autostart entry {}", temp.display()))?;
            std::fs::rename(&temp, path)
                .with_context(|| format!("install XDG autostart entry {}", path.display()))?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        write_result
    }

    fn remove_desktop_entry(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("remove XDG autostart entry {}", path.display()))
            }
        }
    }

    fn desktop_entry(exe: &Path) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Solo\nComment=Private local memory and projects\nExec={}\nTerminal=false\nNoDisplay=false\nX-GNOME-Autostart-enabled=true\n",
            quote_exec_path(exe),
        )
    }

    fn quote_exec_path(path: &Path) -> String {
        let escaped = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        format!("\"{escaped}\"")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn desktop_entry_quotes_exec_metacharacters() {
            let entry = desktop_entry(Path::new("/opt/Solo App/$preview`1/solo-tray"));
            assert!(entry.contains("Exec=\"/opt/Solo App/\\$preview\\`1/solo-tray\""));
            assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
        }

        #[test]
        fn remove_missing_entry_is_idempotent() {
            let temp = tempfile::tempdir().expect("tempdir");
            remove_desktop_entry(&temp.path().join(DESKTOP_FILE)).expect("remove missing entry");
        }
    }
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };

    const VALUE_NAME: &str = "Solo Controls";
    const LEGACY_VALUE_NAME: &str = "Solo Tray";
    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    fn to_wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn set_enabled(enabled: bool) -> Result<()> {
        if enabled {
            install_run_value()
        } else {
            remove_run_value()
        }
    }

    #[allow(dead_code)]
    pub fn is_enabled() -> Result<bool> {
        let subkey = to_wide(RUN_KEY);
        unsafe {
            let mut hkey = std::ptr::null_mut();
            let status = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey);
            if status != ERROR_SUCCESS {
                return Ok(false);
            }
            let enabled = [VALUE_NAME, LEGACY_VALUE_NAME].into_iter().any(|name| {
                let value = to_wide(name);
                let mut data_len: u32 = 0;
                RegQueryValueExW(
                    hkey,
                    value.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut data_len,
                ) == ERROR_SUCCESS
            });
            RegCloseKey(hkey);
            Ok(enabled)
        }
    }

    fn install_run_value() -> Result<()> {
        let exe = std::env::current_exe().context("resolve current_exe for autostart")?;
        // Quote the path so spaces in the install dir don't break the
        // Run-value parser.
        let cmd = format!("\"{}\"", exe.display());
        let cmd_wide: Vec<u16> = std::ffi::OsStr::new(&cmd)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let subkey = to_wide(RUN_KEY);
        let value = to_wide(VALUE_NAME);
        unsafe {
            let mut hkey = std::ptr::null_mut();
            let open = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut hkey);
            if open != ERROR_SUCCESS {
                anyhow::bail!("open HKCU\\{}: status {}", RUN_KEY, open);
            }
            let set = RegSetValueExW(
                hkey,
                value.as_ptr(),
                0,
                REG_SZ,
                cmd_wide.as_ptr() as *const u8,
                (cmd_wide.len() * 2) as u32,
            );
            let legacy_value = to_wide(LEGACY_VALUE_NAME);
            let _ = RegDeleteValueW(hkey, legacy_value.as_ptr());
            RegCloseKey(hkey);
            if set != ERROR_SUCCESS {
                anyhow::bail!("RegSetValueExW: status {}", set);
            }
        }
        Ok(())
    }

    fn remove_run_value() -> Result<()> {
        let subkey = to_wide(RUN_KEY);
        unsafe {
            let mut hkey = std::ptr::null_mut();
            let open = RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_WRITE, &mut hkey);
            if open != ERROR_SUCCESS {
                anyhow::bail!("open HKCU\\{}: status {}", RUN_KEY, open);
            }
            let mut first_error: Option<(&str, u32)> = None;
            for name in [VALUE_NAME, LEGACY_VALUE_NAME] {
                let value = to_wide(name);
                let del = RegDeleteValueW(hkey, value.as_ptr());
                if del != ERROR_SUCCESS && del != ERROR_FILE_NOT_FOUND && first_error.is_none() {
                    first_error = Some((name, del));
                }
            }
            RegCloseKey(hkey);
            // ERROR_FILE_NOT_FOUND just means "already absent" → success
            if let Some((name, status)) = first_error {
                anyhow::bail!("RegDeleteValueW({name}): status {}", status);
            }
        }
        Ok(())
    }
}
