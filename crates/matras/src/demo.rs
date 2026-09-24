//! Sample data for looking at the layout without a server.
//!
//! Enabled with `MATRAS_DEMO=1`. Nothing here touches the network; the
//! [`Client`] it builds points at a URL that does not resolve, and every
//! request the UI makes against it fails harmlessly.

use std::cell::RefCell;
use std::rc::Rc;

use mattermost_api::models::*;
use mattermost_api::Client;

use crate::state::{AppState, ChannelFeed, SharedState};

/// `2026-08-21T09:00:00Z` in Mattermost's Unix-millisecond timestamps.
const BASE: Millis = 1_787_302_800_000;

pub fn state() -> SharedState {
    let client = Client::new("https://demo.invalid").expect("static url");
    let me = user("u-me", "anton", "Anton", "Ivanov");

    let mut config = StringMap::new();
    config.insert("TeammateNameDisplay".into(), "full_name".into());
    config.insert("Version".into(), "11.11.0".into());

    let mut st = AppState::new(client, me.clone(), ClientConfig(config), false);

    let mut lena = user("u-lena", "lena", "Lena", "Petrova");
    if std::env::var_os("MATRAS_DEMO_STATUS").is_some() {
        lena.props.insert(
            "customStatus".into(),
            serde_json::json!({
                "emoji": "house",
                "text": "Working from home",
                "duration": "today"
            })
            .to_string(),
        );
    }

    for u in [
        lena,
        user("u-mikk", "mikk", "Mikk", "Tamm"),
        user("u-sara", "sara", "Sara", "Okafor"),
        user("u-bot", "buildbot", "Build", "Bot"),
    ] {
        st.users.insert(u.id.clone(), u);
    }

    st.teams = vec![
        team("t-core", "Core Platform"),
        team("t-design", "Design"),
        team("t-ops", "Operations"),
    ];
    st.current_team = Some("t-core".into());

    // --- channels ------------------------------------------------------
    let channels = vec![
        channel("c-general", "general", "General", ChannelType::Open, 420),
        channel(
            "c-dev",
            "development",
            "Development",
            ChannelType::Open,
            1180,
        ),
        channel(
            "c-releases",
            "releases",
            "Releases",
            ChannelType::Private,
            96,
        ),
        channel(
            "c-design",
            "design-crit",
            "Design Crit",
            ChannelType::Open,
            61,
        ),
        dm("c-dm-lena", "u-me", "u-lena", 34),
        dm("c-dm-mikk", "u-me", "u-mikk", 12),
    ];
    for c in channels {
        st.channels.insert(c.id.clone(), c);
    }

    // Memberships carry the read state. Unread = channel total minus mine.
    st.memberships
        .insert("c-general".into(), membership("c-general", 420, 0, false));
    // 7 unread messages, 2 of them mentions.
    st.memberships
        .insert("c-dev".into(), membership("c-dev", 1173, 2, false));
    // Muted and unread: shows no bold, but would still show a mention pill.
    st.memberships
        .insert("c-releases".into(), membership("c-releases", 90, 0, true));
    st.memberships
        .insert("c-design".into(), membership("c-design", 61, 0, false));
    st.memberships
        .insert("c-dm-lena".into(), membership("c-dm-lena", 31, 1, false));
    st.memberships
        .insert("c-dm-mikk".into(), membership("c-dm-mikk", 12, 0, false));

    // --- sidebar categories --------------------------------------------
    st.categories = OrderedSidebarCategories {
        order: vec!["cat-fav".into(), "cat-chan".into(), "cat-dm".into()],
        categories: vec![
            category("cat-fav", "Favourites", CategoryType::Favorites, &["c-dev"]),
            category(
                "cat-chan",
                "Channels",
                CategoryType::Channels,
                &["c-general", "c-releases", "c-design"],
            ),
            category(
                "cat-dm",
                "Direct Messages",
                CategoryType::DirectMessages,
                &["c-dm-lena", "c-dm-mikk"],
            ),
        ],
    };

    // --- a conversation --------------------------------------------------
    st.current_channel = Some("c-dev".into());
    st.feeds.insert("c-dev".into(), dev_channel_feed());
    st.feeds.insert(
        "c-general".into(),
        feed(vec![post(
            "p-g1",
            "c-general",
            "u-sara",
            "Standup moved to 10:15 tomorrow.",
            BASE - 3_600_000,
        )]),
    );

    // A thread hanging off p-5, so the right-hand panel has something real to
    // show, and an inbox with one mention and one followed thread.
    st.threads.insert(
        "p-5".into(),
        feed(vec![
            post(
                "p-5",
                "c-dev",
                "u-me",
                "Confirmed against rtcd/client/utils.go. I'll add a test that pins the `screen-audio` case, since the hyphen makes it look like it would split wrong.",
                BASE + 300_000,
            ),
            post(
                "p-5-r1",
                "c-dev",
                "u-sara",
                "The server is stricter than the client here — it wants exactly three fields.",
                BASE + 360_000,
            ),
            post(
                "p-5-r2",
                "c-dev",
                "u-mikk",
                "Worth a comment in the parser then.",
                BASE + 420_000,
            ),
        ]),
    );

    st.mentions = vec![post(
        "p-7",
        "c-dev",
        "u-mikk",
        "@anton staging is rejecting binary websocket frames — please don't cut the release until we know why.",
        BASE + 1_500_000,
    )];

    st.thread_inbox = vec![UserThread {
        id: "p-5".into(),
        reply_count: 2,
        last_reply_at: BASE + 420_000,
        unread_replies: 2,
        unread_mentions: 0,
        is_following: true,
        post: post(
            "p-5",
            "c-dev",
            "u-me",
            "Confirmed against rtcd/client/utils.go. I'll add a test that pins the `screen-audio` case.",
            BASE + 300_000,
        ),
        ..Default::default()
    }];

    // A call is happening in Development.
    st.active_calls.insert(
        "c-dev".into(),
        vec!["u-lena".into(), "u-mikk".into(), "u-sara".into()],
    );

    Rc::new(RefCell::new(st))
}

