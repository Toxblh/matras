//! The dialogs that ask a question and hand the answer back.
//!
//! Nothing here talks to the server or to [`crate::state`]: each function takes
//! a callback and calls it with what the user chose. That keeps the API calls
//! in [`super`], where the client and the action loop already live, and it
//! means these can be read (and moved) without tracing a request through them.
//!
//! Three shapes, as the GNOME HIG has them (libadwaita 1.5):
//! - a question about something that cannot be undone is an
//!   [`adw::AlertDialog`] — see [`confirm`];
//! - a form is an [`adw::Dialog`] with Cancel and a verb in its header — see
//!   [`form`];
//! - a list to browse is an [`adw::Dialog`] with a plain header — see
//!   [`list_dialog`].
//!
//! On a phone all three become bottom sheets, and Escape closes them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// Desktop notification levels for a single channel, in menu order. The ids
/// are Mattermost's own; `"default"` means "whatever the account says".
const CHANNEL_LEVELS: [(&str, &str); 4] = [
    ("default", "Global default"),
    ("all", "All new messages"),
    ("mention", "Mentions only"),
    ("none", "Nothing"),
];

/// The same list for the account, which has nothing to fall back to.
const ACCOUNT_LEVELS: [(&str, &str); 3] = [
    ("all", "All new messages"),
    ("mention", "Mentions only"),
    ("none", "Nothing"),
];

/// Asks before something that cannot be taken back. `heading` is a question
/// ("Delete Message?"), `body` one sentence of consequence, `verb` the button.
///
/// There is deliberately no default response: Enter must not be the key that
/// deletes something.
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    verb: &str,
    on_confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("confirm", verb)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

/// A dialog that asks for something: Cancel at the start of the header, the
/// suggested `verb` at the end, `content` scrolling underneath. The caller
/// wires the verb button (and closes the dialog from it).
pub fn form(
    title: &str,
    verb: &str,
    content: &impl IsA<gtk::Widget>,
) -> (adw::Dialog, gtk::Button) {
    let cancel = gtk::Button::with_label("Cancel");
    let submit = gtk::Button::with_label(verb);
    submit.add_css_class("suggested-action");

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&submit);

    let clamp = adw::Clamp::builder()
        .maximum_size(460)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(24)
        .child(content)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&clamp)
            .build(),
    ));

    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(420)
        .child(&view)
        .build();
    // Enter in a field submits, as it would in any form.
    dialog.set_default_widget(Some(&submit));
    cancel.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });
    (dialog, submit)
}

/// Create a channel. Answers (display_name, url_name, purpose, private).
pub fn create_channel(
    parent: &impl IsA<gtk::Widget>,
    on_create: impl Fn(String, String, String, bool) + 'static,
) {
    let name = adw::EntryRow::builder()
        .title("Name")
        .activates_default(true)
        .build();
    let url = adw::EntryRow::builder().title("URL name").build();
    let purpose = adw::EntryRow::builder().title("Purpose (optional)").build();
    let private = adw::SwitchRow::builder()
        .title("Private channel")
        .subtitle("Only invited people can find it")
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&name);
    group.add(&url);
    group.add(&purpose);
    group.add(&private);

    let (dialog, create) = form("New Channel", "Create", &group);
    // A channel with no name is the one mistake worth blocking outright.
    create.set_sensitive(false);

    // The URL follows the name until someone edits it themselves; from then on
    // it is theirs, and we notice by remembering what we last wrote there.
    let autofilled = Rc::new(RefCell::new(String::new()));
    name.connect_changed({
        let url = url.clone();
        let create = create.clone();
        let autofilled = autofilled.clone();
        move |name| {
            let text = name.text();
            create.set_sensitive(!text.trim().is_empty());
            let ours = url.text().as_str() == autofilled.borrow().as_str();
            if ours {
                let slug = slugify(text.trim());
                url.set_text(&slug);
                *autofilled.borrow_mut() = slug;
            }
        }
    });

    create.connect_clicked({
        let dialog = dialog.clone();
        let name = name.clone();
        move |_| {
            dialog.close();
            let display_name = name.text().trim().to_string();
            let mut url_name = url.text().trim().to_string();
            // Emptied by hand, or a name with nothing ASCII in it to slug.
            if url_name.is_empty() {
                url_name = slugify(&display_name);
            }
            on_create(
                display_name,
                url_name,
                purpose.text().trim().to_string(),
                private.is_active(),
            );
        }
    });
    dialog.present(Some(parent));
    name.grab_focus();
}

