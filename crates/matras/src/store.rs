//! Channels, posts and users on disk, so the window draws something real
//! before the first HTTP response lands.
//!
//! This is the upgrade [`crate::cache`] names in its own doc comment. The
//! snapshot there is one JSON file rewritten whole, which is why it could only
//! afford to keep the eight most recently active feeds (`cache::trim_feeds`):
//! the cost of a write scaled with everything you had ever read, not with what
//! had changed. Here a post is a row, so every channel keeps its history and a
//! write touches only the rows it names.
//!
//! Still a cache, not a source of truth: the network replaces all of it the
//! moment it answers, and any doubt about the file's contents is settled by
//! deleting it (see [`ensure_schema`]).

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mattermost_api::models::{Channel, ChannelMember, Post, User};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

/// Bumped whenever the tables or the shape of a `body` blob change.
const SCHEMA_VERSION: i64 = 1;

/// Each row keeps its model verbatim as JSON in `body`, with real columns only
/// for what is queried on. The models carry dozens of fields, half of them
/// free-form bags (`props`, `notify_props`, `metadata`); normalising them would
/// be a schema to maintain against a server that adds fields between releases,
/// while serde already round-trips them exactly.
const SCHEMA: &str = "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
    CREATE TABLE channels (id TEXT PRIMARY KEY, body TEXT NOT NULL);
    CREATE TABLE channel_members (
        channel_id TEXT NOT NULL,
        user_id    TEXT NOT NULL,
        body       TEXT NOT NULL,
        PRIMARY KEY (channel_id, user_id)
    );
    CREATE TABLE users (id TEXT PRIMARY KEY, body TEXT NOT NULL);
    CREATE TABLE posts (
        id         TEXT PRIMARY KEY,
        channel_id TEXT NOT NULL,
        create_at  INTEGER NOT NULL,
        body       TEXT NOT NULL
    );
    -- The only query shape there is: the newest N posts of one channel.
    CREATE INDEX posts_by_channel ON posts (channel_id, create_at);
";

/// How many posts per channel [`Store::prune`] keeps by default.
///
/// Generous on purpose — the store exists so that history is *there*, and a
/// number that only covers the first screen would make it a splash screen. The
/// window draws 60 posts on a cold start (`ui::INITIAL_POSTS`), so this is
/// roughly sixteen screens of scrollback that works with the network down.
///
/// Bounded on purpose too: a post's JSON is on the order of a kilobyte once
/// metadata and reactions are on it, which puts a busy channel near a megabyte
/// and the whole file in the tens of megabytes for an account with dozens of
/// them. That is a cache size worth having; a year of scrollback is not.
pub const DEFAULT_KEEP_PER_CHANNEL: usize = 1_000;

const DROP_ALL: &str = "
    DROP TABLE IF EXISTS meta;
    DROP TABLE IF EXISTS channels;
    DROP TABLE IF EXISTS channel_members;
    DROP TABLE IF EXISTS users;
    DROP TABLE IF EXISTS posts;
";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("store: a cached row is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("store: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: no usable host in the server URL {0:?}")]
    BadServer(String),
}

/// One database per server, holding that server's cached rows.
///
/// Cheap to clone: every clone shares the one connection.
#[derive(Clone)]
pub struct Store {
    // `rusqlite::Connection` is `Send` but not `Sync`, so it needs a lock to be
    // shared, and the lock has to be an async one: these methods are awaited on
    // a Tokio worker via `crate::runtime::spawn`, and a `std::sync::Mutex`
    // guard held across the call would make the future `!Send`.
    //
    // One long-lived connection rather than open-per-operation because opening
    // is the expensive part — a fresh file handle, header read and page-cache
    // per websocket post is real work to avoid a lock that is uncontended in
    // practice (one app, one store, writes measured in microseconds).
    //
    // ponytail: the blocking SQLite call runs on a Tokio worker rather than
    // `spawn_blocking`. Writes here are a handful of small rows; move to
    // `spawn_blocking` if a sync ever imports enough history to be felt.
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (creating if needed) the database for `server`.
    pub async fn open(server: &str) -> Result<Store, Error> {
        let dir = crate::writable_data_dir().join(crate::APP_ID);
        Store::open_at(&dir, &host_of(server)?)
    }

