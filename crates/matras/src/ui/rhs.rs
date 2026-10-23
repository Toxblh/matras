//! The right-hand panel: a thread, or the notification inbox.
//!
//! Mattermost puts both in the same place and swaps between them, which is
//! worth copying — the two are alternatives, never side by side, and sharing
//! one surface keeps the window from growing a fifth column.

use std::cell::{Cell, RefCell};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use mattermost_api::models::{Millis, Post};

use crate::avatars::Avatars;
use crate::state::SharedState;
use crate::ui::chat::{self, composer_content_height, post_revision, RenderContext};
use crate::ui::message::{self, MessageActions, RowOptions};

const THREAD_COMPOSER_SINGLE_LINE_HEIGHT: i32 = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelMode {
    Hidden,
    /// Viewing the thread rooted at this post id.
    Thread(String),
    /// Mentions and unread threads.
    Inbox,
    /// Results for a search, held so a redraw does not lose them.
    Search(String),
}

/// Whether the panel needs its own close control in addition to the window
/// close button.
pub fn explicit_close_visible(mode: &PanelMode) -> bool {
    !matches!(mode, PanelMode::Inbox | PanelMode::Thread(_))
}

/// Whether a window close request should dismiss the inbox panel first.
pub fn window_close_dismisses_panel(mode: &PanelMode) -> bool {
    matches!(mode, PanelMode::Inbox | PanelMode::Thread(_))
}

/// Threads become an overlay when the window cannot comfortably hold three
/// panes. The live width is also checked because a panel can open before the
/// first breakpoint notification has arrived.
pub fn panel_should_overlay(narrow: bool, window_width: i32, mode: &PanelMode) -> bool {
    narrow || window_width < 1200 || matches!(mode, PanelMode::Inbox | PanelMode::Search(_))
}

pub struct RightPanel {
    pub widget: gtk::Box,
    mode: Rc<RefCell<PanelMode>>,
    /// What is currently drawn, as one value. Every refresh of the message
    /// surfaces reaches this panel, including the ones caused by a post in a
    /// channel it is not showing — and a rebuild here destroys the row under
    /// the pointer, so its hover highlight and action bar blink out until the
    /// pointer moves again. Redrawing only when this changes is what stops
    /// that.
    drawn: Cell<Option<u64>>,
    title: gtk::Label,
    subtitle: gtk::Label,
    stack: gtk::Stack,

    thread_items: gtk::gio::ListStore,
    thread_list: gtk::ListView,
    thread_context: Rc<RefCell<Option<RenderContext>>>,
    /// Which thread the model holds, so a redraw can tell itself apart from
    /// arriving in a different one.
    showing_thread: RefCell<Option<String>>,
    /// False while the reader is scrolled up in a thread, so an arriving reply
    /// does not drag them to the end of it.
    pinned_to_bottom: Rc<RefCell<bool>>,
    thread_entry: gtk::TextView,
    follow: gtk::ToggleButton,
    /// Set while the toggle is being synced from state, so it does not report
    /// that as a click.
    updating_follow: Rc<RefCell<bool>>,
    /// Set while a draft is being restored, so it is not mistaken for typing.
    restoring: Rc<RefCell<bool>>,

    search_list: gtk::Box,

    inbox_stack: gtk::Stack,
    saved_list: gtk::ListBox,
    mentions_list: gtk::ListBox,
    threads_list: gtk::ListBox,

    on_open_thread: Rc<dyn Fn(String)>,
    /// (channel id, root post id — empty when the target is a root post)
    on_open_post: Rc<dyn Fn(String, String)>,
    /// Channel id and post id: open that channel *at* that message.
    on_jump: Rc<dyn Fn(String, String)>,
    close: gtk::Button,
}