/// Browse and join channels.
///
/// The dialog outlives this call, so the handle keeps the list around: the
/// caller searches the server on [`Self::present`]'s `on_search` and pours the
/// answer back in through [`Self::set_results`].
pub struct ChannelBrowser {
    list: gtk::ListBox,
    /// The box these results answer. A search is a round trip, and a narrow
    /// term can come back before the broad one typed before it — without
    /// this, the broad answer lands last and replaces the filtered list.
    search: gtk::SearchEntry,
    on_join: Rc<dyn Fn(String)>,
}

impl ChannelBrowser {
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        on_search: impl Fn(String) + 'static,
        on_join: impl Fn(String) + 'static,
    ) -> Self {
        // Per keystroke, not per activate: GtkSearchEntry already holds the
        // signal back until typing pauses, and the caller sees a whole term.
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search channels")
            .hexpand(true)
            .build();
        let on_search = Rc::new(on_search);
        search.connect_search_changed({
            let on_search = on_search.clone();
            move |entry| on_search(entry.text().trim().to_string())
        });

        let list = loading_list();
        let dialog = list_dialog("Browse Channels", Some(search.upcast_ref()), &list);
        dialog.present(Some(parent));
        search.grab_focus();
        // Ask once on open: a browser that is empty until you type is a search
        // box, and browsing is the point.
        on_search(String::new());

        ChannelBrowser {
            list,
            search,
            on_join: Rc::new(on_join),
        }
    }

    pub fn set_results(&self, term: &str, channels: Vec<(String, String, String, bool)>) {
        if self.search.text().trim() != term.trim() {
            return; // Answered a question nobody is asking any more.
        }
        reset_list(&self.list, "system-search-symbolic", "No Channels Found");

        for (id, display_name, purpose, already_member) in channels {
            // AdwActionRow reads its title as Pango markup, and a channel name
            // is free text — an ampersand in one would otherwise blank the row.
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name))
                .build();
            if !purpose.is_empty() {
                row.set_subtitle(&glib::markup_escape_text(&purpose));
                row.set_subtitle_lines(2);
            }
            // The dialog stays open so you can join several at once; the
            // button acknowledges the click itself, because the caller's
            // answer arrives over the network much later.
            row.add_suffix(&join_button(already_member, &id, &self.on_join));
            self.list.append(&row);
        }
    }
}

/// "Join", or a dim "Joined" where there is nothing left to do.
fn join_button(already_member: bool, id: &str, on_join: &Rc<dyn Fn(String)>) -> gtk::Widget {
    if already_member {
        let joined = gtk::Label::new(Some("Joined"));
        joined.add_css_class("dim-label");
        return joined.upcast();
    }
    let button = gtk::Button::builder()
        .label("Join")
        .valign(gtk::Align::Center)
        .build();
    button.add_css_class("suggested-action");
    button.connect_clicked({
        let on_join = on_join.clone();
        let id = id.to_string();
        move |button| {
            button.set_sensitive(false);
            button.set_label("Joined");
            on_join(id.clone());
        }
    });
    button.upcast()
}

