//! The channel sidebar: an account/team switcher in the header, the list below.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use adw::prelude::*;
use mattermost_api::models::{CategoryType, Channel, ChannelType, Presence, User};

use crate::avatars::Avatars;
use crate::state::SharedState;

/// The channel list, pane one.
/// What the channel row's own menu can ask for.
#[derive(Debug, Clone)]
pub enum RowAction {
    MarkRead,
    MarkUnread,
    SetMuted(bool),
    MoveTo(String),
    RenameCategory,
    DeleteCategory,
}

/// Which row a widget is, so a refresh can tell "this row moved" apart from
/// "a different row landed in that position".
#[derive(PartialEq, Eq, Hash)]
enum RowKey {
    Category(String),
    Channel(String),
}

pub struct ChannelSidebar {
    pub widget: adw::ToolbarView,
    categories: Rc<dyn Fn(String, RowAction)>,
    search_button: gtk::ToggleButton,
    search_entry: gtk::SearchEntry,
    inbox_button: gtk::Button,
    inbox_badge: gtk::Label,
    list: gtk::ListBox,
    title: gtk::Label,
    switcher: Switcher,
    updating: Rc<RefCell<bool>>,
    signature: std::cell::Cell<Option<u64>>,
    /// What the list currently shows, in order: each row, the state it was
    /// drawn from, and the widget drawn for it. Kept beside the `ListBox`
    /// because the box itself remembers only widgets, and a refresh has to
    /// answer "is this still the right picture of this channel" without
    /// asking the widget.
    rows: RefCell<Vec<(RowKey, u64, gtk::ListBoxRow)>>,
}

impl ChannelSidebar {
    pub fn new(
        on_select: impl Fn(String) + 'static,
        on_select_team: impl Fn(String) + 'static,
        on_search: impl Fn(String) + 'static,
        on_inbox: impl Fn() + 'static,
        on_status: impl Fn(String) + 'static,
        on_row_action: impl Fn(String, RowAction) + 'static,
        dock: &gtk::Widget,
    ) -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        list.add_css_class("navigation-sidebar");
        list.add_css_class("channel-list");

        // Category headers are non-selectable rows, so skip them when the user
        // arrows through the list.
        list.set_header_func(|_row, _before| {});

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let title = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("heading");

        let switcher = Switcher::new(on_select_team, on_status);

        let header = adw::HeaderBar::builder()
            .title_widget(&title)
            .show_end_title_buttons(false)
            .build();
        header.pack_start(&switcher.button);

        // HeaderBar packs end children in reverse visual order. Build them
        // menu, inbox, search so the visible order reads search, inbox, menu.
        // The main menu holds actions for the *list*, not any one channel.
        let menu = gtk::gio::Menu::new();
        let channels_section = gtk::gio::Menu::new();
        channels_section.append(Some("Jump to…"), Some("win.quick-switch"));
        channels_section.append(Some("Scheduled Messages"), Some("win.scheduled-posts"));
        channels_section.append(Some("New Channel…"), Some("win.new-channel"));
        channels_section.append(Some("New Category…"), Some("win.new-category"));
        channels_section.append(Some("Browse Teams…"), Some("win.browse-teams"));
        channels_section.append(Some("Leave This Team"), Some("win.leave-team"));
        channels_section.append(Some("Browse Channels…"), Some("win.browse-channels"));
        menu.append_section(None, &channels_section);
        let account_section = gtk::gio::Menu::new();
        account_section.append(Some("Edit Profile…"), Some("win.edit-profile"));
        account_section.append(Some("Set a Status…"), Some("win.custom-status"));
        account_section.append(Some("Notifications…"), Some("win.notification-settings"));
        account_section.append(Some("Sign Out"), Some("win.sign-out"));

        let app_section = gtk::gio::Menu::new();
        app_section.append(Some("Storage…"), Some("win.storage"));
        app_section.append(Some("Keep Running in Background"), Some("app.background"));
        app_section.append(Some("Quit"), Some("app.quit"));
        menu.append_section(None, &app_section);
        menu.append_section(None, &account_section);

        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main menu")
            .menu_model(&menu)
            .primary(true)
            .build();
        menu_button.add_css_class("flat");
        header.pack_end(&menu_button);

        let inbox_badge = gtk::Label::builder().visible(false).build();
        inbox_badge.add_css_class("mention-badge");
        inbox_badge.add_css_class("button-badge");
        let inbox_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build();
        inbox_content.append(&gtk::Image::from_icon_name("mail-unread-symbolic"));
        inbox_content.append(&inbox_badge);
        let inbox_button = gtk::Button::builder()
            .child(&inbox_content)
            .tooltip_text("Mentions and threads")
            .build();
        inbox_button.add_css_class("flat");
        inbox_button.set_visible(false);
        inbox_button.connect_clicked(move |_| on_inbox());
        header.pack_end(&inbox_button);