    /// Sign-out: delete the database. The next person to use this account
    /// should not find the last one's messages.
    pub async fn clear(server: &str) -> Result<(), Error> {
        let dir = crate::writable_data_dir().join(crate::APP_ID);
        remove_at(&dir, &host_of(server)?);
        Ok(())
    }

    /// The real constructor, with the directory passed in.
    ///
    /// Separate from [`Store::open`] so the tests can point at a temporary
    /// directory: they must never touch — or delete — the user's real cache.
    fn open_at(dir: &Path, host: &str) -> Result<Store, Error> {
        fs::create_dir_all(dir)?;
        let path = db_path(dir, host);
        // 0600 from the start. This holds messages, and SQLite would create the
        // file 0666-minus-umask; creating it ourselves first is the only way it
        // is never, even briefly, world readable. SQLite copies this mode onto
        // its journal files.
        let _created = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        let conn = Connection::open(&path)?;
        ensure_schema(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn save_channels(
        &self,
        channels: Vec<Channel>,
        members: Vec<ChannelMember>,
    ) -> Result<(), Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT OR REPLACE INTO channels (id, body) VALUES (?1, ?2)")?;
            for c in &channels {
                stmt.execute(params![c.id, serde_json::to_string(c)?])?;
            }
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO channel_members (channel_id, user_id, body)
                 VALUES (?1, ?2, ?3)",
            )?;
            for m in &members {
                stmt.execute(params![m.channel_id, m.user_id, serde_json::to_string(m)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn save_posts(&self, posts: Vec<Post>) -> Result<(), Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO posts (id, channel_id, create_at, body)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for p in &posts {
                stmt.execute(params![
                    p.id,
                    p.channel_id,
                    p.create_at,
                    serde_json::to_string(p)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn save_users(&self, users: Vec<User>) -> Result<(), Error> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT OR REPLACE INTO users (id, body) VALUES (?1, ?2)")?;
            for u in &users {
                stmt.execute(params![u.id, serde_json::to_string(u)?])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Newest `limit` posts in a channel, returned oldest first — the order a
    /// feed is drawn in.
    pub async fn posts(&self, channel_id: &str, limit: usize) -> Result<Vec<Post>, Error> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare_cached(
            "SELECT body FROM posts WHERE channel_id = ?1
             ORDER BY create_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![channel_id, limit as i64], |r| r.get::<_, String>(0))?;
        let mut posts = Vec::new();
        for row in rows {
            posts.push(serde_json::from_str(&row?)?);
        }
        posts.reverse();
        Ok(posts)
    }

    /// A cached window around one stable post, returned oldest first. The
    /// anchor itself is included exactly once; ties use the post id just like
    /// `posts()` so two messages sharing a millisecond remain deterministic.
    pub async fn posts_around(
        &self,
        channel_id: &str,
        post_id: &str,
        before: usize,
        after: usize,
    ) -> Result<Vec<Post>, Error> {
        let conn = self.conn.lock().await;
        let anchor: Option<(i64, String)> = conn
            .query_row(
                "SELECT create_at, body FROM posts WHERE channel_id = ?1 AND id = ?2",
                params![channel_id, post_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((create_at, body)) = anchor else {
            return Ok(Vec::new());
        };

        let mut older = Vec::new();
        let mut stmt = conn.prepare_cached(
            "SELECT body FROM posts WHERE channel_id = ?1
             AND (create_at < ?2 OR (create_at = ?2 AND id < ?3))
             ORDER BY create_at DESC, id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![channel_id, create_at, post_id, before as i64],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            older.push(serde_json::from_str(&row?)?);
        }
        older.reverse();
        older.push(serde_json::from_str(&body)?);

        let mut stmt = conn.prepare_cached(
            "SELECT body FROM posts WHERE channel_id = ?1
             AND (create_at > ?2 OR (create_at = ?2 AND id > ?3))
             ORDER BY create_at ASC, id ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![channel_id, create_at, post_id, after as i64],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            older.push(serde_json::from_str(&row?)?);
        }
        Ok(older)
    }

    /// Drops every post in a channel past the newest `keep_per_channel`, in
    /// every channel at once.
    ///
    /// Channels, members and users are left alone: there is one row per thing
    /// that exists on the server, so they are bounded by the account, not by
    /// how much it is used. Posts are the only table that grows with time.
    pub async fn prune(&self, keep_per_channel: usize) -> Result<(), Error> {
        let conn = self.conn.lock().await;
        // One statement: number the posts of each channel newest-first and
        // delete everything numbered past the limit. The tiebreak matches
        // `posts()` exactly, so pruning can never drop a post that a read of
        // the same size would have returned.
        let n = conn.execute(
            "DELETE FROM posts WHERE id IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY channel_id ORDER BY create_at DESC, id DESC
                     ) AS rank FROM posts
                 ) WHERE rank > ?1
             )",
            params![keep_per_channel as i64],
        )?;
        tracing::debug!(pruned = n, keep_per_channel, "trimmed the post cache");

        // A delete never shrinks the file, it only puts pages on the free list,
        // and the next posts to arrive reuse them — so in the steady state, a
        // pass that trims a few percent off each channel wants no VACUUM at
        // all, and running one would rewrite the whole database for nothing.
        //
        // Measured, 20 channels x 1000 posts, ~21 MB: trimming to 100 per
        // channel freed 4622 of 5183 pages and left the file at 21 MB; VACUUM
        // brought it to 2.1 MB; refilling it grew back to 21 MB and no further.
        // So it is worth exactly one case — the first prune on a file that grew
        // before there was a cap — and that case announces itself on the free
        // list. Vacuum when most of the file is holes, not on a schedule.
        let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        let total: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        if free * 2 > total {
            tracing::info!(free, total, "post cache is mostly free space, vacuuming");
            conn.execute_batch("VACUUM")?;
        }
        Ok(())
    }

    pub async fn channels(&self) -> Result<(Vec<Channel>, Vec<ChannelMember>), Error> {
        let conn = self.conn.lock().await;
        Ok((
            all(&conn, "SELECT body FROM channels")?,
            all(&conn, "SELECT body FROM channel_members")?,
        ))
    }

    pub async fn users(&self) -> Result<Vec<User>, Error> {
        let conn = self.conn.lock().await;
        all(&conn, "SELECT body FROM users")
    }
}

/// Every row of a one-column `body` query, deserialised.
fn all<T: DeserializeOwned>(conn: &Connection, sql: &str) -> Result<Vec<T>, Error> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str(&row?)?);
    }
    Ok(out)
}

fn db_path(dir: &Path, host: &str) -> PathBuf {
    dir.join(format!("{host}.sqlite"))
}

fn remove_at(dir: &Path, host: &str) {
    let path = db_path(dir, host);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = fs::remove_file(p);
    }
}

/// One database per server, named after its host.
///
/// The host reaches us from a text entry the user typed into, so it is treated
/// as hostile: anything that is not a letter, digit, dot or dash becomes `_`,
/// which leaves no separator, no `..` and no absolute path that could put the
/// file somewhere other than `dir`.
fn host_of(server: &str) -> Result<String, Error> {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // `[::1]:8065` keeps its brackets around the address; everything else stops
    // at the port separator.
    let host = match authority.strip_prefix('[').and_then(|h| h.split_once(']')) {
        Some((inside, _)) => inside,
        None => authority.split(':').next().unwrap_or(""),
    };
    let clean: String = host
        .chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    // Rejects "", "." and ".." — the three names that would not be a new file.
    if clean.trim_matches('.').is_empty() {
        return Err(Error::BadServer(server.to_string()));
    }
    Ok(clean)
}

/// Creates the tables, or drops and recreates them if the file was written by a
/// different version of this code.
///
/// No migrations, deliberately: everything in here can be fetched again, so the
/// cost of a wrong guess is one slower launch, whereas migration code is
/// maintained forever and only ever tested on the machines that already broke.
fn ensure_schema(conn: &Connection) -> Result<(), Error> {
    let version: Option<i64> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if version == Some(SCHEMA_VERSION) {
        return Ok(());
    }
    if version.is_some() {
        tracing::info!(?version, "cache schema is out of date, starting it over");
    }
    conn.execute_batch(DROP_ALL)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mattermost_api::models::Millis;

    /// A directory of our own per test. `Store::open_at` exists for this: the
    /// tests must not read, write or delete anything in the real user data dir.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("matras-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn post(id: &str, channel: &str, at: Millis) -> Post {
        Post {
            id: id.into(),
            channel_id: channel.into(),
            create_at: at,
            message: format!("message {id}"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn rows_come_back_the_way_they_went_in() {
        let dir = temp_dir("round-trip");
        let store = Store::open_at(&dir, "example.com").unwrap();

        let channel = Channel {
            id: "c1".into(),
            display_name: "Town Square".into(),
            ..Default::default()
        };
        let member = ChannelMember {
            channel_id: "c1".into(),
            user_id: "u1".into(),
            mention_count: 3,
            ..Default::default()
        };
        let user = User {
            id: "u1".into(),
            username: "ada".into(),
            ..Default::default()
        };
        store
            .save_channels(vec![channel], vec![member])
            .await
            .unwrap();
        store.save_users(vec![user]).await.unwrap();
        store.save_posts(vec![post("p1", "c1", 100)]).await.unwrap();

        let (channels, members) = store.channels().await.unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].display_name, "Town Square");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].mention_count, 3);

        let users = store.users().await.unwrap();
        assert_eq!(users[0].username, "ada");

        let posts = store.posts("c1", 10).await.unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].message, "message p1");

        // Reopening the same file finds it all again: this is the whole point.
        let reopened = Store::open_at(&dir, "example.com").unwrap();
        assert_eq!(reopened.posts("c1", 10).await.unwrap().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn posts_are_the_newest_few_but_read_oldest_first() {
        let dir = temp_dir("order");
        let store = Store::open_at(&dir, "example.com").unwrap();
        let mut posts: Vec<Post> = (0..50).map(|i| post(&format!("p{i}"), "c1", i)).collect();
        posts.push(post("other", "c2", 999));
        store.save_posts(posts).await.unwrap();

        let got = store.posts("c1", 10).await.unwrap();
        assert_eq!(got.len(), 10);
        // The newest ten...
        assert_eq!(got.first().unwrap().id, "p40");
        assert_eq!(got.last().unwrap().id, "p49");
        // ...ascending, and nothing from the other channel.
        assert!(got.windows(2).all(|w| w[0].create_at < w[1].create_at));
        assert!(got.iter().all(|p| p.channel_id == "c1"));

        // A limit past the end is not an error.
        assert_eq!(store.posts("c1", 1000).await.unwrap().len(), 50);
        assert!(store.posts("nobody", 10).await.unwrap().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn saving_a_post_again_edits_it() {
        let dir = temp_dir("upsert");
        let store = Store::open_at(&dir, "example.com").unwrap();
        store.save_posts(vec![post("p1", "c1", 100)]).await.unwrap();

        let mut edited = post("p1", "c1", 100);
        edited.message = "fixed the typo".into();
        store.save_posts(vec![edited]).await.unwrap();

        let posts = store.posts("c1", 10).await.unwrap();
        assert_eq!(posts.len(), 1, "the edit duplicated the post");
        assert_eq!(posts[0].message, "fixed the typo");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_file_from_another_schema_is_thrown_away() {
        let dir = temp_dir("version");
        let store = Store::open_at(&dir, "example.com").unwrap();
        store.save_posts(vec![post("p1", "c1", 100)]).await.unwrap();
        drop(store);

        // Pretend the file was written by a future version of this code.
        let conn = Connection::open(db_path(&dir, "example.com")).unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
            params![SCHEMA_VERSION + 1],
        )
        .unwrap();
        drop(conn);

        let store = Store::open_at(&dir, "example.com").unwrap();
        assert!(
            store.posts("c1", 10).await.unwrap().is_empty(),
            "the old rows survived a schema change"
        );
        // And it is usable again immediately.
        store.save_posts(vec![post("p2", "c1", 200)]).await.unwrap();
        assert_eq!(store.posts("c1", 10).await.unwrap().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleting_the_database_leaves_nothing_behind() {
        let dir = temp_dir("clear");
        let store = Store::open_at(&dir, "example.com").unwrap();
        store.save_posts(vec![post("p1", "c1", 100)]).await.unwrap();
        drop(store);

        remove_at(&dir, "example.com");
        assert!(!db_path(&dir, "example.com").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pruning_keeps_the_newest_few_of_every_channel() {
        // The whole statement rests on window functions; SQLite has had them
        // since 3.25 (2018) and this is the bundled build, so check the claim
        // rather than assume it.
        assert!(
            rusqlite::version_number() >= 3_025_000,
            "bundled SQLite {} is too old for ROW_NUMBER() OVER (...)",
            rusqlite::version()
        );

        let dir = temp_dir("prune");
        let store = Store::open_at(&dir, "example.com").unwrap();
        let mut posts: Vec<Post> = (0..50).map(|i| post(&format!("a{i}"), "c1", i)).collect();
        posts.extend((0..3).map(|i| post(&format!("b{i}"), "c2", i)));
        store.save_posts(posts).await.unwrap();

        store.prune(10).await.unwrap();

        // Exactly the newest ten of the busy channel...
        let kept = store.posts("c1", 1000).await.unwrap();
        assert_eq!(kept.len(), 10);
        assert_eq!(kept.first().unwrap().id, "a40");
        assert_eq!(kept.last().unwrap().id, "a49");
        // ...and the quiet channel is untouched: the limit is per channel.
        assert_eq!(store.posts("c2", 1000).await.unwrap().len(), 3);

        // Idempotent: a second pass has nothing left to take.
        store.prune(10).await.unwrap();
        assert_eq!(store.posts("c1", 1000).await.unwrap().len(), 10);
        assert_eq!(store.posts("c2", 1000).await.unwrap().len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pruning_an_empty_store_is_not_an_error() {
        let dir = temp_dir("prune-empty");
        let store = Store::open_at(&dir, "example.com").unwrap();
        store.prune(DEFAULT_KEEP_PER_CHANNEL).await.unwrap();
        // And zero is a real answer, not a no-op that quietly keeps everything.
        store.save_posts(vec![post("p1", "c1", 100)]).await.unwrap();
        store.prune(0).await.unwrap();
        assert!(store.posts("c1", 10).await.unwrap().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The one case VACUUM is there for: a file that grew before there was a
    /// cap, trimmed hard the first time a pruning build runs.
    #[tokio::test]
    async fn a_big_trim_gives_the_disk_space_back() {
        let dir = temp_dir("prune-size");
        let store = Store::open_at(&dir, "example.com").unwrap();
        let posts: Vec<Post> = (0..2000)
            .map(|i| {
                let mut p = post(&format!("p{i}"), "c1", i);
                p.message = "x".repeat(600);
                p
            })
            .collect();
        store.save_posts(posts).await.unwrap();
        let path = db_path(&dir, "example.com");
        let size = |p: &PathBuf| fs::metadata(p).unwrap().len();
        let before = size(&path);

        store.prune(10).await.unwrap();
        assert!(
            size(&path) < before / 2,
            "pruned 99% of the rows and the file is still {} of {before} bytes",
            size(&path)
        );

        // ...and the steady state does not: a pass that takes nothing leaves
        // the file alone rather than rewriting it every time.
        let settled = size(&path);
        store.prune(10).await.unwrap();
        assert_eq!(size(&path), settled);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_cannot_name_a_file_outside_the_directory() {
        assert_eq!(
            host_of("https://mm.example.com/").unwrap(),
            "mm.example.com"
        );
        assert_eq!(
            host_of("https://MM.Example.com:8065").unwrap(),
            "mm.example.com"
        );
        assert_eq!(host_of("mm.example.com").unwrap(), "mm.example.com");
        assert_eq!(
            host_of("https://user:pw@mm.example.com").unwrap(),
            "mm.example.com"
        );
        assert_eq!(host_of("http://[::1]:8065").unwrap(), "__1");
        // The interesting ones: nothing here escapes `dir`.
        assert_eq!(host_of("https://a/../../etc/passwd").unwrap(), "a");
        assert!(!host_of("https://..%2f..%2fetc").unwrap().contains('/'));
        assert!(host_of("https://").is_err());
        assert!(host_of("https://../").is_err());
    }
}
