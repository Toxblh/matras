//! Bounded, expiring cache for immutable-ish HTTP resources.
//!
//! One file per response keeps eviction honest: deleting an LRU entry really
//! gives its bytes back, unlike deleting a SQLite BLOB without vacuuming. The
//! small fixed header carries the expiry and the original key; the latter also
//! makes the otherwise tiny 64-bit filename collision detectable.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"MMADCHE1";
const HEADER: usize = MAGIC.len() + 8 + 4;

#[derive(Clone)]
pub struct ResourceCache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    dir: PathBuf,
    limit: AtomicU64,
}

impl ResourceCache {
    pub fn open(server: &str) -> Self {
        let root = crate::cache::cache_root().join(crate::APP_ID).join("http");
        let dir = root.join(format!("{:016x}", stable_hash(server)));
        Self::open_paths(root, dir, crate::background::cache_limit_bytes())
    }

    #[cfg(test)]
    fn open_at(dir: PathBuf, limit: u64) -> Self {
        Self::open_paths(dir.clone(), dir, limit)
    }

    fn open_paths(root: PathBuf, dir: PathBuf, limit: u64) -> Self {
        if let Err(error) = fs::create_dir_all(&dir) {
            tracing::warn!(%error, "could not create the HTTP cache");
        } else if let Err(error) = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)) {
            tracing::warn!(%error, "could not protect the HTTP cache directory");
        }
        ResourceCache {
            inner: Arc::new(Inner {
                root,
                dir,
                limit: AtomicU64::new(limit),
            }),
        }
    }

    pub async fn get_or_fetch<E, F, Fut>(
        &self,
        key: String,
        ttl: Duration,
        fetch: F,
    ) -> Result<Vec<u8>, E>
    where
        E: 'static,
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, E>>,
    {
        if let Some(bytes) = self.get(key.clone()).await {
            return Ok(bytes);
        }
        let bytes = fetch().await?;
        let cache = self.clone();
        Ok(tokio::task::spawn_blocking(move || {
            cache.put_sync(&key, &bytes, ttl);
            bytes
        })
        .await
        .expect("resource cache writer panicked"))
    }

    pub async fn get(&self, key: String) -> Option<Vec<u8>> {
        let cache = self.clone();
        tokio::task::spawn_blocking(move || cache.get_sync(&key))
            .await
            .ok()
            .flatten()
    }

    pub fn set_limit(&self, bytes: u64) {
        self.inner.limit.store(bytes, Ordering::Relaxed);
        let cache = self.clone();
        crate::runtime::runtime().spawn_blocking(move || cache.prune_sync());
    }

    pub async fn size(&self) -> u64 {
        let cache = self.clone();
        tokio::task::spawn_blocking(move || {
            cache.prune_sync();
            cache.entries().iter().map(|e| e.size).sum()
        })
        .await
        .unwrap_or(0)
    }

    pub async fn clear(&self) {
        let cache = self.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for entry in cache.entries() {
                let _ = fs::remove_file(entry.path);
            }
        })
        .await;
    }

    pub async fn remove(&self, key: String) {
        let cache = self.clone();
        let _ = tokio::task::spawn_blocking(move || fs::remove_file(cache.path(&key))).await;
    }

    fn get_sync(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path(key);
        let mut file = fs::File::open(&path).ok()?;
        let mut header = [0u8; HEADER];
        file.read_exact(&mut header).ok()?;
        if &header[..MAGIC.len()] != MAGIC {
            let _ = fs::remove_file(path);
            return None;
        }
        let expires = u64::from_be_bytes(header[8..16].try_into().ok()?);
        if expires <= now() {
            let _ = fs::remove_file(path);
            return None;
        }
        let key_len = u32::from_be_bytes(header[16..20].try_into().ok()?) as usize;
        if key_len > 4096 {
            let _ = fs::remove_file(path);
            return None;
        }
        let mut stored_key = vec![0; key_len];
        file.read_exact(&mut stored_key).ok()?;
        if stored_key != key.as_bytes() {
            return None;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let _ = file.set_times(fs::FileTimes::new().set_accessed(SystemTime::now()));
        Some(bytes)
    }

    fn put_sync(&self, key: &str, bytes: &[u8], ttl: Duration) {
        let limit = self.inner.limit.load(Ordering::Relaxed);
        if limit == 0 || bytes.len() as u64 > limit {
            return;
        }
        if fs::create_dir_all(&self.inner.dir).is_err() {
            return;
        }
        let path = self.path(key);
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp);
        let Ok(mut file) = file else { return };
        let expires = now().saturating_add(ttl.as_secs().max(1));
        let result = file
            .write_all(MAGIC)
            .and_then(|_| file.write_all(&expires.to_be_bytes()))
            .and_then(|_| file.write_all(&(key.len() as u32).to_be_bytes()))
            .and_then(|_| file.write_all(key.as_bytes()))
            .and_then(|_| file.write_all(bytes))
            .and_then(|_| file.sync_data());
        drop(file);
        if result.is_err() || fs::rename(&temp, &path).is_err() {
            let _ = fs::remove_file(temp);
            return;
        }
        self.prune_sync();
    }

    fn prune_sync(&self) {
        let limit = self.inner.limit.load(Ordering::Relaxed);
        let mut entries = self.entries();
        let current = now();
        entries.retain(|entry| {
            if entry.expires.is_some_and(|expires| expires <= current) {
                let _ = fs::remove_file(&entry.path);
                false
            } else {
                true
            }
        });
        let mut total: u64 = entries.iter().map(|e| e.size).sum();
        if total <= limit {
            return;
        }
        entries.sort_by_key(|entry| entry.accessed);
        for entry in entries {
            if total <= limit {
                break;
            }
            if fs::remove_file(&entry.path).is_ok() {
                total = total.saturating_sub(entry.size);
            }
        }
    }

    fn entries(&self) -> Vec<Entry> {
        let paths: Vec<PathBuf> = fs::read_dir(&self.inner.root)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    fs::read_dir(path)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|child| child.path())
                        .collect::<Vec<_>>()
                } else {
                    vec![path]
                }
            })
            .collect();
        paths
            .into_iter()
            .filter_map(|path| {
                if path.extension().and_then(|s| s.to_str()) != Some("cache") {
                    return None;
                }
                let metadata = path.metadata().ok()?;
                Some(Entry {
                    expires: expiry(&path),
                    path,
                    size: metadata.len(),
                    accessed: metadata.accessed().or_else(|_| metadata.modified()).ok()?,
                })
            })
            .collect()
    }

    fn path(&self, key: &str) -> PathBuf {
        self.inner
            .dir
            .join(format!("{:016x}.cache", stable_hash(key)))
    }
}