fn dev_channel_feed() -> ChannelFeed {
    let mut posts = vec![
        // Yesterday, so the day separator has something to separate.
        post(
            "p-1",
            "c-dev",
            "u-lena",
            "Pushed the SFU reconnect fix — the signalling lock was being taken on the first negotiation, which deadlocked against the server's own 10s wait.",
            BASE - 26 * 3_600_000,
        ),
        post(
            "p-2",
            "c-dev",
            "u-mikk",
            "That explains the joins that hung at exactly ten seconds. Nice find.",
            BASE - 26 * 3_600_000 + 240_000,
        ),
        // Today.
        post(
            "p-3",
            "c-dev",
            "u-sara",
            "Morning. I'm looking at the track attribution — reminder that `MediaMap.sender_id` is wrong server-side, it carries the *receiver's* session id.",
            BASE + 60_000,
        ),
        post(
            "p-4",
            "c-dev",
            "u-sara",
            "So parse the track id instead:\n\n```go\nparts := strings.Split(trackID, \"_\")\n```\n\n- **type** first\n- then the *session*\n- see [the notes](https://example.com/rtc)",
            BASE + 95_000,
        ),
        post(
            "p-5",
            "c-dev",
            "u-me",
            "Confirmed against rtcd/client/utils.go. I'll add a test that pins the `screen-audio` case, since the hyphen makes it look like it would split wrong.",
            BASE + 300_000,
        ),
    ];

    // A reply thread.
    posts[4].reply_count = 2;

    // An edited message with a reaction and an attachment.
    let mut edited = post(
        "p-6",
        "c-dev",
        "u-lena",
        "Release notes draft is attached — shout if the Calls section overstates what works.",
        BASE + 900_000,
    );
    edited.edit_at = BASE + 960_000;
    edited.metadata = Some(PostMetadata {
        files: vec![FileInfo {
            id: "f-1".into(),
            name: "release-notes-0.1.0.md".into(),
            extension: "md".into(),
            size: 14_820,
            mime_type: "text/markdown".into(),
            ..Default::default()
        }],
        reactions: vec![
            reaction("p-6", "u-me", "eyes"),
            reaction("p-6", "u-mikk", "eyes"),
            reaction("p-6", "u-sara", "tada"),
        ],
        ..Default::default()
    });
    posts.push(edited);

    if std::env::var_os("MATRAS_DEMO_VIDEO").is_some() {
        let mut video = post(
            "p-demo-video",
            "c-dev",
            "u-sara",
            "Recording from the call",
            BASE + 1_200_000,
        );
        video.metadata = Some(PostMetadata {
            files: vec![FileInfo {
                id: "f-demo-video".into(),
                name: "call-recording.mp4".into(),
                extension: "mp4".into(),
                size: 8_388_608,
                mime_type: "video/mp4".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        posts.push(video);
    }

    // An urgent message, to exercise the priority tag.
    let mut urgent = post(
        "p-7",
        "c-dev",
        "u-mikk",
        "Staging is rejecting binary websocket frames — please don't cut the release until we know why.",
        BASE + 1_500_000,
    );
    urgent.metadata = Some(PostMetadata {
        priority: Some(PostPriority {
            priority: Some("urgent".into()),
            requested_ack: Some(true),
            persistent_notifications: None,
        }),
        ..Default::default()
    });
    posts.push(urgent);

    // A system message and a bot post.
    let mut joined = post(
        "p-8",
        "c-dev",
        "u-sara",
        "sara joined the channel.",
        BASE + 1_560_000,
    );
    joined.r#type = "system_join_channel".into();
    posts.push(joined);

    let mut bot = post(
        "p-9",
        "c-dev",
        "u-bot",
        "Build #2418 passed in 6m 12s.",
        BASE + 1_800_000,
    );
    bot.props.insert(
        "from_webhook".into(),
        serde_json::Value::String("true".into()),
    );
    bot.props.insert(
        "override_username".into(),
        serde_json::Value::String("CI".into()),
    );
    posts.push(bot);

    feed(posts)
}

// ---------------------------------------------------------------- builders

fn feed(posts: Vec<Post>) -> ChannelFeed {
    let last = posts.last().map(|p| p.create_at).unwrap_or(0);
    ChannelFeed {
        posts,
        at_latest: true,
        at_oldest: true,
        last_fetched_at: last,
    }
}

fn user(id: &str, username: &str, first: &str, last: &str) -> User {
    User {
        id: id.into(),
        username: username.into(),
        first_name: first.into(),
        last_name: last.into(),
        roles: "system_user".into(),
        ..Default::default()
    }
}

fn team(id: &str, display: &str) -> Team {
    Team {
        id: id.into(),
        name: display.to_lowercase().replace(' ', "-"),
        display_name: display.into(),
        r#type: "O".into(),
        ..Default::default()
    }
}

fn channel(id: &str, name: &str, display: &str, kind: ChannelType, total: i64) -> Channel {
    Channel {
        id: id.into(),
        team_id: "t-core".into(),
        name: name.into(),
        display_name: display.into(),
        r#type: kind,
        header: if id == "c-dev" {
            "Client work · rtcd protocol notes in the pinned post".into()
        } else {
            String::new()
        },
        total_msg_count: total,
        total_msg_count_root: total,
        last_post_at: BASE,
        ..Default::default()
    }
}

fn dm(id: &str, me: &str, other: &str, total: i64) -> Channel {
    Channel {
        id: id.into(),
        team_id: String::new(),
        // A DM's name is the two user ids joined by a double underscore, and
        // its display name is empty — the client resolves the label itself.
        name: format!("{me}__{other}"),
        r#type: ChannelType::Direct,
        total_msg_count: total,
        total_msg_count_root: total,
        last_post_at: BASE,
        ..Default::default()
    }
}

fn membership(channel_id: &str, read: i64, mentions: i64, muted: bool) -> ChannelMember {
    let mut notify_props = StringMap::new();
    if muted {
        notify_props.insert("mark_unread".into(), "mention".into());
    }
    ChannelMember {
        channel_id: channel_id.into(),
        user_id: "u-me".into(),
        msg_count: read,
        msg_count_root: read,
        mention_count: mentions,
        mention_count_root: mentions,
        last_viewed_at: Some(BASE),
        notify_props,
        ..Default::default()
    }
}

fn category(id: &str, display: &str, kind: CategoryType, channels: &[&str]) -> SidebarCategory {
    SidebarCategory {
        id: id.into(),
        team_id: "t-core".into(),
        display_name: display.into(),
        r#type: kind,
        sorting: "alpha".into(),
        channel_ids: channels.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn post(id: &str, channel: &str, author: &str, message: &str, at: Millis) -> Post {
    Post {
        id: id.into(),
        channel_id: channel.into(),
        user_id: author.into(),
        message: message.into(),
        create_at: at,
        update_at: at,
        ..Default::default()
    }
}

fn reaction(post_id: &str, user_id: &str, emoji: &str) -> Reaction {
    Reaction {
        post_id: post_id.into(),
        user_id: user_id.into(),
        emoji_name: emoji.into(),
        create_at: BASE,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_thread_starts_with_its_root() {
        let state = state();
        let st = state.borrow();
        let thread = st.threads.get("p-5").expect("the demo thread");
        assert_eq!(thread.posts[0].id, "p-5", "the root must come first");
        assert_eq!(thread.posts.len(), 3);
    }

    #[test]
    fn demo_state_is_internally_consistent() {
        let state = state();
        let st = state.borrow();

        // Every channel a category references must exist.
        for category in &st.categories.categories {
            for id in &category.channel_ids {
                assert!(st.channels.contains_key(id), "missing channel {id}");
            }
        }
        // Every category in `order` must exist.
        for id in &st.categories.order {
            assert!(
                st.categories.categories.iter().any(|c| &c.id == id),
                "missing category {id}"
            );
        }
        // Development should read as unread with mentions; General should not.
        let dev = st.unread("c-dev");
        assert!(dev.is_unread());
        assert_eq!(dev.mentions, 2);
        assert!(!st.unread("c-general").is_unread());

        // The muted channel has unread messages but must not count as unread.
        let releases = st.unread("c-releases");
        assert!(releases.muted);
        assert!(releases.messages > 0);
        assert!(!releases.is_unread(), "muted channels hide message unreads");
    }

    #[test]
    fn dm_labels_resolve_to_the_other_person() {
        let state = state();
        let st = state.borrow();
        let dm = st.channel("c-dm-lena").unwrap();
        assert_eq!(st.channel_title(dm), "Lena Petrova");
    }

    #[test]
    fn webhook_posts_use_their_override_name() {
        let state = state();
        let st = state.borrow();
        let bot = st.feeds["c-dev"]
            .posts
            .iter()
            .find(|p| p.id == "p-9")
            .unwrap();
        assert_eq!(st.author_name(bot), "CI");
    }
}
