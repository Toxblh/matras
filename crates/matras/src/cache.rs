//! Which team and channel were open, so the next launch reopens them.
//!
//! The messages themselves live in [`crate::store`] — this is only the pointer
//! into it. It stayed a JSON file rather than becoming two more columns
//! because it is two strings, it is read once before anything else exists, and
//! a database open is a slower way to answer "where was I".

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Which server this belongs to. A pointer from a different server is
    /// meaningless here and is dropped rather than followed.
    pub server: String,
    #[serde(default)]
    pub current_team: Option<String>,
    #[serde(default)]
    pub current_channel: Option<String>,
    #[serde(default)]
    pub scroll_anchors: HashMap<String, String>,
}

/// A writable cache root.
///
/// The GTK Android runtime sets `XDG_DATA_HOME` and `XDG_CONFIG_HOME` but not
/// `XDG_CACHE_HOME`, so `glib::user_cache_dir()` resolves to an unwritable
/// `~/.cache` there and every cache write fails with EACCES. The data dir the
/// runtime *does* set is writable, so cache lives beside it; the desktop keeps
/// the real XDG cache directory.
pub(crate) fn cache_root() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        glib::user_data_dir().join("cache")
    }
    #[cfg(not(target_os = "android"))]
    {
        glib::user_cache_dir()
    }
}

fn path() -> PathBuf {
    cache_root().join(crate::APP_ID).join("snapshot.json")
}

/// Reads the snapshot, if it belongs to this server.
pub fn load(server: &str) -> Option<Snapshot> {
    let raw = fs::read_to_string(path()).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&raw).ok()?;
    (snapshot.server == server).then_some(snapshot)
}

/// Replaces the snapshot. Failures are logged and otherwise ignored: a cache
/// that cannot be written is a slower next launch, not an error.
pub fn save(snapshot: &Snapshot) {
    let path = path();
    if let Some(dir) = path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            tracing::warn!(error = %e, "could not create the cache directory");
            return;
        }
    }
    // 0600 from the start: this holds messages, and a file created world
    // readable is readable for however long it takes to chmod it.
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path);
    let mut file = match file {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "could not write the cache");
            return;
        }
    };
    let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    match serde_json::to_vec(snapshot) {
        Ok(bytes) => {
            if let Err(e) = file.write_all(&bytes) {
                tracing::warn!(error = %e, "could not write the cache");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not serialise the cache"),
    }
}

/// Removes it. Called on sign-out: the next person to use this account should
/// not find the last one's messages.
pub fn clear() {
    let _ = fs::remove_file(path());
}
