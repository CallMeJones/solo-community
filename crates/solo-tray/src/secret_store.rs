// SPDX-License-Identifier: Apache-2.0

//! OS keychain storage for tray-owned secrets.

use zeroize::Zeroizing;

pub const KEYCHAIN_SERVICE: &str = "solo-tray";
pub const DAEMON_PASSPHRASE_ACCOUNT: &str = "daemon-passphrase";
pub const BEARER_TOKEN_ACCOUNT: &str = "bearer-token";

pub fn backend_label() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Windows Credential Manager"
    }
    #[cfg(target_os = "macos")]
    {
        "macOS Keychain"
    }
    #[cfg(target_os = "linux")]
    {
        "Secret Service"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "OS keychain"
    }
}

pub fn load_daemon_passphrase() -> Result<Option<Zeroizing<String>>, String> {
    load_secret(DAEMON_PASSPHRASE_ACCOUNT)
}

pub fn load_bearer_token() -> Result<Option<Zeroizing<String>>, String> {
    load_secret(BEARER_TOKEN_ACCOUNT)
}

fn load_secret(account: &str) -> Result<Option<Zeroizing<String>>, String> {
    match entry(account)?.get_password() {
        Ok(passphrase) if passphrase.is_empty() => Ok(None),
        Ok(passphrase) => Ok(Some(Zeroizing::new(passphrase))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("read {} entry: {error}", backend_label())),
    }
}

pub fn store_daemon_passphrase(passphrase: &str) -> Result<(), String> {
    if passphrase.is_empty() {
        return Err("passphrase must not be empty".to_string());
    }
    store_secret(DAEMON_PASSPHRASE_ACCOUNT, passphrase)
}

pub fn store_bearer_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("token must not be empty".to_string());
    }
    store_secret(BEARER_TOKEN_ACCOUNT, token)
}

fn store_secret(account: &str, secret: &str) -> Result<(), String> {
    entry(account)?
        .set_password(secret)
        .map_err(|error| format!("write {} entry: {error}", backend_label()))
}

pub fn forget_daemon_passphrase() -> Result<(), String> {
    forget_secret(DAEMON_PASSPHRASE_ACCOUNT)
}

pub fn forget_bearer_token() -> Result<(), String> {
    forget_secret(BEARER_TOKEN_ACCOUNT)
}

fn forget_secret(account: &str) -> Result<(), String> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("delete {} entry: {error}", backend_label())),
    }
}

pub fn has_daemon_passphrase() -> Result<bool, String> {
    load_daemon_passphrase().map(|passphrase| passphrase.is_some())
}

pub fn has_bearer_token() -> Result<bool, String> {
    load_bearer_token().map(|token| token.is_some())
}

fn entry(account: &str) -> Result<keyring::Entry, String> {
    ensure_native_keyring_backend_available();
    keyring::Entry::new(KEYCHAIN_SERVICE, account)
        .map_err(|error| format!("open {} entry: {error}", backend_label()))
}

#[cfg(target_os = "windows")]
fn ensure_native_keyring_backend_available() {
    let _ = std::any::TypeId::of::<keyring::windows::WinCredential>();
}

#[cfg(not(target_os = "windows"))]
fn ensure_native_keyring_backend_available() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_entry_names_are_stable() {
        assert_eq!(KEYCHAIN_SERVICE, "solo-tray");
        assert_eq!(DAEMON_PASSPHRASE_ACCOUNT, "daemon-passphrase");
        assert_eq!(BEARER_TOKEN_ACCOUNT, "bearer-token");
    }

    #[test]
    fn empty_passphrase_is_rejected_before_keychain_io() {
        assert_eq!(
            store_daemon_passphrase("").unwrap_err(),
            "passphrase must not be empty"
        );
    }

    #[test]
    fn empty_token_is_rejected_before_keychain_io() {
        assert_eq!(
            store_bearer_token("").unwrap_err(),
            "token must not be empty"
        );
    }

    #[test]
    fn backend_label_is_not_empty() {
        assert!(!backend_label().is_empty());
    }
}
