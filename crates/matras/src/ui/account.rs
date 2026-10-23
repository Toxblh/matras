//! Editing your own account: the profile, the custom status, and which server
//! you sign in to.
//!
//! Like [`super::dialogs`], nothing here talks to the server or to
//! [`crate::state`] — each function takes callbacks and hands back what the
//! user chose, so the API calls stay in [`super`] with the client.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// The clear-after choices, in menu order. The index into this table is what
/// [`expires_at`] reads; index 0 never expires.
/// What the other clients offer, with the timeout each one carries: emoji,
/// its shortcode, the wording, and an index into [`DURATIONS`].
const SUGGESTIONS: &[(&str, &str, &str, u32)] = &[
    ("📅", "calendar", "In a meeting", 2),
    ("🍔", "hamburger", "At lunch", 1),
    ("🤒", "face_with_thermometer", "Out sick", 4),
    ("🏠", "house", "Working from home", 4),
    ("🌴", "palm_tree", "On holiday", 5),
];

const DURATIONS: [&str; 6] = [
    "Don't clear",
    "30 minutes",
    "1 hour",
    "4 hours",
    "Today",
    "This week",
];

/// Edit your own profile. `current` is (first_name, last_name, nickname, position).
/// `on_save` gets the same four. `on_avatar` gets a chosen image file.
pub fn edit_profile(
    parent: &impl IsA<gtk::Widget>,
    current: (String, String, String, String),
    avatar: Option<gtk::gdk::Texture>,
    on_save: impl Fn(String, String, String, String) + 'static,
    on_avatar: impl Fn(PathBuf) + 'static,
) {
    let (first_name, last_name, nickname, position) = current;

    let picture = adw::Avatar::builder()
        .size(96)
        .show_initials(true)
        .text(format!("{first_name} {last_name}").trim())
        .halign(gtk::Align::Center)
        .build();
    if let Some(texture) = &avatar {
        picture.set_custom_image(Some(texture));
    }

    let change = gtk::Button::builder()
        .label("Change Picture…")
        .halign(gtk::Align::Center)
        .margin_top(12)
        .build();
    change.add_css_class("pill");

    let first = adw::EntryRow::builder()
        .title("First name")
        .text(&first_name)
        .build();
    let last = adw::EntryRow::builder()
        .title("Last name")
        .text(&last_name)
        .build();
    let nick = adw::EntryRow::builder()
        .title("Nickname")
        .text(&nickname)
        .build();
    let role = adw::EntryRow::builder()
        .title("Position")
        .text(&position)
        .build();

    let group = adw::PreferencesGroup::builder().margin_top(24).build();
    group.add(&first);
    group.add(&last);
    group.add(&nick);
    group.add(&role);

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .build();
    column.append(&picture);
    column.append(&change);
    column.append(&group);

    let (dialog, save) = super::dialogs::form("Edit Profile", "Save", &column);

    // The picker can be opened again after a mis-click, so the callback is
    // shared rather than moved into the first click.
    let on_avatar = Rc::new(on_avatar);
    change.connect_clicked({
        let picture = picture.clone();
        let on_avatar = on_avatar.clone();
        move |button| {
            let window = button.root().and_downcast::<gtk::Window>();
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Images"));
            for mime in ["image/png", "image/jpeg", "image/webp"] {
                filter.add_mime_type(mime);
            }
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);

            let chooser = gtk::FileDialog::builder()
                .title("Choose a Picture")
                .modal(true)
                .filters(&filters)
                .default_filter(&filter)
                .build();
            chooser.open(window.as_ref(), gtk::gio::Cancellable::NONE, {
                let picture = picture.clone();
                let on_avatar = on_avatar.clone();
                move |result| {
                    let Ok(file) = result else {
                        return; // Cancelled, which is not an error worth showing.
                    };
                    // Show it straight away: the upload and the reload of the
                    // server's copy both happen much later.
                    if let Ok(texture) = gtk::gdk::Texture::from_file(&file) {
                        picture.set_custom_image(Some(&texture));
                    }
                    if let Some(path) = file.path() {
                        on_avatar(path);
                    }
                }
            });
        }
    });

    save.connect_clicked({
        let dialog = dialog.clone();
        let first = first.clone();
        move |_| {
            dialog.close();
            on_save(
                first.text().trim().to_string(),
                last.text().trim().to_string(),
                nick.text().trim().to_string(),
                role.text().trim().to_string(),
            );
        }
    });

    dialog.present(Some(parent));
    first.grab_focus();
}