/// Per-channel notification settings. `current` is (desktop_level,
/// mark_unread_all, ignore_channel_mentions) where desktop_level is one of
/// "default"|"all"|"mention"|"none". Answers the same triple.
pub fn channel_notifications(
    parent: &impl IsA<gtk::Widget>,
    channel_name: &str,
    current: (String, bool, bool),
    on_save: impl Fn(String, bool, bool) + 'static,
) {
    let (level, mark_unread_all, ignore_channel_mentions) = current;

    let desktop = level_row("Desktop notifications", &CHANNEL_LEVELS, &level);
    let unread = adw::SwitchRow::builder()
        .title("Mark as unread")
        .subtitle("For every message, not only mentions")
        .active(mark_unread_all)
        .build();
    let ignore = adw::SwitchRow::builder()
        .title("Ignore @channel, @here and @all")
        .active(ignore_channel_mentions)
        .build();

    let group = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(channel_name))
        .build();
    group.add(&desktop);
    group.add(&unread);
    group.add(&ignore);

    let (dialog, save) = form("Notifications", "Save", &group);
    save.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
            on_save(
                selected_level(&desktop, &CHANNEL_LEVELS),
                unread.is_active(),
                ignore.is_active(),
            );
        }
    });
    dialog.present(Some(parent));
}

/// Puts a row back the way it was after the server refused a change.
pub type Revert = Box<dyn FnOnce()>;

