//! Desktop notifications.
//!
//! The websocket does not say "raise a toast" — it says what happened and who
//! was mentioned, and the client decides. [`should_notify`] is that decision,
//! kept as a pure function of the event and the user's settings so it can be
//! tested without a server or a desktop.
//!
//! The rules mirror the server's own (`app/notification.go`): a channel-level
//! setting beats the global one, a DM is always worth a mention-level toast,
//! and nothing is worth interrupting yourself over.

use adw::prelude::*;
use mattermost_api::models::{ChannelMember, User};
use mattermost_api::ws::Posted;

/// How much a person wants to be told, in the order the server writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    All,
    Mention,
    None,
}

impl Level {
    fn parse(value: Option<&str>) -> Option<Level> {
        match value {
            Some("all") => Some(Level::All),
            Some("mention") => Some(Level::Mention),
            Some("none") => Some(Level::None),
            // "default" on a channel means "whatever the account says", which
            // is the same as having no channel setting at all.
            _ => None,
        }
    }
}

/// Whether this post should raise a desktop notification.
///
/// `focused_channel` is the channel the window is showing *while it has focus*
/// — reading a message as it arrives is not something to be told about.
pub fn should_notify(
    posted: &Posted,
    me: &User,
    membership: Option<&ChannelMember>,
    focused_channel: Option<&str>,
) -> bool {
    if posted.post.user_id == me.id {
        return false;
    }
    // Joins, leaves and the rest are noise; the server does not notify for
    // them either. Only *system* types, though — a bot's answer carries a type
    // of its own and is still worth being told about.
    if posted.post.is_system() {
        return false;
    }
    if focused_channel == Some(posted.post.channel_id.as_str()) {
        return false;
    }

    // A muted channel is muted: the server stores that as
    // mark_unread == "mention" and stops sending push for anything else, so a
    // desktop toast for ordinary activity there would be the one client
    // ignoring the setting.
    let muted = membership.is_some_and(|m| m.is_muted());

    let level = membership
        .and_then(|m| Level::parse(m.notify_props.get("desktop").map(String::as_str)))
        .or_else(|| Level::parse(me.notify_props.get("desktop").map(String::as_str)))
        // The server's default is to notify on mentions.
        .unwrap_or(Level::Mention);
    let level = if muted && level == Level::All {
        Level::Mention
    } else {
        level
    };

    match level {
        Level::None => false,
        Level::All => true,
        // A direct message is addressed to you whether or not it says your
        // name, so it counts as a mention.
        // "D" and "G" are the wire values for a direct and a group message.
        Level::Mention => {
            posted.mentions_user(&me.id) || matches!(posted.channel_type.as_str(), "D" | "G")
        }
    }
}

/// Sends a notification that opens the channel when clicked.
///
/// The id is the channel, so a second message in the same conversation
/// replaces the first rather than stacking — a busy channel should not bury
/// the rest of the desktop; [`withdraw`] takes it back once it is read.
///
/// While one of our windows is the active one the person is already looking
/// at the app, so a banner on top of it is noise: only the sound plays (when
/// `sound` is on, which is the account's "Notification sound").
pub fn show(app: &gtk::Application, channel_id: &str, title: &str, body: &str, sound: bool) {
    if app.windows().iter().any(|window| window.is_active()) {
        if sound {
            play_sound();
        }
        return;
    }
    let notification = gtk::gio::Notification::new(title);
    notification.set_body(Some(body));
    notification.set_priority(gtk::gio::NotificationPriority::Normal);
    notification
        .set_default_action_and_target_value("app.open-channel", Some(&channel_id.to_variant()));
    app.send_notification(Some(channel_id), &notification);
}

/// Takes back the channel's notification: it has been read, here or on
/// another device, so it is no longer news.
pub fn withdraw(app: &gtk::Application, channel_id: &str) {
    app.withdraw_notification(channel_id);
}

/// The message sound the desktop would have played with the notification.
fn play_sound() {
    const SOUND: &str = "/usr/share/sounds/freedesktop/stereo/message-new-instant.oga";
    thread_local! {
        // Held until the next one: a media file nobody holds stops playing.
        static PLAYING: std::cell::RefCell<Option<gtk::MediaFile>> = const { std::cell::RefCell::new(None) };
    }
    if !std::path::Path::new(SOUND).exists() {
        // No sound theme (a bare Flatpak runtime, Android): the bell at least.
        if let Some(display) = gtk::gdk::Display::default() {
            display.beep();
        }
        return;
    }
    let media = gtk::MediaFile::for_filename(SOUND);
    media.play();
    PLAYING.with_borrow_mut(|playing| *playing = Some(media));
}