/// Set a custom status. `current` is (emoji_shortcode, text).
/// `on_set` gets (emoji, text, expires_at_unix_millis or 0 for never).
/// `on_clear` clears it.
pub fn custom_status(
    parent: &impl IsA<gtk::Widget>,
    current: (String, String),
    recents: Vec<(String, String)>,
    on_set: impl Fn(String, String, i64) + 'static,
    on_clear: impl Fn() + 'static,
) {
    let (current_emoji, current_text) = current;
    let is_set = !current_emoji.is_empty() || !current_text.is_empty();

    let emoji = adw::EntryRow::builder()
        .title("Emoji")
        .text(&current_emoji)
        .build();
    let text = adw::EntryRow::builder()
        .title("Message")
        .text(&current_text)
        .build();
    let duration = adw::ComboRow::builder().title("Clear after").build();
    duration.set_model(Some(&gtk::StringList::new(&DURATIONS)));

    let group = adw::PreferencesGroup::builder()
        .description("The emoji is a shortcode without colons, like coffee.")
        .build();
    group.add(&emoji);
    group.add(&text);
    group.add(&duration);

    // The ones people actually pick, with the timeout each one implies —
    // "in a meeting" is an hour, "on holiday" is a week. Typing that out every
    // time is the reason nobody sets a status.
    let suggestions = adw::PreferencesGroup::builder()
        .title("Suggestions")
        .build();
    for (glyph, shortcode, label, choice) in SUGGESTIONS {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(label).as_str())
            .subtitle(DURATIONS[*choice as usize])
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Label::new(Some(glyph)));
        row.connect_activated({
            let emoji = emoji.clone();
            let text = text.clone();
            let duration = duration.clone();
            let shortcode = shortcode.to_string();
            let label = label.to_string();
            let choice = *choice;
            // Filling the fields rather than submitting: the wording is often
            // nearly right, and editing it beats retyping it.
            move |_| {
                emoji.set_text(&shortcode);
                text.set_text(&label);
                duration.set_selected(choice);
            }
        });
        suggestions.add(&row);
    }

    let recents_group = adw::PreferencesGroup::builder().title("Recent").build();
    for (recent_emoji, recent_text) in &recents {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(recent_text).as_str())
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Label::new(Some(&crate::emoji::label(recent_emoji))));
        row.connect_activated({
            let emoji = emoji.clone();
            let text = text.clone();
            let recent_emoji = recent_emoji.clone();
            let recent_text = recent_text.clone();
            move |_| {
                emoji.set_text(&recent_emoji);
                text.set_text(&recent_text);
            }
        });
        recents_group.add(&row);
    }

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    content.append(&group);
    if !recents.is_empty() {
        content.append(&recents_group);
    }
    content.append(&suggestions);

    let (dialog, set) = super::dialogs::form("Set a Status", "Set", &content);

    // Nothing to clear until there is a status, so the button is not offered.
    // Clearing is no loss — the status is retyped in a second, and it is in
    // Recent anyway — so it is an ordinary button, not a destructive one.
    if is_set {
        let clear = gtk::Button::builder()
            .label("Clear Status")
            .halign(gtk::Align::Center)
            .margin_top(12)
            .build();
        clear.add_css_class("pill");
        clear.connect_clicked({
            let dialog = dialog.clone();
            move |_| {
                dialog.close();
                on_clear();
            }
        });
        content.append(&clear);
    }

    // A status is an emoji, some words, or both; neither is nothing to set.
    let refresh = {
        let set = set.clone();
        let emoji = emoji.clone();
        let text = text.clone();
        move || set.set_sensitive(!emoji.text().trim().is_empty() || !text.text().trim().is_empty())
    };
    refresh();
    emoji.connect_changed({
        let refresh = refresh.clone();
        move |_| refresh()
    });
    text.connect_changed(move |_| refresh());

    set.connect_clicked({
        let dialog = dialog.clone();
        let emoji = emoji.clone();
        move |_| {
            dialog.close();
            let base = glib::DateTime::now_local().ok();
            let expires = base.map_or(0, |now| expires_at(&now, duration.selected()));
            on_set(
                emoji.text().trim().trim_matches(':').to_string(),
                text.text().trim().to_string(),
                expires,
            );
        }
    });
    dialog.present(Some(parent));
    emoji.grab_focus();
}

