//! Pane 3: the conversation — header, call banner, message list, composer.

use std::cell::{Cell, RefCell};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::rc::Rc;
use std::sync::OnceLock;

use adw::prelude::*;
use gtk::glib;
use mattermost_api::models::{Millis, Post};

use crate::avatars::Avatars;
use crate::state::SharedState;
use crate::ui::message::{self, MessageActions, RowOptions};

pub(super) const COMPOSER_MIN_CONTENT_HEIGHT: i32 = 28;
const COMPOSER_SINGLE_LINE_HEIGHT: i32 = COMPOSER_MIN_CONTENT_HEIGHT;
pub(super) const COMPOSER_MAX_CONTENT_HEIGHT: i32 = 108;
const COMPOSER_OPTIONS_BUTTON_WIDTH: i32 = 34;
const COMPOSER_SEND_ICON: &str = "mail-send-symbolic";

pub(super) fn scroll_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MATRAS_SCROLL_TRACE")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    })
}

pub(super) fn composer_content_height(lines: i32) -> i32 {
    (COMPOSER_MIN_CONTENT_HEIGHT + lines.saturating_sub(1) * 20)
        .clamp(COMPOSER_MIN_CONTENT_HEIGHT, COMPOSER_MAX_CONTENT_HEIGHT)
}

/// Everything the conversation pane reports back to the action loop. A struct
/// rather than nine positional closures: at that count the compiler stops
/// catching a swapped pair, and the caller cannot see which is which.
pub struct ChatCallbacks {
    pub on_send: Box<dyn Fn(String)>,
    pub on_typing: Box<dyn Fn(bool)>,
    pub on_attach: Box<dyn Fn()>,
    pub on_files: Box<dyn Fn(Vec<std::path::PathBuf>)>,
    pub on_complete: Box<dyn Fn(Option<super::autocomplete::Query>)>,
    pub on_schedule: Box<dyn Fn()>,
    pub on_scrollback: Box<dyn Fn()>,
    pub on_agent: Box<dyn Fn()>,
    pub on_call: Box<dyn Fn()>,
    pub on_inbox: Box<dyn Fn()>,
}

/// Post id paired with the widget drawing it, weakly held.
type VisiblePosts = Rc<RefCell<Vec<(String, glib::WeakRef<gtk::Widget>)>>>;

pub struct ChatView {
    pub widget: adw::ToolbarView,
    title: gtk::Label,
    subtitle: gtk::Label,
    messages: gtk::gio::ListStore,
    message_list: gtk::ListView,
    /// Realised message widgets, weakly held so GtkListView remains free to
    /// recycle them. Used to save a stable post id at 30% of the viewport.
    visible_posts: VisiblePosts,
    render_context: Rc<RefCell<Option<RenderContext>>>,
    entry: gtk::TextView,
    call_button: gtk::Button,
    inbox_button: gtk::Button,
    complete: Rc<super::autocomplete::Autocomplete>,
    /// The priority chosen for the next message, as a stateful action.
    priority_action: gtk::gio::SimpleAction,
    agent_button: gtk::MenuButton,
    /// Kept so the menu can be rebuilt without losing the fixed entries.
    agent_actions: Rc<RefCell<Vec<gtk::gio::SimpleAction>>>,
    inbox_badge: gtk::Label,
    call_banner: gtk::Box,
    call_banner_label: gtk::Label,
    join_button: gtk::Button,
    stack: gtk::Stack,
    /// Runs only while the first page is visibly loading. A permanently
    /// spinning child keeps GTK's frame clock alive even after Stack hides it.
    loading_spinner: gtk::Spinner,
    /// Which channel the feed currently holds, so a redraw can tell itself
    /// apart from a channel switch.
    showing: RefCell<Option<String>>,
    /// Shown instead of an empty feed while the first page is in flight, so a
    /// slow channel reads as loading rather than as empty.
    loading: Rc<RefCell<bool>>,
    /// Overlay shown while the page before this one is fetched. It must not be
    /// a model row: inserting/removing a loading row would itself move the
    /// scroll coordinate the reader is asking us to preserve.
    older_spinner: gtk::Spinner,
    connection: adw::Banner,
    typing: gtk::Label,
    attachments: gtk::Box,
    edit_banner: adw::Banner,
    /// The post being edited, when the composer is in edit mode.
    editing: Rc<RefCell<Option<String>>>,
    /// Set while a draft is being put back, so the change it causes is not
    /// mistaken for the user typing.
    restoring: Rc<RefCell<bool>>,
    /// False while the reader is scrolled up in history, so live messages do
    /// not yank them back to the bottom.
    pinned_to_bottom: Rc<RefCell<bool>>,
    /// Edge-trigger for history pagination. A physical approach to the top
    /// produces one request, not one request per high-resolution scroll tick.
    pagination_armed: Rc<Cell<bool>>,
    /// A programmatic scroll asked for before GtkListView had been measured.
    /// The list drops such a request — there is no allocation to scroll
    /// within — and on a cold start every scroll is one of these, which is
    /// how a restored channel came up at the top of its block however loudly
    /// the trace said otherwise. Re-applied the moment the adjustment gains
    /// bounds, which is the first thing that happens after the measure.
    pending_scroll: Rc<Cell<Option<(u64, u32)>>>,
    /// Bumped by every programmatic scroll. Both of them keep nudging the
    /// adjustment for a few frames after the request, because GtkListView
    /// measures late, so two requests issued in the same frame would spend
    /// those frames overwriting each other — which is what landing at the
    /// bottom and then restoring a saved anchor is. The later request wins:
    /// an earlier one whose number has moved on stops.
    scroll_epoch: Rc<Cell<u64>>,
}

/// Everything needed to turn a data item into a message row. Shared with the
/// thread panel, which draws the same rows out of the same three things.
#[derive(Clone)]
pub(super) struct RenderContext {
    pub(super) state: SharedState,
    pub(super) avatars: Avatars,
    pub(super) actions: MessageActions,
}

/// A cheap data item in the feed. `GtkListView` turns only the visible
/// screenful into widgets and recycles those widgets while scrolling.
#[derive(Clone)]
enum FeedItem {
    Start(String),
    Empty,
    Day(String),
    Unread,
    System(Rc<Vec<Post>>),
    Post {
        post: Rc<Post>,
        grouped: bool,
        highlight: bool,
        /// Computed once while the data item is built. Diffing the common
        /// suffix then compares integers instead of serialising every old and
        /// new Post again for every comparison.
        revision: u64,
    },
}

impl FeedItem {
    fn post_id(&self) -> Option<&str> {
        match self {
            FeedItem::Post { post, .. } => Some(&post.id),
            _ => None,
        }
    }

    fn uses_resource(&self, key: &str) -> bool {
        let matches = |post: &Post| {
            if let Some(file_id) = key
                .strip_prefix("file:")
                .or_else(|| key.strip_prefix("preview:"))
                .or_else(|| key.strip_prefix("video-head:"))
            {
                return post.file_ids.iter().any(|id| id == file_id)
                    || post.files().iter().any(|file| file.id == file_id);
            }
            if let Some(name) = key.strip_prefix("emoji:") {
                return post.message.contains(&format!(":{name}:"));
            }
            false
        };
        match self {
            FeedItem::Post { post, .. } => matches(post),
            FeedItem::System(posts) => posts.iter().any(matches),
            _ => false,
        }
    }

    /// Reactions are part of the serialised post, so they invalidate a row
    /// even when Mattermost leaves `update_at` untouched.
    fn fingerprint(&self) -> u64 {
        let mut hash = DefaultHasher::new();
        match self {
            FeedItem::Start(title) => {
                0u8.hash(&mut hash);
                title.hash(&mut hash);
            }
            FeedItem::Empty => 1u8.hash(&mut hash),
            FeedItem::Day(day) => {
                3u8.hash(&mut hash);
                day.hash(&mut hash);
            }
            FeedItem::Unread => 4u8.hash(&mut hash),
            FeedItem::System(posts) => {
                5u8.hash(&mut hash);
                serde_json::to_vec(posts.as_ref())
                    .unwrap_or_default()
                    .hash(&mut hash);
            }
            FeedItem::Post {
                post,
                grouped,
                highlight,
                revision,
            } => {
                6u8.hash(&mut hash);
                grouped.hash(&mut hash);
                highlight.hash(&mut hash);
                post.id.hash(&mut hash);
                revision.hash(&mut hash);
            }
        }
        hash.finish()
    }
}

fn model_item(store: &gtk::gio::ListStore, position: u32) -> Option<FeedItem> {
    store
        .item(position)?
        .downcast::<glib::BoxedAnyObject>()
        .ok()
        .map(|boxed| boxed.borrow::<FeedItem>().clone())
}

fn render_item(item: &FeedItem, context: &RenderContext) -> gtk::Widget {
    match item {
        FeedItem::Start(title) => start_label(title),
        FeedItem::Empty => adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("No Messages Yet")
            .description("Say something to get started")
            .vexpand(true)
            .build()
            .upcast(),
        FeedItem::Day(day) => message::day_separator(day),
        FeedItem::Unread => message::unread_line(),
        FeedItem::System(posts) => {
            let refs: Vec<&Post> = posts.iter().collect();
            let block = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .build();
            for row in message::system_block(&refs, &context.state, &context.actions) {
                block.append(&row);
            }
            block.upcast()
        }
        FeedItem::Post {
            post,
            grouped,
            highlight,
            ..
        } => {
            let row = message::build(
                post,
                &context.state,
                &context.avatars,
                &context.actions,
                RowOptions {
                    grouped: *grouped,
                    show_thread_footer: true,
                },
            );
            if *highlight {
                row.add_css_class("message-highlight");
            }
            row
        }
    }
}