/// Account-wide notification settings for the preferences dialog: desktop
/// level, sound on/off, mention keywords (comma separated), and whether first
/// name counts. `current` is those four.
///
/// Each row applies on its own: `on_change` gets the `notify_props` key, its
/// new value, and a [`Revert`] to call if saving it fails.
pub fn account_notifications(
    current: (String, bool, String, bool),
    on_change: impl Fn(&'static str, String, Revert) + 'static,
) -> adw::PreferencesGroup {
    let (level, sound, keywords, first_name) = current;
    let on_change: Rc<dyn Fn(&'static str, String, Revert)> = Rc::new(on_change);
    // Set while a refused change is being put back, so that undoing it is not
    // itself sent to the server as a change.
    let reverting = Rc::new(Cell::new(false));

    let desktop = level_row("Desktop notifications", &ACCOUNT_LEVELS, &level);
    let shown = Rc::new(Cell::new(desktop.selected()));
    desktop.connect_selected_notify({
        let on_change = on_change.clone();
        let reverting = reverting.clone();
        move |row| {
            if reverting.get() {
                return;
            }
            let old = shown.replace(row.selected());
            let revert: Revert = Box::new({
                let row = row.clone();
                let reverting = reverting.clone();
                let shown = shown.clone();
                move || {
                    reverting.set(true);
                    shown.set(old);
                    row.set_selected(old);
                    reverting.set(false);
                }
            });
            on_change("desktop", selected_level(row, &ACCOUNT_LEVELS), revert);
        }
    });

    let sound_row = adw::SwitchRow::builder()
        .title("Notification sound")
        .active(sound)
        .build();
    let first_name_row = adw::SwitchRow::builder()
        .title("My first name")
        .subtitle("Notify me when someone types it")
        .active(first_name)
        .build();
    for (row, key) in [
        (&sound_row, "desktop_sound"),
        (&first_name_row, "first_name"),
    ] {
        row.connect_active_notify({
            let on_change = on_change.clone();
            let reverting = reverting.clone();
            move |row| {
                if reverting.get() {
                    return;
                }
                let old = !row.is_active();
                let revert: Revert = Box::new({
                    let row = row.clone();
                    let reverting = reverting.clone();
                    move || {
                        reverting.set(true);
                        row.set_active(old);
                        reverting.set(false);
                    }
                });
                on_change(key, row.is_active().to_string(), revert);
            }
        });
    }

    // Typing is not deciding, so the keywords wait for the row's own apply
    // button (or Enter) rather than saving per keystroke.
    let keywords_row = adw::EntryRow::builder()
        .title("Keywords that mention me")
        .text(&keywords)
        .show_apply_button(true)
        .build();
    let saved = Rc::new(RefCell::new(keywords));
    keywords_row.connect_apply({
        let on_change = on_change.clone();
        move |row| {
            let value = row.text().trim().to_string();
            let old = saved.replace(value.clone());
            let revert: Revert = Box::new({
                let row = row.clone();
                let saved = saved.clone();
                move || {
                    row.set_text(&old);
                    *saved.borrow_mut() = old;
                }
            });
            on_change("mention_keys", value, revert);
        }
    });

    let group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .description("Saved to your account on the server. Keywords are separated by commas and ignore case.")
        .build();
    group.add(&desktop);
    group.add(&sound_row);
    group.add(&keywords_row);
    group.add(&first_name_row);
    group
}

/// Confirm leaving a channel.
pub fn confirm_leave(
    parent: &impl IsA<gtk::Widget>,
    channel_name: &str,
    on_leave: impl Fn() + 'static,
) {
    confirm(
        parent,
        &format!("Leave {channel_name}?"),
        "Its messages will stop arriving until you join again.",
        "Leave",
        on_leave,
    );
}

/// Confirm leaving a team, which takes every channel in it along.
pub fn confirm_leave_team(
    parent: &impl IsA<gtk::Widget>,
    team_name: &str,
    on_leave: impl Fn() + 'static,
) {
    confirm(
        parent,
        &format!("Leave {team_name}?"),
        "All of its channels will disappear until someone adds you back.",
        "Leave",
        on_leave,
    );
}

/// Schedule a message: answers a Unix millisecond timestamp.
pub fn schedule_message(parent: &impl IsA<gtk::Widget>, on_schedule: impl Fn(i64) + 'static) {
    let on_schedule = Rc::new(on_schedule);

    // The usual times are one tap each; the calendar below is for the rest.
    let presets = adw::PreferencesGroup::new();
    let calendar = gtk::Calendar::new();

    let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let minute = gtk::SpinButton::with_range(0.0, 59.0, 5.0);
    hour.set_value(9.0);
    minute.set_value(0.0);
    // Wrapping so 23 rolls to 00 rather than stopping dead at the end.
    hour.set_wrap(true);
    minute.set_wrap(true);
    hour.update_property(&[gtk::accessible::Property::Label("Hour")]);
    minute.update_property(&[gtk::accessible::Property::Label("Minute")]);

    let time = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .halign(gtk::Align::Center)
        .margin_top(12)
        .build();
    time.append(&hour);
    time.append(&gtk::Label::new(Some(":")));
    time.append(&minute);

    let custom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(18)
        .build();
    custom.append(&calendar);
    custom.append(&time);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&presets);
    content.append(&custom);

    let (dialog, schedule) = form("Schedule Message", "Schedule", &content);

    for (label, when) in [
        ("Tomorrow Morning", morning_in as fn(i32) -> Option<i64>),
        ("Monday Morning", |_| next_monday_morning()),
    ] {
        let row = adw::ActionRow::builder()
            .title(label)
            .subtitle("9:00")
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated({
            let dialog = dialog.clone();
            let on_schedule = on_schedule.clone();
            move |_| {
                dialog.close();
                if let Some(millis) = when(1) {
                    on_schedule(millis);
                }
            }
        });
        presets.add(&row);
    }

    schedule.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
            let millis = glib::DateTime::from_local(
                calendar.year(),
                // GtkCalendar counts months from zero, GDateTime from one.
                calendar.month() + 1,
                calendar.day(),
                hour.value_as_int(),
                minute.value_as_int(),
                0.0,
            )
            .ok()
            .map(unix_millis);
            if let Some(millis) = millis {
                on_schedule(millis);
            }
        }
    });
    dialog.present(Some(parent));
}

/// Set a reminder about a post: answers a Unix millisecond timestamp.
pub fn post_reminder(parent: &impl IsA<gtk::Widget>, on_remind: impl Fn(i64) + 'static) {
    let dialog = adw::AlertDialog::new(
        Some("Set a Reminder"),
        Some("The system bot will send you the message again."),
    );
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("30m", "In 30 Minutes"),
        ("1h", "In 1 Hour"),
        ("2h", "In 2 Hours"),
        ("tomorrow", "Tomorrow Morning"),
    ]);
    dialog.set_default_response(Some("tomorrow"));
    dialog.set_close_response("cancel");

    dialog.connect_response(None, move |_, response| {
        let millis = match response {
            "30m" => from_now(|now| now.add_minutes(30)),
            "1h" => from_now(|now| now.add_hours(1)),
            "2h" => from_now(|now| now.add_hours(2)),
            "tomorrow" => morning_in(1),
            _ => None,
        };
        if let Some(millis) = millis {
            on_remind(millis);
        }
    });
    dialog.present(Some(parent));
}

