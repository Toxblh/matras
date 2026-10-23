//! WebSocket envelopes and typed events.
//!
//! # The double-encoding trap
//!
//! Almost every event payload that carries a model struct carries it as a
//! **JSON-encoded string**, not as a nested object — `posted.post`,
//! `reaction_added.reaction`, `preferences_changed.preferences`,
//! `channel_updated.channel` and friends all need `from_str` on a `&str`, not
//! `from_value` on an object. A handful (`user_updated.user`, `config_changed`,
//! `license_changed`) really are objects. [`extract`] tries the string form
//! first and falls back to the object form, so callers never have to remember
//! which is which.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::models::{Draft, Post, Preference, Reaction, User};

pub type Data = Map<String, Value>;

/// `server/public/model/websocket_request.go` — what we send.
#[derive(Debug, Clone, Serialize)]
pub struct WsRequest<T> {
    pub seq: i64,
    pub action: String,
    pub data: T,
}

/// `broadcast` on every event. The channel/team/user an event applies to
/// frequently lives *here* rather than in `data` — `typing` is the classic
/// example: the channel id is only in `broadcast.channel_id`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Broadcast {
    #[serde(default)]
    pub omit_users: Option<HashMap<String, bool>>,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub team_id: String,
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub omit_connection_id: String,
}

/// A raw server frame. A frame with a non-zero `seq_reply` is a reply to one of
/// our requests; a frame with a non-empty `event` is a broadcast event.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WsFrame {
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub data: Data,
    #[serde(default)]
    pub broadcast: Broadcast,
    #[serde(default)]
    pub seq: i64,
    #[serde(default)]
    pub seq_reply: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl WsFrame {
    pub fn is_reply(&self) -> bool {
        self.seq_reply.is_some_and(|s| s != 0)
    }
}

/// Pulls `key` out of `data`, tolerating both the JSON-string and the plain
/// object encoding.
pub fn extract<T: DeserializeOwned>(data: &Data, key: &str) -> Option<T> {
    match data.get(key)? {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => serde_json::from_value(other.clone()).ok(),
    }
}