/// A message's Markdown reduced to its text: rendered to Pango markup the way
/// the feed does, then stripped of the markup.
fn plain_text(message: &str) -> String {
    crate::markdown::parse(message)
        .into_iter()
        .map(|block| match block {
            crate::markdown::Block::Text(markup) => {
                // Every `<` left in the markup opens a tag (text is escaped),
                // and `<a>` is GtkLabel's, which Pango's parser refuses — so
                // the tags go by hand and Pango only unescapes what is left.
                let mut bare = String::with_capacity(markup.len());
                let mut in_tag = false;
                for ch in markup.chars() {
                    match ch {
                        '<' => in_tag = true,
                        '>' if in_tag => in_tag = false,
                        _ if !in_tag => bare.push(ch),
                        _ => {}
                    }
                }
                gtk::pango::parse_markup(&bare, '\0')
                    .map(|(_, text, _)| text.to_string())
                    .unwrap_or(bare)
            }
            crate::markdown::Block::Code { text, .. } => text,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The line under the title: who said it, or what they said — as plain text,
/// because a notification shows `**this**` literally.
pub fn body(posted: &Posted) -> String {
    let plain = plain_text(&posted.post.message);
    let text = plain.trim();
    if text.is_empty() {
        return if posted.post.file_ids.is_empty() {
            "Sent a message".to_string()
        } else {
            "Sent an attachment".to_string()
        };
    }
    // A notification is a glance, not a read.
    let mut line = text.lines().next().unwrap_or(text).to_string();
    if line.chars().count() > 140 {
        line = line.chars().take(140).collect::<String>() + "…";
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted(sender: &str, channel_type: &str, mentions: &[&str]) -> Posted {
        let post = mattermost_api::models::Post {
            user_id: sender.to_string(),
            channel_id: "c1".into(),
            message: "hello".into(),
            ..Default::default()
        };
        Posted {
            post,
            channel_type: channel_type.to_string(),
            channel_display_name: String::new(),
            channel_name: String::new(),
            sender_name: String::new(),
            team_id: String::new(),
            mentions: mentions.iter().map(|m| m.to_string()).collect(),
            followers: Vec::new(),
            should_ack: false,
        }
    }

    fn me() -> User {
        User {
            id: "me".into(),
            ..Default::default()
        }
    }

    fn member(desktop: &str) -> ChannelMember {
        let mut notify_props = mattermost_api::models::StringMap::new();
        notify_props.insert("desktop".into(), desktop.to_string());
        ChannelMember {
            notify_props,
            ..Default::default()
        }
    }

    #[test]
    fn the_default_is_mentions_only() {
        let quiet = posted("them", "O", &[]);
        let named = posted("them", "O", &["me"]);
        assert!(!should_notify(&quiet, &me(), None, None));
        assert!(should_notify(&named, &me(), None, None));
    }

    #[test]
    fn a_direct_message_counts_as_a_mention() {
        assert!(should_notify(&posted("them", "D", &[]), &me(), None, None));
    }

    #[test]
    fn never_for_your_own_post_or_the_channel_you_are_reading() {
        assert!(!should_notify(&posted("me", "D", &[]), &me(), None, None));
        assert!(!should_notify(
            &posted("them", "D", &[]),
            &me(),
            None,
            Some("c1")
        ));
    }

    #[test]
    fn the_channel_setting_beats_the_account_setting() {
        let mut loud = me();
        loud.notify_props.insert("desktop".into(), "none".into());
        // The account says nothing, the channel says everything.
        assert!(should_notify(
            &posted("them", "O", &[]),
            &loud,
            Some(&member("all")),
            None
        ));

        let mut quiet = me();
        quiet.notify_props.insert("desktop".into(), "all".into());
        assert!(!should_notify(
            &posted("them", "O", &[]),
            &quiet,
            Some(&member("none")),
            None
        ));
    }

    #[test]
    fn a_channel_set_to_default_defers_to_the_account() {
        let mut loud = me();
        loud.notify_props.insert("desktop".into(), "all".into());
        assert!(should_notify(
            &posted("them", "O", &[]),
            &loud,
            Some(&member("default")),
            None
        ));
    }

    #[test]
    fn a_muted_channel_only_notifies_for_mentions() {
        let mut muted = ChannelMember::default();
        muted
            .notify_props
            .insert("mark_unread".into(), "mention".into());
        muted.notify_props.insert("desktop".into(), "all".into());

        // "Everything" in a muted channel still means "only me".
        assert!(!should_notify(
            &posted("them", "O", &[]),
            &me(),
            Some(&muted),
            None
        ));
        assert!(should_notify(
            &posted("them", "O", &["me"]),
            &me(),
            Some(&muted),
            None
        ));
    }

    #[test]
    fn the_body_is_text_without_markdown() {
        let mut post = posted("them", "O", &[]);
        post.post.message = "**Deploy** is _done_ & `green`, see [logs](https://x)".into();
        assert_eq!(body(&post), "Deploy is done &  green , see logs");
    }

    #[test]
    fn system_messages_never_notify() {
        let mut join = posted("them", "D", &["me"]);
        join.post.r#type = "system_join_channel".into();
        assert!(!should_notify(&join, &me(), None, None));
    }
}