impl RightPanel {
    pub fn new(
        on_close: impl Fn() + 'static,
        on_reply: impl Fn(String) + 'static,
        on_open_thread: impl Fn(String) + 'static,
        on_open_post: impl Fn(String, String) + 'static,
        on_jump: impl Fn(String, String) + 'static,
        on_draft: impl Fn() + 'static,
        on_follow: impl Fn(bool) + 'static,
    ) -> Rc<Self> {
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

        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close Panel")
            .build();
        close.add_css_class("flat");
        close.connect_clicked(move |_| on_close());

        let header = adw::HeaderBar::builder()
            .title_widget(&title_box)
            .show_start_title_buttons(false)
            .build();
        // Following a thread is how you keep getting told about it after you
        // stop being mentioned in it, so it belongs on the thread itself.
        let follow = gtk::ToggleButton::builder()
            .icon_name("bookmark-new-symbolic")
            .tooltip_text("Follow this thread")
            .visible(false)
            .build();
        follow.add_css_class("flat");
        let updating_follow = Rc::new(RefCell::new(false));
        follow.connect_toggled({
            let updating = updating_follow.clone();
            move |button| {
                if !*updating.borrow() {
                    on_follow(button.is_active());
                }
            }
        });

        header.pack_end(&close);
        header.pack_end(&follow);

        // ---- thread page
        let thread_items = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
        let thread_context: Rc<RefCell<Option<RenderContext>>> = Rc::new(RefCell::new(None));
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_bind({
            let thread_context = thread_context.clone();
            move |_, object| {
                let list_item = object
                    .downcast_ref::<gtk::ListItem>()
                    .expect("factory must receive GtkListItem");
                let Some(item) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let Some(context) = thread_context.borrow().clone() else {
                    return;
                };
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                let child = render_thread_item(&item.borrow::<ThreadItem>(), &context);
                child.set_hexpand(true);
                list_item.set_child(Some(&child));
            }
        });
        factory.connect_unbind(|_, object| {
            object
                .downcast_ref::<gtk::ListItem>()
                .expect("factory must receive GtkListItem")
                .set_child(gtk::Widget::NONE)
        });

        let selection = gtk::NoSelection::new(Some(thread_items.clone()));
        let thread_list = gtk::ListView::new(Some(selection), Some(factory));
        thread_list.add_css_class("message-list");
        thread_list.set_single_click_activate(false);
        thread_list.set_tab_behavior(gtk::ListTabBehavior::Item);
        // Align::End used to bottom-anchor a short thread, but it also caps the
        // list at its natural height, and a virtual list needs the whole
        // viewport to work out which rows are visible.
        thread_list.set_valign(gtk::Align::Fill);
        thread_list.set_vexpand(true);
        thread_list.set_hexpand(true);
        thread_list.set_show_separators(false);
        thread_list.set_margin_top(12);
        thread_list.set_margin_bottom(12);
        thread_list.set_margin_start(12);
        thread_list.set_margin_end(12);

        let thread_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            // It must be the direct child, or the list cannot tell which rows
            // are off-screen and therefore recyclable.
            .child(&thread_list)
            .build();

        let pinned_to_bottom = Rc::new(RefCell::new(true));
        {
            let pinned = pinned_to_bottom.clone();
            let candidate = Rc::new(Cell::new(false));
            thread_scroller
                .vadjustment()
                .connect_value_changed(move |adj| {
                    chat::update_live_edge_pin(adj, &pinned, &candidate);
                });
        }

        let thread_entry = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .hexpand(true)
            .height_request(THREAD_COMPOSER_SINGLE_LINE_HEIGHT)
            .top_margin(4)
            .bottom_margin(4)
            .left_margin(8)
            .right_margin(8)
            .build();
        thread_entry.update_property(&[gtk::accessible::Property::Label("Reply")]);
        let entry_frame = gtk::Frame::builder()
            .hexpand(true)
            .child(&thread_entry)
            .build();
        entry_frame.add_css_class("thread-composer-entry");

        let send = gtk::Button::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Reply  (Enter)")
            .valign(gtk::Align::End)
            .build();
        send.add_css_class("suggested-action");
        send.add_css_class("circular");

        let composer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        composer.append(&entry_frame);
        composer.append(&send);

        let on_reply = Rc::new(on_reply);
        let submit = {
            let entry = thread_entry.clone();
            let on_reply = on_reply.clone();
            move || {
                let buffer = entry.buffer();
                let (start, end) = buffer.bounds();
                let text = buffer.text(&start, &end, false).trim().to_string();
                if text.is_empty() {
                    return;
                }
                buffer.set_text("");
                on_reply(text);
            }
        };
        send.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let submit = submit.clone();
            move |_, key, _, modifier| {
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                if enter && !modifier.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                    submit();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        });
        thread_entry.add_controller(keys);

        let restoring = Rc::new(RefCell::new(false));
        thread_entry.buffer().connect_changed({
            let restoring = restoring.clone();
            let on_draft = Rc::new(on_draft);
            let entry_frame = entry_frame.clone();
            move |buffer| {
                let lines = buffer.line_count();
                entry_frame.set_height_request(composer_content_height(lines));
                if !*restoring.borrow() {
                    on_draft();
                }
            }
        });

        let thread_page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        thread_page.append(&thread_scroller);
        thread_page.append(&composer);

        // ---- inbox page
        let mentions_list = list_box();
        let threads_list = list_box();

        let inbox_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        let saved_list = list_box();
        inbox_stack.add_titled(&scroller(&mentions_list), Some("mentions"), "Mentions");
        inbox_stack.add_titled(&scroller(&threads_list), Some("threads"), "Threads");
        inbox_stack.add_titled(&scroller(&saved_list), Some("saved"), "Saved");

