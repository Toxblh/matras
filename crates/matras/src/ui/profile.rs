//! The profile card, the way Mattermost shows it when you click a name.

use adw::prelude::*;
use gtk::glib;
use mattermost_api::models::{Presence, User};

use crate::avatars::Avatars;
use crate::emoji;
use crate::state::SharedState;

/// Builds a profile popover for `user_id` and points it at `anchor`.
///
/// Returns `None` when we do not have the user cached — the caller should have
/// fetched them; every path that shows a name already has.
pub fn popover(
    user_id: &str,
    state: &SharedState,
    avatars: &Avatars,
    anchor: &gtk::Widget,
    on_message: impl Fn(String) + 'static,
) -> Option<gtk::Popover> {
    let st = state.borrow();
    let user = st.users.get(user_id)?.clone();
    let display = st.display_name(&user);
    let presence = st.presence(user_id);
    let is_me = user.id == st.me.id;
    // Read before the borrow closes; the card is built afterwards.
    let last_seen = st.last_seen.get(user_id).copied().filter(|at| *at > 0);
    drop(st);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .width_request(260)
        .build();

    let avatar = adw::Avatar::builder()
        .size(72)
        .halign(gtk::Align::Center)
        .build();
    avatars.apply(&avatar, &user.id, &display);
    column.append(&avatar);

    let name = gtk::Label::builder()
        .label(&display)
        .halign(gtk::Align::Center)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .margin_top(10)
        .build();
    name.add_css_class("title-3");
    column.append(&name);

    let handle = gtk::Label::builder()
        .label(format!("@{}", user.username))
        .halign(gtk::Align::Center)
        .build();
    handle.add_css_class("dim-label");
    column.append(&handle);

    // Presence, as a coloured dot plus a word — colour alone would not survive
    // a high-contrast theme or a colour-blind reader.
    let presence_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Center)
        .margin_top(8)
        .build();
    let dot = gtk::Label::new(Some("●"));
    dot.add_css_class(presence_class(presence));
    dot.add_css_class("presence-dot");
    presence_row.append(&dot);
    presence_row.append(&gtk::Label::new(Some(presence_label(presence))));
    column.append(&presence_row);

    // When somebody was last seen. Only for people who are not online now —
    // "last seen 26 August" under a green dot would be nonsense.
    if presence != Presence::Online {
        if let Some(seen) = last_seen {
            let label = gtk::Label::builder()
                .label(format!("Last seen {}", super::message::format_day(seen)))
                .halign(gtk::Align::Center)
                .build();
            label.add_css_class("caption");
            label.add_css_class("dim-label");
            column.append(&label);
        }
    }

    if let Some(status) = user.custom_status() {
        if !status.text.is_empty() || !status.emoji.is_empty() {
            let label = gtk::Label::builder()
                // The card has room, so the expiry goes in the text rather
                // than hiding in a tooltip on a tooltip.
                .label(
                    format!(
                        "{} {}",
                        emoji::label(&status.emoji),
                        super::message::custom_status_tooltip(&status)
                    )
                    .trim(),
                )
                .halign(gtk::Align::Center)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .margin_top(6)
                .build();
            column.append(&label);
        }
    }

    if !user.position.is_empty() {
        let position = gtk::Label::builder()
            .label(&user.position)
            .halign(gtk::Align::Center)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(6)
            .build();
        position.add_css_class("dim-label");
        column.append(&position);
    }

    if let Some(local) = local_time(&user) {
        let row = detail_row("Local time", &local);
        row.set_margin_top(12);
        column.append(&row);
    }
    if !user.email.is_empty() {
        column.append(&detail_row("Email", &user.email));
    }

    if !is_me {
        let message = gtk::Button::builder()
            .label("Send message")
            .margin_top(14)
            .build();
        message.add_css_class("suggested-action");
        message.add_css_class("pill");
        message.connect_clicked({
            let id = user.id.clone();
            move |_| on_message(id.clone())
        });
        column.append(&message);
    }

    let popover = gtk::Popover::builder().child(&column).build();
    popover.set_parent(anchor);

    // A popover is a child of what it points at, and nothing takes it back
    // off: click an avatar ten times and ten of them hang off that button,
    // and a row rebuilt while one is open is finalized with a child still
    // attached — which is what GTK was complaining about on every refresh.
    // Unparenting from the close handler itself would run inside GTK's own
    // emission, so it waits a tick.
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    anchor.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    Some(popover)
}

fn detail_row(label: &str, value: &str) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let key = gtk::Label::builder().label(label).xalign(0.0).build();
    key.add_css_class("dim-label");
    key.add_css_class("caption");
    let val = gtk::Label::builder()
        .label(value)
        .xalign(1.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    val.add_css_class("caption");
    row.append(&key);
    row.append(&val);
    row
}

/// The user's wall-clock time, which is the detail that actually changes how
/// you treat a message.
///
/// `timezone` is a `map[string]string`: `useAutomaticTimezone` is the *string*
/// `"true"`/`"false"`, and the effective zone is whichever of the two fields
/// that flag selects.
fn local_time(user: &User) -> Option<String> {
    let automatic = user
        .timezone
        .get("useAutomaticTimezone")
        .map(String::as_str)
        == Some("true");
    let zone = if automatic {
        user.timezone.get("automaticTimezone")
    } else {
        user.timezone.get("manualTimezone")
    }?;
    if zone.is_empty() {
        return None;
    }
    // `TimeZone::from_identifier`, which reports a bad identifier, sits behind
    // glib's `v2_68` feature and the GTK build here does not enable it.
    // `new` falls back to UTC for anything it cannot parse, which is a
    // reasonable answer for a field the server may not have filled in.
    let tz = glib::TimeZone::new(Some(zone));
    let now = glib::DateTime::now(&tz).ok()?;
    let time = now.format("%H:%M").ok()?;
    Some(time.to_string())
}

/// The avatar with Mattermost's presence badge pinned to its corner.
pub fn with_presence(avatar: &adw::Avatar, presence: Presence) -> gtk::Overlay {
    // Scaled off the avatar so the same helper works for a 40px message
    // avatar and a 20px sidebar one.
    let size = presence_badge_size(avatar.size());
    let dot = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        // Colour alone is not a label: the word has to be reachable somehow,
        // and on a 12px dot a tooltip is the only place it fits.
        .tooltip_text(presence_label(presence))
        .build();
    dot.add_css_class("presence-badge");
    dot.add_css_class(presence_class(presence));

    let overlay = gtk::Overlay::builder().child(avatar).build();
    overlay.add_overlay(&dot);
    overlay
}

pub(super) fn presence_badge_size(avatar_size: i32) -> i32 {
    (avatar_size / 3).clamp(8, 14)
}

fn presence_label(presence: Presence) -> &'static str {
    match presence {
        Presence::Online => "Online",
        Presence::Away => "Away",
        Presence::Dnd => "Do not disturb",
        Presence::OutOfOffice => "Out of office",
        Presence::Offline => "Offline",
    }
}

pub fn presence_class(presence: Presence) -> &'static str {
    match presence {
        Presence::Online => "presence-online",
        Presence::Away => "presence-away",
        Presence::Dnd => "presence-dnd",
        _ => "presence-offline",
    }
}
