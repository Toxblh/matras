//! A fake Mattermost server, just real enough to exercise the client.
//!
//! It is not a mock in the unit-test sense — it holds actual state, assigns
//! real ids, and pushes the same websocket events the real server does,
//! including the double-encoded payloads (`posted.post` as a JSON *string*) and
//! the inconsistent key casing. Getting those wrong here would defeat the
//! purpose: the point is to catch the client mis-parsing what the server
//! actually sends.
//!
//! ```sh
//! cargo run -p matras-testserver           # listens on 127.0.0.1:8065
//! ```
//!
//! Log in with any username and password. A background "colleague" posts every
//! few seconds so live updates are visible without a second client, and every
//! fifth of those goes into the seeded thread.
//!
//! Seeding and behaviour come from the environment: `MM_HISTORY`,
//! `MM_CHANNELS`, `MM_THREAD_REPLIES`, `MM_BOT_SECONDS`, `MM_TESTSERVER_ADDR`,
//! and `MM_DUPLICATE_POSTS=1` to send every `posted` event twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Request;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast;

const ME: &str = "uuuuuuuuuuuuuuuuuuuuuuuume";
const LENA: &str = "uuuuuuuuuuuuuuuuuuuuuulena";
const MIKK: &str = "uuuuuuuuuuuuuuuuuuuuuumikk";
const SARA: &str = "uuuuuuuuuuuuuuuuuuuuuusara";
const TEAM: &str = "tttttttttttttttttttttttcor";
const GENERAL: &str = "cccccccccccccccccccccgener";
const DEV: &str = "ccccccccccccccccccccccdeve";
const DM_LENA: &str = "cccccccccccccccccccccdmlen";
/// Stands in for the seeded thread's root id in the colleague bot's script.
const THREAD: &str = "<thread>";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Mattermost ids are exactly 26 lowercase alphanumerics, and the client
/// validates that, so pad rather than truncate.
fn id(prefix: &str, n: i64) -> String {
    let mut s = String::from(prefix);
    let tail = format!("{n}");
    while s.len() + tail.len() < 26 {
        s.push('0');
    }
    s.push_str(&tail);
    s.truncate(26);
    s
}

struct Post {
    v: Value,
}

#[derive(Default)]
struct Db {
    posts: Vec<Post>,
    next_id: i64,
    /// The seeded thread's root, so the colleague bot can answer in it. Its id
    /// is generated, so it cannot be written into the bot's script.
    thread_root: String,
}

struct App {
    db: Mutex<Db>,
    events: broadcast::Sender<Value>,
    seq: AtomicI64,
    conn: AtomicI64,
}

