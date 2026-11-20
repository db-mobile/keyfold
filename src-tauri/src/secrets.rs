//! Passwords live in the OS keychain (CONCEPT 3.4): Keychain Services on
//! macOS, the Credential Manager on Windows, the Secret Service on Linux.
//! One entry per stored profile, named by the profile's id, and a second
//! one (see [`ssh_account`]) for the password or key passphrase of its SSH
//! bastion. Nothing here
//! writes a password to a file. The calls block (D-Bus on Linux), so
//! callers run them on the blocking pool.

use keyring::{Entry, Error};

/// The keychain service every entry of this app is filed under.
const SERVICE: &str = "dev.keyfold.app";

/// The keychain account of a profile's SSH secret. A profile id is a UUID
/// and holds no colon, so this never names another profile's entry.
pub fn ssh_account(profile_id: &str) -> String {
    format!("{profile_id}:ssh")
}

fn entry(profile_id: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, profile_id).map_err(|e| unavailable(&e))
}

fn unavailable(e: &Error) -> String {
    format!("the OS keychain is not available: {e}")
}

/// The stored password of a profile, `None` when the keychain has none.
pub fn get(profile_id: &str) -> Result<Option<String>, String> {
    match entry(profile_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(unavailable(&e)),
    }
}

pub fn set(profile_id: &str, password: &str) -> Result<(), String> {
    entry(profile_id)?.set_password(password).map_err(|e| unavailable(&e))
}

/// Remove the entry; one that does not exist is not an error.
pub fn delete(profile_id: &str) -> Result<(), String> {
    match entry(profile_id)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(e) => Err(unavailable(&e)),
    }
}