impl ChatView {
    pub fn new(callbacks: ChatCallbacks) -> Self {
        let ChatCallbacks {
            on_send,
            on_typing,
            on_attach,
            on_files,
            on_complete,
            on_schedule,
            on_scrollback,
            on_agent,
            on_call,
            on_inbox,
        } = callbacks;
        let title = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("heading");
        let subtitle = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .build();
        subtitle.add_css_class("channel-header-subtitle");
        subtitle.add_css_class("dim-label");

        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        title_box.append(&title);
        title_box.append(&subtitle);

        let call_button = gtk::Button::builder()
            .icon_name("call-start-symbolic")
            .tooltip_text("Start or Join Call")
            .sensitive(false)
            .build();
        call_button.add_css_class("flat");
        let on_call = std::rc::Rc::new(on_call);
        call_button.connect_clicked({
            let on_call = on_call.clone();
            move |_| on_call()
        });

        // The inbox button carries its own count, the way a mail client's does:
        // the number is the reason to click it.
        let inbox_badge = gtk::Label::builder().visible(false).build();
        inbox_badge.add_css_class("mention-badge");
        inbox_badge.add_css_class("button-badge");
        let inbox_icon = gtk::Image::from_icon_name("mail-unread-symbolic");
        let inbox_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build();
        inbox_content.append(&inbox_icon);
        inbox_content.append(&inbox_badge);
        let inbox_button = gtk::Button::builder()
            .child(&inbox_content)
            .tooltip_text("Mentions and Threads")
            .build();
        inbox_button.add_css_class("flat");
        inbox_button.set_visible(false);
        inbox_button.connect_clicked(move |_| on_inbox());

        let header = adw::HeaderBar::builder().title_widget(&title_box).build();
        // Only there when a server actually has an agent to ask. A menu
        // rather than a button because there are two different things to want
        // from an agent: a summary of here, or a conversation with it.
        let agent_button = gtk::MenuButton::builder()
            .icon_name("bot-symbolic")
            .tooltip_text("Agents")
            .visible(false)
            .build();
        agent_button.add_css_class("flat");
        let agent_actions = gtk::gio::SimpleActionGroup::new();
        let catch_up = gtk::gio::SimpleAction::new("catch-up", None);
        catch_up.connect_activate(move |_, _| on_agent());
        agent_actions.add_action(&catch_up);
        agent_button.insert_action_group("agent", Some(&agent_actions));

        // What you do to *this* channel, as opposed to the message under the
        // pointer or the list in the sidebar.
        let channel_menu = gtk::gio::Menu::new();
        channel_menu.append(Some("Channel Details…"), Some("win.edit-channel"));
        channel_menu.append(Some("Members…"), Some("win.channel-members"));
        channel_menu.append(Some("Bookmarks…"), Some("win.channel-bookmarks"));
        channel_menu.append(Some("Pinned Messages"), Some("win.pinned-posts"));
        channel_menu.append(Some("Notifications…"), Some("win.channel-notifications"));
        let danger = gtk::gio::Menu::new();
        danger.append(Some("Leave Channel…"), Some("win.leave-channel"));
        danger.append(Some("Archive Channel…"), Some("win.archive-channel"));
        channel_menu.append_section(None, &danger);
        let channel_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Channel Menu")
            .menu_model(&channel_menu)
            .build();
        channel_button.add_css_class("flat");

        header.pack_end(&channel_button);
        header.pack_end(&call_button);
        header.pack_end(&agent_button);
        header.pack_end(&inbox_button);

        // --- call banner
        let call_banner_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        let call_banner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .visible(false)
            .build();
        call_banner.add_css_class("call-banner");
        call_banner.append(&gtk::Image::from_icon_name("call-start-symbolic"));
        call_banner.append(&call_banner_label);

        // The banner is where you notice a call, so it is where joining it
        // belongs — the header button is for starting one.
        let join_button = gtk::Button::with_label("Join");
        join_button.add_css_class("pill");
        join_button.add_css_class("call-banner-join");
        join_button.connect_clicked(move |_| on_call());
        call_banner.append(&join_button);

        // --- messages
        let messages = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
        let render_context: Rc<RefCell<Option<RenderContext>>> = Rc::new(RefCell::new(None));
        let visible_posts = Rc::new(RefCell::new(Vec::new()));
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_bind({
            let render_context = render_context.clone();
            let visible_posts = visible_posts.clone();
            move |_, object| {
                let list_item = object
                    .downcast_ref::<gtk::ListItem>()
                    .expect("factory must receive GtkListItem");
                let Some(item) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let Some(context) = render_context.borrow().clone() else {
                    return;
                };
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                let item = item.borrow::<FeedItem>();
                let post_id = item.post_id().map(str::to_owned);
                let child = render_item(&item, &context);
                child.set_hexpand(true);
                if let Some(post_id) = post_id {
                    visible_posts
                        .borrow_mut()
                        .push((post_id, child.downgrade()));
                }
                list_item.set_child(Some(&child));
            }
        });
        factory.connect_unbind(|_, object| {
            object
                .downcast_ref::<gtk::ListItem>()
                .expect("factory must receive GtkListItem")
                .set_child(gtk::Widget::NONE)
        });

        let selection = gtk::NoSelection::new(Some(messages.clone()));
        let message_list = gtk::ListView::new(Some(selection), Some(factory));
        message_list.add_css_class("message-list");
        message_list.set_single_click_activate(false);
        message_list.set_tab_behavior(gtk::ListTabBehavior::Item);
        // A virtual list needs the full viewport to decide what is visible.
        // Align::End constrains it to a small natural allocation, clipping
        // both rows and long lines instead of merely bottom-aligning content.
        message_list.set_valign(gtk::Align::Fill);
        message_list.set_vexpand(true);
        message_list.set_hexpand(true);
        message_list.set_show_separators(false);
        message_list.set_margin_top(12);
        message_list.set_margin_bottom(12);
        message_list.set_margin_start(12);
        message_list.set_margin_end(12);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            // It must be the direct child or it cannot tell which rows are
            // off-screen and therefore recyclable.
            .child(&message_list)
            .build();
        let older_spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(8)
            .visible(false)
            .build();
        let scroll_overlay = gtk::Overlay::new();
        scroll_overlay.set_vexpand(true);
        scroll_overlay.set_child(Some(&scroller));
        scroll_overlay.add_overlay(&older_spinner);

        {
            let adjustment = scroller.vadjustment();
            messages.connect_items_changed(move |model, position, removed, added| {
                if scroll_trace_enabled() {
                    tracing::info!(
                        target: "matras::scroll",
                        event = "model-items-changed",
                        position,
                        removed,
                        added,
                        items = model.n_items(),
                        value = adjustment.value(),
                        upper = adjustment.upper(),
                        page_size = adjustment.page_size(),
                        "scroll trace"
                    );
                }
            });
        }

        {
            let traced = Rc::new(Cell::new(false));
            message_list.connect_map(move |widget| {
                if !scroll_trace_enabled() || traced.replace(true) {
                    return;
                }
                let Some(clock) = widget.frame_clock() else {
                    return;
                };
                let previous_time = Rc::new(Cell::new(0i64));
                clock.connect_after_paint(move |clock| {
                    let frame_time = clock.frame_time();
                    let previous = previous_time.replace(frame_time);
                    let delta_us = if previous == 0 {
                        0
                    } else {
                        frame_time.saturating_sub(previous)
                    };
                    let timings = clock.current_timings();
                    tracing::trace!(
                        target: "matras::scroll",
                        event = "frame-after-paint",
                        frame = clock.frame_counter(),
                        frame_time_us = frame_time,
                        delta_us,
                        refresh_interval_us = timings
                            .as_ref()
                            .map(|timings| timings.refresh_interval())
                            .unwrap_or_default(),
                        presentation_time_us = timings
                            .as_ref()
                            .map(|timings| timings.presentation_time())
                            .unwrap_or_default(),
                        predicted_presentation_time_us = timings
                            .as_ref()
                            .map(|timings| timings.predicted_presentation_time())
                            .unwrap_or_default(),
                        "scroll trace"
                    );
                });
            });
        }

        let pinned_to_bottom = Rc::new(RefCell::new(true));
        let pagination_armed = Rc::new(Cell::new(true));
        {
            let pinned = pinned_to_bottom.clone();
            let pin_candidate = Rc::new(Cell::new(false));
            let pagination_armed = pagination_armed.clone();
            scroller.vadjustment().connect_value_changed(move |adj| {
                let was_pinned = *pinned.borrow();
                let at_bottom = at_live_edge(adj);
                update_live_edge_pin(adj, &pinned, &pin_candidate);

                if scroll_trace_enabled() {
                    tracing::info!(
                        target: "matras::scroll",
                        event = "adjustment-value-changed",
                        value = adj.value(),
                        upper = adj.upper(),
                        page_size = adj.page_size(),
                        was_pinned,
                        at_bottom,
                        near_history_start = adj.value() < adj.page_size() * 2.0,
                        "scroll trace"
                    );
                }

                if at_bottom || adj.value() > adj.page_size() * 3.0 {
                    pagination_armed.set(true);
                }
                // This deliberately mirrors Fractal: observe GTK's scroll
                // state without installing a competing gesture controller or
                // writing Adjustment.value. The latch makes the threshold
                // edge-triggered while preserving GTK's kinetic scrolling.
                if !at_bottom
                    && pagination_armed.get()
                    && adj.value() < adj.page_size() * 2.0
                    && adj.upper() > adj.page_size()
                {
                    pagination_armed.set(false);
                    on_scrollback();
                }
            });
        }
        let pending_scroll: Rc<Cell<Option<(u64, u32)>>> = Rc::new(Cell::new(None));
        let scroll_epoch = Rc::new(Cell::new(0u64));
        scroller.vadjustment().connect_changed({
            let pending_scroll = pending_scroll.clone();
            let scroll_epoch = scroll_epoch.clone();
            let list = message_list.clone();
            move |adj| {
                if scroll_trace_enabled() {
                    tracing::info!(
                        target: "matras::scroll",
                        event = "adjustment-bounds-changed",
                        value = adj.value(),
                        upper = adj.upper(),
                        page_size = adj.page_size(),
                        "scroll trace"
                    );
                }
                // The list has just been measured, so a request it dropped
                // for want of an allocation can be made again — but not from
                // inside the measure itself, where it is dropped a second
                // time. On the idle right after it, the list is allocated and
                // takes it.
                if let Some(position) = pending_scroll
                    .take()
                    .filter(|(epoch, _)| *epoch == scroll_epoch.get())
                    .map(|(_, position)| position)
                {
                    let list = list.clone();
                    glib::idle_add_local_once(move || {
                        if scroll_trace_enabled() {
                            tracing::warn!(
                                target: "matras::scroll",
                                event = "programmatic-scroll-to",
                                reason = "deferred-until-measured",
                                position,
                                "scroll trace"
                            );
                        }
                        list.scroll_to(position, gtk::ListScrollFlags::NONE, None);
                    });
                }
            }
        });

        // --- composer
        let entry = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .height_request(COMPOSER_SINGLE_LINE_HEIGHT)
            .top_margin(4)
            .bottom_margin(4)
            .left_margin(8)
            .right_margin(8)
            .build();

        let entry_frame = build_composer_frame(&entry);
        entry_frame.add_css_class("composer-entry");

        let send = gtk::Button::builder()
            .icon_name(COMPOSER_SEND_ICON)
            .tooltip_text("Send")
            .valign(gtk::Align::End)
            .build();
        send.add_css_class("suggested-action");
        send.add_css_class("circular");

        let priority_menu = gtk::gio::Menu::new();
        for (label, value) in [
            ("Standard", "standard"),
            ("Important", "important"),
            ("Urgent", "urgent"),
        ] {
            priority_menu.append(Some(label), Some(&format!("composer.priority::{value}")));
        }
        let priority_action = gtk::gio::SimpleAction::new_stateful(
            "priority",
            Some(&String::static_variant_type()),
            &"standard".to_variant(),
        );
        priority_action.connect_activate(|action, target| {
            let Some(value) = target.and_then(|t| t.get::<String>()) else {
                return;
            };
            action.set_state(&value.to_variant());
        });
        let composer_actions = gtk::gio::SimpleActionGroup::new();
        composer_actions.add_action(&priority_action);

        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        composer.insert_action_group("composer", Some(&composer_actions));

        // Keep the high-frequency composer controls reachable without
        // reserving three icon buttons at every viewport width. On a
        // PinePhone those controls used more than 100px beside the text box;
        // group them behind the familiar four-square action button instead.
        let attach_action = gtk::gio::SimpleAction::new("attach", None);
        attach_action.connect_activate(move |_, _| on_attach());
        composer_actions.add_action(&attach_action);
        let schedule_action = gtk::gio::SimpleAction::new("schedule", None);
        schedule_action.connect_activate(move |_, _| on_schedule());
        composer_actions.add_action(&schedule_action);

        let options_menu = composer_options_menu(&priority_menu);
        let options = composer_options_button(&options_menu);

        composer.append(&options);
        composer.append(&entry_frame);
        composer.append(&send);

        let on_send = Rc::new(on_send);
        let submit = {
            let entry = entry.clone();
            let on_send = on_send.clone();
            move || {
                let buffer = entry.buffer();
                let (start, end) = buffer.bounds();
                let text = buffer.text(&start, &end, false).trim().to_string();
                if text.is_empty() {
                    return;
                }
                buffer.set_text("");
                on_send(text);
            }
        };

        send.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });

        // Enter sends; Shift+Enter inserts a newline.
        // The completion popover has first refusal on keys: Enter picks a
        // candidate when the list is open, and only sends when it is not.
        let complete = Rc::new(super::autocomplete::Autocomplete::new(&entry, {
            let on_complete = Rc::new(on_complete);
            move |query| on_complete(query)
        }));

        // Ctrl+V with an image on the clipboard attaches it. Pasting a
        // screenshot is how most images get into a chat, and the alternative
        // is saving it to disk first for no reason.
        let on_files = Rc::new(on_files);
        let paste = gtk::EventControllerKey::new();
        paste.connect_key_pressed({
            let on_files = on_files.clone();
            move |controller, key, _, modifier| {
                let ctrl_v = key == gtk::gdk::Key::v
                    && modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
                if !ctrl_v {
                    return glib::Propagation::Proceed;
                }
                let Some(clipboard) = controller.widget().map(|w| w.clipboard()) else {
                    return glib::Propagation::Proceed;
                };
                // Only claim the keystroke when there really is an image;
                // otherwise the text paste has to go through untouched.
                if !clipboard
                    .formats()
                    .contains_type(gtk::gdk::Texture::static_type())
                {
                    return glib::Propagation::Proceed;
                }

                let on_files = on_files.clone();
                clipboard.read_texture_async(gtk::gio::Cancellable::NONE, move |result| {
                    let Ok(Some(texture)) = result else { return };
                    // The upload path takes paths, so the pasted image lands
                    // in a temp file that the OS cleans up.
                    let path = std::env::temp_dir()
                        .join(format!("matras-paste-{}.png", glib::monotonic_time()));
                    if let Err(e) = texture.save_to_png(&path) {
                        tracing::warn!(error = %e, "could not save the pasted image");
                        return;
                    }
                    on_files(vec![path]);
                });
                glib::Propagation::Stop
            }
        });
        entry.add_controller(paste);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let complete = complete.clone();
            let submit = submit.clone();
            move |_, key, _, modifier| {
                if complete.handle_key(key) {
                    return glib::Propagation::Stop;
                }
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                if enter && !modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                    submit();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        entry.add_controller(keys);

        let restoring = Rc::new(RefCell::new(false));
        entry.buffer().connect_changed({
            let restoring = restoring.clone();
            let entry_frame = entry_frame.clone();
            move |buffer| {
                let lines = buffer.line_count();
                entry_frame.set_height_request(composer_content_height(lines));
                // Restoring a draft is not typing, and must not be reported as
                // either a keystroke or a fresh draft.
                if *restoring.borrow() {
                    return;
                }
                on_typing(buffer.char_count() > 0);
            }
        });

        // Uploaded files wait here until a message carries them.
        let attachments = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(4)
            .visible(false)
            .build();

        let editing: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        // Editing is a mode, and a mode you cannot see is a trap: the banner
        // says so and offers the way out.
        let edit_banner = adw::Banner::builder()
            .title("Editing a message")
            .button_label("Cancel")
            .revealed(false)
            .build();
        edit_banner.connect_button_clicked({
            let editing = editing.clone();
            let entry = entry.clone();
            move |banner| {
                *editing.borrow_mut() = None;
                banner.set_revealed(false);
                entry.buffer().set_text("");
            }
        });

        // Sits between the feed and the composer, reserving no space when
        // empty: a line that appears and disappears must not shove the
        // conversation up and down.
        let typing = gtk::Label::builder()
            .xalign(0.0)
            .margin_start(14)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .build();
        typing.add_css_class("typing-line");
        typing.add_css_class("dim-label");

        // Losing the socket is a state of the whole window, not an event, so
        // it gets a banner that stays up rather than a toast that scrolls by.
        // The text is a socket error, not markup.
        let connection = adw::Banner::builder()
            .use_markup(false)
            .revealed(false)
            .build();

        let conversation = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        conversation.append(&connection);
        conversation.append(&call_banner);
        conversation.append(&scroll_overlay);
        conversation.append(&edit_banner);
        conversation.append(&typing);
        conversation.append(&attachments);
        conversation.append(&composer);

        let placeholder = adw::StatusPage::builder()
            .icon_name("chat-message-new-symbolic")
            .title("No Channel Selected")
            .description("Pick a channel from the sidebar to start reading")
            .build();

        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(32)
            .height_request(32)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&placeholder, Some("empty"));
        stack.add_named(&spinner, Some("loading"));
        stack.add_named(&conversation, Some("conversation"));
        stack.set_visible_child_name("empty");

        // Dropping files anywhere over the conversation attaches them. The
        // target is the whole pane rather than the composer: aiming at a
        // one-line text box is a needlessly precise thing to ask of a drag.
        let drop = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );
        drop.connect_drop({
            let on_files = on_files.clone();
            move |_, value, _, _| {
                let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                let paths: Vec<std::path::PathBuf> =
                    files.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() {
                    return false;
                }
                on_files(paths);
                true
            }
        });
        conversation.add_controller(drop);

        // A toolbar view rather than a plain box: on a narrow window the call
        // dock moves in here as a bottom bar, since the sidebar it normally
        // lives in is off-screen.
        let widget = adw::ToolbarView::builder()
            .bottom_bar_style(adw::ToolbarStyle::RaisedBorder)
            .reveal_bottom_bars(false)
            .build();
        widget.add_top_bar(&header);
        widget.set_content(Some(&stack));

        ChatView {
            widget,
            title,
            subtitle,
            messages,
            message_list,
            visible_posts,
            render_context,
            entry,
            call_button,
            inbox_button,
            inbox_badge,
            call_banner,
            call_banner_label,
            join_button,
            stack,
            loading_spinner: spinner,
            complete,
            priority_action,
            agent_button,
            agent_actions: Rc::new(RefCell::new(vec![catch_up.clone()])),
            typing,
            attachments,
            edit_banner,
            editing,
            restoring,
            showing: RefCell::new(None),
            older_spinner,
            loading: Rc::new(RefCell::new(false)),
            connection,
            pinned_to_bottom,
            pagination_armed,
            pending_scroll,
            scroll_epoch,
        }
    }

    /// Asks the list for a position, remembering the request when the list is
    /// not in a state to act on it.
    fn request_scroll(&self, position: u32) {
        self.message_list
            .scroll_to(position, gtk::ListScrollFlags::NONE, None);
        // Held only while the list has no allocation to scroll within, and
        // stamped with the request it belongs to: by the time the list is
        // measured this may have been superseded by a saved anchor, and
        // re-issuing it then would drag the reader to the bottom the anchor
        // had just moved them away from.
        self.pending_scroll
            .set((self.message_list.height() == 0).then(|| (self.scroll_epoch.get(), position)));
    }

    /// Claims the feed for a new programmatic scroll, invalidating the one
    /// before it.
    fn begin_scroll(&self) -> (u64, Rc<Cell<u64>>) {
        let epoch = self.scroll_epoch.get().wrapping_add(1);
        self.scroll_epoch.set(epoch);
        (epoch, self.scroll_epoch.clone())
    }

    /// Marks the first page of a channel as in flight. Only matters while
    /// there is nothing to show: a reload over existing messages should leave
    /// them on screen rather than blank the pane.
    pub fn set_loading(&self, loading: bool) {
        *self.loading.borrow_mut() = loading;
    }

    pub fn set_typing(&self, names: &[String]) {
        let text = match names {
            [] => String::new(),
            [one] => format!("{one} is typing…"),
            [one, two] => format!("{one} and {two} are typing…"),
            [one, two, ..] => format!("{one}, {two} and others are typing…"),
        };
        self.typing.set_visible(!text.is_empty());
        self.typing.set_text(&text);
    }

    /// `None` means connected. Anything else is shown until it is cleared.
    pub fn set_connection_problem(&self, problem: Option<&str>) {
        match problem {
            Some(text) => {
                self.connection.set_title(text);
                self.connection.set_revealed(true);
            }
            None => self.connection.set_revealed(false),
        }
    }

    /// Rebuilds the agent menu: one entry to summarise this channel, and one
    /// per bot to go and talk to it.
    /// How many people are in the channel, shown beside the topic. It answers
    /// "who can see this" without opening the member list.
    pub fn set_member_count(&self, count: Option<i64>) {
        let topic = self.subtitle.text().to_string();
        let topic = topic.split(" · ").next().unwrap_or("").to_string();
        let line = match count {
            Some(n) if !topic.is_empty() => format!(
                "{topic} · {n} {}",
                super::message::plural(n, "member", "members")
            ),
            Some(n) => format!("{n} {}", super::message::plural(n, "member", "members")),
            None => topic,
        };
        self.subtitle.set_visible(!line.is_empty());
        self.subtitle.set_text(&line);
    }

    pub fn set_agents(&self, bots: &[(String, String)], on_open: impl Fn(String) + 'static) {
        self.agent_button.set_visible(!bots.is_empty());
        if bots.is_empty() {
            return;
        }

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Catch Me Up"), Some("agent.catch-up"));

        let chats = gtk::gio::Menu::new();
        let on_open = Rc::new(on_open);
        let group = gtk::gio::SimpleActionGroup::new();
        for (index, (id, name)) in bots.iter().enumerate() {
            let action_name = format!("chat-{index}");
            let action = gtk::gio::SimpleAction::new(&action_name, None);
            action.connect_activate({
                let on_open = on_open.clone();
                let id = id.clone();
                move |_, _| on_open(id.clone())
            });
            group.add_action(&action);
            chats.append(Some(name), Some(&format!("agent.{action_name}")));
        }
        menu.append_section(None, &chats);

        // Rebuilding replaces the action group, so the fixed entries have to
        // be carried over or "Catch me up" stops working after the first
        // refresh.
        for action in self.agent_actions.borrow().iter() {
            group.add_action(action);
        }
        self.agent_button.set_menu_model(Some(&menu));
        self.agent_button.insert_action_group("agent", Some(&group));
    }

    /// Redraws the row of files waiting to go out with the next message.
    pub fn set_attachments(
        &self,
        files: &[(String, String)],
        on_remove: impl Fn(String) + 'static,
    ) {
        while let Some(child) = self.attachments.first_child() {
            self.attachments.remove(&child);
        }
        self.attachments.set_visible(!files.is_empty());

        let on_remove = Rc::new(on_remove);
        for (id, name) in files {
            let label = gtk::Label::builder()
                .label(name)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(24)
                .build();
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text("Remove")
                .build();
            remove.add_css_class("flat");
            remove.add_css_class("circular");
            remove.connect_clicked({
                let on_remove = on_remove.clone();
                let id = id.clone();
                move |_| on_remove(id.clone())
            });

            let chip = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .build();
            chip.add_css_class("attachment-chip");
            chip.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
            chip.append(&label);
            chip.append(&remove);
            self.attachments.append(&chip);
        }
    }

    /// A file being uploaded shows as a chip that is not yet removable.
    pub fn set_uploading(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.attachments.set_visible(true);
        let spinner = gtk::Spinner::new();
        spinner.start();
        let chip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        chip.add_css_class("attachment-chip");
        chip.append(&spinner);
        chip.append(&gtk::Label::new(Some(&match count {
            1 => "Uploading…".to_string(),
            n => format!("Uploading {n} files…"),
        })));
        self.attachments.append(&chip);
    }

    /// Puts the composer into edit mode for an existing post.
    ///
    /// Editing reuses the composer rather than opening a second one: there is
    /// only ever one message being written at a time, and a separate box would
    /// be a second place to lose text in.
    pub fn begin_edit(&self, post_id: &str, text: &str) {
        *self.editing.borrow_mut() = Some(post_id.to_string());
        self.set_composer_text(text);
        self.edit_banner.set_revealed(true);
        self.entry.grab_focus();
    }

    /// Leaves edit mode, clearing the composer.
    pub fn end_edit(&self) {
        *self.editing.borrow_mut() = None;
        self.edit_banner.set_revealed(false);
        self.set_composer_text("");
    }

    /// The post being edited, if any.
    pub fn editing(&self) -> Option<String> {
        self.editing.borrow().clone()
    }

    /// Answers an outstanding completion query.
    pub fn set_completions(&self, items: Vec<super::autocomplete::Candidate>) {
        self.complete.set_candidates(items);
    }

    /// The priority for the message being written: "", "important" or
    /// "urgent". Empty means standard, which is what the server expects.
    pub fn priority(&self) -> String {
        match self.priority_action.state().and_then(|s| s.get::<String>()) {
            Some(value) if value != "standard" => value,
            _ => String::new(),
        }
    }

    /// Back to standard once a message has gone out. Priority is per message,
    /// and a sticky "urgent" would quietly escalate everything after it.
    pub fn reset_priority(&self) {
        self.priority_action.set_state(&"standard".to_variant());
    }

    /// What is in the composer right now.
    pub fn composer_text(&self) -> String {
        let buffer = self.entry.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Puts a draft back. Setting the buffer fires `changed`, which would
    /// otherwise be read as the user typing and save the draft straight back —
    /// hence the guard.
    pub fn set_composer_text(&self, text: &str) {
        if self.composer_text() == text {
            return;
        }
        *self.restoring.borrow_mut() = true;
        self.entry.buffer().set_text(text);
        // Put the cursor where they left off, not at the front.
        let buffer = self.entry.buffer();
        buffer.place_cursor(&buffer.end_iter());
        *self.restoring.borrow_mut() = false;
    }

    pub fn focus_composer(&self) {
        self.entry.grab_focus();
    }

    pub fn set_calls_available(&self, available: bool, reason: Option<&str>) {
        self.call_button.set_sensitive(available);
        if let Some(reason) = reason {
            self.call_button.set_tooltip_text(Some(reason));
        }
    }

    /// Reflects our own membership. Everything you can *do* inside a call is
    /// in the dock now; the header only starts, joins or ends one.
    pub fn set_in_call(&self, in_call: bool, ongoing: bool) {
        self.call_button.set_icon_name(if in_call {
            "call-stop-symbolic"
        } else {
            "call-start-symbolic"
        });
        self.call_button
            .set_tooltip_text(Some(match (in_call, ongoing) {
                (true, _) => "Leave Call",
                // A call is running here and we are outside it: the button
                // joins, which is not something an icon alone ever says.
                (false, true) => "Join Call",
                (false, false) => "Start Call",
            }));
        set_active(&self.call_button, in_call, "destructive-action");
        if in_call {
            self.call_button.set_sensitive(true);
        } else {
            // Accent while a call is waiting for you, so the header agrees
            // with the banner instead of looking idle.
            set_active(&self.call_button, ongoing, "suggested-action");
        }
        self.join_button.set_visible(!in_call);
    }

    pub fn set_call_in_progress(&self, participants: Option<usize>) {
        match participants {
            Some(n) => {
                self.call_banner_label.set_text(&match n {
                    0 => "A call is starting".to_string(),
                    1 => "1 person is in a call".to_string(),
                    n => format!("{n} people are in a call"),
                });
                self.call_banner.set_visible(true);
            }
            None => self.call_banner.set_visible(false),
        }
    }

    fn set_inbox_count(&self, count: i64) {
        self.inbox_badge.set_visible(count > 0);
        self.inbox_badge.set_text(&count.to_string());
        self.inbox_button.set_tooltip_text(Some(&if count > 0 {
            format!("{count} unread — mentions and threads")
        } else {
            "Mentions and Threads".to_string()
        }));
    }

    pub fn set_inbox_button_visible(&self, visible: bool) {
        self.inbox_button.set_visible(visible);
    }

    /// Scrolls the feed so a particular message is in view and briefly
    /// highlights it. Returns false when that message is not on screen — the
    /// caller then knows it has to fetch further back first.
    pub fn scroll_to_post(&self, post_id: &str) -> bool {
        let Some(position) = (0..self.messages.n_items()).find(|&position| {
            model_item(&self.messages, position)
                .and_then(|i| i.post_id().map(str::to_owned))
                .as_deref()
                == Some(post_id)
        }) else {
            return false;
        };

        if let Some(FeedItem::Post {
            post,
            grouped,
            revision,
            ..
        }) = model_item(&self.messages, position)
        {
            let replacement = glib::BoxedAnyObject::new(FeedItem::Post {
                post,
                grouped,
                highlight: true,
                revision,
            });
            self.messages.splice(position, 1, &[replacement]);
        }
        if scroll_trace_enabled() {
            tracing::warn!(
                target: "matras::scroll",
                event = "programmatic-scroll-to",
                reason = "explicit-post-navigation",
                post_id,
                position,
                "scroll trace"
            );
        }
        self.message_list
            .scroll_to(position, gtk::ListScrollFlags::FOCUS, None);

        let model = self.messages.clone();
        let id = post_id.to_string();
        glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            let Some(position) = (0..model.n_items()).find(|&position| {
                model_item(&model, position)
                    .and_then(|i| i.post_id().map(str::to_owned))
                    .as_deref()
                    == Some(id.as_str())
            }) else {
                return;
            };
            if let Some(FeedItem::Post {
                post,
                grouped,
                revision,
                ..
            }) = model_item(&model, position)
            {
                model.splice(
                    position,
                    1,
                    &[glib::BoxedAnyObject::new(FeedItem::Post {
                        post,
                        grouped,
                        highlight: false,
                        revision,
                    })],
                );
            }
        });
        true
    }

    /// The stable item currently crossing 30% of the viewport. Returning a
    /// post id rather than pixels makes the position survive fonts, window
    /// width and late media layout.
    pub fn current_anchor(&self) -> Option<(String, Option<String>)> {
        let channel_id = self.showing.borrow().clone()?;
        if *self.pinned_to_bottom.borrow() {
            return Some((channel_id, None));
        }
        let target_y = self.message_list.height() as f32 * 0.30;
        let mut best: Option<(String, f32)> = None;
        self.visible_posts.borrow_mut().retain(|(post_id, weak)| {
            let Some(widget) = weak.upgrade() else {
                return false;
            };
            if !widget.is_mapped() {
                return true;
            }
            if let Some(bounds) = widget.compute_bounds(&self.message_list) {
                let distance = (bounds.y() - target_y).abs();
                if best.as_ref().is_none_or(|(_, old)| distance < *old) {
                    best = Some((post_id.clone(), distance));
                }
            }
            true
        });
        Some((channel_id, best.map(|(post_id, _)| post_id)))
    }

    /// Restores a saved post and then aligns its top to 30% of the viewport.
    /// This is an explicit navigation operation, not pagination compensation;
    /// normal kinetic scrolling never writes Adjustment.value.
    pub fn restore_anchor(&self, post_id: &str) -> bool {
        let Some(position) = (0..self.messages.n_items()).find(|&position| {
            model_item(&self.messages, position)
                .and_then(|item| item.post_id().map(str::to_owned))
                .as_deref()
                == Some(post_id)
        }) else {
            return false;
        };
        if scroll_trace_enabled() {
            tracing::warn!(
                target: "matras::scroll",
                event = "programmatic-scroll-to",
                reason = "restore-saved-anchor",
                post_id,
                position,
                "scroll trace"
            );
        }
        let wanted = post_id.to_string();
        let rows = self.visible_posts.clone();
        let Some(adjustment) = self.message_list.vadjustment() else {
            return false;
        };
        let (epoch, epochs) = self.begin_scroll();
        self.request_scroll(position);

        let attempts = Rc::new(Cell::new(0u8));
        let started = std::time::Instant::now();
        self.message_list.add_tick_callback(move |list, _| {
            if epochs.get() != epoch {
                return glib::ControlFlow::Break;
            }
            // Nothing can be placed against a list that has not been measured:
            // every row reports y = 0, 30% of a zero height is the top, and
            // the restore "succeeds" by parking the feed at the beginning of
            // the block — the exact position it exists to avoid. Frames spent
            // waiting for the first allocation do not count against the
            // budget either; a cold window can tick a hundred times before it
            // has a size, which used to exhaust the budget before there was
            // anything to scroll within. Time-bounded rather than
            // frame-bounded, so a window that never gets one stops asking.
            if list.height() == 0 {
                return match started.elapsed() < std::time::Duration::from_secs(2) {
                    true => glib::ControlFlow::Continue,
                    false => glib::ControlFlow::Break,
                };
            }
            attempts.set(attempts.get() + 1);
            let widget = rows
                .borrow()
                .iter()
                .find_map(|(post_id, weak)| (post_id == &wanted).then(|| weak.upgrade()).flatten());
            if let Some(widget) = widget {
                if let Some(bounds) = widget.compute_bounds(list) {
                    let target = list.height() as f64 * 0.30;
                    let value = adjustment.value() + bounds.y() as f64 - target;
                    let maximum = (adjustment.upper() - adjustment.page_size()).max(0.0);
                    adjustment.set_value(value.clamp(0.0, maximum));
                    return glib::ControlFlow::Break;
                }
            }
            // The row is not realised, so the scroll has not happened: the
            // request was made in the same frame as the splice that brought
            // the posts in, and the list had no allocation to act on. Ask
            // again now that it has one.
            list.scroll_to(position, gtk::ListScrollFlags::NONE, None);
            if attempts.get() >= 8 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        true
    }

    fn scroll_to_newest(&self, reason: &str) {
        let position = self.messages.n_items().saturating_sub(1);
        if scroll_trace_enabled() {
            tracing::warn!(
                target: "matras::scroll",
                event = "programmatic-scroll-to",
                reason,
                position,
                "scroll trace"
            );
        }
        let Some(adjustment) = self.message_list.vadjustment() else {
            return;
        };
        let (epoch, epochs) = self.begin_scroll();
        self.request_scroll(position);

        let attempts = Rc::new(Cell::new(0u8));
        let started = std::time::Instant::now();
        self.message_list.add_tick_callback(move |list, _| {
            if epochs.get() != epoch {
                return glib::ControlFlow::Break;
            }
            // Same reason as the anchor restore: before the first allocation
            // the bottom is zero and every frame spent here is a frame of the
            // budget wasted on a list that cannot move yet.
            if list.height() == 0 {
                return match started.elapsed() < std::time::Duration::from_secs(2) {
                    true => glib::ControlFlow::Continue,
                    false => glib::ControlFlow::Break,
                };
            }
            attempts.set(attempts.get() + 1);
            let bottom = (adjustment.upper() - adjustment.page_size()).max(0.0);
            if adjustment.value() < bottom {
                adjustment.set_value(bottom);
            }
            // Four frames is about one re-measure pass and short enough that
            // it cannot fight a reader who starts scrolling away.
            if attempts.get() >= 4 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    /// Whether a post belongs in the feed on screen at all — the question
    /// both "should this row be appended" and "should sending it move the
    /// reader" turn on.
    fn feed_shows(showing: Option<&str>, post: &Post, crt: bool) -> bool {
        showing == Some(post.channel_id.as_str())
            && !post.is_deleted()
            && !post.is_system()
            && !(crt && post.is_reply())
    }

    /// Puts the feed at the bottom because *you* posted. Following only when
    /// the reader was already at the live edge is the right rule for someone
    /// else's message and the wrong one for your own: every other client
    /// shows you what you just sent, wherever you had been reading. It
    /// deliberately ignores `at_latest` too — the post is appended to the
    /// block on screen, so the end of that block is where it is.
    ///
    /// Does nothing when the post is not in this feed (a reply CRT keeps in
    /// its thread, or a thread whose root lives in another channel), so
    /// answering in the thread panel does not drag the channel behind it.
    pub fn follow_own_post(&self, post: &Post, state: &SharedState) {
        if !Self::feed_shows(
            self.showing.borrow().as_deref(),
            post,
            state.borrow().crt_enabled,
        ) {
            return;
        }
        *self.pinned_to_bottom.borrow_mut() = true;
        self.scroll_to_newest("own-message-sent");
    }

    /// Says, at the top of the feed, that the page before this one is on its
    /// way. Without it a scrollback that takes a moment looks like the start
    /// of the channel.
    pub fn set_loading_older(&self, loading: bool) {
        if scroll_trace_enabled() {
            tracing::info!(
                target: "matras::scroll",
                event = "older-loading-indicator",
                loading,
                "scroll trace"
            );
        }
        if loading {
            self.older_spinner.set_visible(true);
            self.older_spinner.start();
        } else {
            self.older_spinner.stop();
            self.older_spinner.set_visible(false);
        }
    }

    /// A network error should be retryable on the next adjustment change even
    /// if the reader is still close to the history edge.
    pub fn retry_older_on_next_edge_change(&self) {
        self.pagination_armed.set(true);
    }

    fn replace_feed(
        &self,
        items: Vec<FeedItem>,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) {
        *self.render_context.borrow_mut() = Some(RenderContext {
            state: state.clone(),
            avatars: avatars.clone(),
            actions: actions.clone(),
        });

        let old: Vec<u64> = (0..self.messages.n_items())
            .filter_map(|position| model_item(&self.messages, position))
            .map(|item| item.fingerprint())
            .collect();
        let new: Vec<u64> = items.iter().map(FeedItem::fingerprint).collect();
        let (prefix, removed, end) = splice_window(&old, &new);
        if removed == 0 && prefix == end {
            return;
        }
        let additions: Vec<glib::BoxedAnyObject> = items[prefix..end]
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        if scroll_trace_enabled() {
            let adjustment = self.message_list.vadjustment();
            tracing::info!(
                target: "matras::scroll",
                event = "feed-splice",
                old_items = old.len(),
                new_items = new.len(),
                prefix,
                removed,
                added = additions.len(),
                value = adjustment.as_ref().map(gtk::Adjustment::value),
                upper = adjustment.as_ref().map(gtk::Adjustment::upper),
                page_size = adjustment.as_ref().map(gtk::Adjustment::page_size),
                "scroll trace"
            );
        }
        self.messages
            .splice(prefix as u32, removed as u32, &additions);
    }

    /// Rebinds only rows that use a newly-arrived non-avatar resource.
    /// Avatars update their weakly registered widgets directly; file previews,
    /// video heads and custom emoji need their containing row rebuilt because
    /// they can change the row's widget shape.
    pub fn refresh_resource(
        &self,
        key: &str,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) {
        if !key.contains(':') {
            return;
        }
        *self.render_context.borrow_mut() = Some(RenderContext {
            state: state.clone(),
            avatars: avatars.clone(),
            actions: actions.clone(),
        });
        let affected: Vec<(u32, FeedItem)> = (0..self.messages.n_items())
            .filter_map(|position| {
                let item = model_item(&self.messages, position)?;
                item.uses_resource(key).then_some((position, item))
            })
            .collect();
        if scroll_trace_enabled() && !affected.is_empty() {
            tracing::info!(
                target: "matras::scroll",
                event = "resource-rows-refresh",
                key,
                rows = affected.len(),
                "scroll trace"
            );
        }
        for (position, item) in affected {
            self.messages
                .splice(position, 1, &[glib::BoxedAnyObject::new(item)]);
        }
    }

    /// Adds the just-fetched older page to the top of the feed without
    /// touching a single row that was already on screen.
    ///
    /// `refresh` used to be the only way in: a scrollback page landing called
    /// it same as everything else, which meant tearing down and rebuilding
    /// every row already read — avatars, rich text, reaction strips, the
    /// works — for the sake of showing `posts.len()` new ones above them.
    /// Scroll to the top of a busy channel a few times and that rebuild grows
    /// with everything ever paged in, which is exactly the kind of
    /// main-thread stall GNOME calls "not responding". Nothing about an
    /// already-rendered row changes when older history arrives, so this only
    /// builds the new rows and splices them in.
    ///
    /// `merged` is the channel's full post list (oldest first) *after* the
    /// new page was folded in — the same shape `refresh` itself already works
    /// from, so a caller that already has the feed does not have to reshape it
    /// to use this.
    ///
    /// `at_oldest_title` carries both the "did this page reach the very
    /// start of the channel" flag and the name that goes on the label if so
    /// — folded into one `Option` rather than two parameters, since the name
    /// is never wanted without the flag.
    pub fn prepend_older(
        &self,
        merged: &[Post],
        at_oldest_title: Option<&str>,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
        restored: impl FnOnce() + 'static,
    ) {
        // Keep the existing ListStore objects and let GtkListView retain its
        // own scroll anchor across the prefix splice. Do not intercept smooth
        // scrolling or write Adjustment.value here: both fight GTK's kinetic
        // scrolling and can turn one gesture into a jump through the new page.
        let crt = state.borrow().crt_enabled;
        let items = build_feed_items(merged, state, crt, None, at_oldest_title);
        self.replace_feed(items, state, avatars, actions);
        restored();
    }

    /// Appends a just-arrived post as one row, instead of rebuilding the
    /// whole feed for it — the live-event counterpart of `prepend_older`.
    /// Only sound when it lands at the very end of the channel already on
    /// screen and there is a real message row to append after; anything else
    /// (a different channel, CRT hiding a reply, an empty or still-loading
    /// feed) returns false so the caller falls back to `refresh`.
    pub fn append_post(
        &self,
        post: &Post,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) -> bool {
        if !Self::feed_shows(
            self.showing.borrow().as_deref(),
            post,
            state.borrow().crt_enabled,
        ) {
            return false;
        }

        let Some((
            _,
            FeedItem::Post {
                post: prev_post, ..
            },
        )) = (0..self.messages.n_items()).rev().find_map(|position| {
            model_item(&self.messages, position)
                .and_then(|item| matches!(item, FeedItem::Post { .. }).then_some((position, item)))
        })
        else {
            return false;
        };

        let day = message::format_day(post.create_at);
        let same_day = message::format_day(prev_post.create_at) == day;
        if !same_day {
            self.messages
                .append(&glib::BoxedAnyObject::new(FeedItem::Day(day)));
        }

        *self.render_context.borrow_mut() = Some(RenderContext {
            state: state.clone(),
            avatars: avatars.clone(),
            actions: actions.clone(),
        });
        self.messages
            .append(&glib::BoxedAnyObject::new(FeedItem::Post {
                post: Rc::new(post.clone()),
                grouped: groups_with(Some(prev_post.as_ref()), post, state),
                highlight: false,
                revision: post_revision(post, state),
            }));

        if *self.pinned_to_bottom.borrow() {
            self.scroll_to_newest("live-append-while-pinned");
        }
        true
    }

    /// Rebuilds exactly the row for `post`, in place — used for an edit, a
    /// reaction, or an acknowledgement, none of which move a post or change
    /// its neighbours. Returns false when the row is not on screen (a
    /// different channel, or history not paged in this far), so the caller
    /// falls back to `refresh`.
    pub fn replace_post(
        &self,
        post: &Post,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) -> bool {
        let Some(position) = (0..self.messages.n_items()).find(|&position| {
            model_item(&self.messages, position)
                .and_then(|item| item.post_id().map(str::to_owned))
                .as_deref()
                == Some(post.id.as_str())
        }) else {
            return false;
        };
        let prev_post = (0..position)
            .rev()
            .filter_map(|at| model_item(&self.messages, at))
            .find_map(|item| match item {
                FeedItem::Post { post, .. } => Some(post),
                _ => None,
            });
        *self.render_context.borrow_mut() = Some(RenderContext {
            state: state.clone(),
            avatars: avatars.clone(),
            actions: actions.clone(),
        });
        self.messages.splice(
            position,
            1,
            &[glib::BoxedAnyObject::new(FeedItem::Post {
                post: Rc::new(post.clone()),
                grouped: groups_with(prev_post.as_deref(), post, state),
                highlight: false,
                revision: post_revision(post, state),
            })],
        );
        true
    }

    /// Drops the row for a deleted post, if it is on screen. The post itself
    /// already left the feed with the delete event, so there is nothing to
    /// rebuild it into. A day separator or system block that turns out to
    /// have nothing left under it after this is a small cosmetic leftover,
    /// gone on the next full `refresh` (a channel switch, for instance).
    pub fn remove_post(&self, post_id: &str) {
        if let Some(position) = (0..self.messages.n_items()).find(|&position| {
            model_item(&self.messages, position)
                .and_then(|item| item.post_id().map(str::to_owned))
                .as_deref()
                == Some(post_id)
        }) {
            self.messages.remove(position);
        }
    }

    /// Redraws the whole feed for the current channel.
    pub fn refresh(&self, state: &SharedState, avatars: &Avatars, actions: &MessageActions) {
        let st = state.borrow();
        let inbox_count = (st
            .mentions
            .len()
            .min(99)
            .max(st.unread_threads().max(0) as usize) as i64)
            + st.reaction_unread;
        let Some(channel_id) = st.current_channel.clone() else {
            drop(st);
            self.set_inbox_count(inbox_count);
            self.loading_spinner.stop();
            self.stack.set_visible_child_name("empty");
            return;
        };
        let Some(channel) = st.channel(&channel_id).cloned() else {
            drop(st);
            self.set_inbox_count(inbox_count);
            self.loading_spinner.stop();
            self.stack.set_visible_child_name("empty");
            return;
        };

        // A channel with no posts *yet* and a fetch in flight is loading; one
        // with no posts and nothing in flight is genuinely empty.
        let waiting =
            *self.loading.borrow() && st.feeds.get(&channel_id).is_none_or(|f| f.posts.is_empty());
        if waiting {
            self.loading_spinner.start();
        } else {
            self.loading_spinner.stop();
        }
        self.stack
            .set_visible_child_name(if waiting { "loading" } else { "conversation" });
        self.title.set_text(&st.channel_title(&channel));
        let header_line = channel.header.lines().next().unwrap_or("").to_string();
        self.subtitle.set_text(&header_line);
        self.subtitle.set_visible(!header_line.is_empty());

        // Only when something is actually unread: the line is a landmark, not
        // a permanent divider, and it must not sit under every channel you
        // have already read.
        let unread_since = st
            .memberships
            .get(&channel_id)
            .filter(|_| st.unread(&channel_id).is_unread())
            .and_then(|m| m.last_viewed_at)
            .filter(|at| *at > 0);
        // Whether this is a redraw of what is already on screen, as opposed
        // to arriving in a different channel — the scroll position is only
        // worth keeping in the first case.
        let same_channel = self.showing.borrow().as_deref() == Some(channel_id.as_str());
        if !same_channel {
            *self.pinned_to_bottom.borrow_mut() = true;
            self.pagination_armed.set(true);
        }
        *self.showing.borrow_mut() = Some(channel_id.clone());

        let empty = crate::state::ChannelFeed::default();
        let feed = st.feeds.get(&channel_id).unwrap_or(&empty);
        let posts = feed.posts.clone();
        let at_latest = feed.at_latest || posts.is_empty();
        let at_oldest = feed.at_oldest;
        let channel_title = st.channel_title(&channel);
        let crt = st.crt_enabled;
        drop(st);

        self.set_inbox_count(inbox_count);
        let items = build_feed_items(
            &posts,
            state,
            crt,
            unread_since,
            at_oldest.then_some(channel_title.as_str()),
        );
        if scroll_trace_enabled() {
            tracing::info!(
                target: "matras::scroll",
                event = "feed-refresh",
                channel_id,
                same_channel,
                at_latest,
                pinned = *self.pinned_to_bottom.borrow(),
                posts = posts.len(),
                model_items = self.messages.n_items(),
                "scroll trace"
            );
        }
        self.replace_feed(items, state, avatars, actions);

        // Two different questions, which used to share one answer. *Following*
        // the feed as messages arrive is only right when the block runs to the
        // newest message, or a reader parked in history would be dragged
        // forward. *Arriving* in a channel is not following: the reader has
        // asked for it, and the end of what is loaded is the closest thing to
        // now that exists, whereas the top of the block is 60 messages of last
        // session's cache ago. A saved anchor overrides this — the caller
        // restores it right after this returns, and the later scroll wins.
        if (at_latest || !same_channel) && *self.pinned_to_bottom.borrow() {
            // GtkListView owns deferred measurement of its virtual rows. Its
            // position API can target an item before layout; adjustment.upper
            // cannot and left a freshly-opened channel at its oldest message.
            self.scroll_to_newest(match (same_channel, at_latest) {
                (true, _) => "same-channel-refresh-while-pinned",
                (false, true) => "channel-open-at-latest",
                (false, false) => "channel-open-at-end-of-block",
            });
        }
    }
}

/// How close to the end still counts as being at the live edge.
const LIVE_EDGE_SLACK: f64 = 32.0;

pub(super) fn at_live_edge(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - LIVE_EDGE_SLACK
}

/// Folds one adjustment change into `pinned`, the flag that decides whether an
/// arriving message may move the list.
///
/// Leaving the bottom drops the pin at once; arriving at it only counts if it
/// is still true 80 ms later. A virtual list reports the exact bottom briefly
/// while it re-estimates row heights, and taking that transient at face value
/// is what lets the next arriving post yank a reader out of what they were
/// reading.
pub(super) fn update_live_edge_pin(
    adjustment: &gtk::Adjustment,
    pinned: &Rc<RefCell<bool>>,
    candidate: &Rc<Cell<bool>>,
) {
    if !at_live_edge(adjustment) {
        *pinned.borrow_mut() = false;
        candidate.set(false);
        return;
    }
    if *pinned.borrow() || candidate.replace(true) {
        return;
    }
    let adjustment = adjustment.clone();
    let pinned = pinned.clone();
    let candidate = candidate.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
        if candidate.replace(false) && at_live_edge(&adjustment) {
            *pinned.borrow_mut() = true;
        }
    });
}

/// The one window of a list that actually changed, as `(start, removed, end)`:
/// everything before `start`, and everything from `end` on, has the same
/// fingerprint in both. Splicing that window instead of replacing the model is
/// what keeps one appended message from rebuilding every row above it — and,
/// in a GtkListView, what keeps the scroll anchor alive across the update.
pub(super) fn splice_window(old: &[u64], new: &[u64]) -> (usize, usize, usize) {
    let mut prefix = 0usize;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (
        prefix,
        old.len().saturating_sub(prefix + suffix),
        new.len().saturating_sub(suffix),
    )
}

/// Puts a virtual list on its last row and holds it there while GtkListView
/// finishes measuring.
///
/// `scroll_to` on its own is not enough for a row spliced in this frame: the
/// list has not measured it yet, so it places the target exactly on the bottom
/// edge of the viewport, concludes nothing has to move, and by the time the row
/// has a height the request is spent. `upper` grew, the value did not, and the
/// message sat just below the fold — which is what made a message you had only
/// just sent fail to show up. Nudging the adjustment over the following frames
/// is the same explicit-navigation escape hatch `restore_anchor` already uses;
/// it never moves the list upwards, so a re-measure that shrinks the estimate
/// cannot jump anyone into history.
pub(super) fn scroll_list_to_newest(list: &gtk::ListView, items: u32, reason: &str) {
    let position = items.saturating_sub(1);
    if scroll_trace_enabled() {
        tracing::warn!(
            target: "matras::scroll",
            event = "programmatic-scroll-to",
            reason,
            position,
            "scroll trace"
        );
    }
    list.scroll_to(position, gtk::ListScrollFlags::NONE, None);

    let Some(adjustment) = list.vadjustment() else {
        return;
    };
    let attempts = Rc::new(Cell::new(0u8));
    list.add_tick_callback(move |_, _| {
        attempts.set(attempts.get() + 1);
        let bottom = (adjustment.upper() - adjustment.page_size()).max(0.0);
        if adjustment.value() < bottom {
            adjustment.set_value(bottom);
        }
        // Four frames is about one re-measure pass and short enough that it
        // cannot fight a reader who starts scrolling away.
        if attempts.get() >= 4 {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn build_composer_frame(entry: &gtk::TextView) -> gtk::Frame {
    gtk::Frame::builder().hexpand(true).child(entry).build()
}

/// The compact composer menu keeps attachment, scheduling, and priority
/// available while letting the message field use almost all of a phone width.
fn composer_options_menu(priority_menu: &gtk::gio::Menu) -> gtk::gio::Menu {
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Attach File…"), Some("composer.attach"));
    menu.append(Some("Send Later…"), Some("composer.schedule"));
    menu.append_submenu(Some("Message Priority"), priority_menu);
    menu
}

fn composer_options_button(menu: &gtk::gio::Menu) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("More Message Options")
        .menu_model(menu)
        .valign(gtk::Align::End)
        .width_request(COMPOSER_OPTIONS_BUTTON_WIDTH)
        .build();
    button.add_css_class("flat");
    button.add_css_class("circular");
    button
}

struct HashWriter<'a>(&'a mut DefaultHasher);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything that changes one rendered post, reduced to one value while the
/// item is built. JSON is streamed straight into the hasher, so even complex
/// Mattermost props and metadata allocate no intermediate Vec/String.
pub(super) fn post_revision(post: &Post, state: &SharedState) -> u64 {
    let mut hash = DefaultHasher::new();
    let _ = serde_json::to_writer(HashWriter(&mut hash), post);
    let st = state.borrow();
    st.author_name(post).hash(&mut hash);
    st.presence(&post.user_id).hash(&mut hash);
    st.saved_posts.contains(&post.id).hash(&mut hash);
    st.me.id.hash(&mut hash);
    if let Some(status) = st
        .users
        .get(&post.user_id)
        .and_then(|user| user.custom_status())
    {
        status.emoji.hash(&mut hash);
        status.text.hash(&mut hash);
        status.expires_at.hash(&mut hash);
    }
    hash.finish()
}

fn flush_system(run: &mut Vec<&Post>, items: &mut Vec<FeedItem>) {
    if run.is_empty() {
        return;
    }
    items.push(FeedItem::System(Rc::new(
        run.iter().map(|post| (*post).clone()).collect(),
    )));
    run.clear();
}

fn build_feed_items(
    posts: &[Post],
    state: &SharedState,
    crt: bool,
    unread_since: Option<Millis>,
    at_oldest_title: Option<&str>,
) -> Vec<FeedItem> {
    let mut items = Vec::with_capacity(posts.len() + 4);
    if let Some(title) = at_oldest_title.filter(|_| !posts.is_empty()) {
        items.push(FeedItem::Start(title.to_string()));
    }
    if posts.is_empty() {
        items.push(FeedItem::Empty);
        return items;
    }

    let mut unread_drawn = false;
    let mut last_author: Option<String> = None;
    let mut last_at: Millis = 0;
    let mut last_day: Option<String> = None;
    let mut system_run: Vec<&Post> = Vec::new();

    for post in posts {
        if post.is_deleted() || (crt && post.is_reply()) {
            continue;
        }
        if unread_since.is_some_and(|at| post.create_at > at) && !unread_drawn {
            flush_system(&mut system_run, &mut items);
            items.push(FeedItem::Unread);
            unread_drawn = true;
        }

        let day = message::format_day(post.create_at);
        if last_day.as_deref() != Some(day.as_str()) {
            flush_system(&mut system_run, &mut items);
            items.push(FeedItem::Day(day.clone()));
            last_day = Some(day);
            last_author = None;
        }

        if post.is_system() {
            system_run.push(post);
            last_author = None;
            continue;
        }

        flush_system(&mut system_run, &mut items);
        let author = state.borrow().author_name(post);
        let grouped = last_author.as_deref() == Some(author.as_str())
            && post.create_at.saturating_sub(last_at) < message::GROUPING_WINDOW_MS;
        items.push(FeedItem::Post {
            post: Rc::new(post.clone()),
            grouped,
            highlight: false,
            revision: post_revision(post, state),
        });
        last_author = Some(author);
        last_at = post.create_at;
    }
    flush_system(&mut system_run, &mut items);
    items
}

/// The label marking the true start of a channel's history. Shared by a full
/// `refresh` and by `prepend_older`, which only gets to show it once a
/// scrollback page turns out to be the last one.
fn start_label(channel_title: &str) -> gtk::Widget {
    let start = gtk::Label::builder()
        .label(format!("This is the beginning of {channel_title}"))
        .xalign(0.0)
        .wrap(true)
        .margin_bottom(8)
        .build();
    start.add_css_class("dim-label");
    start.upcast()
}

/// Whether `post` groups with the post drawn immediately before it: same
/// author, same day, close enough in time — the rule `refresh` and
/// `prepend_older` apply while walking the whole feed, answered here for one
/// row instead. Author is compared by name rather than user id because a
/// webhook can post under a different name per message with the same id.
fn groups_with(prev: Option<&Post>, post: &Post, state: &SharedState) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    message::format_day(post.create_at) == message::format_day(prev.create_at)
        && state.borrow().author_name(post) == state.borrow().author_name(prev)
        && post.create_at.saturating_sub(prev.create_at) < message::GROUPING_WINDOW_MS
}

/// Colours a header button while its feature is on.
///
/// `.flat` paints the background transparent and comes later in the Adwaita
/// stylesheet than `.suggested-action`, so a flat button given an accent class
/// stays stubbornly grey. Dropping `.flat` is what actually shows the state.
fn set_active(button: &gtk::Button, active: bool, class: &str) {
    if active {
        button.remove_css_class("flat");
        button.add_css_class(class);
    } else {
        button.remove_css_class(class);
        button.add_css_class("flat");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mattermost_api::models::{ClientConfig, User};
    use mattermost_api::Client;

    fn state_with(users: &[User]) -> SharedState {
        let client = Client::new("http://x.test").unwrap();
        let mut app =
            crate::state::AppState::new(client, User::default(), ClientConfig::default(), false);
        for user in users {
            app.users.insert(user.id.clone(), user.clone());
        }
        Rc::new(RefCell::new(app))
    }

    fn post(user_id: &str, at: Millis) -> Post {
        Post {
            id: format!("p{at}"),
            user_id: user_id.into(),
            create_at: at,
            ..Default::default()
        }
    }

    #[test]
    fn an_unchanged_list_splices_nothing() {
        assert_eq!(splice_window(&[1, 2, 3], &[1, 2, 3]), (3, 0, 3));
    }

    #[test]
    fn an_appended_row_splices_only_itself() {
        assert_eq!(splice_window(&[1, 2, 3], &[1, 2, 3, 4]), (3, 0, 4));
    }

    #[test]
    fn a_prepended_row_splices_only_itself() {
        assert_eq!(splice_window(&[1, 2, 3], &[0, 1, 2, 3]), (0, 0, 1));
    }

    #[test]
    fn a_changed_row_replaces_only_itself() {
        assert_eq!(splice_window(&[1, 2, 3], &[1, 9, 3]), (1, 1, 2));
    }

    #[test]
    fn a_removed_row_takes_only_itself_out() {
        assert_eq!(splice_window(&[1, 2, 3], &[1, 3]), (1, 1, 1));
    }

    #[test]
    fn an_emptied_list_removes_everything() {
        assert_eq!(splice_window(&[1, 2, 3], &[]), (0, 3, 0));
    }

    #[test]
    fn same_author_within_the_window_groups() {
        let state = state_with(&[User {
            id: "u1".into(),
            username: "anna".into(),
            ..Default::default()
        }]);
        let prev = post("u1", 1_000);
        let next = post("u1", 1_000 + message::GROUPING_WINDOW_MS - 1);
        assert!(groups_with(Some(&prev), &next, &state));
    }

    #[test]
    fn a_gap_past_the_window_does_not_group() {
        let state = state_with(&[User {
            id: "u1".into(),
            username: "anna".into(),
            ..Default::default()
        }]);
        let prev = post("u1", 1_000);
        let next = post("u1", 1_000 + message::GROUPING_WINDOW_MS);
        assert!(!groups_with(Some(&prev), &next, &state));
    }

    #[test]
    fn a_different_author_never_groups_even_seconds_apart() {
        let state = state_with(&[
            User {
                id: "u1".into(),
                username: "anna".into(),
                ..Default::default()
            },
            User {
                id: "u2".into(),
                username: "bob".into(),
                ..Default::default()
            },
        ]);
        let prev = post("u1", 1_000);
        let next = post("u2", 1_001);
        assert!(!groups_with(Some(&prev), &next, &state));
    }

    #[test]
    fn nothing_before_it_never_groups() {
        let state = state_with(&[]);
        let next = post("u1", 1_000);
        assert!(!groups_with(None, &next, &state));
    }

    #[test]
    fn the_feed_only_shows_posts_that_belong_in_it() {
        let mine = Post {
            channel_id: "c1".into(),
            ..post("u1", 1_000)
        };
        assert!(ChatView::feed_shows(Some("c1"), &mine, true));
        assert!(!ChatView::feed_shows(Some("c2"), &mine, true));
        assert!(!ChatView::feed_shows(None, &mine, true));

        // A reply is a thread's business while CRT is on, and the channel's
        // once it is off — sending one must move the reader in exactly the
        // second case.
        let reply = Post {
            root_id: "root".into(),
            ..mine.clone()
        };
        assert!(!ChatView::feed_shows(Some("c1"), &reply, true));
        assert!(ChatView::feed_shows(Some("c1"), &reply, false));
    }

    #[test]
    fn composer_is_one_line_high_and_uses_a_clear_send_affordance() {
        assert_eq!(COMPOSER_SINGLE_LINE_HEIGHT, 28);
        assert_eq!(COMPOSER_MAX_CONTENT_HEIGHT, 108);
        assert_eq!(composer_content_height(1), 28);
        assert_eq!(composer_content_height(2), 48);
        assert_eq!(composer_content_height(99), 108);
        assert_eq!(COMPOSER_SEND_ICON, "mail-send-symbolic");
    }

    #[test]
    fn compact_composer_menu_keeps_the_secondary_actions_available() {
        let priorities = gtk::gio::Menu::new();
        for label in ["Standard", "Important", "Urgent"] {
            priorities.append(Some(label), None);
        }
        let menu = composer_options_menu(&priorities);
        let model: &gtk::gio::MenuModel = menu.upcast_ref();

        const { assert!(COMPOSER_OPTIONS_BUTTON_WIDTH <= 40) };
        assert_eq!(model.n_items(), 3);
        assert_eq!(
            model.item_attribute_value(0, "label", None).unwrap().str(),
            Some("Attach File…")
        );
        assert_eq!(
            model.item_attribute_value(1, "label", None).unwrap().str(),
            Some("Send Later…")
        );
        assert_eq!(
            model.item_attribute_value(2, "label", None).unwrap().str(),
            Some("Message Priority")
        );
    }
}