impl App {
    /// Broadcasts an event with the server's own monotonic sequence, which the
    /// client validates strictly.
    fn emit(&self, event: &str, data: Value, broadcast_: Value) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let _ = self.events.send(json!({
            "event": event,
            "data": data,
            "broadcast": broadcast_,
            "seq": seq,
        }));
    }

    fn add_post_with_pending(
        &self,
        channel_id: &str,
        user_id: &str,
        message: &str,
        root_id: &str,
        pending_post_id: &str,
    ) -> Value {
        let mut post = self.add_post(channel_id, user_id, message, root_id);
        // The real server echoes this back so clients can retire their
        // optimistic copy; not doing so makes every sent message appear twice.
        post["pending_post_id"] = json!(pending_post_id);
        let post_id = post["id"].clone();
        let mut db = self.db.lock().unwrap();
        if let Some(stored) = db.posts.iter_mut().find(|p| p.v["id"] == post_id) {
            stored.v["pending_post_id"] = json!(pending_post_id);
        }
        post
    }

    fn add_post(&self, channel_id: &str, user_id: &str, message: &str, root_id: &str) -> Value {
        let mut db = self.db.lock().unwrap();
        db.next_id += 1;
        let post_id = id("p", db.next_id);
        // Seeding happens inside one millisecond, and a real server would not
        // hand out identical timestamps for a whole conversation — nudge each
        // post forward so ordering is deterministic.
        let at = now() + db.next_id;
        let post = json!({
            "id": post_id,
            "create_at": at,
            "update_at": at,
            "edit_at": 0,
            "delete_at": 0,
            "is_pinned": false,
            "user_id": user_id,
            "channel_id": channel_id,
            "root_id": root_id,
            "original_id": "",
            "message": message,
            "type": "",
            "props": {},
            "hashtags": "",
            "file_ids": [],
            "pending_post_id": "",
            "reply_count": 0,
            "last_reply_at": 0,
            "participants": null,
            "metadata": {},
        });
        db.posts.push(Post { v: post.clone() });

        // Keep the root's reply_count honest — the client renders it.
        if !root_id.is_empty() {
            let replies = db
                .posts
                .iter()
                .filter(|p| p.v["root_id"] == root_id)
                .count() as i64;
            if let Some(root) = db.posts.iter_mut().find(|p| p.v["id"] == root_id) {
                root.v["reply_count"] = json!(replies);
                root.v["last_reply_at"] = json!(at);
            }
        }
        post
    }

    fn post_channel_event(&self, post: &Value, mentions: Vec<&str>) {
        // A resuming client is replayed events it may already have applied, so
        // the same post really does arrive twice. `MM_DUPLICATE_POSTS=1` makes
        // that the normal case instead of a race that has to be won.
        if std::env::var("MM_DUPLICATE_POSTS").as_deref() == Ok("1") {
            self.emit_channel_event(post, &mentions);
        }
        self.emit_channel_event(post, &mentions);
    }

    fn emit_channel_event(&self, post: &Value, mentions: &[&str]) {
        let channel_id = post["channel_id"].as_str().unwrap_or_default().to_string();
        let user_id = post["user_id"].as_str().unwrap_or_default();
        self.emit(
            "posted",
            json!({
                // The real server sends the post as a JSON *string*.
                "post": serde_json::to_string(post).unwrap(),
                "channel_type": if channel_id == DM_LENA { "D" } else { "O" },
                "channel_display_name": channel_name(&channel_id),
                "channel_name": channel_id,
                "sender_name": username(user_id),
                "team_id": TEAM,
                "set_online": true,
                "mentions": serde_json::to_string(mentions).unwrap(),
            }),
            json!({ "channel_id": channel_id, "team_id": TEAM }),
        );
    }
}

fn username(user_id: &str) -> &'static str {
    match user_id {
        LENA => "lena",
        MIKK => "mikk",
        SARA => "sara",
        _ => "anton",
    }
}

fn channel_name(channel_id: &str) -> String {
    match channel_id {
        DEV => "Development".to_string(),
        DM_LENA => String::new(),
        _ => match filler_index(channel_id) {
            Some(n) => format!("Channel {n:02}"),
            None => "General".to_string(),
        },
    }
}