        // A search bar rather than a dialog: it belongs to the list it filters
        // into, and Escape puts it away without losing your place.
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search messages, or file: to search attachments")
            .hexpand(true)
            .build();
        let search_bar = gtk::SearchBar::builder()
            .child(&search_entry)
            .key_capture_widget(&header)
            .build();
        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search Messages")
            .build();
        search_button.add_css_class("flat");
        search_button
            .bind_property("active", &search_bar, "search-mode-enabled")
            .bidirectional()
            .sync_create()
            .build();
        header.pack_end(&search_button);

        // On activate, not on every keystroke: a post search is a round trip
        // to the server, and searching per character would be a request per
        // character.
        search_entry.connect_activate(move |entry| {
            if let Some(terms) = submitted_search_terms(entry.text().as_str()) {
                on_search(terms);
            }
        });

        // The call dock is the toolbar view's bottom bar rather than another
        // child of a box: that reserves its space, draws the separator, and
        // animates the reveal, none of which a plain box would do.
        let widget = adw::ToolbarView::builder()
            .bottom_bar_style(adw::ToolbarStyle::RaisedBorder)
            .reveal_bottom_bars(false)
            .build();
        widget.add_top_bar(&header);
        widget.add_top_bar(&search_bar);
        widget.set_content(Some(&scroller));
        widget.add_bottom_bar(dock);

        let updating = Rc::new(RefCell::new(false));
        list.connect_row_selected({
            let updating = updating.clone();
            move |_, row| {
                if *updating.borrow() {
                    return;
                }
                if let Some(row) = row {
                    if let Some(id) = unsafe { row.data::<String>("channel-id") } {
                        on_select(unsafe { id.as_ref() }.clone());
                    }
                }
            }
        });

        ChannelSidebar {
            widget,
            inbox_button,
            inbox_badge,
            search_button: search_button.clone(),
            search_entry: search_entry.clone(),
            categories: Rc::new(on_row_action),
            list,
            title,
            switcher,
            updating,
            signature: std::cell::Cell::new(None),
            rows: RefCell::new(Vec::new()),
        }
    }

    /// Opens the search bar and puts the cursor in it.
    pub fn focus_search(&self) {
        self.search_button.set_active(true);
        self.search_entry.grab_focus();
    }

    /// Update selection for a collapsed navigation view. Returning to the
    /// sidebar clears ListBox's retained selection so its current row can be
    /// activated with one click.
    pub fn navigation_state_changed(&self, collapsed: bool, showing_content: bool) {
        clear_selection_if_return(&self.list, collapsed, showing_content);
    }

    pub fn set_inbox_button_visible(&self, visible: bool) {
        self.inbox_button.set_visible(visible);
    }

    /// Emits the inbox control's real click signal for a deterministic demo.
    pub fn activate_inbox_button(&self) {
        self.inbox_button.emit_clicked();
    }

    pub fn set_inbox_count(&self, count: i64) {
        self.inbox_badge.set_visible(count > 0);
        self.inbox_badge.set_text(&count.to_string());
    }

    pub fn refresh(&self, state: &SharedState, avatars: &Avatars) {
        let (groups, signature) = {
            let st = state.borrow();
            let groups = st.sidebar_groups();
            let signature = sidebar_signature(&st, &groups);
            (groups, signature)
        };
        if self.signature.replace(Some(signature)) == Some(signature) {
            return;
        }
        self.switcher.refresh(state, avatars);

        let st = state.borrow();
        let team_name = st
            .current_team
            .as_ref()
            .and_then(|id| st.teams.iter().find(|t| &t.id == id))
            .map(|t| t.display_name.clone())
            .unwrap_or_else(|| "Matras".to_string());
        self.title.set_text(&team_name);

        // What every row depends on but no row owns: the naming style decides
        // every title, and the category names are the "Move to …" items in
        // every row's menu.
        let shared = {
            let mut hash = DefaultHasher::new();
            st.teammate_name_display().hash(&mut hash);
            for category in &st.categories.categories {
                (&category.id, &category.display_name).hash(&mut hash);
            }
            hash.finish()
        };

        let mut old: HashMap<RowKey, (u64, gtk::ListBoxRow)> = self
            .rows
            .take()
            .into_iter()
            .map(|(key, revision, widget)| (key, (revision, widget)))
            .collect();
        // Removing, inserting and selecting rows all make the ListBox emit
        // `row-selected`, and none of it is the user picking a channel.
        *self.updating.borrow_mut() = true;

        let mut rows: Vec<(RowKey, u64, gtk::ListBoxRow)> = Vec::new();
        for (category, channels) in groups {
            let key = RowKey::Category(category.id.clone());
            // Custom categories can be renamed and deleted; the built-in ones
            // (Favourites, Channels, Direct Messages) cannot, and offering it
            // would only produce a server error.
            let custom = category.r#type == CategoryType::Custom;
            let revision = {
                let mut hash = DefaultHasher::new();
                (&category.display_name, custom).hash(&mut hash);
                hash.finish()
            };
            let widget = reuse_or_build(&self.list, &mut old, &key, revision, || {
                let header = category_header(&category.display_name);
                if custom {
                    attach_category_menu(&header, &category.id, &self.categories);
                }
                header
            });
            rows.push((key, revision, widget));

            for channel in channels {
                let key = RowKey::Channel(channel.id.clone());
                let revision = channel_revision(&channel, &st, shared);
                let widget = reuse_or_build(&self.list, &mut old, &key, revision, || {
                    let title = st.channel_title(&channel);
                    // A DM is a person, so it gets that person's face with the
                    // presence badge, exactly like a message row.
                    let icon = channel
                        .dm_teammate_id(&st.me.id)
                        .map(|user_id| dm_avatar(avatars, user_id, &title, st.presence(user_id)));
                    // A DM is a person, and their status says whether writing
                    // to them is worth doing now.
                    let status = channel
                        .dm_teammate_id(&st.me.id)
                        .and_then(|id| st.users.get(id))
                        .and_then(|user| user.custom_status())
                        .filter(|status| !status.emoji.is_empty());
                    let row = channel_row(&channel, &title, icon, status, avatars, &st);
                    unsafe { row.set_data("channel-id", channel.id.clone()) };
                    attach_row_menu(&row, &channel.id, &st, &self.categories);
                    row
                });
                rows.push((key, revision, widget));
            }
        }

        // Whatever nothing claimed held a place that no longer exists.
        for (_, (_, widget)) in old {
            self.list.remove(&widget);
        }

        // The list now holds exactly the kept rows, in the order they were in
        // before. Walk the positions once and move the ones that are in the
        // wrong place: reordering is a channel changing category or a
        // recency sort turning over, which happens far less than a badge
        // changing, and an unparent/insert pair is still cheaper than
        // rebuilding the widget tree it moves.
        for (index, (_, _, widget)) in rows.iter().enumerate() {
            let index = index as i32;
            if self.list.row_at_index(index).as_ref() == Some(widget) {
                continue;
            }
            if widget.parent().is_some() {
                self.list.remove(widget);
            }
            self.list.insert(widget, index);
        }
        // Nothing should be left over — but a channel that the server briefly
        // reports in two categories would leave one widget unclaimed on every
        // refresh, and a sidebar that grows a row a second is worse than one
        // that shows a duplicate.
        while let Some(extra) = self.list.row_at_index(rows.len() as i32) {
            self.list.remove(&extra);
        }

        // Selection follows the current channel rather than being written
        // while the rows are built: a row that never moved keeps its
        // selection, so switching channels no longer touches any row but the
        // two whose highlight actually changes.
        let selected = st.current_channel.as_deref().and_then(|current| {
            rows.iter()
                .find(|(key, _, _)| matches!(key, RowKey::Channel(id) if id == current))
                .map(|(_, _, widget)| widget)
        });
        self.list.select_row(selected);

        *self.rows.borrow_mut() = rows;
        drop(st);
        *self.updating.borrow_mut() = false;
    }
}