fn str_field(data: &Data, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Payload of the `posted` event.
#[derive(Debug, Clone)]
pub struct Posted {
    pub post: Post,
    pub channel_type: String,
    pub channel_display_name: String,
    pub channel_name: String,
    pub sender_name: String,
    pub team_id: String,
    /// User ids mentioned by this post. The official clients take the mention
    /// decision from *here* rather than re-parsing the message text.
    pub mentions: Vec<String>,
    pub followers: Vec<String>,
    pub should_ack: bool,
}

impl Posted {
    fn parse(data: &Data) -> Option<Self> {
        Some(Posted {
            post: extract(data, "post")?,
            channel_type: str_field(data, "channel_type"),
            channel_display_name: str_field(data, "channel_display_name"),
            channel_name: str_field(data, "channel_name"),
            sender_name: str_field(data, "sender_name"),
            team_id: str_field(data, "team_id"),
            mentions: extract(data, "mentions").unwrap_or_default(),
            followers: extract(data, "followers").unwrap_or_default(),
            should_ack: data
                .get("should_ack")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub fn mentions_user(&self, user_id: &str) -> bool {
        self.mentions.iter().any(|m| m == user_id)
    }
}

/// The events a chat client actually needs to act on. Everything else stays
/// available as [`Event::Other`] with its raw payload.
#[derive(Debug, Clone)]
pub enum Event {
    Hello {
        connection_id: String,
        server_version: String,
    },
    Posted(Posted),
    PostEdited(Post),
    PostDeleted(Post),
    PostUnread {
        channel_id: String,
        post_id: String,
        msg_count: i64,
        mention_count: i64,
    },
    ReactionAdded(Reaction),
    ReactionRemoved(Reaction),
    Typing {
        channel_id: String,
        user_id: String,
        parent_id: String,
    },
    StatusChange {
        user_id: String,
        status: String,
    },
    UserUpdated(Box<User>),
    /// Another of your sessions marked channels read.
    ChannelsViewed {
        channel_times: HashMap<String, i64>,
    },
    ChannelCreated {
        channel_id: String,
        team_id: String,
    },
    ChannelUpdated {
        channel_id: String,
    },
    ChannelDeleted {
        channel_id: String,
        delete_at: i64,
    },
    ChannelMemberUpdated {
        channel_id: String,
    },
    DirectAdded {
        channel_id: String,
    },
    UserAdded {
        user_id: String,
        channel_id: String,
        team_id: String,
    },
    UserRemoved {
        user_id: String,
        channel_id: String,
    },
    PreferencesChanged(Vec<Preference>),
    /// Fired on *any* preference change, sometimes with no payload at all —
    /// treat it as "refetch this team's categories", not as a delta.
    SidebarCategoriesInvalidated {
        team_id: String,
    },
    AddedToTeam {
        team_id: String,
        user_id: String,
    },
    LeaveTeam {
        team_id: String,
        user_id: String,
    },
    /// A draft you wrote on another device. Both create and update arrive as
    /// `draft_created` — `draft_updated` exists in the server's enum but is
    /// never published.
    /// Teams or their names changed; the switcher needs refetching.
    TeamsChanged,
    /// The user directory changed — someone joined or was deactivated.
    UsersChanged,
    /// The server's custom emoji set changed.
    EmojiChanged,
    /// Text the server wants shown to this person.
    Notice {
        message: String,
    },
    /// A list this client can show — scheduled posts, channel bookmarks —
    /// changed elsewhere.
    ListsChanged,
    /// Something about a followed thread moved: new reply, follow toggled,
    /// or read state changed. Carries no useful delta, so it means "refetch".
    ThreadsChanged,
    /// A priority message's acknowledgements changed; the post has to be
    /// refetched to know who.
    AcknowledgementChanged {
        post_id: String,
    },
    /// An integration wants a form filled in.
    OpenDialog(Box<crate::models::dialog::OpenDialogRequest>),
    /// A message shown only to you and never stored — how slash commands and
    /// plugins answer.
    EphemeralMessage(Box<Post>),
    DraftCreated(Box<Draft>),
    DraftDeleted(Box<Draft>),
    /// Anything we do not model, including every `custom_<plugin>_<name>`
    /// event — this is how Calls signalling reaches the calls crate.
    Other {
        event: String,
        data: Data,
        broadcast: Broadcast,
    },
}

impl Event {
    pub fn from_frame(frame: &WsFrame) -> Event {
        let d = &frame.data;
        let b = &frame.broadcast;
        match frame.event.as_str() {
            "hello" => Event::Hello {
                connection_id: str_field(d, "connection_id"),
                server_version: str_field(d, "server_version"),
            },
            "posted" => match Posted::parse(d) {
                Some(p) => Event::Posted(p),
                None => Event::other(frame),
            },
            // A scheduled post was created, sent or cancelled somewhere, and
            // a bookmark likewise. Neither carries a usable delta for us, so
            // both mean "the list you may be looking at is stale".
            "scheduled_post_created"
            | "scheduled_post_updated"
            | "scheduled_post_deleted"
            | "channel_bookmark_created"
            | "channel_bookmark_updated"
            | "channel_bookmark_deleted"
            | "channel_bookmark_sorted" => Event::ListsChanged,
            // CRT bookkeeping: a thread you follow changed, or your follow
            // state for one did. Both mean the inbox is stale.
            "thread_updated" | "thread_follow_changed" | "thread_read_changed" => {
                Event::ThreadsChanged
            }
            // Somebody confirmed reading a priority message, or took it back.
            "post_acknowledgement_added" | "post_acknowledgement_removed" => {
                match extract(d, "acknowledgement")
                    .and_then(|a: serde_json::Value| Some(a.get("post_id")?.as_str()?.to_string()))
                    .or_else(|| d.get("post_id").and_then(|v| v.as_str()).map(str::to_owned))
                {
                    Some(post_id) => Event::AcknowledgementChanged { post_id },
                    None => Event::other(frame),
                }
            }
            "draft_created" | "draft_updated" => match extract(d, "draft") {
                Some(draft) => Event::DraftCreated(Box::new(draft)),
                None => Event::other(frame),
            },
            "draft_deleted" => match extract(d, "draft") {
                Some(draft) => Event::DraftDeleted(Box::new(draft)),
                None => Event::other(frame),
            },
            // The channel list changed shape in a way that is not about
            // membership: renamed, converted between public and private,
            // restored from the archive.
            "channel_converted" | "channel_restored" => Event::ChannelUpdated {
                channel_id: str_field(d, "channel_id"),
            },
            // Team membership or naming moved. Both mean the switcher is
            // stale, and neither carries enough to patch it.
            "update_team" | "delete_team" | "restore_team" | "update_team_scheme" => {
                Event::TeamsChanged
            }
            // Somebody was deactivated, or the server gained a user or an
            // emoji. All three make a cached copy wrong rather than absent.
            "user_activation_status_change" | "new_user" => Event::UsersChanged,
            "emoji_added" => Event::EmojiChanged,
            // A plugin's own toast, and the "you have unread urgent messages"
            // nudge. Both are text aimed at the person.
            "show_toast" | "persistent_notification_triggered" => Event::Notice {
                message: str_field(d, "message"),
            },
            // A plugin or slash command asking for a form. The dialog arrives
            // as a JSON *string* inside the payload, not as an object.
            "open_dialog" => match extract(d, "dialog") {
                Some(request) => Event::OpenDialog(Box::new(request)),
                None => Event::other(frame),
            },
            // A slash command's answer to you alone. It is a post, but one
            // the server never stores, so it arrives under its own name.
            "ephemeral_message" => match extract(d, "post") {
                Some(post) => Event::EphemeralMessage(Box::new(post)),
                None => Event::other(frame),
            },
            "post_edited" => match extract(d, "post") {
                Some(p) => Event::PostEdited(p),
                None => Event::other(frame),
            },
            "post_deleted" => match extract(d, "post") {
                Some(p) => Event::PostDeleted(p),
                None => Event::other(frame),
            },
            "post_unread" => Event::PostUnread {
                channel_id: b.channel_id.clone(),
                post_id: str_field(d, "post_id"),
                msg_count: d.get("msg_count").and_then(Value::as_i64).unwrap_or(0),
                mention_count: d.get("mention_count").and_then(Value::as_i64).unwrap_or(0),
            },
            "reaction_added" => match extract(d, "reaction") {
                Some(r) => Event::ReactionAdded(r),
                None => Event::other(frame),
            },
            "reaction_removed" => match extract(d, "reaction") {
                Some(r) => Event::ReactionRemoved(r),
                None => Event::other(frame),
            },
            "typing" => Event::Typing {
                // The channel is only in the broadcast envelope.
                channel_id: b.channel_id.clone(),
                user_id: str_field(d, "user_id"),
                parent_id: str_field(d, "parent_id"),
            },
            "status_change" => Event::StatusChange {
                user_id: str_field(d, "user_id"),
                status: str_field(d, "status"),
            },
            "user_updated" => match extract::<User>(d, "user") {
                Some(u) => Event::UserUpdated(Box::new(u)),
                None => Event::other(frame),
            },
            "multiple_channels_viewed" => Event::ChannelsViewed {
                channel_times: d
                    .get("channel_times")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default(),
            },
            "channel_created" => Event::ChannelCreated {
                channel_id: str_field(d, "channel_id"),
                team_id: str_field(d, "team_id"),
            },
            "channel_updated" => Event::ChannelUpdated {
                channel_id: if d.contains_key("channel_id") {
                    str_field(d, "channel_id")
                } else {
                    b.channel_id.clone()
                },
            },
            "channel_deleted" => Event::ChannelDeleted {
                channel_id: str_field(d, "channel_id"),
                delete_at: d.get("delete_at").and_then(Value::as_i64).unwrap_or(0),
            },
            // Note the camelCase key — the server is inconsistent here.
            "channel_member_updated" => Event::ChannelMemberUpdated {
                channel_id: extract::<crate::models::ChannelMember>(d, "channelMember")
                    .map(|m| m.channel_id)
                    .unwrap_or_else(|| b.channel_id.clone()),
            },
            "direct_added" | "group_added" => Event::DirectAdded {
                channel_id: b.channel_id.clone(),
            },
            "user_added" => Event::UserAdded {
                user_id: str_field(d, "user_id"),
                channel_id: b.channel_id.clone(),
                team_id: str_field(d, "team_id"),
            },
            "user_removed" => Event::UserRemoved {
                user_id: str_field(d, "user_id"),
                channel_id: if d.contains_key("channel_id") {
                    str_field(d, "channel_id")
                } else {
                    b.channel_id.clone()
                },
            },
            "preferences_changed" | "preferences_deleted" => {
                Event::PreferencesChanged(extract(d, "preferences").unwrap_or_default())
            }
            "preference_changed" => Event::PreferencesChanged(
                extract::<Preference>(d, "preference")
                    .map(|p| vec![p])
                    .unwrap_or_default(),
            ),
            "sidebar_category_created"
            | "sidebar_category_updated"
            | "sidebar_category_deleted"
            | "sidebar_category_order_updated" => Event::SidebarCategoriesInvalidated {
                team_id: b.team_id.clone(),
            },
            "added_to_team" => Event::AddedToTeam {
                team_id: str_field(d, "team_id"),
                user_id: str_field(d, "user_id"),
            },
            "leave_team" => Event::LeaveTeam {
                team_id: str_field(d, "team_id"),
                user_id: str_field(d, "user_id"),
            },
            _ => Event::other(frame),
        }
    }

    fn other(frame: &WsFrame) -> Event {
        Event::Other {
            event: frame.event.clone(),
            data: frame.data.clone(),
            broadcast: frame.broadcast.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posted_unwraps_the_double_encoded_post() {
        let raw = r#"{
            "event":"posted",
            "data":{
                "post":"{\"id\":\"p1\",\"message\":\"hi\",\"channel_id\":\"c1\",\"user_id\":\"u1\"}",
                "channel_display_name":"Town Square",
                "channel_name":"town-square",
                "channel_type":"O",
                "sender_name":"alice",
                "team_id":"t1",
                "mentions":"[\"u2\"]"
            },
            "broadcast":{"channel_id":"c1"},
            "seq":7
        }"#;
        let frame: WsFrame = serde_json::from_str(raw).unwrap();
        match Event::from_frame(&frame) {
            Event::Posted(p) => {
                assert_eq!(p.post.id, "p1");
                assert_eq!(p.post.message, "hi");
                assert_eq!(p.sender_name, "alice");
                assert!(p.mentions_user("u2"));
                assert!(!p.mentions_user("u1"));
            }
            other => panic!("expected Posted, got {other:?}"),
        }
    }

    #[test]
    fn typing_takes_the_channel_from_the_broadcast_envelope() {
        let raw = r#"{"event":"typing","data":{"user_id":"u1","parent_id":""},
                      "broadcast":{"channel_id":"c9"},"seq":3}"#;
        let frame: WsFrame = serde_json::from_str(raw).unwrap();
        match Event::from_frame(&frame) {
            Event::Typing {
                channel_id,
                user_id,
                ..
            } => {
                assert_eq!(channel_id, "c9");
                assert_eq!(user_id, "u1");
            }
            other => panic!("expected Typing, got {other:?}"),
        }
    }

    #[test]
    fn user_updated_carries_a_real_object_not_a_string() {
        let raw = r#"{"event":"user_updated","data":{"user":{"id":"u1","username":"bob"}},
                      "broadcast":{},"seq":1}"#;
        let frame: WsFrame = serde_json::from_str(raw).unwrap();
        match Event::from_frame(&frame) {
            Event::UserUpdated(u) => assert_eq!(u.username, "bob"),
            other => panic!("expected UserUpdated, got {other:?}"),
        }
    }

    #[test]
    fn status_change_reads_status_and_user_from_data() {
        let raw = r#"{"event":"status_change","data":{"status":"away","user_id":"u1"},
                      "broadcast":{"user_id":"u1"},"seq":3}"#;
        let frame: WsFrame = serde_json::from_str(raw).unwrap();
        match Event::from_frame(&frame) {
            Event::StatusChange { user_id, status } => {
                assert_eq!(user_id, "u1");
                assert_eq!(status, "away");
            }
            other => panic!("expected StatusChange, got {other:?}"),
        }
    }

    #[test]
    fn replies_are_distinguishable_from_events() {
        let reply: WsFrame = serde_json::from_str(r#"{"status":"OK","seq_reply":4}"#).unwrap();
        assert!(reply.is_reply());
        let ev: WsFrame = serde_json::from_str(r#"{"event":"hello","data":{},"seq":0}"#).unwrap();
        assert!(!ev.is_reply());
    }

    #[test]
    fn plugin_events_fall_through_to_other() {
        let raw = r#"{"event":"custom_com.mattermost.calls_join",
                      "data":{"connID":"abc"},"broadcast":{},"seq":2}"#;
        let frame: WsFrame = serde_json::from_str(raw).unwrap();
        match Event::from_frame(&frame) {
            Event::Other { event, data, .. } => {
                assert_eq!(event, "custom_com.mattermost.calls_join");
                assert_eq!(data.get("connID").unwrap(), "abc");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