        let switcher = inbox_tab_bar(&inbox_stack);

        let inbox_page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        inbox_page.append(&switcher);
        inbox_page.append(&inbox_stack);

        let stack = gtk::Stack::new();
        let search_list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let search_page = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&search_list)
            .build();

        stack.add_named(&thread_page, Some("thread"));
        stack.add_named(&inbox_page, Some("inbox"));
        stack.add_named(&search_page, Some("search"));

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        widget.add_css_class("right-panel");
        widget.append(&header);
        widget.append(&stack);

        Rc::new(RightPanel {
            widget,
            mode: Rc::new(RefCell::new(PanelMode::Hidden)),
            drawn: Cell::new(None),
            title,
            subtitle,
            stack,
            thread_items,
            thread_list,
            thread_context,
            showing_thread: RefCell::new(None),
            pinned_to_bottom,
            thread_entry,
            follow,
            updating_follow,
            restoring,
            search_list,
            inbox_stack,
            saved_list,
            mentions_list,
            threads_list,
            on_open_thread: Rc::new(on_open_thread),
            on_open_post: Rc::new(on_open_post),
            on_jump: Rc::new(on_jump),
            close,
        })
    }

    pub fn mode(&self) -> PanelMode {
        self.mode.borrow().clone()
    }

    pub fn set_mode(&self, mode: PanelMode) {
        // Navigating anywhere — including re-opening the thread already on
        // screen — is a request to see its newest reply, so the signature and
        // the remembered thread both go. A redraw caused by an event never
        // comes through here, which is what keeps the guard below meaningful.
        self.drawn.set(None);
        self.showing_thread.borrow_mut().take();
        self.close.set_visible(explicit_close_visible(&mode));
        *self.mode.borrow_mut() = mode;
    }

    /// Puts the thread at its newest reply because *you* answered. The rule
    /// that following the live edge is conditional is right for someone else's
    /// reply and wrong for your own: every client shows you what you just
    /// sent, wherever you had been reading.
    ///
    /// Does nothing unless the panel is showing the thread the reply lands in,
    /// so answering in the channel composer does not move the panel.
    pub fn follow_own_reply(&self, post: &Post) {
        if self.mode() != PanelMode::Thread(post.thread_root().to_string()) {
            return;
        }
        *self.pinned_to_bottom.borrow_mut() = true;
        chat::scroll_list_to_newest(
            &self.thread_list,
            self.thread_items.n_items(),
            "own-reply-sent",
        );
    }

    /// Whether the panel already shows exactly this. Records the signature on
    /// the way through, so the caller can simply return when it is true.
    fn already_drawn(&self, signature: u64) -> bool {
        if self.drawn.get() == Some(signature) {
            return true;
        }
        self.drawn.set(Some(signature));
        false
    }

    pub fn focus_composer(&self) {
        self.thread_entry.grab_focus();
    }

    /// Reflects whether the open thread is followed, without reporting the
    /// change back as though someone had clicked it.
    pub fn set_following(&self, following: bool) {
        if self.follow.is_active() == following {
            return;
        }
        *self.updating_follow.borrow_mut() = true;
        self.follow.set_active(following);
        *self.updating_follow.borrow_mut() = false;
    }

    /// What is in the thread's reply box.
    pub fn composer_text(&self) -> String {
        let buffer = self.thread_entry.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Puts a thread draft back. Guarded like the channel composer's: setting
    /// the buffer fires `changed`, which must not be read as typing.
    pub fn set_composer_text(&self, text: &str) {
        if self.composer_text() == text {
            return;
        }
        *self.restoring.borrow_mut() = true;
        self.thread_entry.buffer().set_text(text);
        let buffer = self.thread_entry.buffer();
        buffer.place_cursor(&buffer.end_iter());
        *self.restoring.borrow_mut() = false;
    }

    /// Redraws whatever the panel is currently showing.
    pub fn refresh(&self, state: &SharedState, avatars: &Avatars, actions: &MessageActions) {
        match self.mode() {
            PanelMode::Hidden => {}
            PanelMode::Thread(root_id) => self.render_thread(&root_id, state, avatars, actions),
            PanelMode::Inbox => self.render_inbox(state, avatars),
            PanelMode::Search(terms) => self.render_search(&terms, state, avatars, actions),
        }
    }

    /// Search results, newest first, each one a jump into its channel.
    fn render_search(
        &self,
        terms: &str,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) {
        let signature = {
            let st = state.borrow();
            let mut hash = DefaultHasher::new();
            terms.hash(&mut hash);
            st.searching.hash(&mut hash);
            for post in &st.search_results {
                post_revision(post, state).hash(&mut hash);
            }
            hash.finish()
        };
        if self.already_drawn(signature) {
            return;
        }

        self.stack.set_visible_child_name("search");
        clear(&self.search_list);
        self.title.set_text("Search");
        self.subtitle.set_text(terms);
        self.subtitle.set_visible(true);

        let st = state.borrow();
        if st.searching {
            let spinner = gtk::Spinner::builder()
                .halign(gtk::Align::Center)
                .margin_top(24)
                .build();
            spinner.start();
            self.search_list.append(&spinner);
            return;
        }
        if st.search_results.is_empty() {
            self.search_list.append(
                &adw::StatusPage::builder()
                    .icon_name("system-search-symbolic")
                    .title("No Results Found")
                    .description("Try a different search")
                    .css_classes(["compact"])
                    .build(),
            );
            return;
        }

        for post in &st.search_results {
            let channel = st
                .channel(&post.channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            let row = message::build(
                post,
                state,
                avatars,
                actions,
                RowOptions {
                    grouped: false,
                    show_thread_footer: false,
                },
            );
            // Which channel a hit came from is most of what makes it useful —
            // and it is also the obvious place to press to go there.
            let label = gtk::Label::builder()
                .label(format!("{channel}  ›"))
                .xalign(0.0)
                .build();
            label.add_css_class("caption-heading");
            label.add_css_class("dim-label");

            let go = gtk::Button::builder()
                .child(&label)
                .halign(gtk::Align::Start)
                .margin_start(10)
                .margin_top(8)
                .tooltip_text("Go to Message")
                .build();
            go.add_css_class("flat");
            go.add_css_class("thread-link");
            go.connect_clicked({
                let jump = self.on_jump.clone();
                let channel_id = post.channel_id.clone();
                let post_id = post.id.clone();
                move |_| jump(channel_id.clone(), post_id.clone())
            });
            self.search_list.append(&go);
            self.search_list.append(&row);
        }
    }

    fn render_thread(
        &self,
        root_id: &str,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) {
        let st = state.borrow();
        let channel_title = st
            .threads
            .get(root_id)
            .and_then(|feed| feed.posts.first())
            .and_then(|p| st.channel(&p.channel_id))
            .map(|c| st.channel_title(c))
            .unwrap_or_default();
        drop(st);

        let signature = {
            let st = state.borrow();
            let mut hash = DefaultHasher::new();
            root_id.hash(&mut hash);
            channel_title.hash(&mut hash);
            match st.threads.get(root_id) {
                Some(feed) => {
                    for post in &feed.posts {
                        post_revision(post, state).hash(&mut hash);
                    }
                }
                None => match st.find_post(root_id) {
                    Some(root) => post_revision(root, state).hash(&mut hash),
                    None => "loading".hash(&mut hash),
                },
            }
            hash.finish()
        };
        if self.already_drawn(signature) {
            return;
        }

        self.stack.set_visible_child_name("thread");
        self.title.set_text("Thread");
        self.follow.set_visible(true);
        self.subtitle.set_text(&channel_title);
        self.subtitle.set_visible(!channel_title.is_empty());

        let st = state.borrow();
        // While the replies are in flight, show the root on its own if we
        // already hold it — the reader clicked a message they were looking at,
        // and an empty panel makes the click feel lost.
        let posts = match st.threads.get(root_id) {
            Some(feed) => Some(feed.posts.clone()),
            None => st.find_post(root_id).map(|root| vec![root.clone()]),
        };
        drop(st);

        let items = match posts {
            None => vec![ThreadItem::Loading],
            Some(mut posts) => {
                // The feed arrives sorted by `create_at` (see
                // `ChannelFeed::from_list`, which has to sort because the
                // thread endpoint does not). That is still not enough: two
                // posts can share a millisecond on a busy server, and then the
                // conversation starts with a reply. So the root goes first,
                // always.
                if let Some(index) = posts.iter().position(|p| p.id == root_id) {
                    let root = posts.remove(index);
                    posts.insert(0, root);
                }
                build_thread_items(&posts, state)
            }
        };

        // Arriving in a thread starts at its newest reply; a redraw of the one
        // already on screen must leave the reader where they are.
        let opening = self.showing_thread.borrow().as_deref() != Some(root_id);
        if opening {
            *self.pinned_to_bottom.borrow_mut() = true;
            *self.showing_thread.borrow_mut() = Some(root_id.to_string());
        }
        let spliced = self.replace_thread(items, state, avatars, actions);
        if (opening || spliced) && *self.pinned_to_bottom.borrow() {
            chat::scroll_list_to_newest(
                &self.thread_list,
                self.thread_items.n_items(),
                if opening {
                    "thread-open"
                } else {
                    "thread-splice-while-pinned"
                },
            );
        }
    }

    /// Splices in only the rows that actually differ. An arriving reply then
    /// costs two rows instead of a teardown and rebuild of every row above it,
    /// and — as in the channel feed — GtkListView keeps its scroll anchor
    /// across the update. Returns whether anything moved, which doubles as the
    /// answer to "was this redraw about this thread at all".
    ///
    /// The head (the root, then its "N replies" divider) is diffed apart from
    /// the replies on purpose. That divider's count changes with every reply
    /// that arrives, so a single common-prefix scan would stop dead on it while
    /// the common-suffix scan stopped dead on the new last reply — and the one
    /// window between them is the whole thread. The rebuild this exists to
    /// avoid, reintroduced by the one row that always changes.
    fn replace_thread(
        &self,
        items: Vec<ThreadItem>,
        state: &SharedState,
        avatars: &Avatars,
        actions: &MessageActions,
    ) -> bool {
        *self.thread_context.borrow_mut() = Some(RenderContext {
            state: state.clone(),
            avatars: avatars.clone(),
            actions: actions.clone(),
        });

        let old: Vec<ThreadItem> = (0..self.thread_items.n_items())
            .filter_map(|position| thread_item(&self.thread_items, position))
            .collect();
        let old_head = head_rows(&old);
        let new_head = head_rows(&items);
        // Head first, so the replies below it are offset by the head length
        // this call has just written.
        let mut moved = self.splice_region(0, &old[..old_head], &items[..new_head]);
        moved |= self.splice_region(new_head as u32, &old[old_head..], &items[new_head..]);
        moved
    }

    /// Replaces the one window of `region` that changed, `offset` rows into the
    /// model.
    fn splice_region(&self, offset: u32, old: &[ThreadItem], new: &[ThreadItem]) -> bool {
        let old_marks: Vec<u64> = old.iter().map(ThreadItem::fingerprint).collect();
        let new_marks: Vec<u64> = new.iter().map(ThreadItem::fingerprint).collect();
        let (start, removed, end) = chat::splice_window(&old_marks, &new_marks);
        if removed == 0 && start == end {
            return false;
        }
        let additions: Vec<glib::BoxedAnyObject> = new[start..end]
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect();
        if chat::scroll_trace_enabled() {
            let adjustment = self.thread_list.vadjustment();
            tracing::info!(
                target: "matras::scroll",
                event = "thread-splice",
                offset,
                old_rows = old.len(),
                new_rows = new.len(),
                prefix = start,
                removed,
                added = additions.len(),
                pinned = *self.pinned_to_bottom.borrow(),
                value = adjustment.as_ref().map(gtk::Adjustment::value),
                upper = adjustment.as_ref().map(gtk::Adjustment::upper),
                page_size = adjustment.as_ref().map(gtk::Adjustment::page_size),
                "scroll trace"
            );
        }
        self.thread_items
            .splice(offset + start as u32, removed as u32, &additions);
        true
    }

    fn render_inbox(&self, state: &SharedState, avatars: &Avatars) {
        let signature = {
            let st = state.borrow();
            let mut hash = DefaultHasher::new();
            for post in &st.mentions {
                post_revision(post, state).hash(&mut hash);
            }
            // Saved posts are gathered from every feed on the fly, so the
            // signature has to walk the same ground the render does.
            for post in st
                .feeds
                .values()
                .chain(st.threads.values())
                .flat_map(|feed| feed.posts.iter())
                .filter(|post| st.saved_posts.contains(&post.id))
            {
                post_revision(post, state).hash(&mut hash);
            }
            for thread in &st.thread_inbox {
                thread.id.hash(&mut hash);
                thread.reply_count.hash(&mut hash);
                thread.last_reply_at.hash(&mut hash);
                thread.unread_replies.hash(&mut hash);
                thread.unread_mentions.hash(&mut hash);
                thread.is_following.hash(&mut hash);
            }
            hash.finish()
        };
        if self.already_drawn(signature) {
            return;
        }

        self.stack.set_visible_child_name("inbox");
        self.follow.set_visible(false);
        self.title.set_text("Inbox");
        self.subtitle.set_visible(false);
        self.mentions_list.remove_all();
        self.threads_list.remove_all();
        self.saved_list.remove_all();

        let st = state.borrow();

        if st.mentions.is_empty() {
            add_placeholder(&self.mentions_list, &empty_state(
                "mail-unread-symbolic",
                "No Recent Mentions",
                "Messages that name you show up here",
            ));
        }
        for post in &st.mentions {
            let channel = st
                .channel(&post.channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_else(|| "unknown channel".into());
            let row = inbox_row(
                avatars,
                &post.user_id,
                &st.author_name(post),
                &channel,
                &post.message,
                post.create_at,
                None,
            );
            let channel_id = post.channel_id.clone();
            let root_id = post.thread_root().to_string();
            let is_reply = post.is_reply();
            let open = self.on_open_post.clone();
            let jump = self.on_jump.clone();
            let post_id = post.id.clone();
            add_row(&self.mentions_list, &row, move || {
                if is_reply {
                    open(channel_id.clone(), root_id.clone());
                } else {
                    // Not a reply, so there is no thread to open — the useful
                    // thing is the message itself.
                    jump(channel_id.clone(), post_id.clone());
                }
            });
        }

        // Saved posts: the ones you flagged, newest first. They are held as a
        // set of ids, so this walks what is loaded rather than fetching —
        // anything not in memory shows up as soon as its channel is opened.
        let mut saved: Vec<&Post> = st
            .feeds
            .values()
            .chain(st.threads.values())
            .flat_map(|feed| feed.posts.iter())
            .filter(|p| st.saved_posts.contains(&p.id))
            .collect();
        saved.sort_by_key(|p| std::cmp::Reverse(p.create_at));
        saved.dedup_by_key(|p| p.id.clone());

        if saved.is_empty() {
            add_placeholder(&self.saved_list, &empty_state(
                "starred-symbolic",
                "No Saved Messages",
                "Save a message from its menu and it waits here",
            ));
        }
        for post in saved {
            let channel = st
                .channel(&post.channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_else(|| "unknown channel".into());
            let row = inbox_row(
                avatars,
                &post.user_id,
                &st.author_name(post),
                &channel,
                &post.message,
                post.create_at,
                None,
            );
            let channel_id = post.channel_id.clone();
            let root_id = post.thread_root().to_string();
            let is_reply = post.is_reply();
            let open = self.on_open_post.clone();
            let jump = self.on_jump.clone();
            let post_id = post.id.clone();
            add_row(&self.saved_list, &row, move || {
                if is_reply {
                    open(channel_id.clone(), root_id.clone());
                } else {
                    jump(channel_id.clone(), post_id.clone());
                }
            });
        }

        if st.thread_inbox.is_empty() {
            add_placeholder(&self.threads_list, &empty_state(
                "chat-message-new-symbolic",
                "No Threads Yet",
                "Threads you follow appear here",
            ));
        }
        for thread in &st.thread_inbox {
            let author = st.author_name(&thread.post);
            let channel = st
                .channel(&thread.post.channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            let row = inbox_row(
                avatars,
                &thread.post.user_id,
                &author,
                &channel,
                &thread.post.message,
                thread.last_reply_at.max(thread.post.create_at),
                Some((
                    thread.reply_count,
                    thread.unread_replies,
                    thread.unread_mentions,
                )),
            );
            let root_id = thread.id.clone();
            let open = self.on_open_thread.clone();
            add_row(&self.threads_list, &row, move || open(root_id.clone()));
        }
    }

    /// Switches the inbox to its threads tab — used when a badge is clicked.
    pub fn show_threads_tab(&self) {
        self.inbox_stack.set_visible_child_name("threads");
    }
}

/// One row of a thread, as data. `GtkListView` turns only the visible
/// screenful into widgets and recycles them while scrolling; the panel used to
/// build a message widget tree per reply before it drew anything, which at
/// ~1.6 ms a row is half a second of frozen main thread on a 300-reply thread.
#[derive(Clone)]
enum ThreadItem {
    /// We do not hold the root yet, let alone the replies.
    Loading,
    /// The "N replies" divider that separates the root from its replies.
    Divider(String),
    Post {
        post: Rc<Post>,
        grouped: bool,
        /// Computed once while the item is built, so diffing compares integers
        /// instead of re-serialising every post once per comparison.
        revision: u64,
    },
}

impl ThreadItem {
    fn fingerprint(&self) -> u64 {
        let mut hash = DefaultHasher::new();
        match self {
            ThreadItem::Loading => 0u8.hash(&mut hash),
            ThreadItem::Divider(text) => {
                1u8.hash(&mut hash);
                text.hash(&mut hash);
            }
            ThreadItem::Post {
                post,
                grouped,
                revision,
            } => {
                2u8.hash(&mut hash);
                post.id.hash(&mut hash);
                grouped.hash(&mut hash);
                revision.hash(&mut hash);
            }
        }
        hash.finish()
    }
}

/// How many rows sit above the replies: the root post, and the "N replies"
/// divider once there is one.
fn head_rows(items: &[ThreadItem]) -> usize {
    items
        .iter()
        .position(|item| matches!(item, ThreadItem::Divider(_)))
        .map_or_else(|| items.len().min(1), |at| at + 1)
}

fn thread_item(store: &gtk::gio::ListStore, position: u32) -> Option<ThreadItem> {
    store
        .item(position)?
        .downcast::<glib::BoxedAnyObject>()
        .ok()
        .map(|boxed| boxed.borrow::<ThreadItem>().clone())
}

fn render_thread_item(item: &ThreadItem, context: &RenderContext) -> gtk::Widget {
    match item {
        ThreadItem::Loading => adw::StatusPage::builder()
            .title("Loading Thread…")
            .vexpand(true)
            .build()
            .upcast(),
        ThreadItem::Divider(text) => message::day_separator(text),
        ThreadItem::Post { post, grouped, .. } => message::build(
            post,
            &context.state,
            &context.avatars,
            &context.actions,
            RowOptions {
                grouped: *grouped,
                // We are already in the thread.
                show_thread_footer: false,
            },
        ),
    }
}

/// Turns a thread into rows. Author grouping has to be settled here: in a
/// virtual list there is no neighbouring widget to look at when a row is built,
/// because the neighbour may not exist yet.
fn build_thread_items(posts: &[Post], state: &SharedState) -> Vec<ThreadItem> {
    let reply_count = posts.len().saturating_sub(1) as i64;
    let mut items = Vec::with_capacity(posts.len() + 1);
    let mut last_author: Option<String> = None;
    let mut last_at: Millis = 0;

    for (index, post) in posts.iter().enumerate() {
        if post.is_deleted() {
            continue;
        }
        // A divider after the root makes the thread readable at a glance, and
        // it is where Mattermost puts the reply count too.
        if index == 1 && reply_count > 0 {
            items.push(ThreadItem::Divider(format!(
                "{reply_count} {}",
                message::plural(reply_count, "reply", "replies")
            )));
            last_author = None;
        }

        let author = state.borrow().author_name(post);
        let grouped = index != 0
            && last_author.as_deref() == Some(author.as_str())
            && post.create_at - last_at < message::GROUPING_WINDOW_MS;
        items.push(ThreadItem::Post {
            post: Rc::new(post.clone()),
            grouped,
            revision: post_revision(post, state),
        });
        last_author = Some(author);
        last_at = post.create_at;
    }
    items
}

fn inbox_row(
    avatars: &Avatars,
    user_id: &str,
    author: &str,
    channel: &str,
    message: &str,
    at: Millis,
    counts: Option<(i64, i64, i64)>,
) -> gtk::Box {
    let avatar = adw::Avatar::builder()
        .size(32)
        .valign(gtk::Align::Start)
        .build();
    avatars.apply(&avatar, user_id, author);

    let heading = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let who = gtk::Label::builder().label(author).xalign(0.0).build();
    who.add_css_class("message-author");
    heading.append(&who);

    let where_ = gtk::Label::builder()
        .label(channel)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    where_.add_css_class("dim-label");
    where_.add_css_class("caption");
    heading.append(&where_);

    let when = gtk::Label::new(Some(&message::format_relative(at)));
    when.add_css_class("message-timestamp");
    heading.append(&when);

    let preview = gtk::Label::builder()
        .label(message.lines().next().unwrap_or_default())
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .lines(2)
        .wrap(true)
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    body.append(&heading);
    body.append(&preview);

    if let Some((replies, unread_replies, unread_mentions)) = counts {
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(2)
            .build();
        let count = gtk::Label::new(Some(&format!(
            "{replies} {}",
            message::plural(replies, "reply", "replies")
        )));
        count.add_css_class("message-timestamp");
        footer.append(&count);

        if unread_mentions > 0 {
            let badge = gtk::Label::new(Some(&unread_mentions.to_string()));
            badge.add_css_class("mention-badge");
            footer.append(&badge);
        } else if unread_replies > 0 {
            let dot = gtk::Label::new(Some("●"));
            dot.add_css_class("accent");
            dot.add_css_class("presence-dot");
            dot.set_tooltip_text(Some("Unread replies"));
            footer.append(&dot);
        }
        body.append(&footer);
    }

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&avatar);
    row_box.append(&body);
    row_box
}

/// What a row does when activated, kept on the row itself.
type RowActivate = Rc<dyn Fn()>;

/// A navigation-sidebar list: rows are one target each, arrow keys move
/// between them and Enter or a click opens the entry.
fn list_box() -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    list.add_css_class("navigation-sidebar");
    list.connect_row_activated(|_, row| {
        // SAFETY: only `add_row` stores this key, always as a `RowActivate`.
        if let Some(activate) = unsafe { row.data::<RowActivate>("activate") } {
            let activate = unsafe { activate.as_ref() }.clone();
            activate();
        }
    });
    list
}

fn add_row(list: &gtk::ListBox, child: &gtk::Box, on_activate: impl Fn() + 'static) {
    let row = gtk::ListBoxRow::builder().child(child).build();
    let activate: RowActivate = Rc::new(on_activate);
    unsafe { row.set_data("activate", activate) };
    list.append(&row);
}

/// An empty state inside a list: shown, but not a row to land on.
fn add_placeholder(list: &gtk::ListBox, child: &gtk::Widget) {
    let row = gtk::ListBoxRow::builder()
        .child(child)
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .build();
    list.append(&row);
}

fn scroller(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build()
}

const INBOX_TAB_PAGES: [(&str, &str); 3] = [
    ("mentions", "Mentions"),
    ("threads", "Threads"),
    ("saved", "Saved"),
];

fn inbox_tab_bar(stack: &gtk::Stack) -> gtk::Box {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .hexpand(true)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(8)
        .margin_end(8)
        .build();
    let mut first: Option<gtk::ToggleButton> = None;
    for (name, title) in INBOX_TAB_PAGES {
        let button = gtk::ToggleButton::with_label(title);
        button.add_css_class("flat");
        if let Some(group) = first.as_ref() {
            button.set_group(Some(group));
        } else {
            first = Some(button.clone());
        }
        button.set_active(stack.visible_child_name().as_deref() == Some(name));
        let target = stack.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                target.set_visible_child_name(name);
            }
        });
        bar.append(&button);
    }
    let bar_buttons = bar.clone();
    stack.connect_visible_child_name_notify(move |stack| {
        let mut child = bar_buttons.first_child();
        for (name, _) in INBOX_TAB_PAGES {
            if let Some(button) = child.and_downcast::<gtk::ToggleButton>() {
                button.set_active(stack.visible_child_name().as_deref() == Some(name));
                child = button.next_sibling();
            } else {
                break;
            }
        }
    });
    bar
}