/// A combo row over one of the level tables, showing the labels and answering
/// with the ids.
fn level_row(title: &str, levels: &[(&str, &str)], current: &str) -> adw::ComboRow {
    let labels: Vec<&str> = levels.iter().map(|(_, label)| *label).collect();
    let row = adw::ComboRow::builder().title(title).build();
    row.set_model(Some(&gtk::StringList::new(&labels)));
    row.set_selected(level_index(levels, current));
    row
}

fn selected_level(row: &adw::ComboRow, levels: &[(&str, &str)]) -> String {
    levels
        .get(row.selected() as usize)
        .map_or(levels[0].0, |(id, _)| *id)
        .to_string()
}

/// Where `value` sits in `levels`, falling back to the first entry: the server
/// can send a level we do not offer, and there is nothing better to show.
fn level_index(levels: &[(&str, &str)], value: &str) -> u32 {
    levels.iter().position(|(id, _)| *id == value).unwrap_or(0) as u32
}

fn unix_millis(time: glib::DateTime) -> i64 {
    time.to_unix() * 1000
}

/// `offset` applied to now, in Unix milliseconds. Every glib date operation
/// can fail, and a reminder we cannot place is simply not set.
fn from_now(
    offset: impl Fn(&glib::DateTime) -> Result<glib::DateTime, glib::BoolError>,
) -> Option<i64> {
    let now = glib::DateTime::now_local().ok()?;
    offset(&now).ok().map(unix_millis)
}

/// 9am local time, `days` days from today.
fn morning_in(days: i32) -> Option<i64> {
    let day = glib::DateTime::now_local().ok()?.add_days(days).ok()?;
    glib::DateTime::from_local(day.year(), day.month(), day.day_of_month(), 9, 0, 0.0)
        .ok()
        .map(unix_millis)
}

fn next_monday_morning() -> Option<i64> {
    let today = glib::DateTime::now_local().ok()?;
    morning_in(days_until_next_monday(today.day_of_week()))
}

/// Days from `day_of_week` (glib numbers them 1 = Monday … 7 = Sunday) to the
/// next Monday. Always in the future: asked on a Monday it means the next one,
/// because "Monday morning" is never the morning you are already in.
fn days_until_next_monday(day_of_week: i32) -> i32 {
    ((7 - day_of_week) % 7) + 1
}

/// Mattermost channel URLs take lowercase letters, digits, dashes and
/// underscores, so everything else becomes a separator.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Who is in a channel, with a search for adding more.
///
/// Like [`ChannelBrowser`] the dialog outlives this call: the caller answers
/// `on_search` through [`Self::set_candidates`], and refills the roster with
/// [`Self::set_members`] once the server has confirmed an add or a removal.
pub struct MemberList {
    members: gtk::ListBox,
    candidates: gtk::ListBox,
    /// See `ChannelBrowser::search`: the term these candidates answer.
    search: gtk::SearchEntry,
    on_add: Rc<dyn Fn(String)>,
    on_remove: Rc<dyn Fn(String)>,
}