/// The widget for one row: the one already on screen when nothing it draws
/// has moved, a fresh one otherwise. A stale widget leaves the list here, so
/// that by the time positions are assigned the list holds only survivors.
fn reuse_or_build(
    list: &gtk::ListBox,
    old: &mut HashMap<RowKey, (u64, gtk::ListBoxRow)>,
    key: &RowKey,
    revision: u64,
    build: impl FnOnce() -> gtk::ListBoxRow,
) -> gtk::ListBoxRow {
    match old.remove(key) {
        Some((drawn_from, widget)) if drawn_from == revision => widget,
        Some((_, stale)) => {
            list.remove(&stale);
            build()
        }
        None => build(),
    }
}

/// Everything that changes how one channel row is drawn, or what its context
/// menu offers, reduced to one value — the sidebar's equivalent of
/// `chat::post_revision`.
///
/// Deliberately *not* the current channel: which row is selected is the
/// ListBox's own business, and folding it in here would rebuild two rows on
/// every switch for the sake of a CSS class. Nor the raw unread message
/// count, only `is_unread()`: a second unread message draws the same dot as
/// the first.
fn channel_revision(channel: &Channel, state: &crate::state::AppState, shared: u64) -> u64 {
    let mut hash = DefaultHasher::new();
    shared.hash(&mut hash);
    channel.id.hash(&mut hash);
    channel.r#type.hash(&mut hash);
    state.channel_title(channel).hash(&mut hash);

    let unread = state.unread(&channel.id);
    (
        unread.mentions,
        unread.urgent,
        unread.muted,
        unread.is_unread(),
    )
        .hash(&mut hash);
    state.drafts.contains_key(&channel.id).hash(&mut hash);

    if let Some(people) = state.active_calls.get(&channel.id) {
        people.hash(&mut hash);
        // The faces carry names, and a name can arrive after the call does.
        for user_id in people {
            state
                .users
                .get(user_id)
                .map(|user| user.display_name(state.teammate_name_display()))
                .hash(&mut hash);
        }
    }

    if let Some(user_id) = channel.dm_teammate_id(&state.me.id) {
        state.presence(user_id).hash(&mut hash);
        if let Some(status) = state
            .users
            .get(user_id)
            .and_then(|user| user.custom_status())
        {
            (
                status.emoji,
                status.text,
                status.duration,
                status.expires_at,
            )
                .hash(&mut hash);
        }
    }

    // Which category the channel is in decides which "Move to …" items its
    // menu leaves out.
    state
        .categories
        .categories
        .iter()
        .find(|c| c.channel_ids.contains(&channel.id))
        .map(|c| &c.id)
        .hash(&mut hash);

    hash.finish()
}