/// Extra open channels beyond the three hand-written ones. Three rows rebuild
/// too fast to time, so any work on how the sidebar redraws itself needs a
/// realistic list to show up at all — `MM_CHANNELS=100` is what the sidebar
/// profiling runs use.
fn filler_count() -> usize {
    std::env::var("MM_CHANNELS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn filler_id(n: usize) -> String {
    id("cx", n as i64)
}

/// The inverse of `filler_id`, so the per-channel handlers can answer for a
/// generated channel without keeping a table of them.
fn filler_index(channel_id: &str) -> Option<usize> {
    channel_id.strip_prefix("cx")?.parse().ok()
}

fn user(user_id: &str) -> Value {
    let (username, first, last, position) = match user_id {
        LENA => ("lena", "Lena", "Petrova", "Media engineer"),
        MIKK => ("mikk", "Mikk", "Tamm", "Release manager"),
        SARA => ("sara", "Sara", "Okafor", "Protocol archaeologist"),
        _ => ("anton", "Anton", "Ivanov", "Desktop client"),
    };
    json!({
        "id": user_id,
        "create_at": 1_700_000_000_000i64,
        "update_at": 1_700_000_000_000i64,
        "delete_at": 0,
        "username": username,
        "first_name": first,
        "last_name": last,
        "nickname": "",
        "email": format!("{username}@example.com"),
        "position": position,
        "roles": "system_user",
        "locale": "en",
        "last_picture_update": 1_700_000_000_000i64,
        "timezone": {"useAutomaticTimezone": "true", "automaticTimezone": "Europe/Tallinn", "manualTimezone": ""},
        "props": if user_id == SARA {
            json!({"customStatus": r#"{"emoji":"coffee","text":"Reading rtcd","duration":"today"}"#})
        } else { json!({}) },
        "notify_props": {},
    })
}

fn channel(channel_id: &str) -> Value {
    if let Some(n) = filler_index(channel_id) {
        return json!({
            "id": channel_id,
            "create_at": 1_700_000_000_000i64,
            "update_at": 1_700_000_000_000i64,
            "delete_at": 0,
            "team_id": TEAM,
            "type": "O",
            "display_name": channel_name(channel_id),
            "name": format!("channel-{n}"),
            "header": "",
            "purpose": "",
            "last_post_at": now(),
            "total_msg_count": 6,
            "total_msg_count_root": 6,
            "creator_id": SARA,
        });
    }
    let (name, display, kind, total) = match channel_id {
        DEV => ("development", "Development", "O", 12),
        DM_LENA => (
            // A DM's name is the two ids joined by a double underscore.
            Box::leak(format!("{ME}__{LENA}").into_boxed_str()) as &str,
            "",
            "D",
            3,
        ),
        _ => ("general", "General", "O", 6),
    };
    json!({
        "id": channel_id,
        "create_at": 1_700_000_000_000i64,
        "update_at": 1_700_000_000_000i64,
        "delete_at": 0,
        "team_id": if kind == "D" { "" } else { TEAM },
        "type": kind,
        "display_name": display,
        "name": name,
        "header": if channel_id == DEV { "Client work · protocol notes pinned" } else { "" },
        "purpose": "",
        "last_post_at": now(),
        "total_msg_count": total,
        "total_msg_count_root": total,
        "creator_id": SARA,
    })
}

fn membership(channel_id: &str, unread: i64, mentions: i64) -> Value {
    let total = channel(channel_id)["total_msg_count"].as_i64().unwrap_or(0);
    json!({
        "channel_id": channel_id,
        "user_id": ME,
        "roles": "channel_user",
        "last_viewed_at": now(),
        "msg_count": total - unread,
        "msg_count_root": total - unread,
        "mention_count": mentions,
        "mention_count_root": mentions,
        "urgent_mention_count": 0,
        "notify_props": {},
        "last_update_at": now(),
        "scheme_admin": false,
    })
}

// ────────────────────────────────────────────────────────────── handlers

async fn login(State(app): State<Arc<App>>) -> Response {
    let _ = &app;
    let mut headers = HeaderMap::new();
    // The token comes back in a *header*, not the body.
    headers.insert("Token", "test-session-token".parse().unwrap());
    (headers, Json(user(ME))).into_response()
}

async fn client_config() -> Json<Value> {
    Json(json!({
        "Version": "11.11.0",
        "SiteName": "Test Mattermost",
        "TeammateNameDisplay": "full_name",
        // Collapsed reply threads on, so the client exercises the CRT path.
        "CollapsedThreads": "default_on",
        "EnableCustomEmoji": "true",
        "PostPriority": "true",
    }))
}

async fn empty_object() -> Json<Value> {
    Json(json!({}))
}

async fn empty_array() -> Json<Value> {
    Json(json!([]))
}

async fn me() -> Json<Value> {
    Json(user(ME))
}

async fn my_teams() -> Json<Value> {
    Json(json!([{
        "id": TEAM,
        "create_at": 1_700_000_000_000i64,
        "update_at": 1_700_000_000_000i64,
        "delete_at": 0,
        "display_name": "Core Platform",
        "name": "core",
        "description": "",
        "type": "O",
        "allow_open_invite": false,
    }]))
}

async fn my_team_members() -> Json<Value> {
    Json(json!([{
        "team_id": TEAM, "user_id": ME, "roles": "team_user",
        "delete_at": 0, "scheme_user": true, "scheme_admin": false,
    }]))
}

async fn my_channels() -> Json<Value> {
    let mut out = vec![channel(GENERAL), channel(DEV), channel(DM_LENA)];
    out.extend((0..filler_count()).map(|n| channel(&filler_id(n))));
    Json(Value::Array(out))
}

async fn my_channel_members() -> Json<Value> {
    let mut out = vec![
        membership(GENERAL, 0, 0),
        membership(DEV, 4, 1),
        membership(DM_LENA, 1, 0),
    ];
    // Not all alike: a few carry mentions and a few are already read, so the
    // sidebar has badges, dots and plain rows to redraw rather than one shape.
    out.extend((0..filler_count()).map(|n| {
        membership(
            &filler_id(n),
            (n % 4) as i64,
            if n % 7 == 0 { 1 + (n % 3) as i64 } else { 0 },
        )
    }));
    Json(Value::Array(out))
}

async fn categories() -> Json<Value> {
    let mut channels = vec![GENERAL.to_string(), DEV.to_string()];
    channels.extend((0..filler_count()).map(filler_id));
    Json(json!({
        "categories": [
            {"id": "channels_cat", "user_id": ME, "team_id": TEAM, "sort_order": 10,
             "sorting": "alpha", "type": "channels", "display_name": "Channels",
             "muted": false, "collapsed": false, "channel_ids": channels},
            {"id": "dm_cat", "user_id": ME, "team_id": TEAM, "sort_order": 20,
             "sorting": "recent", "type": "direct_messages", "display_name": "Direct Messages",
             "muted": false, "collapsed": false, "channel_ids": [DM_LENA]}
        ],
        "order": ["channels_cat", "dm_cat"]
    }))
}

fn post_list(app: &App, filter: impl Fn(&Value) -> bool) -> Value {
    let db = app.db.lock().unwrap();
    let mut matched: Vec<&Value> = db
        .posts
        .iter()
        .map(|p| &p.v)
        .filter(|p| filter(p))
        .collect();
    matched.sort_by_key(|p| -p["create_at"].as_i64().unwrap_or(0));

    let order: Vec<String> = matched
        .iter()
        .map(|p| p["id"].as_str().unwrap_or_default().to_string())
        .collect();
    let mut posts = serde_json::Map::new();
    for p in matched {
        posts.insert(p["id"].as_str().unwrap_or_default().to_string(), p.clone());
    }
    json!({
        "order": order,
        "posts": posts,
        // Empty both ways means "this page is the whole channel".
        "next_post_id": "",
        "prev_post_id": "",
        "first_inaccessible_post_time": 0,
    })
}

async fn channel_posts(
    State(app): State<Arc<App>>,
    Path(channel_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let since: i64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
    let crt = q.get("collapsedThreads").map(String::as_str) == Some("true");
    let before = q.get("before").map(String::as_str);
    let page = q.get("page").and_then(|s| s.parse().ok()).unwrap_or(0usize);
    let per_page = q
        .get("per_page")
        .and_then(|s| s.parse().ok())
        .unwrap_or(60usize)
        .clamp(1, 200);

    let db = app.db.lock().unwrap();
    let mut matched: Vec<Value> = db
        .posts
        .iter()
        .map(|p| &p.v)
        .filter(|p| {
            p["channel_id"] == channel_id.as_str()
                && p["update_at"].as_i64().unwrap_or(0) > since
                // Under CRT, replies live in threads and never in the channel feed.
                && (!crt || p["root_id"].as_str().unwrap_or_default().is_empty())
        })
        .cloned()
        .collect();
    matched.sort_by_key(|p| -p["create_at"].as_i64().unwrap_or(0));

    let start = before
        .and_then(|id| matched.iter().position(|post| post["id"] == id))
        .map_or_else(|| page.saturating_mul(per_page), |position| position + 1)
        .min(matched.len());
    let end = start.saturating_add(per_page).min(matched.len());
    let page = &matched[start..end];
    let order: Vec<String> = page
        .iter()
        .filter_map(|post| post["id"].as_str().map(str::to_string))
        .collect();
    let posts: serde_json::Map<String, Value> = page
        .iter()
        .filter_map(|post| Some((post["id"].as_str()?.to_string(), post.clone())))
        .collect();
    Json(json!({
        "order": order,
        "posts": posts,
        "next_post_id": if start > 0 { "newer" } else { "" },
        "prev_post_id": if end < matched.len() { "older" } else { "" },
        "first_inaccessible_post_time": 0,
    }))
}

async fn unread_posts(
    State(app): State<Arc<App>>,
    Path(channel_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let crt = q.get("collapsedThreads").map(String::as_str) == Some("true");
    Json(post_list(&app, |p| {
        p["channel_id"] == channel_id.as_str()
            && (!crt || p["root_id"].as_str().unwrap_or_default().is_empty())
    }))
}

async fn post_thread(State(app): State<Arc<App>>, Path(post_id): Path<String>) -> Json<Value> {
    Json(post_list(&app, |p| {
        p["id"] == post_id.as_str() || p["root_id"] == post_id.as_str()
    }))
}

async fn create_post(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Json<Value> {
    let channel_id = body["channel_id"].as_str().unwrap_or_default();
    let message = body["message"].as_str().unwrap_or_default();
    let root_id = body["root_id"].as_str().unwrap_or_default();
    let pending = body["pending_post_id"].as_str().unwrap_or_default();
    let post = app.add_post_with_pending(channel_id, ME, message, root_id, pending);
    app.post_channel_event(&post, vec![]);
    Json(post)
}

async fn users_by_ids(Json(ids): Json<Vec<String>>) -> Json<Value> {
    Json(Value::Array(ids.iter().map(|i| user(i)).collect()))
}

async fn users_query(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let _ = q;
    Json(json!([user(ME), user(LENA), user(MIKK), user(SARA)]))
}

async fn statuses(Json(ids): Json<Vec<String>>) -> Json<Value> {
    // Lena drifts between online and away every `MM_AWAY_SECONDS` (default
    // 90), and — exactly like the real server, which publishes status_change
    // to that user's own sessions only — nobody else is told over the socket.
    let period = std::env::var("MM_AWAY_SECONDS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(90);
    let lena = if (now() / 1000 / period) % 2 == 0 {
        "online"
    } else {
        "away"
    };
    Json(Value::Array(
        ids.iter()
            .map(|i| {
                let status = match i.as_str() {
                    LENA => lena,
                    MIKK => "away",
                    SARA => "dnd",
                    _ => "online",
                };
                json!({"user_id": i, "status": status, "manual": false,
                       "last_activity_at": now(), "dnd_end_time": 0})
            })
            .collect(),
    ))
}

/// A flat colour per user, so "the right avatar for the right person" is
/// verifiable at a glance.
async fn user_image(Path(user_id): Path<String>) -> Response {
    let rgb = match user_id.as_str() {
        LENA => [0xE8, 0x6A, 0x33],
        MIKK => [0x2E, 0x9E, 0x6B],
        SARA => [0x3B, 0x74, 0xD8],
        _ => [0xC9, 0x9A, 0x1E],
    };
    let mut img = image::RgbImage::new(128, 128);
    for (x, y, px) in img.enumerate_pixels_mut() {
        // A diagonal band makes it obvious this is a real image, not a colour
        // block the client painted itself.
        let band = ((x + y) / 16) % 2 == 0;
        let scale = if band { 1.0 } else { 0.82 };
        *px = image::Rgb([
            (rgb[0] as f32 * scale) as u8,
            (rgb[1] as f32 * scale) as u8,
            (rgb[2] as f32 * scale) as u8,
        ]);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();

    ([(header::CONTENT_TYPE, "image/png")], png.into_inner()).into_response()
}

async fn add_reaction(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Json<Value> {
    let post_id = body["post_id"].as_str().unwrap_or_default().to_string();
    let emoji = body["emoji_name"].as_str().unwrap_or_default().to_string();
    let reaction = json!({
        "user_id": ME, "post_id": post_id, "emoji_name": emoji,
        "create_at": now(), "update_at": now(), "delete_at": 0,
        "channel_id": "",
    });
    apply_reaction(&app, &post_id, &reaction, true);
    app.emit(
        "reaction_added",
        // Again: a JSON string, not an object.
        json!({ "reaction": serde_json::to_string(&reaction).unwrap() }),
        json!({ "channel_id": "" }),
    );
    Json(reaction)
}

async fn remove_reaction(
    State(app): State<Arc<App>>,
    Path((user_id, post_id, emoji)): Path<(String, String, String)>,
) -> StatusCode {
    let reaction = json!({
        "user_id": user_id, "post_id": post_id, "emoji_name": emoji,
        "create_at": 0, "update_at": 0, "delete_at": now(), "channel_id": "",
    });
    apply_reaction(&app, &post_id, &reaction, false);
    app.emit(
        "reaction_removed",
        json!({ "reaction": serde_json::to_string(&reaction).unwrap() }),
        json!({ "channel_id": "" }),
    );
    StatusCode::OK
}

fn apply_reaction(app: &App, post_id: &str, reaction: &Value, add: bool) {
    let mut db = app.db.lock().unwrap();
    let Some(post) = db.posts.iter_mut().find(|p| p.v["id"] == post_id) else {
        return;
    };
    let list = post.v["metadata"]["reactions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut list: Vec<Value> = list;
    if add {
        list.push(reaction.clone());
    } else {
        list.retain(|r| {
            !(r["user_id"] == reaction["user_id"] && r["emoji_name"] == reaction["emoji_name"])
        });
    }
    post.v["has_reactions"] = json!(!list.is_empty());
    post.v["metadata"]["reactions"] = json!(list);
}

async fn view_channel() -> Json<Value> {
    Json(json!({"status": "OK", "last_viewed_at_times": {}}))
}

async fn team_unreads() -> Json<Value> {
    Json(json!([{
        "team_id": TEAM, "msg_count": 5, "mention_count": 1,
        "mention_count_root": 1, "msg_count_root": 5,
        "thread_count": 1, "thread_mention_count": 1, "thread_urgent_mention_count": 0,
    }]))
}

/// `GET /users/{me}/teams/{team}/threads` — the CRT inbox.
async fn threads(State(app): State<Arc<App>>) -> Json<Value> {
    let db = app.db.lock().unwrap();
    let roots: Vec<Value> = db
        .posts
        .iter()
        .map(|p| &p.v)
        .filter(|p| p["reply_count"].as_i64().unwrap_or(0) > 0)
        .map(|p| {
            json!({
                "id": p["id"],
                "reply_count": p["reply_count"],
                "last_reply_at": p["last_reply_at"],
                "last_viewed_at": 0,
                "unread_replies": p["reply_count"],
                "unread_mentions": 0,
                "is_following": true,
                "post": p,
                "participants": [user(SARA), user(ME)],
            })
        })
        .collect();
    Json(json!({
        "threads": roots,
        "total": 0,
        "total_unread_threads": 0,
        "total_unread_mentions": 0,
    }))
}

async fn search_posts(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Json<Value> {
    let terms = body["terms"].as_str().unwrap_or_default().to_lowercase();
    let needles: Vec<String> = terms
        .split_whitespace()
        .map(|t| t.trim_start_matches('@').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let mut list = post_list(&app, |p| {
        let message = p["message"].as_str().unwrap_or_default().to_lowercase();
        needles.iter().any(|n| message.contains(n))
    });
    list["matches"] = json!({});
    Json(list)
}

// ────────────────────────────────────────────────────────────── websocket

async fn websocket(ws: WebSocketUpgrade, State(app): State<Arc<App>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: Arc<App>) {
    use futures_util::{SinkExt, StreamExt};
    let (mut sink, mut source) = socket.split();

    let conn_id = id("w", app.conn.fetch_add(1, Ordering::SeqCst));
    let mut events = app.events.subscribe();

    // `hello` is always sequence 0 of a stream.
    app.seq.store(0, Ordering::SeqCst);
    let hello = json!({
        "event": "hello",
        "data": {"connection_id": conn_id, "server_version": "11.11.0.test"},
        "broadcast": {},
        "seq": app.seq.fetch_add(1, Ordering::SeqCst),
    });
    if sink
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            event = events.recv() => {
                let Ok(event) = event else { continue };
                if sink.send(Message::Text(event.to_string().into())).await.is_err() {
                    return;
                }
            }
            incoming = source.next() => {
                let Some(Ok(msg)) = incoming else { return };
                let Message::Text(text) = msg else { continue };
                let Ok(req) = serde_json::from_str::<Value>(&text) else { continue };
                // The client pings every 30s and closes the socket if the reply
                // does not arrive before the next tick.
                if req["action"] == "ping" {
                    let reply = json!({
                        "status": "OK",
                        "seq_reply": req["seq"],
                        "data": {"text": "pong", "version": "11.11.0"},
                    });
                    if sink.send(Message::Text(reply.to_string().into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ────────────────────────────────────────────────────────────── seed & bot

fn seed(app: &App) -> String {
    // A scrollback long enough to page through. Seven seeded posts fit on one
    // screen, so pagination and scroll-anchor work cannot be exercised without
    // it. `MM_HISTORY=400` is what the profiling runs use.
    let history: usize = std::env::var("MM_HISTORY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for i in 0..history {
        let author = [SARA, MIKK, LENA, ME][i % 4];
        app.add_post(DEV, author, &format!("Backlog message {}", i + 1), "");
    }

    let root = app.add_post(
        DEV,
        SARA,
        "Reminder: MediaMap.sender_id is the *receiver's* session id. Parse the track id instead.",
        "",
    );
    let root_id = root["id"].as_str().unwrap().to_string();
    app.add_post(
        DEV,
        MIKK,
        "That explains the mis-attributed screen shares.",
        &root_id,
    );
    app.add_post(
        DEV,
        ME,
        "Adding a test for the screen-audio case.",
        &root_id,
    );

    // The same argument as `MM_HISTORY`, one panel to the right: two replies
    // fit on one screen, so nothing about the thread panel's virtualisation or
    // its scroll anchoring can be exercised against them.
    // `MM_THREAD_REPLIES=300` is what the profiling runs use.
    let thread_replies: usize = std::env::var("MM_THREAD_REPLIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    for i in 0..thread_replies {
        let author = [MIKK, LENA, SARA, ME][i % 4];
        app.add_post(DEV, author, &format!("Thread reply {}", i + 1), &root_id);
    }
    // The bot answers in this thread, so a reply can arrive without a hand
    // on the keyboard — which is the only way to watch the counter move.
    app.db.lock().unwrap().thread_root = root_id.clone();

    app.add_post(
        DEV,
        LENA,
        "Release notes draft is up — shout if the Calls section overstates things.",
        "",
    );
    app.add_post(
        DEV,
        MIKK,
        "@anton staging rejects binary websocket frames, hold the release.",
        "",
    );
    app.add_post(GENERAL, SARA, "Standup moved to 10:15 tomorrow.", "");
    app.add_post(DM_LENA, LENA, "Did the reconnect fix land?", "");

    // A couple of reactions on the root, so the emoji rendering has something
    // to show without anyone clicking.
    for (user_id, emoji) in [(LENA, "eyes"), (MIKK, "eyes"), (SARA, "tada")] {
        let reaction = json!({
            "user_id": user_id, "post_id": root_id, "emoji_name": emoji,
            "create_at": now(), "update_at": now(), "delete_at": 0, "channel_id": DEV,
        });
        apply_reaction(app, &root_id, &reaction, true);
    }
    root_id
}

/// Posts every few seconds so live updates are visible with one client open.
///
/// One of the lines lands in the seeded thread rather than in its channel:
/// whether the thread panel drags a reader to the end when a reply arrives can
/// only be seen with a reply actually arriving into an open thread.
async fn colleague(app: Arc<App>, thread_root: String) {
    let lines = [
        (LENA, DEV, "Rebased onto the reconnect branch.", ""),
        (
            MIKK,
            DEV,
            "@anton can you look at the ICE buffering patch?",
            "",
        ),
        (
            MIKK,
            DEV,
            "Confirmed against the track-id path as well.",
            thread_root.as_str(),
        ),
        (
            SARA,
            GENERAL,
            "Docs for the data channel are in the wiki now.",
            "",
        ),
        (LENA, DM_LENA, "It did — thanks.", ""),
        // Answers in the seeded thread, whose root id is only known at run
        // time — hence the placeholder.
        (SARA, DEV, "One more thought on that thread.", THREAD),
    ];
    let mut i = 0usize;
    loop {
        // Slow enough that a human — or a screenshot test — can click on
        // something before the list moves under them.
        let interval = std::env::var("MM_BOT_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let (user_id, channel_id, message, root) = lines[i % lines.len()];
        let root = match root {
            THREAD => app.db.lock().unwrap().thread_root.clone(),
            other => other.to_string(),
        };
        // With generated channels present, spread the noise across them: the
        // sidebar's redraw cost is per row, and it only shows when the badges
        // move around a long list rather than in the same three places.
        // A reply is the exception: it belongs to its root's channel.
        let channel_id = match filler_count() {
            _ if !root.is_empty() => channel_id.to_string(),
            0 => channel_id.to_string(),
            n => filler_id(i % n),
        };
        i += 1;
        let post = app.add_post(&channel_id, user_id, message, &root);
        let mentions = if message.contains("@anton") {
            vec![ME]
        } else {
            vec![]
        };
        app.post_channel_event(&post, mentions);
    }
}

#[tokio::main]
async fn main() {
    let (events, _) = broadcast::channel(256);
    let app = Arc::new(App {
        db: Mutex::new(Db::default()),
        events,
        seq: AtomicI64::new(0),
        conn: AtomicI64::new(1),
    });
    let thread_root = seed(&app);
    tokio::spawn(colleague(app.clone(), thread_root));

    let api = Router::new()
        .route("/users/login", post(login))
        .route("/users/logout", post(empty_object))
        .route("/config/client", get(client_config))
        .route("/license/client", get(empty_object))
        .route("/users/me", get(me))
        .route("/users/me/preferences", get(empty_array))
        .route("/users/me/teams", get(my_teams))
        .route("/users/me/teams/members", get(my_team_members))
        .route("/users/me/teams/unread", get(team_unreads))
        .route("/users/me/teams/{team}/channels", get(my_channels))
        .route(
            "/users/me/teams/{team}/channels/members",
            get(my_channel_members),
        )
        .route(
            "/users/me/teams/{team}/channels/categories",
            get(categories),
        )
        .route("/users/me/teams/{team}/threads", get(threads))
        .route("/users/{user}/teams/{team}/threads", get(threads))
        .route("/users/ids", post(users_by_ids))
        .route("/users", get(users_query))
        .route("/users/status/ids", post(statuses))
        .route("/users/{user}/image", get(user_image))
        .route(
            "/users/me/channels/{channel}/posts/unread",
            get(unread_posts),
        )
        .route("/channels/{channel}/posts", get(channel_posts))
        .route("/channels/members/me/view", post(view_channel))
        .route("/posts", post(create_post))
        .route("/posts/{post}/thread", get(post_thread))
        .route("/reactions", post(add_reaction))
        .route(
            "/users/{user}/posts/{post}/reactions/{emoji}",
            delete(remove_reaction),
        )
        .route("/teams/{team}/posts/search", post(search_posts))
        .route("/emoji", get(empty_array))
        .route("/websocket", get(websocket));

    // Request logging: without it there is no way to tell "the client never
    // asked" from "the server answered wrongly", which is most of debugging.
    let router = Router::new()
        .nest("/api/v4", api)
        .with_state(app)
        .layer(middleware::from_fn(|req: Request, next: Next| async move {
            let method = req.method().clone();
            let path = req.uri().to_string();
            let response = next.run(req).await;
            println!("{method} {path} -> {}", response.status().as_u16());
            response
        }));

    let address =
        std::env::var("MM_TESTSERVER_ADDR").unwrap_or_else(|_| "127.0.0.1:8065".to_string());
    let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
    println!("fake Mattermost on http://{address} — any username and password will do");
    axum::serve(listener, router).await.unwrap();
}