impl MemberList {
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        channel_name: &str,
        on_search: impl Fn(String) + 'static,
        on_add: impl Fn(String) + 'static,
        on_remove: impl Fn(String) + 'static,
    ) -> Self {
        let members = loading_list();
        let members_group = adw::PreferencesGroup::builder().title("Members").build();
        members_group.add(&members);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search people")
            .build();
        let on_search = Rc::new(on_search);
        search.connect_search_changed({
            let on_search = on_search.clone();
            move |entry| on_search(entry.text().trim().to_string())
        });
        // Ask once on open, as the channel browser does: the people you want to
        // add are usually the ones the server would have listed anyway.
        on_search(String::new());

        let candidates = loading_list();
        let add_group = adw::PreferencesGroup::builder().title("Add People").build();
        add_group.add(&search);
        add_group.add(&candidates);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        content.append(&members_group);
        content.append(&add_group);

        let title = adw::WindowTitle::new("Members", channel_name);
        list_dialog("Members", Some(title.upcast_ref()), &content).present(Some(parent));

        MemberList {
            members,
            candidates,
            search,
            on_add: Rc::new(on_add),
            on_remove: Rc::new(on_remove),
        }
    }

    /// `members` is (user_id, display_name, @username, is_admin).
    pub fn set_members(&self, members: Vec<(String, String, String, bool)>) {
        reset_list(&self.members, "system-users-symbolic", "No Members");

        for (id, display_name, username, is_admin) in members {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name))
                .subtitle(glib::markup_escape_text(&username))
                .build();

            if is_admin {
                // Removing an admin needs permissions we cannot check from
                // here, so the row explains itself instead of offering a
                // button the server would refuse.
                let admin = gtk::Label::new(Some("Admin"));
                admin.add_css_class("dim-label");
                row.add_suffix(&admin);
            } else {
                let button = gtk::Button::builder()
                    .icon_name("list-remove-symbolic")
                    .tooltip_text("Remove From Channel")
                    .valign(gtk::Align::Center)
                    .build();
                button.add_css_class("flat");
                button.connect_clicked({
                    let on_remove = self.on_remove.clone();
                    // The row stays until the caller sends the roster back; the
                    // button only stops a second click on the way there.
                    move |button| {
                        button.set_sensitive(false);
                        on_remove(id.clone());
                    }
                });
                row.add_suffix(&button);
            }

            self.members.append(&row);
        }
    }

    /// `users` is (user_id, display_name, @username).
    pub fn set_candidates(&self, term: &str, users: Vec<(String, String, String)>) {
        if self.search.text().trim() != term.trim() {
            return; // Answered a question nobody is asking any more.
        }
        reset_list(
            &self.candidates,
            "system-search-symbolic",
            "No Matching People",
        );

        for (id, display_name, username) in users {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name))
                .subtitle(glib::markup_escape_text(&username))
                .build();

            let button = gtk::Button::builder()
                .label("Add")
                .valign(gtk::Align::Center)
                .build();
            button.add_css_class("suggested-action");
            button.connect_clicked({
                let on_add = self.on_add.clone();
                move |button| {
                    button.set_sensitive(false);
                    button.set_label("Added");
                    on_add(id.clone());
                }
            });
            row.add_suffix(&button);

            self.candidates.append(&row);
        }
    }
}

/// Teams you could join, filled in by [`Self::set_teams`].
///
/// There is no search: an account sees the teams it is allowed to join and
/// that list is short enough to read.
pub struct TeamBrowser {
    list: gtk::ListBox,
    on_join: Rc<dyn Fn(String)>,
}

impl TeamBrowser {
    pub fn present(parent: &impl IsA<gtk::Widget>, on_join: impl Fn(String) + 'static) -> Self {
        let list = loading_list();
        list_dialog("Browse Teams", None, &list).present(Some(parent));

        TeamBrowser {
            list,
            on_join: Rc::new(on_join),
        }
    }

    /// `teams` is (id, display_name, description, already_member).
    pub fn set_teams(&self, teams: Vec<(String, String, String, bool)>) {
        reset_list(&self.list, "network-workgroup-symbolic", "No Teams to Join");

        for (id, display_name, description, already_member) in teams {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name))
                .build();
            if !description.is_empty() {
                row.set_subtitle(&glib::markup_escape_text(&description));
                row.set_subtitle_lines(2);
            }
            row.add_suffix(&join_button(already_member, &id, &self.on_join));
            self.list.append(&row);
        }
    }
}

/// A channel's bookmarks, filled in by [`Self::set_bookmarks`].
///
/// Adding one is a form in the same dialog rather than a second dialog: it is
/// two fields, and you usually add several in a row.
pub struct BookmarkList {
    list: gtk::ListBox,
    on_open: Rc<dyn Fn(String)>,
    on_delete: Rc<dyn Fn(String)>,
}