fn sidebar_signature(
    state: &crate::state::AppState,
    groups: &[(mattermost_api::models::SidebarCategory, Vec<Channel>)],
) -> u64 {
    let mut hash = DefaultHasher::new();
    state.current_team.hash(&mut hash);
    state.current_channel.hash(&mut hash);
    state.teammate_name_display().hash(&mut hash);
    serde_json::to_vec(&(&state.me, &state.teams, &state.team_unreads, groups))
        .unwrap_or_default()
        .hash(&mut hash);
    for (_, channels) in groups {
        for channel in channels {
            serde_json::to_vec(&state.memberships.get(&channel.id))
                .unwrap_or_default()
                .hash(&mut hash);
            state.active_calls.get(&channel.id).hash(&mut hash);
            // The pencil is drawn from this and nothing else, so without it
            // here the refresh that saving a draft asks for returns early:
            // emptying a composer left the pencil on the row until some
            // unrelated event happened to move the signature.
            state.drafts.contains_key(&channel.id).hash(&mut hash);
            state.channel_title(channel).hash(&mut hash);
            if let Some(user_id) = channel.dm_teammate_id(&state.me.id) {
                serde_json::to_vec(&state.users.get(user_id))
                    .unwrap_or_default()
                    .hash(&mut hash);
                state.presence(user_id).hash(&mut hash);
            }
        }
    }
    state.presence(&state.me.id).hash(&mut hash);
    hash.finish()
}

pub(crate) fn should_clear_selection_on_return(collapsed: bool, showing_content: bool) -> bool {
    collapsed && !showing_content
}

pub(crate) fn inbox_button_visibility(collapsed: bool, showing_content: bool) -> (bool, bool) {
    let in_chat = collapsed && showing_content;
    (!in_chat, in_chat)
}

/// The inbox panel lives in the conversation page, so a phone must navigate
/// there before showing it when the channel list is currently on screen.
pub(crate) fn inbox_requires_chat_navigation(collapsed: bool, showing_content: bool) -> bool {
    collapsed && !showing_content
}

fn submitted_search_terms(input: &str) -> Option<String> {
    let terms = input.trim();
    (!terms.is_empty()).then(|| terms.to_string())
}

pub(crate) fn should_close_inbox_on_return(
    collapsed: bool,
    showing_content: bool,
    inbox_open: bool,
) -> bool {
    collapsed && !showing_content && inbox_open
}

fn clear_selection_if_return(list: &gtk::ListBox, collapsed: bool, showing_content: bool) {
    if should_clear_selection_on_return(collapsed, showing_content) {
        list.unselect_all();
    }
}

/// The status dot drawn over your own avatar.
fn presence_dot(avatar_size: i32) -> gtk::Box {
    let size = super::profile::presence_badge_size(avatar_size);
    let dot = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .build();
    dot.add_css_class("presence-badge");
    dot
}

/// The right-click menu on a category heading.
fn attach_category_menu(
    row: &gtk::ListBoxRow,
    category_id: &str,
    on_action: &Rc<dyn Fn(String, RowAction)>,
) {
    let menu = gtk::gio::Menu::new();
    let group = gtk::gio::SimpleActionGroup::new();

    for (label, name, action) in [
        ("Rename…", "rename", RowAction::RenameCategory),
        ("Delete", "delete", RowAction::DeleteCategory),
    ] {
        let item = gtk::gio::SimpleAction::new(name, None);
        item.connect_activate({
            let on_action = on_action.clone();
            let category_id = category_id.to_string();
            let action = action.clone();
            move |_, _| on_action(category_id.clone(), action.clone())
        });
        group.add_action(&item);
        menu.append(Some(label), Some(&format!("category.{name}")));
    }

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(row);
    popover.set_has_arrow(false);
    row.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    row.insert_action_group("category", Some(&group));

    attach_context_triggers(
        row,
        Rc::new(move |_: &gtk::ListBoxRow, x: f64, y: f64| {
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }),
    );
}

/// Opens a row's context menu at a point in the row.
type OpenMenu = Rc<dyn Fn(&gtk::ListBoxRow, f64, f64)>;