#[cfg(test)]
mod inbox_tab_layout_tests {
    use super::*;

    #[test]
    fn inbox_tabs_fit_a_narrow_sidebar() {
        // Keep short labels rather than GtkStackSwitcher’s homogeneous
        // 3×button-width allocation, which asked for 400px at PinePhone size.
        assert_eq!(INBOX_TAB_PAGES.len(), 3);
        assert!(INBOX_TAB_PAGES
            .iter()
            .all(|(_, title)| title.chars().count() <= 8));
    }
}

#[cfg(test)]
mod inbox_close_tests {
    use super::*;

    #[test]
    fn the_inbox_uses_only_the_window_close_control() {
        assert!(!explicit_close_visible(&PanelMode::Inbox));
    }

    #[test]
    fn a_thread_uses_only_the_window_close_control() {
        assert!(!explicit_close_visible(&PanelMode::Thread("post".into())));
    }

    #[test]
    fn a_window_close_dismisses_an_open_inbox() {
        assert!(window_close_dismisses_panel(&PanelMode::Inbox));
        assert!(window_close_dismisses_panel(&PanelMode::Thread(
            "post".into()
        )));
    }

    #[test]
    fn closing_another_panel_is_left_to_window_background_mode() {
        assert!(!window_close_dismisses_panel(&PanelMode::Hidden));
        assert!(!window_close_dismisses_panel(&PanelMode::Search(
            "post".into()
        )));
    }
}

#[cfg(test)]
mod panel_width_tests {
    use super::*;

    #[test]
    fn thread_panels_overlay_at_phone_and_tablet_widths_even_before_breakpoints_fire() {
        let thread = PanelMode::Thread("post".into());
        assert!(panel_should_overlay(false, 360, &thread));
        assert!(panel_should_overlay(false, 720, &thread));
        assert!(panel_should_overlay(true, 1320, &thread));
        assert!(!panel_should_overlay(false, 1320, &thread));
    }

    #[test]
    fn inbox_and_search_always_overlay() {
        assert!(panel_should_overlay(false, 1320, &PanelMode::Inbox));
        assert!(panel_should_overlay(
            false,
            1320,
            &PanelMode::Search("x".into())
        ));
        assert_eq!(THREAD_COMPOSER_SINGLE_LINE_HEIGHT, 28);
    }
}

fn empty_state(icon: &str, title: &str, description: &str) -> gtk::Widget {
    adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .vexpand(true)
        .build()
        .upcast()
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}