struct Entry {
    path: PathBuf,
    size: u64,
    accessed: SystemTime,
    expires: Option<u64>,
}

fn expiry(path: &Path) -> Option<u64> {
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).ok()?;
    (&header[..8] == MAGIC).then(|| u64::from_be_bytes(header[8..16].try_into().unwrap()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// FNV-1a is fixed across compiler releases; changing Rust versions should
/// not make every existing cache entry unreachable.
fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(name: &str, limit: u64) -> ResourceCache {
        let dir = std::env::temp_dir().join(format!(
            "matras-resource-cache-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        ResourceCache::open_at(dir, limit)
    }

    #[test]
    fn round_trip_and_collision_check() {
        let cache = cache("round-trip", 1024);
        cache.put_sync("avatar:u1", b"picture", Duration::from_secs(60));
        assert_eq!(cache.get_sync("avatar:u1"), Some(b"picture".to_vec()));
        assert_eq!(cache.get_sync("avatar:u2"), None);
        let _ = fs::remove_dir_all(&cache.inner.dir);
    }

    #[test]
    fn lru_limit_is_a_physical_limit() {
        let cache = cache("limit", 100);
        cache.put_sync("one", &[1; 60], Duration::from_secs(60));
        cache.put_sync("two", &[2; 60], Duration::from_secs(60));
        let total: u64 = cache.entries().iter().map(|entry| entry.size).sum();
        assert!(total <= 100, "{total} bytes must fit the configured cap");
        let _ = fs::remove_dir_all(&cache.inner.dir);
    }
}