/// The ways to ask for a row's menu: right click, a long press on a
/// touchscreen, and Shift+F10 or the Menu key while the row has focus.
fn attach_context_triggers(row: &gtk::ListBoxRow, open: OpenMenu) {
    // The row comes from the controller rather than being captured: a
    // controller belongs to its widget, so a closure holding that widget is
    // a cycle and the row would never be freed.
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    click.connect_pressed({
        let open = open.clone();
        move |gesture, _, x, y| {
            if let Some(row) = gesture.widget().and_downcast::<gtk::ListBoxRow>() {
                open(&row, x, y);
            }
        }
    });
    row.add_controller(click);

    // A press that turns into the menu must not also select the channel, so
    // the gesture claims the sequence and the list's own click is cancelled.
    let long_press = gtk::GestureLongPress::new();
    long_press.set_touch_only(true);
    long_press.connect_pressed({
        let open = open.clone();
        move |gesture, x, y| {
            if let Some(row) = gesture.widget().and_downcast::<gtk::ListBoxRow>() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                open(&row, x, y);
            }
        }
    });
    row.add_controller(long_press);

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |controller, key, _, modifier| {
        let shift_f10 =
            key == gtk::gdk::Key::F10 && modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        if !(shift_f10 || key == gtk::gdk::Key::Menu) {
            return gtk::glib::Propagation::Proceed;
        }
        let Some(row) = controller.widget().and_downcast::<gtk::ListBoxRow>() else {
            return gtk::glib::Propagation::Proceed;
        };
        open(&row, 24.0, f64::from(row.height()) / 2.0);
        gtk::glib::Propagation::Stop
    });
    row.add_controller(keys);
}

