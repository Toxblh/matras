//! Remembering a session across restarts.
//!
//! Only the server URL and the session token are kept — never the password.
//! The token is what Mattermost itself hands out and can be revoked from
//! Security settings, so losing it is recoverable in a way a password is not.
//!
//! It lives in the Secret Service (GNOME Keyring, KWallet, anything else
//! speaking the D-Bus interface), which means it is encrypted at rest and
//! another process running as this user cannot simply read it. That is a real
//! difference for a chat token: it grants your whole account for as long as
//! nobody revokes it.
//!
//! There is a plaintext fallback at `~/.config/<app>/session.json`, for
//! machines with no Secret Service running at all — a headless box, a minimal
//! session. Falling back is announced in the log, because the difference
//! matters and silently downgrading a security property is how it stops being
//! one. The fallback is also what older versions wrote, so it is read once and
//! migrated.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use gtk::glib;
use serde_json::json;

fn path() -> PathBuf {
    glib::user_config_dir()
        .join(crate::APP_ID)
        .join("session.json")
}

/// What the keyring item is labelled and keyed by. The attributes are the
/// lookup key, so they have to be stable across versions.
#[cfg(feature = "secret-service")]
const LABEL: &str = "Matras session token";

#[cfg(feature = "secret-service")]
fn attributes() -> std::collections::HashMap<&'static str, &'static str> {
    std::collections::HashMap::from([("application", crate::APP_ID), ("type", "session")])
}

/// The same attributes, narrowed to one server. The server is part of the key
/// so several accounts can be stored side by side rather than overwriting each
/// other.
#[cfg(feature = "secret-service")]
fn attributes_for(server: &str) -> std::collections::HashMap<&str, &str> {
    std::collections::HashMap::from([
        ("application", crate::APP_ID),
        ("type", "session"),
        ("server", server),
    ])
}

/// Every stored session, newest last. Used to offer a choice when more than
/// one server is signed in.
#[cfg(feature = "secret-service")]
pub async fn load_all_async() -> Vec<(String, String)> {
    let found = async {
        let keyring = oo7::Keyring::new().await?;
        let items = keyring.search_items(&attributes()).await?;
        let mut sessions = Vec::new();
        for item in items {
            let Ok(secret) = item.secret().await else {
                continue;
            };
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&secret) {
                let read = |key: &str| {
                    value
                        .get(key)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .filter(|s| !s.is_empty())
                };
                if let (Some(server), Some(token)) = (read("server"), read("token")) {
                    sessions.push((server, token));
                }
            }
        }
        Ok::<_, oo7::Error>(sessions)
    }
    .await;

    match found {
        Ok(sessions) if !sessions.is_empty() => sessions,
        // Nothing in the keyring: either a first run, or an upgrade from the
        // version that wrote a file. Migrate it in and take it out of
        // circulation, so the fallback is read at most once.
        Ok(_) => match load() {
            Some((server, token)) => {
                save_async(&server, &token).await;
                clear_file();
                vec![(server, token)]
            }
            None => Vec::new(),
        },
        Err(e) => {
            tracing::warn!("keyring unavailable, using the file: {e}");
            load().into_iter().collect()
        }
    }
}

/// Forgets one server's session, leaving any others alone.
#[cfg(feature = "secret-service")]
pub async fn forget_async(server: &str) {
    let result = async {
        let keyring = oo7::Keyring::new().await?;
        keyring.delete(&attributes_for(server)).await
    }
    .await;
    if let Err(e) = result {
        tracing::debug!("could not forget {server}: {e}");
    }
}

/// Stores the session in the keyring, replacing whatever was there.
#[cfg(feature = "secret-service")]
pub async fn save_async(server: &str, token: &str) {
    let secret = json!({ "server": server, "token": token }).to_string();
    let result = async {
        let keyring = oo7::Keyring::new().await?;
        keyring
            .create_item(LABEL, &attributes_for(server), secret.as_bytes(), true)
            .await
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("could not reach the keyring, falling back to a file: {e}");
        save(server, token);
    }
}