impl BookmarkList {
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        on_add: impl Fn(String, String) + 'static,
        on_open: impl Fn(String) + 'static,
        on_delete: impl Fn(String) + 'static,
    ) -> Self {
        let list = loading_list();
        let list_group = adw::PreferencesGroup::builder().title("Bookmarks").build();
        list_group.add(&list);

        let name = adw::EntryRow::builder().title("Name (optional)").build();
        let link = adw::EntryRow::builder().title("Link").build();

        let add = gtk::Button::builder()
            .label("Add")
            .valign(gtk::Align::Center)
            // A bookmark without a link is nothing to save; the name can be
            // filled in from the page by whoever handles this.
            .sensitive(false)
            .build();
        add.add_css_class("suggested-action");
        link.connect_changed({
            let add = add.clone();
            move |link| add.set_sensitive(!link.text().trim().is_empty())
        });
        add.connect_clicked({
            let name = name.clone();
            let link = link.clone();
            move |_| {
                on_add(
                    name.text().trim().to_string(),
                    link.text().trim().to_string(),
                );
                // Ready for the next one, and emptying the link disables the
                // button again through the handler above.
                name.set_text("");
                link.set_text("");
            }
        });

        let form = adw::PreferencesGroup::builder()
            .title("Add Bookmark")
            .build();
        form.set_header_suffix(Some(&add));
        form.add(&name);
        form.add(&link);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .build();
        content.append(&list_group);
        content.append(&form);

        list_dialog("Bookmarks", None, &content).present(Some(parent));

        BookmarkList {
            list,
            on_open: Rc::new(on_open),
            on_delete: Rc::new(on_delete),
        }
    }

    /// `bookmarks` is (id, display_name, link_url).
    pub fn set_bookmarks(&self, bookmarks: Vec<(String, String, String)>) {
        reset_list(&self.list, "bookmark-new-symbolic", "No Bookmarks");

        for (id, display_name, link_url) in bookmarks {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&display_name))
                .subtitle(glib::markup_escape_text(&link_url))
                .activatable(true)
                .build();
            // The whole row opens it, which is what a bookmark is for; the
            // button beside it is the only other thing you can do.
            row.connect_activated({
                let on_open = self.on_open.clone();
                let link_url = link_url.clone();
                move |_| on_open(link_url.clone())
            });

            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove Bookmark")
                .valign(gtk::Align::Center)
                .build();
            delete.add_css_class("flat");
            delete.connect_clicked({
                let on_delete = self.on_delete.clone();
                move |button| {
                    button.set_sensitive(false);
                    on_delete(id.clone());
                }
            });
            row.add_suffix(&delete);

            self.list.append(&row);
        }
    }
}

/// Rename a channel, or change its topic. Answers (display_name, header).
pub fn edit_channel(
    parent: &impl IsA<gtk::Widget>,
    current: (String, String),
    on_save: impl Fn(String, String) + 'static,
) {
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(&current.0)
        .activates_default(true)
        .build();
    let header = adw::EntryRow::builder()
        .title("Topic")
        .text(&current.1)
        .activates_default(true)
        .build();

    let group = adw::PreferencesGroup::builder().build();
    group.add(&name);
    group.add(&header);

    let (dialog, save) = form("Channel Details", "Save", &group);
    save.set_sensitive(!current.0.trim().is_empty());
    name.connect_changed({
        let save = save.clone();
        move |name| save.set_sensitive(!name.text().trim().is_empty())
    });
    save.connect_clicked({
        let dialog = dialog.clone();
        let name = name.clone();
        move |_| {
            dialog.close();
            on_save(name.text().trim().to_string(), header.text().to_string());
        }
    });
    dialog.present(Some(parent));
    name.grab_focus();
}