/// Pick a server to sign in to, or add another. `servers` is (url, label).
/// `on_pick` selects an existing one; `on_add` starts a new sign-in.
pub fn choose_server(
    parent: &impl IsA<gtk::Widget>,
    servers: Vec<(String, String)>,
    on_pick: impl Fn(String) + 'static,
    on_add: impl Fn() + 'static,
) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");

    let dialog = super::dialogs::list_dialog("Choose a Server", None, &list);
    dialog.set_content_height(420);

    // Every way out goes through `closed`, so dismissing the dialog (Escape,
    // the close button, a swipe on a phone) still lands somewhere: without a
    // server picked, that is the sign-in form, not a spinner forever.
    let picked: Rc<RefCell<Option<String>>> = Rc::default();
    dialog.connect_closed({
        let picked = picked.clone();
        move |_| match picked.borrow_mut().take() {
            Some(url) => on_pick(url),
            None => on_add(),
        }
    });

    for (url, label) in servers {
        // AdwActionRow reads its title as Pango markup, and both of these are
        // free text the user typed at sign-in.
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&label))
            .subtitle(glib::markup_escape_text(&url))
            .activatable(true)
            .build();
        row.connect_activated({
            let picked = picked.clone();
            let dialog = dialog.downgrade();
            move |_| {
                *picked.borrow_mut() = Some(url.clone());
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
            }
        });
        list.append(&row);
    }

    let add = adw::ActionRow::builder()
        .title("Add Server…")
        .activatable(true)
        .build();
    add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
    add.connect_activated({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });
    list.append(&add);

    dialog.present(Some(parent));
}

/// When the status picked at `base` should expire, in Unix milliseconds.
/// `choice` indexes [`DURATIONS`]; 0 — and anything glib refuses to compute —
/// means "never", which is the answer that loses the least.
fn expires_at(base: &glib::DateTime, choice: u32) -> i64 {
    let time = match choice {
        1 => base.add_minutes(30).ok(),
        2 => base.add_hours(1).ok(),
        3 => base.add_hours(4).ok(),
        4 => end_of_day(base),
        // glib numbers the days 1 = Monday … 7 = Sunday, so the week ends
        // 7 - today days from now.
        5 => base
            .add_days(7 - base.day_of_week())
            .ok()
            .and_then(|day| end_of_day(&day)),
        _ => None,
    };
    time.map_or(0, |time| time.to_unix() * 1000)
}

/// One second before midnight on `day`, in its own local time.
fn end_of_day(day: &glib::DateTime) -> Option<glib::DateTime> {
    glib::DateTime::from_local(day.year(), day.month(), day.day_of_month(), 23, 59, 59.0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> glib::DateTime {
        glib::DateTime::from_local(2026, 9, 2, 10, 30, 0.0).unwrap()
    }

    #[test]
    fn never_is_zero() {
        assert_eq!(expires_at(&base(), 0), 0);
        // An index past the table is a bug elsewhere; it must not expire early.
        assert_eq!(expires_at(&base(), 99), 0);
    }

    #[test]
    fn offsets_are_added_to_the_base() {
        let start = base().to_unix() * 1000;
        assert_eq!(expires_at(&base(), 1) - start, 30 * 60 * 1000);
        assert_eq!(expires_at(&base(), 2) - start, 60 * 60 * 1000);
        assert_eq!(expires_at(&base(), 3) - start, 4 * 60 * 60 * 1000);
    }

    #[test]
    fn today_ends_tonight() {
        let end = glib::DateTime::from_unix_local(expires_at(&base(), 4) / 1000).unwrap();
        assert_eq!((end.year(), end.month(), end.day_of_month()), (2026, 9, 2));
        assert_eq!((end.hour(), end.minute()), (23, 59));
    }

    #[test]
    fn the_week_ends_on_sunday_night() {
        let end = glib::DateTime::from_unix_local(expires_at(&base(), 5) / 1000).unwrap();
        assert_eq!(end.day_of_week(), 7);
        assert_eq!((end.hour(), end.minute()), (23, 59));
        // Always ahead of "Today", never behind it.
        assert!(expires_at(&base(), 5) >= expires_at(&base(), 4));
    }
}