/// Forgets the session in both places. Signing out has to clear the fallback
/// too, or the next launch signs straight back in with it.
#[cfg(feature = "secret-service")]
pub async fn clear_async() {
    let result = async {
        let keyring = oo7::Keyring::new().await?;
        keyring.delete(&attributes()).await
    }
    .await;
    if let Err(e) = result {
        tracing::debug!("could not clear the keyring item: {e}");
    }
    clear_file();
}

/// The same four entry points without a Secret Service anywhere in the build:
/// the app-private file is the storage, not the fallback. On Android that
/// means `/data/data/<pkg>/`, which the kernel isolates by UID and File-Based
/// Encryption covers at rest — an acceptable home for a token that can be
/// revoked server-side (see PORT-PLAN.md 3.4; a hardware-keystore backend can
/// slot in behind these signatures later).
#[cfg(not(feature = "secret-service"))]
pub async fn load_all_async() -> Vec<(String, String)> {
    load().into_iter().collect()
}

#[cfg(not(feature = "secret-service"))]
pub async fn forget_async(server: &str) {
    // The file holds one session, so forgetting a server only means anything
    // when it is the one stored.
    if load().is_some_and(|(stored, _)| stored == server) {
        clear_file();
    }
}

#[cfg(not(feature = "secret-service"))]
pub async fn save_async(server: &str, token: &str) {
    save(server, token);
}

#[cfg(not(feature = "secret-service"))]
pub async fn clear_async() {
    clear_file();
}

/// The stored session from the plaintext fallback, if there is one.
pub fn load() -> Option<(String, String)> {
    load_from(&path())
}

fn load_from(file: &std::path::Path) -> Option<(String, String)> {
    let raw = fs::read_to_string(file).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let server = v.get("server")?.as_str()?.to_string();
    let token = v.get("token")?.as_str()?.to_string();
    if server.is_empty() || token.is_empty() {
        return None;
    }
    Some((server, token))
}

/// Replaces the stored session. Failures are logged, never fatal: not being
/// able to write the file is a reason to sign in again next launch, not a
/// reason to interrupt a session that is otherwise working.
pub fn save(server: &str, token: &str) {
    if let Err(e) = write_to(&path(), server, token) {
        tracing::warn!("could not save the session: {e}");
    }
}

fn write_to(file: &std::path::Path, server: &str, token: &str) -> std::io::Result<()> {
    let dir = file.parent().expect("session path always has a parent");
    fs::create_dir_all(dir)?;
    // The token would otherwise sit in a directory anyone can list.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    // Created 0600 up front rather than chmod-ed afterwards: the gap between
    // the two is long enough for another process to open the file.
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?;
    f.write_all(
        json!({ "server": server, "token": token })
            .to_string()
            .as_bytes(),
    )?;
    f.sync_all()
}

/// Forgets the plaintext fallback.
pub fn clear_file() {
    clear_file_at(&path());
}

fn clear_file_at(file: &std::path::Path) {
    match fs::remove_file(file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("could not clear the session: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_not_readable_by_anyone_else() {
        let dir = std::env::temp_dir().join(format!("matras-session-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join(crate::APP_ID).join("session.json");

        assert_eq!(load_from(&file), None, "nothing stored yet");

        write_to(&file, "https://mm.example.com", "tok-1").unwrap();
        assert_eq!(
            load_from(&file),
            Some(("https://mm.example.com".into(), "tok-1".into()))
        );

        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token must not be world-readable");

        write_to(&file, "https://mm.example.com", "tok-2").unwrap();
        assert_eq!(
            load_from(&file).unwrap().1,
            "tok-2",
            "save replaces, never appends"
        );

        clear_file_at(&file);
        assert_eq!(load_from(&file), None);
        clear_file_at(&file); // clearing twice is not an error

        let _ = fs::remove_dir_all(&dir);
    }
}