/// Confirm archiving a channel. Archiving hides it for everyone, so it asks
/// in the same words the other clients use.
pub fn confirm_archive(
    parent: &impl IsA<gtk::Widget>,
    channel_name: &str,
    on_archive: impl Fn() + 'static,
) {
    confirm(
        parent,
        &format!("Archive {channel_name}?"),
        "It will be hidden for everyone; its messages are kept and an admin can bring it back.",
        "Archive",
        on_archive,
    );
}

/// Name a category, whether new or being renamed. One field, so it stays a
/// small alert rather than a whole form.
pub fn name_category(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    current: &str,
    on_save: impl Fn(String) + 'static,
) {
    let entry = adw::EntryRow::builder()
        .title("Name")
        .text(current)
        .activates_default(true)
        .build();
    let group = adw::PreferencesGroup::builder().build();
    group.add(&entry);

    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.set_extra_child(Some(&group));
    dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("save", !current.trim().is_empty());
    entry.connect_changed({
        let dialog = dialog.clone();
        move |entry| dialog.set_response_enabled("save", !entry.text().trim().is_empty())
    });
    dialog.connect_response(None, {
        let entry = entry.clone();
        move |_, response| {
            let name = entry.text().trim().to_string();
            if response == "save" && !name.is_empty() {
                on_save(name);
            }
        }
    });
    dialog.present(Some(parent));
    entry.grab_focus();
}

/// Confirm removing someone from a channel.
pub fn confirm_remove_member(
    parent: &impl IsA<gtk::Widget>,
    name: &str,
    on_remove: impl Fn() + 'static,
) {
    confirm(
        parent,
        &format!("Remove {name}?"),
        "They will stop receiving its messages until someone adds them back.",
        "Remove",
        on_remove,
    );
}

/// A boxed list that shows a spinner until its first answer arrives; from then
/// on [`reset_list`] has it say why it is empty instead of leaving a gap.
pub fn loading_list() -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .build();
    list.add_css_class("boxed-list");

    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .build();
    list.set_placeholder(Some(&spinner));
    list
}

/// Empties `list` for a fresh answer, and has it say `title` if the answer
/// turns out to be nothing. GtkListBox shows the placeholder only while there
/// are no rows.
pub fn reset_list(list: &gtk::ListBox, icon: &str, title: &str) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let empty = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .build();
    empty.add_css_class("compact");
    list.set_placeholder(Some(&empty));
}

/// The dialog a list lives in: a header (with `title_widget` in the middle
/// when given) over a scrolling clamp. On a phone it is a bottom sheet.
pub fn list_dialog(
    title: &str,
    title_widget: Option<&gtk::Widget>,
    content: &impl IsA<gtk::Widget>,
) -> adw::Dialog {
    let clamp = adw::Clamp::builder()
        .maximum_size(560)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .child(content)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let header = adw::HeaderBar::new();
    if let Some(widget) = title_widget {
        header.set_title_widget(Some(widget));
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));

    adw::Dialog::builder()
        .title(title)
        .content_width(460)
        .content_height(620)
        .child(&view)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_url_safe() {
        assert_eq!(slugify("Release Planning"), "release-planning");
        assert_eq!(slugify("  Q3 / 2026 — plans!  "), "q3-2026-plans");
        assert_eq!(slugify("keep_underscores"), "keep_underscores");
        // Nothing ASCII to work with: the caller has to notice and ask again.
        assert_eq!(slugify("Привет"), "");
    }

    #[test]
    fn next_monday_is_always_ahead() {
        assert_eq!(days_until_next_monday(1), 7); // Monday -> the one after
        assert_eq!(days_until_next_monday(2), 6); // Tuesday
        assert_eq!(days_until_next_monday(5), 3); // Friday
        assert_eq!(days_until_next_monday(7), 1); // Sunday -> tomorrow
    }

    #[test]
    fn unknown_levels_fall_back_to_the_first() {
        assert_eq!(level_index(&CHANNEL_LEVELS, "mention"), 2);
        assert_eq!(level_index(&ACCOUNT_LEVELS, "mention"), 1);
        assert_eq!(level_index(&CHANNEL_LEVELS, "something_new"), 0);
    }
}