/// The right-click menu on a channel: mute it, favourite it, or file it under
/// a different category. All three are things people expect to reach from the
/// row itself rather than from a settings screen.
fn attach_row_menu(
    row: &gtk::ListBoxRow,
    channel_id: &str,
    state: &crate::state::AppState,
    categories: &Rc<dyn Fn(String, RowAction)>,
) {
    let muted = state
        .memberships
        .get(channel_id)
        .is_some_and(|m| m.is_muted());
    let mut entries: Vec<(String, String, RowAction)> = Vec::new();
    if state.unread(channel_id).is_unread() {
        entries.push((
            "Mark as Read".to_string(),
            "read".to_string(),
            RowAction::MarkRead,
        ));
    } else {
        entries.push((
            "Mark as Unread".to_string(),
            "unread".to_string(),
            RowAction::MarkUnread,
        ));
    }
    entries.push((
        if muted { "Unmute" } else { "Mute" }.to_string(),
        "mute".to_string(),
        RowAction::SetMuted(!muted),
    ));

    // Where it is now is not somewhere to move it to.
    let current = state
        .categories
        .categories
        .iter()
        .find(|c| c.channel_ids.iter().any(|id| id == channel_id))
        .map(|c| c.id.clone())
        .unwrap_or_default();
    for category in &state.categories.categories {
        if category.id == current {
            continue;
        }
        entries.push((
            format!("Move to {}", category.display_name),
            format!("move-{}", category.id),
            RowAction::MoveTo(category.id.clone()),
        ));
    }

    // Nothing above this line is a GTK object: `entries` is a handful of
    // strings. Everything below is, and none of it is built until somebody
    // actually right-clicks — a menu, an action group and an action per item
    // for every channel in the sidebar, rebuilt on every sidebar refresh, was
    // twenty thousand allocations for menus that are almost never opened.
    let built: Rc<RefCell<Option<gtk::PopoverMenu>>> = Rc::new(RefCell::new(None));
    let open: OpenMenu = Rc::new({
        let categories = categories.clone();
        let channel_id = channel_id.to_string();
        move |row, x, y| {
            let mut slot = built.borrow_mut();
            let popover = slot.get_or_insert_with(|| {
                let menu = gtk::gio::Menu::new();
                let group = gtk::gio::SimpleActionGroup::new();
                for (label, name, action) in &entries {
                    let item = gtk::gio::SimpleAction::new(name, None);
                    item.connect_activate({
                        let categories = categories.clone();
                        let channel_id = channel_id.clone();
                        let action = action.clone();
                        move |_, _| categories(channel_id.clone(), action.clone())
                    });
                    group.add_action(&item);
                    menu.append(Some(label), Some(&format!("row.{name}")));
                }

                let popover = gtk::PopoverMenu::from_model(Some(&menu));
                popover.set_parent(row);
                popover.set_has_arrow(false);
                popover.set_halign(gtk::Align::Start);
                // A parented popover is a child of the row, and the sidebar
                // rebuilds its rows constantly — without this, every rebuild
                // finalises a row that still owns a popover and GTK says so,
                // once per row, forever.
                row.connect_destroy({
                    let popover = popover.clone();
                    move |_| popover.unparent()
                });
                row.insert_action_group("row", Some(&group));
                popover
            });
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    });
    attach_context_triggers(row, open);
}

fn category_header(name: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .build();
    label.add_css_class("dim-label");

    // A box, not a bare label: every row in this list is styled through its
    // child, so a header without one would miss the shared indentation.
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();
    row_box.add_css_class("channel-category");
    row_box.append(&label);

    gtk::ListBoxRow::builder()
        .child(&row_box)
        .selectable(false)
        .activatable(false)
        .build()
}

fn dm_avatar(
    avatars: &Avatars,
    user_id: &str,
    display_name: &str,
    presence: Presence,
) -> gtk::Widget {
    let avatar = adw::Avatar::builder().size(20).build();
    avatars.apply(&avatar, user_id, display_name);
    super::profile::with_presence(&avatar, presence).upcast()
}

fn channel_row(
    channel: &Channel,
    title: &str,
    icon: Option<gtk::Widget>,
    status: Option<mattermost_api::models::CustomStatus>,
    avatars: &Avatars,
    state: &crate::state::AppState,
) -> gtk::ListBoxRow {
    let unread = state.unread(&channel.id);
    let in_call = state.active_calls.get(&channel.id);
    let has_draft = state.drafts.contains_key(&channel.id);

    // Public channels get a literal "#", the way Mattermost writes them; the
    // rest get an icon. A glyph also sidesteps icon-theme gaps on minimal
    // systems, where a missing SVG loader turns every symbolic icon into a
    // broken-image box.
    let icon: gtk::Widget = match icon {
        Some(widget) => widget,
        None => match channel.r#type {
            ChannelType::Open => {
                let hash = gtk::Label::new(Some("#"));
                hash.add_css_class("dim-label");
                hash.set_width_request(16);
                hash.upcast()
            }
            _ => {
                let image = gtk::Image::from_icon_name(channel_icon(&channel.r#type));
                image.add_css_class("dim-label");
                image.upcast()
            }
        },
    };

    let label = channel_name_label(title);

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    row_box.add_css_class("channel-row-content");
    row_box.append(&icon);
    row_box.append(&label);

    if let Some(status) = status {
        let chip = gtk::Label::new(Some(&crate::emoji::label(&status.emoji)));
        chip.add_css_class("custom-status");
        chip.set_tooltip_text(Some(&super::message::custom_status_tooltip(&status)));
        row_box.append(&chip);
    }

    // Keep the custom-status emoji with the person's name and let the spare
    // width carry unread counts and other trailing channel indicators.
    row_box.append(&gtk::Box::builder().hexpand(true).build());

    // A call in this channel matters more than an unread badge, so it goes
    // first and is always shown.
    if let Some(people) = in_call {
        row_box.append(&call_badge(people, avatars, state));
    }

    // Something unsent here. Shown even on a muted channel: it is your own
    // text waiting, not someone else's noise.
    if has_draft {
        let pencil = gtk::Image::from_icon_name("document-edit-symbolic");
        pencil.set_pixel_size(12);
        pencil.add_css_class("dim-label");
        pencil.set_tooltip_text(Some("You have an unsent message here"));
        row_box.append(&pencil);
    }

    if unread.mentions > 0 {
        let badge = gtk::Label::new(Some(&unread.mentions.to_string()));
        badge.add_css_class("mention-badge");
        if unread.urgent {
            badge.add_css_class("urgent");
        }
        row_box.append(&badge);
    } else if unread.is_unread() && !unread.muted {
        // Unread without a mention is a dot, the way Fractal marks a room:
        // enough to notice, not enough to demand a number be read.
        let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
        dot.add_css_class("unread-dot");
        dot.add_css_class("dim-label");
        row_box.append(&dot);
    }

    // Muted channels still show mentions, but not bold-for-messages: that is
    // exactly the rule the official clients use.
    if unread.is_unread() {
        label.add_css_class("channel-row-unread");
    }
    if unread.muted {
        row_box.add_css_class("channel-row-muted");
        row_box.add_css_class("dim-label");
    }

    // Paint selection/hover on a wrapper around the content margins. This
    // makes adjacent pills nearly meet without changing row height or moving
    // the icon and label.
    let background = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    background.add_css_class("channel-row-background");
    background.append(&row_box);

    gtk::ListBoxRow::builder().child(&background).build()
}

fn channel_name_label(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(channel_name_expands())
        .max_width_chars(CHANNEL_NAME_MAX_WIDTH_CHARS)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build()
}

const CHANNEL_NAME_MAX_WIDTH_CHARS: i32 = 24;

fn channel_name_expands() -> bool {
    false
}

/// Who is in the call, the way Slack marks a channel: a few faces and the
/// count. Faces beat an icon here — the reason to join is usually who is there.
fn call_badge(people: &[String], avatars: &Avatars, state: &crate::state::AppState) -> gtk::Widget {
    /// Beyond this the faces are unreadable at 16px and the count carries it.
    const FACES: usize = 3;

    let badge = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .valign(gtk::Align::Center)
        .build();
    badge.add_css_class("call-badge");

    let headset = gtk::Image::from_icon_name("audio-headphones-symbolic");
    headset.set_pixel_size(12);
    badge.append(&headset);

    for user_id in people.iter().take(FACES) {
        let name = state
            .users
            .get(user_id)
            .map(|u| u.display_name(state.teammate_name_display()))
            .unwrap_or_default();
        let avatar = adw::Avatar::builder().size(16).build();
        avatars.apply(&avatar, user_id, &name);
        badge.append(&avatar);
    }

    // The count is only news once it exceeds the faces already shown.
    if people.len() > FACES {
        badge.append(&gtk::Label::new(Some(&format!(
            "+{}",
            people.len() - FACES
        ))));
    }

    badge.set_tooltip_text(Some(&match people.len() {
        0 => "A call is starting".to_string(),
        1 => "1 person is in a call".to_string(),
        n => format!("{n} people are in a call"),
    }));
    badge.upcast()
}

fn channel_icon(kind: &ChannelType) -> &'static str {
    match kind {
        // Open is handled with a "#" glyph by the caller.
        ChannelType::Open => "network-workgroup-symbolic",
        ChannelType::Private => "changes-prevent-symbolic",
        ChannelType::Direct => "avatar-default-symbolic",
        ChannelType::Group => "system-users-symbolic",
        _ => "user-available-symbolic",
    }
}

/// The round avatar in the sidebar header and what drops out of it: who you
/// are signed in as, and the teams to switch between. This is where the old
/// 68px team rail went — Fractal puts its account switcher in exactly this
/// spot, and a rail costs a permanent column to say what a popover says on
/// demand.
struct Switcher {
    button: gtk::MenuButton,
    avatar: adw::Avatar,
    avatar_dot: gtk::Box,
    account_avatar: adw::Avatar,
    account_dot: gtk::Box,
    name: gtk::Label,
    username: gtk::Label,
    teams: gtk::ListBox,
}

impl Switcher {
    fn new(
        on_select_team: impl Fn(String) + 'static,
        on_status: impl Fn(String) + 'static,
    ) -> Self {
        let avatar = adw::Avatar::builder().size(24).build();
        let avatar_dot = presence_dot(avatar.size());
        let header_dot = super::profile::header_presence_badge_size(avatar.size());
        avatar_dot.set_size_request(header_dot, header_dot);
        let avatar_stack = gtk::Overlay::builder().child(&avatar).build();
        avatar_stack.add_overlay(&avatar_dot);

        let button = gtk::MenuButton::builder()
            .child(&avatar_stack)
            .tooltip_text("Account and teams")
            .build();
        button.add_css_class("image-button");
        button.add_css_class("circular");
        button.add_css_class("flat");

        let account_avatar = adw::Avatar::builder().size(40).build();
        let account_dot = presence_dot(account_avatar.size());
        let account_stack = gtk::Overlay::builder().child(&account_avatar).build();
        account_stack.add_overlay(&account_dot);
        let name = gtk::Label::builder().xalign(0.0).build();
        name.add_css_class("heading");
        let username = gtk::Label::builder().xalign(0.0).build();
        username.add_css_class("caption");
        username.add_css_class("dim-label");

        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        labels.append(&name);
        labels.append(&username);

        let account = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .build();
        account.append(&account_stack);
        account.append(&labels);

        // Setting your own status belongs with your own name, which is here.
        let statuses = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .homogeneous(true)
            .margin_start(6)
            .margin_end(6)
            .build();
        let on_status = Rc::new(on_status);
        for (label, value, css) in [
            ("Online", "online", "presence-online"),
            ("Away", "away", "presence-away"),
            ("Do not disturb", "dnd", "presence-dnd"),
            ("Offline", "offline", "presence-offline"),
        ] {
            let dot = gtk::Label::new(Some("●"));
            dot.add_css_class("presence-dot");
            dot.add_css_class(css);
            let button = gtk::Button::builder()
                .child(&dot)
                .tooltip_text(label)
                .build();
            button.add_css_class("flat");
            button.connect_clicked({
                let on_status = on_status.clone();
                let button_parent = button.clone();
                move |_| {
                    on_status(value.to_string());
                    // Close the popover the button lives in, so the choice
                    // registers as made.
                    if let Some(popover) = button_parent
                        .ancestor(gtk::Popover::static_type())
                        .and_downcast::<gtk::Popover>()
                    {
                        popover.popdown();
                    }
                }
            });
            statuses.append(&button);
        }

        let teams = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        teams.add_css_class("navigation-sidebar");

        // Row *activation*, not selection: selection also fires while we
        // rebuild the list, and re-selecting the current team would reload it.
        teams.connect_row_activated({
            let button = button.clone();
            move |_, row| {
                if let Some(id) = unsafe { row.data::<String>("team-id") } {
                    button.popdown();
                    on_select_team(unsafe { id.as_ref() }.clone());
                }
            }
        });

        let teams_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(320)
            .child(&teams)
            .build();

        let heading = gtk::Label::builder()
            .label("TEAMS")
            .xalign(0.0)
            .margin_start(12)
            .build();
        heading.add_css_class("caption-heading");
        heading.add_css_class("dim-label");

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .width_request(260)
            .build();
        content.append(&account);
        content.append(&statuses);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&heading);
        content.append(&teams_scroller);

        button.set_popover(Some(&gtk::Popover::builder().child(&content).build()));

        Switcher {
            button,
            avatar,
            avatar_dot,
            account_avatar,
            account_dot,
            name,
            username,
            teams,
        }
    }

    fn refresh(&self, state: &SharedState, avatars: &Avatars) {
        let st = state.borrow();
        let me: &User = &st.me;
        let display = me.display_name(st.teammate_name_display());
        avatars.apply(&self.avatar, &me.id, &display);
        avatars.apply(&self.account_avatar, &me.id, &display);
        // The dot on your own face is the only place the chosen status shows.
        let presence = st.presence(&me.id);
        for dot in [&self.avatar_dot, &self.account_dot] {
            for name in [
                "presence-online",
                "presence-away",
                "presence-dnd",
                "presence-offline",
            ] {
                dot.remove_css_class(name);
            }
            dot.add_css_class(super::profile::presence_class(presence));
        }
        self.name.set_text(&display);
        self.username.set_text(&format!("@{}", me.username));

        while let Some(child) = self.teams.first_child() {
            self.teams.remove(&child);
        }
        for team in st.teams.iter().filter(|t| t.delete_at == 0) {
            let avatar = adw::Avatar::builder()
                .size(24)
                .text(&team.display_name)
                .show_initials(true)
                .build();
            let label = gtk::Label::builder()
                .label(&team.display_name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let row_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .build();
            row_box.append(&avatar);
            row_box.append(&label);

            // Where something is waiting, so switching teams is a decision
            // rather than a guess.
            if let Some((messages, mentions)) = st.team_unreads.get(&team.id) {
                if *mentions > 0 {
                    let badge = gtk::Label::new(Some(&mentions.to_string()));
                    badge.add_css_class("mention-badge");
                    row_box.append(&badge);
                } else if *messages > 0 {
                    let dot = gtk::Box::builder().valign(gtk::Align::Center).build();
                    dot.add_css_class("unread-dot");
                    dot.add_css_class("dim-label");
                    row_box.append(&dot);
                }
            }

            let row = gtk::ListBoxRow::builder().child(&row_box).build();
            unsafe { row.set_data("team-id", team.id.clone()) };
            self.teams.append(&row);

            if st.current_team.as_deref() == Some(team.id.as_str()) {
                self.teams.select_row(Some(&row));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mattermost_api::models::{ChannelMember, ClientConfig};
    use mattermost_api::Client;

    #[test]
    fn account_presence_badges_scale_like_chat_avatar_badges() {
        let small_width = super::super::profile::presence_badge_size(24);
        let large_width = super::super::profile::presence_badge_size(40);
        assert_eq!((small_width, large_width), (10, 13));
    }

    #[test]
    fn search_submission_trims_terms_and_ignores_an_empty_query() {
        assert_eq!(
            submitted_search_terms("  calls regression  "),
            Some("calls regression".into())
        );
        assert_eq!(submitted_search_terms(" \n\t "), None);
    }

    #[test]
    fn the_header_account_presence_badge_is_nearly_as_large_as_the_sidebar_badge() {
        assert_eq!(super::super::profile::header_presence_badge_size(24), 12);
        assert_eq!(super::super::profile::presence_badge_size(40), 13);
    }

    #[test]
    fn channel_name_does_not_expand_status_emoji_away_from_name() {
        assert!(!channel_name_expands());
        assert_eq!(CHANNEL_NAME_MAX_WIDTH_CHARS, 24);
    }

    #[test]
    fn returning_to_collapsed_sidebar_clears_the_selected_channel() {
        assert!(should_clear_selection_on_return(true, false));
        assert!(!should_clear_selection_on_return(false, false));
        assert!(!should_clear_selection_on_return(true, true));
    }

    #[test]
    fn inbox_button_moves_between_desktop_sidebar_and_mobile_content() {
        assert_eq!(inbox_button_visibility(false, true), (true, false));
        assert_eq!(inbox_button_visibility(true, true), (false, true));
        assert_eq!(inbox_button_visibility(true, false), (true, false));
    }

    #[test]
    fn opening_inbox_from_the_mobile_sidebar_returns_to_the_chat_page() {
        assert!(inbox_requires_chat_navigation(true, false));
        assert!(!inbox_requires_chat_navigation(true, true));
        assert!(!inbox_requires_chat_navigation(false, false));
    }

    #[test]
    fn returning_to_mobile_sidebar_closes_an_open_inbox_panel() {
        assert!(should_close_inbox_on_return(true, false, true));
        assert!(!should_close_inbox_on_return(true, true, true));
        assert!(!should_close_inbox_on_return(true, false, false));
        assert!(!should_close_inbox_on_return(false, false, true));
    }

    /// One channel with `total` posts in it, of which `read` have been seen.
    fn revision(total: i64, read: i64, mentions: i64) -> u64 {
        let client = Client::new("http://x.test").unwrap();
        let mut st =
            crate::state::AppState::new(client, User::default(), ClientConfig::default(), false);
        st.channels.insert(
            "c1".into(),
            Channel {
                id: "c1".into(),
                total_msg_count: total,
                ..Default::default()
            },
        );
        st.memberships.insert(
            "c1".into(),
            ChannelMember {
                channel_id: "c1".into(),
                msg_count: read,
                mention_count: mentions,
                ..Default::default()
            },
        );
        channel_revision(&st.channels["c1"], &st, 0)
    }

    /// The dot on an unread row says "something is here", not how much, so a
    /// second unread message must not cost a rebuilt row.
    #[test]
    fn more_of_the_same_unread_does_not_move_the_revision() {
        assert_eq!(revision(10, 9, 0), revision(11, 9, 0));
    }

    #[test]
    fn becoming_unread_moves_the_revision() {
        assert_ne!(revision(10, 10, 0), revision(11, 10, 0));
    }

    #[test]
    fn a_mention_moves_the_revision_because_the_badge_counts() {
        assert_ne!(revision(11, 9, 1), revision(11, 9, 2));
    }
}
