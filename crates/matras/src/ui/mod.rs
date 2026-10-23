//! Window construction and the single place where state changes are applied.
//!
//! Widget callbacks do not mutate state directly. They push an [`Action`] onto
//! a channel, and one loop on the GTK main thread applies it. That keeps the
//! borrow of `RefCell<AppState>` short and obviously non-overlapping, which is
//! otherwise the standard way a GTK + `Rc<RefCell<…>>` app panics at runtime.
//!
//! The one exception is the profile popover, which needs the widget it anchors
//! to; it is built inline from an `Rc<Ui>` capture instead.

mod account;
#[cfg(target_os = "android")]
mod android_context;
#[cfg(target_os = "android")]
mod android_permissions;
mod autocomplete;
mod browser;
mod call_dock;
mod chat;
mod dialogs;
mod interactive;
mod lightbox;
mod login;
mod media;
mod message;
mod notify;
mod profile;
mod rhs;
mod shortcuts;
mod sidebar;
pub mod sso;
mod storage;
mod switcher;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use mattermost_api::bootstrap::Bootstrap;
use mattermost_api::models::*;
use mattermost_api::ws::{Event, WebSocket, WsUpdate};
use mattermost_calls::{CallSession, CallUpdate, JoinOptions};

use crate::audio::AudioIo;
use crate::avatars::Avatars;
use crate::runtime;
use crate::state::{ActiveCall, AppState, ChannelFeed, SharedState};
use crate::video;
use call_dock::CallDock;
use chat::ChatView;
use message::{MessageActions, PostAction};
use rhs::{PanelMode, RightPanel};
use sidebar::{
    inbox_button_visibility, inbox_requires_chat_navigation, should_close_inbox_on_return,
    ChannelSidebar, RowAction,
};

/// The websocket prefix of <https://github.com/Toxblh/mattermost-reactions-notify-plugin>,
/// which notifies you about reactions to your own posts.
/// How long the server is left out of a burst of typing. Short enough that
/// the extra names arrive while the eye is still on the list, long enough
/// that a whole word costs one request.
const MENTION_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(180);

/// How long a landed picture waits for the rest of its flock before the
/// conversation is redrawn. Long enough to catch a screenful of faces
/// arriving together, short enough that nobody watches an avatar appear.
const AVATAR_REDRAW_WAIT: std::time::Duration = std::time::Duration::from_millis(120);

const REACTION_NOTIFY_PREFIX: &str = "custom_ru.toxblh.reactions-notify_";

/// Mentions answerable from memory: anyone whose handle or name *contains*
/// what has been typed.
///
/// Substring, not prefix — people search by surname ("fomche" for Semyon
/// Fomchenkov) far more often than by the start of a handle. Ranked so the
/// prefix matches still come first, because when the prefix is what was meant
/// it is nearly always the one wanted.
///
/// The scan is linear over the directory: username, nickname, first and last
/// name are each lowercased and searched independently, because the server's
/// teammate-name-display setting only picks what is *shown* — a server set to
/// show bare usernames still has to be searchable by surname.
/// `picture` is asked for a face only for the handful of people that survive
/// the filter — it is a side effect (a missing avatar starts a download), so
/// it must not run for the whole directory, and tests pass one that does
/// nothing.
fn local_mentions<'a>(
    users: impl Iterator<Item = &'a User>,
    lowered: &str,
    display: &str,
    picture: &dyn Fn(&str) -> Option<gtk::gdk::Texture>,
) -> Vec<autocomplete::Candidate> {
    /// How the match was made, and therefore how it sorts.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Rank {
        HandlePrefix,
        NamePrefix,
        Contains,
    }

    let mut found: Vec<(Rank, String, &User)> = Vec::new();
    for user in users {
        let handle = user.username.to_lowercase();
        let nickname = user.nickname.to_lowercase();
        let first = user.first_name.to_lowercase();
        let last = user.last_name.to_lowercase();
        // What the row shows respects the display setting; what it is found
        // by does not.
        let shown = user.display_name(display);

        let name_prefix = nickname.starts_with(lowered)
            || first.starts_with(lowered)
            || last.starts_with(lowered);
        let name_contains =
            nickname.contains(lowered) || first.contains(lowered) || last.contains(lowered);

        let rank = if handle.starts_with(lowered) {
            Rank::HandlePrefix
        } else if name_prefix {
            Rank::NamePrefix
        } else if handle.contains(lowered) || name_contains {
            Rank::Contains
        } else {
            continue;
        };
        found.push((rank, shown, user));
    }

    // Sorted rather than truncated early: a substring match found first must
    // not push out a prefix match found later.
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .take(COMPLETIONS)
        .map(|(_, name, user)| autocomplete::Candidate {
            insert: format!("@{}", user.username),
            primary: name,
            secondary: format!("@{}", user.username),
            image: picture(&user.id),
            user_id: Some(user.id.clone()),
        })
        .collect()
}

/// The `@names` a message mentions, by the same rule the renderer uses.
fn mentioned_names(message: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = message;
    while let Some(at) = rest.find('@') {
        // Mid-word, so it is an email address rather than a mention.
        let preceded = rest[..at].chars().next_back();
        rest = &rest[at + 1..];
        if preceded.is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '-' | '_')))
            .unwrap_or(rest.len());
        if end > 0 {
            names.push(rest[..end].trim_end_matches('.').to_string());
        }
        rest = &rest[end..];
    }
    names
}

/// Opening a post from the inbox is an explicit reading action, even when
/// its channel is already selected. Ordinary channel re-selection remains a
/// no-op; the caller uses this predicate on that early-return path.
fn inbox_post_reselect_needs_read_sync(
    current_channel: Option<&str>,
    target_channel: &str,
) -> bool {
    current_channel == Some(target_channel)
}

#[cfg(test)]
mod inbox_navigation_tests {
    use super::inbox_post_reselect_needs_read_sync;

    #[test]
    fn opening_a_mention_in_the_current_channel_still_syncs_read_state() {
        assert!(inbox_post_reselect_needs_read_sync(
            Some("channel-1"),
            "channel-1"
        ));
    }
}

#[cfg(test)]
mod mention_tests {
    use super::{local_mentions, mentioned_names, COMPLETIONS};
    use mattermost_api::models::User;

    /// A directory the size of a large company, to check that answering from
    /// memory stays instant. The claim is "under a frame"; this asserts an
    /// order of magnitude below that, so it fails long before anyone notices.
    #[test]
    fn ten_thousand_users_complete_instantly() {
        let users: Vec<User> = (0..10_000)
            .map(|i| User {
                id: format!("u{i}"),
                username: format!("person{i}"),
                first_name: "Person".into(),
                last_name: i.to_string(),
                ..Default::default()
            })
            .collect();

        let started = std::time::Instant::now();
        let found = local_mentions(users.iter(), "person9", "full_name", &|_| None);
        let elapsed = started.elapsed();

        assert_eq!(found.len(), COMPLETIONS);
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "took {elapsed:?} for 10k users"
        );

        // The worst case is a term nobody matches: every user is examined and
        // the early exit never fires.
        let started = std::time::Instant::now();
        let none = local_mentions(users.iter(), "nobodyatall", "full_name", &|_| None);
        let elapsed = started.elapsed();
        assert!(none.is_empty());
        // Both bounds are generous on purpose: this is an unoptimised build
        // on a machine that may be compiling something else at the same time,
        // and the old 10ms/30ms failed for that reason rather than for a slow
        // scan. What they guard against is an accidental quadratic, which
        // would be seconds rather than milliseconds.
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "worst case took {elapsed:?} for 10k users"
        );
    }

    #[test]
    fn matches_inside_a_name_not_just_the_start() {
        let users = [
            User {
                id: "u1".into(),
                username: "fomchenkovsv".into(),
                first_name: "Semyon".into(),
                last_name: "Fomchenkov".into(),
                ..Default::default()
            },
            User {
                id: "u2".into(),
                username: "fedorov".into(),
                first_name: "Fedor".into(),
                ..Default::default()
            },
        ];

        // Searching by surname, which is what people actually do.
        let found = local_mentions(users.iter(), "fomche", "full_name", &|_| None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].insert, "@fomchenkovsv");
        assert_eq!(found[0].primary, "Semyon Fomchenkov");

        // A prefix match outranks a substring one even when it is found later.
        let found = local_mentions(users.iter(), "fe", "full_name", &|_| None);
        assert_eq!(found[0].insert, "@fedorov");
    }

    #[test]
    fn finds_by_surname_even_when_the_server_shows_usernames() {
        let users = [User {
            id: "u1".into(),
            username: "ivan42".into(),
            first_name: "Семён".into(),
            last_name: "Фомченко".into(),
            ..Default::default()
        }];

        // The handle has nothing in common with the surname, and the display
        // setting is "username" — so a full_name search would have found
        // nothing, and only searching first/last name directly finds this.
        let found = local_mentions(users.iter(), "фомче", "username", &|_| None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].insert, "@ivan42");
        // The row still shows the configured display (username), not the
        // field it was actually found by.
        assert_eq!(found[0].primary, "ivan42");
    }

    #[test]
    fn finds_mentions_and_ignores_addresses() {
        assert_eq!(mentioned_names("hi @anna and @bob"), ["anna", "bob"]);
        // An email is not a mention, and neither is a trailing full stop.
        assert_eq!(mentioned_names("mail me at a@b.com"), Vec::<String>::new());
        assert_eq!(mentioned_names("ask @anna."), ["anna"]);
        assert_eq!(mentioned_names("nothing here"), Vec::<String>::new());
        assert_eq!(mentioned_names("@"), Vec::<String>::new());
    }
}

/// The icon for a channel in a flat list, where there is no "#" column.
fn channel_icon_name(channel: &Channel) -> String {
    match channel.r#type {
        ChannelType::Open => "network-workgroup-symbolic",
        ChannelType::Private => "changes-prevent-symbolic",
        ChannelType::Direct => "avatar-default-symbolic",
        _ => "system-users-symbolic",
    }
    .to_string()
}

/// A server URL with the scheme stripped, which is how people say it.
fn pretty_server(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// The entries in the two header menus.
#[derive(Debug, Clone, Copy)]
enum MenuAction {
    NewChannel,
    BrowseChannels,
    Preferences,
    EditProfile,
    CustomStatus,
    QuickSwitch,
    SignOut,
    ScheduledPosts,
    ChannelMembers,
    ChannelBookmarks,
    BrowseTeams,
    NewCategory,
    LeaveTeam,
    FocusSearch,
    OpenInbox,
    ClosePanel,
    NextUnread,
    PreviousUnread,
    EditChannel,
    ArchiveChannel,
    ChannelNotifications,
    LeaveChannel,
    PinnedPosts,
}

/// How many rows the quick switcher offers per source. Long enough to find
/// what you meant, short enough to stay a keyboard shortcut.
const QUICK_SWITCH_ROWS: usize = 10;

/// How many completion candidates to offer. More than this and the popover is
/// a list to read rather than a shortcut.
const COMPLETIONS: usize = 8;

/// How many messages to pull when a channel is first opened.
const INITIAL_POSTS: u32 = 60;
/// How many mentions and threads to keep in the inbox.
const INBOX_PAGE: u32 = 25;

enum Action {
    SelectTeam(String),
    SelectChannel(String),
    Send(String),
    /// Join the current channel's call, or leave the one we are in.
    ToggleCall,
    /// Turn our own microphone on or off.
    ToggleMute,
    /// Start or stop the server-side recording.
    ToggleRecording,
    /// Start or stop sharing the screen.
    ToggleScreen,
    /// Start or stop the camera.
    ToggleCamera,
    /// Open the thread rooted at this post id in the right panel.
    OpenThread(String),
    /// Reply into the currently open thread.
    ReplyInThread(String),
    /// Show the mentions and threads inbox.
    OpenInbox,
    CloseRightPanel,
    /// Toggle our own reaction: post id, emoji name.
    ToggleReaction(String, String),
    /// Jump to a channel, and into a thread when the second field is set.
    OpenPost(String, String),
    /// Open a channel and put a specific message on screen.
    JumpToPost(String, String),
    /// Open (or create) the direct-message channel with a user.
    OpenDirectMessage(String),
    /// Jump to the channel whose call we are in.
    OpenCallChannel,
    /// The composer's contents changed; the flag says whether it is non-empty.
    ComposerChanged(bool),
    /// Something from a message's own menu.
    Post(String, PostAction),
    /// Search this team's messages.
    Search(String),
    /// Open the file chooser to attach something.
    PickAttachment,
    /// Ask the LLM agent to summarise what is unread here.
    SummariseUnreads,
    /// Set your own presence.
    SetStatus(String),
    /// Something from a channel row's own menu.
    Row(String, sidebar::RowAction),
    /// Raise or lower your hand in the call.
    ToggleHand,
    /// Do something to another participant, as the call's host.
    HostControl(String, call_dock::HostAction),
    /// React in the call: emoji name and the glyph to show.
    CallReaction(String, String),
    /// Drop an uploaded file before it is sent.
    DropAttachment(String),
    /// Files arrived by drag and drop.
    AttachFiles(Vec<std::path::PathBuf>),
    /// The composer wants candidates for the token under the cursor.
    Complete(Option<autocomplete::Query>),
    /// Send what is in the composer at a chosen time instead of now.
    ScheduleMessage,
    /// The thread panel's reply box changed.
    ThreadDraftChanged,
    /// The reader reached the top of the feed and wants what came before.
    LoadOlder,
    /// Follow or unfollow the thread the panel is showing.
    FollowThread(bool),
}

/// A toast showing `message` as text. `AdwToast` reads its title as Pango
/// markup by default, so an `&` or `<` in an error would blank the toast.
fn plain_toast(message: &str) -> adw::Toast {
    adw::Toast::builder()
        .title(message.trim_end_matches('.'))
        .use_markup(false)
        .build()
}

/// What went wrong, for a person: server and network failures in words, and
/// anything we have no wording for as its own text.
fn human_error(error: &(dyn std::error::Error + 'static)) -> String {
    match error.downcast_ref::<mattermost_api::Error>() {
        Some(e) => login::describe_failure(e),
        None => error.to_string(),
    }
}

pub fn build_window(app: &adw::Application) {
    // `MATRAS_SIZE=400x800` opens at a phone-sized window, which is the only
    // practical way to look at the collapsed layout without a phone.
    let (width, height) = std::env::var("MATRAS_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((1320, 840));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Matras")
        .default_width(width)
        .default_height(height)
        .width_request(360)
        .height_request(294)
        .build();

    // Closing uses the same close-request path as the titlebar button; Ctrl+Q
    // remains the separate application-wide quit action.
    shortcuts::install_window_close_shortcut(app, &window);
    shortcuts::install_help_overlay(app, &window);

    let toast_overlay = adw::ToastOverlay::new();
    window.set_content(Some(&toast_overlay));

    // Ask for the microphone up front, so the first call has audio rather than
    // failing once to raise the grant dialog. The surface only exists once the
    // window is on screen, so wait for the map.
    #[cfg(target_os = "android")]
    window.connect_map(|window| {
        if let Some(surface) = window.surface() {
            // ndk_context first: cpal panics reaching it if it is not set, and
            // the panic is invisible on Android (stderr → /dev/null).
            android_context::ensure(&surface);
            android_permissions::ensure_record_audio(&surface);
        }
    });

    // A layout-only mode, so the panes can be reviewed without a server.
    if std::env::var_os("MATRAS_DEMO").is_some() {
        start_demo(&window, &toast_overlay);
        window.present();
        screenshot_and_quit(&window);
        return;
    }

    // Development shortcut: skip the form when credentials are in the
    // environment. Handy against a local server, and how the UI gets exercised
    // end to end without a human typing.
    if let (Ok(url), Ok(login_id), Ok(password)) = (
        std::env::var("MATRAS_SERVER"),
        std::env::var("MATRAS_USER"),
        std::env::var("MATRAS_PASSWORD"),
    ) {
        if let Ok(client) = mattermost_api::Client::new(&url) {
            let window_ = window.clone();
            let toasts = toast_overlay.clone();
            runtime::spawn(
                async move {
                    client
                        .login(&login_id, &password, None)
                        .await
                        .map(|me| (client, me))
                },
                move |result| match result {
                    Ok((client, me)) => start_session(&window_, &toasts, client, me),
                    Err(e) => toasts.add_toast(plain_toast(&format!(
                        "Sign-in failed: {}",
                        login::describe(&e)
                    ))),
                },
            );
            window.present();
            return;
        }
    }

    let show_login = {
        let window = window.clone();
        let toasts = toast_overlay.clone();
        move || {
            let view = login::build({
                let window = window.clone();
                let toasts = toasts.clone();
                move |result| {
                    // Only now is the token known to work.
                    let token = result.client.token().unwrap_or_default();
                    let server = result.client.site_url().to_string();
                    runtime::spawn(
                        async move { crate::session::save_async(&server, &token).await },
                        |_| {},
                    );
                    start_session(&window, &toasts, result.client, result.me);
                }
            });
            toasts.set_child(Some(&view));
        }
    };

    // Reading the keyring can prompt for an unlock, so the window goes up
    // first and the stored session arrives into it.
    {
        let window_ = window.clone();
        let toasts = toast_overlay.clone();
        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(32)
            .height_request(32)
            .build();
        spinner.start();
        toast_overlay.set_child(Some(&spinner));

        runtime::spawn(
            crate::session::load_all_async(),
            move |stored| match stored.as_slice() {
                [] => show_login(),
                [(server, token)] => {
                    restore_session(&window_, &toasts, server.clone(), token.clone(), show_login)
                }
                // More than one account is stored, so ask rather than guessing
                // which one this launch is for.
                many => {
                    let servers: Vec<(String, String)> = many
                        .iter()
                        .map(|(server, _)| (server.clone(), pretty_server(server)))
                        .collect();
                    let stored = many.to_vec();
                    let window = window_.clone();
                    let toasts_ = toasts.clone();
                    let show_login = Rc::new(show_login);
                    let add = show_login.clone();
                    account::choose_server(
                        &window_.clone(),
                        servers,
                        move |chosen| {
                            let Some((server, token)) =
                                stored.iter().find(|(s, _)| s == &chosen).cloned()
                            else {
                                return;
                            };
                            let show_login = show_login.clone();
                            restore_session(&window, &toasts_, server, token, move || show_login());
                        },
                        move || add(),
                    );
                }
            },
        );
    }

    window.present();
}

/// Tries a stored token before showing the sign-in form. `show_login` runs if
/// the token is gone or stale — a session Mattermost has since revoked is the
/// ordinary case here, not an error worth a dialog.
fn restore_session(
    window: &adw::ApplicationWindow,
    toasts: &adw::ToastOverlay,
    server: String,
    token: String,
    show_login: impl FnOnce() + 'static,
) {
    let client = match mattermost_api::Client::new(&server) {
        Ok(c) => c,
        Err(_) => {
            runtime::spawn(crate::session::clear_async(), |_| {});
            crate::cache::clear();
            show_login();
            return;
        }
    };
    client.set_token(token);

    // No round trip before this. A token read back from the keyring used to
    // be checked with its own `client.me()` call before anything was built —
    // which meant a network reply gated the very first pixel of a channel
    // list that the local store already had sitting on disk. Trusting the
    // token on sight and building the session window straight away lets
    // `bootstrap`'s cache read (see its doc comment) draw into it before the
    // network has said a word. If the token turns out to be no good,
    // `bootstrap`'s own account fetch is what finds that out, and the
    // `on_auth_failure` fallback below undoes the guess.
    let state: SharedState = Rc::new(RefCell::new(AppState::new(
        client,
        User::default(),
        ClientConfig::default(),
        false,
    )));
    let ui = build_session_ui(window, toasts, state);
    bootstrap(ui, Some(Box::new(show_login)));
}

/// Replaces the login view with the main UI and kicks off the startup sequence.
fn start_session(
    window: &adw::ApplicationWindow,
    toasts: &adw::ToastOverlay,
    client: mattermost_api::Client,
    me: User,
) {
    let state: SharedState = Rc::new(RefCell::new(AppState::new(
        client,
        me,
        ClientConfig::default(),
        false,
    )));
    let ui = build_session_ui(window, toasts, state);
    // The account here was already proven live (a fresh login, or the
    // dev-shortcut password login), so there is no stale token to fall back
    // from — unlike `restore_session`'s untested one.
    bootstrap(ui, None);
}

/// Shows the UI filled with sample data, for looking at the layout without a
/// server. Enabled with `MATRAS_DEMO=1`.
pub fn start_demo(window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay) {
    let state = crate::demo::state();
    if std::env::var_os("MATRAS_DEMO_EMPTY_CHAT").is_some() {
        state.borrow_mut().current_channel = None;
    }
    let ui = build_session_ui(window, toasts, state);
    if std::env::var_os("MATRAS_DEMO_VIDEO").is_some() {
        if let Some(path) = std::env::var_os("MATRAS_DEMO_VIDEO_POSTER") {
            let file = gtk::gio::File::for_path(path);
            match gtk::gdk::Texture::from_file(&file) {
                Ok(texture) => ui.avatars.remember_poster("f-demo-video", texture),
                Err(error) => tracing::warn!("could not load demo video poster: {error}"),
            }
        }
    }
    ui.refresh_all();
    if std::env::var_os("MATRAS_DEMO_CLICK_SIDEBAR_INBOX").is_some() {
        glib::timeout_add_local_once(std::time::Duration::from_millis(200), {
            let ui = ui.clone();
            move || ui.channels.activate_inbox_button()
        });
    } else if std::env::var_os("MATRAS_DEMO_OPEN_THREAD").is_some() {
        glib::timeout_add_local_once(std::time::Duration::from_millis(200), {
            let ui = ui.clone();
            move || ui.open_thread("p-5".into())
        });
    } else if std::env::var_os("MATRAS_DEMO_OPEN_MENTION").is_some() {
        let mention = ui
            .state
            .borrow()
            .mentions
            .first()
            .map(|post| (post.channel_id.clone(), post.id.clone()));
        if let Some((channel_id, post_id)) = mention {
            ui.jump_to_post(channel_id, post_id);
        }
    } else if std::env::var_os("MATRAS_DEMO_OPEN_INBOX").is_some() {
        ui.dispatch(Action::OpenInbox);
    } else if std::env::var_os("MATRAS_DEMO_EMPTY_CHAT").is_some() {
        // Render the same no-channel state used by the default startup page.
        ui.right.set_mode(PanelMode::Hidden);
        ui.split.set_show_content(true);
        ui.refresh_messages();
        ui.refresh_call_ui();
    } else if std::env::var_os("MATRAS_DEMO_SHOW_SIDEBAR").is_some() {
        // The narrow breakpoint does not mark the split as collapsed until
        // the presented window has received its first allocation.
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), {
            let ui = ui.clone();
            move || ui.split.set_show_content(false)
        });
    } else {
        let initial = ui.state.borrow().current_channel.clone();
        if let Some(id) = initial {
            ui.dispatch(Action::SelectChannel(id));
        }
    }
    if std::env::var_os("MATRAS_DEMO_DIALOG").is_some() {
        interactive::present(window, &crate::demo::interactive_dialog(), |_| {}, || {});
    }
    if std::env::var_os("MATRAS_DEMO_CALL").is_some() {
        ui.dock.show_demo_preview(true);
        ui.dock_visible.set(true);
        ui.host_toolbar().set_reveal_bottom_bars(true);
    }
}

/// Builds the window contents and starts the action loop.
fn build_session_ui(
    window: &adw::ApplicationWindow,
    toasts: &adw::ToastOverlay,
    state: SharedState,
) -> Rc<Ui> {
    let (tx, rx) = async_channel::unbounded::<Action>();

    // --- pane 3: the conversation
    let chat = Rc::new(ChatView::new(chat::ChatCallbacks {
        on_send: Box::new({
            let tx = tx.clone();
            move |text| {
                let _ = tx.send_blocking(Action::Send(text));
            }
        }),
        on_typing: Box::new({
            let tx = tx.clone();
            move |has_text| {
                let _ = tx.send_blocking(Action::ComposerChanged(has_text));
            }
        }),
        on_attach: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::PickAttachment);
            }
        }),
        on_files: Box::new({
            let tx = tx.clone();
            move |paths| {
                let _ = tx.send_blocking(Action::AttachFiles(paths));
            }
        }),
        on_complete: Box::new({
            let tx = tx.clone();
            move |query| {
                let _ = tx.send_blocking(Action::Complete(query));
            }
        }),
        on_scrollback: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::LoadOlder);
            }
        }),
        on_schedule: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ScheduleMessage);
            }
        }),
        on_agent: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::SummariseUnreads);
            }
        }),
        on_call: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleCall);
            }
        }),
        on_inbox: Box::new({
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::OpenInbox);
            }
        }),
    }));

    // --- pane 4: thread / inbox
    let right = RightPanel::new(
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::CloseRightPanel);
            }
        },
        {
            let tx = tx.clone();
            move |text| {
                let _ = tx.send_blocking(Action::ReplyInThread(text));
            }
        },
        {
            let tx = tx.clone();
            move |root| {
                let _ = tx.send_blocking(Action::OpenThread(root));
            }
        },
        {
            let tx = tx.clone();
            move |channel, root| {
                let _ = tx.send_blocking(Action::OpenPost(channel, root));
            }
        },
        {
            let tx = tx.clone();
            move |channel, post| {
                let _ = tx.send_blocking(Action::JumpToPost(channel, post));
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ThreadDraftChanged);
            }
        },
        {
            let tx = tx.clone();
            move |following| {
                let _ = tx.send_blocking(Action::FollowThread(following));
            }
        },
    );

    // The right panel overlays the conversation rather than adding a permanent
    // column: it is open only while you are reading a thread or the inbox.
    let overlay = adw::OverlaySplitView::builder()
        .content(&chat.widget)
        .sidebar(&right.widget)
        .show_sidebar(false)
        .min_sidebar_width(280.0)
        .max_sidebar_width(460.0)
        .sidebar_width_fraction(0.30)
        .build();
    overlay.set_sidebar_position(gtk::PackType::End);

    // --- the call dock, pinned under the channel list
    let dock = Rc::new(CallDock::new(
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::OpenCallChannel);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleMute);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleScreen);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleCamera);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleRecording);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleHand);
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::ToggleCall);
            }
        },
        {
            let tx = tx.clone();
            move |session_id, action| {
                let _ = tx.send_blocking(Action::HostControl(session_id, action));
            }
        },
        {
            let tx = tx.clone();
            move |action| {
                // These take no target: they apply to the whole call.
                let _ = tx.send_blocking(Action::HostControl(String::new(), action));
            }
        },
        {
            let tx = tx.clone();
            move |name, glyph| {
                let _ = tx.send_blocking(Action::CallReaction(name, glyph));
            }
        },
    ));

    // --- pane 1: channels, with the team switcher in its header
    let channels = Rc::new(ChannelSidebar::new(
        {
            let tx = tx.clone();
            move |id| {
                let _ = tx.send_blocking(Action::SelectChannel(id));
            }
        },
        {
            let tx = tx.clone();
            move |id| {
                let _ = tx.send_blocking(Action::SelectTeam(id));
            }
        },
        {
            let tx = tx.clone();
            move |terms| {
                let _ = tx.send_blocking(Action::Search(terms));
            }
        },
        {
            let tx = tx.clone();
            move || {
                let _ = tx.send_blocking(Action::OpenInbox);
            }
        },
        {
            let tx = tx.clone();
            move |status| {
                let _ = tx.send_blocking(Action::SetStatus(status));
            }
        },
        {
            let tx = tx.clone();
            move |channel_id, action| {
                let _ = tx.send_blocking(Action::Row(channel_id, action));
            }
        },
        dock.widget.upcast_ref(),
    ));

    let chat_page = adw::NavigationPage::builder()
        .title("Conversation")
        .child(&overlay)
        .build();
    let channels_page = adw::NavigationPage::builder()
        .title("Channels")
        .child(&channels.widget)
        .build();

    let split = adw::NavigationSplitView::builder()
        .sidebar(&channels_page)
        .content(&chat_page)
        .min_sidebar_width(220.0)
        .max_sidebar_width(360.0)
        .sidebar_width_fraction(0.24)
        // When collapsed these behave like a navigation stack, and the page a
        // chat app should land on is the conversation — not the sidebar.
        .show_content(true)
        .build();
    // When a narrow window returns from a conversation to the channel list,
    // clear ListBox's retained selection. Otherwise selecting that same
    // conversation again does not emit row-selected, so it appears to need a
    // double click.
    let update_inbox_buttons: Rc<dyn Fn(&adw::NavigationSplitView)> = Rc::new({
        let channels = channels.clone();
        let chat = chat.clone();
        move |split| {
            let (show_in_sidebar, show_in_chat) =
                inbox_button_visibility(split.is_collapsed(), split.shows_content());
            channels.set_inbox_button_visible(show_in_sidebar);
            chat.set_inbox_button_visible(show_in_chat);
        }
    });
    split.connect_show_content_notify({
        let channels = channels.clone();
        let right = right.clone();
        let tx = tx.clone();
        let update = update_inbox_buttons.clone();
        move |split| {
            let collapsed = split.is_collapsed();
            let showing_content = split.shows_content();
            channels.navigation_state_changed(collapsed, showing_content);
            update(split);
            if should_close_inbox_on_return(
                collapsed,
                showing_content,
                right.mode() == PanelMode::Inbox,
            ) {
                let _ = tx.send_blocking(Action::CloseRightPanel);
            }
        }
    });
    update_inbox_buttons(&split);

    // Collapse in two steps: the thread panel overlays the conversation first,
    // then the channel list folds into a navigation stack.
    //
    // Only the channel list is breakpoint-driven. The right panel's `collapsed`
    // is computed in `refresh_panel_mode`, because what it should do depends on
    // *what* is in it as well as the width — and a breakpoint setter fighting
    // our own writes over one property is a bug Planify actually shipped.
    //
    // The width for that decision is read off the window rather than taken from
    // a second breakpoint: only the last matching breakpoint is applied, so an
    // overlapping one would silently cancel this one's setter.
    let narrow = Rc::new(std::cell::Cell::new(false));
    add_breakpoint(window, 700.0, &[(&split, true)], &[]);

    // Remote video floats over the panes rather than in them: a share is
    // something you glance at while carrying on reading.
    let videos = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::End)
        .margin_start(12)
        .margin_bottom(12)
        .visible(false)
        .build();
    let video_overlay = gtk::Overlay::new();
    video_overlay.set_child(Some(&split));
    video_overlay.add_overlay(&videos);

    // A breakpoint bin rather than the window's own breakpoints: `narrow` has to
    // follow the *allocated* width, and a second window breakpoint would cancel
    // the sidebar one — only the last matching breakpoint is ever applied.
    let bin = adw::BreakpointBin::builder()
        .width_request(360)
        .height_request(294)
        .child(&video_overlay)
        .build();
    toasts.set_child(Some(&bin));

    let avatars = Avatars::new(state.borrow().client.clone());

    let ui = Rc::new(Ui {
        window: window.clone(),
        state: state.clone(),
        channels: channels.clone(),
        dock: dock.clone(),
        chat: chat.clone(),
        right: right.clone(),
        overlay: overlay.clone(),
        videos: videos.clone(),
        video_views: RefCell::new(HashMap::new()),
        split: split.clone(),
        avatars: avatars.clone(),
        toasts: toasts.clone(),
        narrow: narrow.clone(),
        sidebar_reload_pending: std::cell::Cell::new(false),
        typing_sweep_pending: std::cell::Cell::new(false),
        draft_save_pending: std::cell::Cell::new(false),
        thread_draft_pending: std::cell::Cell::new(false),
        loading_older: std::cell::Cell::new(false),
        snapshot_pending: std::cell::Cell::new(false),
        pruned_at: std::cell::Cell::new(None),
        store: RefCell::new(None),
        typing_sent_recently: std::cell::Cell::new(false),
        completion_generation: std::cell::Cell::new(0),
        avatar_redraw_pending: std::cell::Cell::new(false),
        mention_query_pending: std::cell::Cell::new(false),
        mention_query: RefCell::new(None),
        last_completions: RefCell::new(Vec::new()),
        dock_in_chat: std::cell::Cell::new(false),
        dock_visible: std::cell::Cell::new(false),
        tx: tx.clone(),
    });

    ui.place_dock(call_dock::dock_belongs_in_chat(
        split.is_collapsed(),
        split.shows_content(),
    ));
    split.connect_show_content_notify({
        let ui = ui.clone();
        move |split| {
            ui.place_dock(call_dock::dock_belongs_in_chat(
                split.is_collapsed(),
                split.shows_content(),
            ));
        }
    });

    // Below this the thread panel has to overlay rather than take a column.
    // `default-width` cannot answer that: a maximized or tiled window keeps the
    // size it would restore to, so a phone compositor left this reading 1320 and
    // the panel took a column that fell off the side of the screen.
    const STATIC_PANEL_MIN_WIDTH: f64 = 1200.0;
    let panel_breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        STATIC_PANEL_MIN_WIDTH,
        adw::LengthUnit::Sp,
    ));
    for (apply, is_narrow) in [(true, true), (false, false)] {
        let ui = ui.clone();
        let narrow = narrow.clone();
        let handler = move |_: &adw::Breakpoint| {
            if narrow.replace(is_narrow) != is_narrow {
                ui.refresh_panel_mode();
            }
        };
        if apply {
            panel_breakpoint.connect_apply(handler);
        } else {
            panel_breakpoint.connect_unapply(handler);
        }
    }
    bin.add_breakpoint(panel_breakpoint);

    // The dock lives in the sidebar, except when the sidebar is a page you have
    // navigated away from — then it belongs under the conversation.
    split.connect_collapsed_notify({
        let ui = ui.clone();
        let update = update_inbox_buttons.clone();
        move |split| {
            ui.place_dock(call_dock::dock_belongs_in_chat(
                split.is_collapsed(),
                split.shows_content(),
            ));
            update(split);
        }
    });

    // The debounce means the last few seconds would otherwise be lost, and
    // closing the window is exactly when the next launch's picture is decided.
    window.connect_close_request({
        let ui = ui.clone();
        move |window| {
            ui.save_snapshot();
            if rhs::window_close_dismisses_panel(&ui.right.mode(), ui.right.overlaid()) {
                ui.dispatch(Action::CloseRightPanel);
                return glib::Propagation::Stop;
            }
            // Closing the window must not tear the session down when the app
            // is meant to keep running: destroying it would drop the socket
            // and then the next activation would build a *second* session
            // alongside the first, which is unreachable. Hiding keeps exactly
            // one, and presenting it again is instant.
            if crate::background::Background::enabled() {
                window.set_visible(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    // The header menus drive these. They are window actions rather than
    // callbacks so the menu models can name them declaratively.
    for (name, action) in [
        ("new-channel", MenuAction::NewChannel),
        ("browse-channels", MenuAction::BrowseChannels),
        ("preferences", MenuAction::Preferences),
        ("edit-profile", MenuAction::EditProfile),
        ("quick-switch", MenuAction::QuickSwitch),
        ("sign-out", MenuAction::SignOut),
        ("scheduled-posts", MenuAction::ScheduledPosts),
        ("channel-members", MenuAction::ChannelMembers),
        ("channel-bookmarks", MenuAction::ChannelBookmarks),
        ("browse-teams", MenuAction::BrowseTeams),
        ("new-category", MenuAction::NewCategory),
        ("search", MenuAction::FocusSearch),
        ("inbox", MenuAction::OpenInbox),
        ("close-panel", MenuAction::ClosePanel),
        ("next-unread", MenuAction::NextUnread),
        ("previous-unread", MenuAction::PreviousUnread),
        ("leave-team", MenuAction::LeaveTeam),
        ("edit-channel", MenuAction::EditChannel),
        ("archive-channel", MenuAction::ArchiveChannel),
        ("custom-status", MenuAction::CustomStatus),
        ("channel-notifications", MenuAction::ChannelNotifications),
        ("leave-channel", MenuAction::LeaveChannel),
        ("pinned-posts", MenuAction::PinnedPosts),
    ] {
        let entry = gtk::gio::SimpleAction::new(name, None);
        entry.connect_activate({
            let ui = ui.clone();
            move |_, _| ui.menu_action(action)
        });
        window.add_action(&entry);
    }

    if let Some(app) = window.application() {
        // The shortcuts a chat client is expected to have. Anything reachable
        // only by mouse is reachable only slowly.
        for (action, keys) in [
            ("win.quick-switch", &["<Control>k"][..]),
            ("win.search", &["<Control>f"]),
            ("win.inbox", shortcuts::INBOX_ACCELERATOR),
            ("win.new-channel", &["<Control>n"]),
            ("win.preferences", &["<Control>comma"]),
            ("win.next-unread", &["<Control><Shift>Down"]),
            ("win.previous-unread", &["<Control><Shift>Up"]),
        ] {
            app.set_accels_for_action(action, keys);
        }
    }

    // Escape is not an application accelerator: those fire in the capture
    // phase, ahead of the focused widget, so the key meant to close a mention
    // popup, clear the search field or dismiss a dialog would leave the
    // channel instead. In the bubble phase anything that wants it gets it first.
    let escape = gtk::ShortcutController::new();
    escape.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("win.close-panel")),
    ));
    window.add_controller(escape);

    // Clicking a notification lands here. The action is on the application so
    // it stays valid while the app is running, which is what the notification
    // holds a reference to.
    if let Some(app) = window.application() {
        let action =
            gtk::gio::SimpleAction::new("open-channel", Some(&String::static_variant_type()));
        action.connect_activate({
            let ui = ui.clone();
            move |_, target| {
                let Some(channel_id) = target.and_then(|t| t.get::<String>()) else {
                    return;
                };
                ui.window.set_visible(true);
                ui.window.present();
                ui.dispatch(Action::SelectChannel(channel_id));
            }
        });
        app.add_action(&action);

        // The buttons on an incoming-call notification.
        for (name, join) in [("join-call", true), ("dismiss-call", false)] {
            let call_action =
                gtk::gio::SimpleAction::new(name, Some(&String::static_variant_type()));
            call_action.connect_activate({
                let ui = ui.clone();
                move |_, target| {
                    let Some(channel_id) = target.and_then(|t| t.get::<String>()) else {
                        return;
                    };
                    ui.stop_ringing();
                    if join {
                        ui.window.set_visible(true);
                        ui.window.present();
                        ui.dispatch(Action::SelectChannel(channel_id));
                        ui.dispatch(Action::ToggleCall);
                    } else {
                        // In a DM there is one person waiting, and declining
                        // tells them; anywhere else there is nobody to tell,
                        // so it is only silenced for us and our other devices.
                        let (client, direct) = {
                            let st = ui.state.borrow();
                            let direct = st
                                .channel(&channel_id)
                                .is_some_and(|c| matches!(c.r#type, ChannelType::Direct));
                            (st.client.clone(), direct)
                        };
                        runtime::spawn(
                            async move {
                                if direct {
                                    mattermost_calls::decline(&client, &channel_id).await
                                } else {
                                    mattermost_calls::dismiss_notification(&client, &channel_id)
                                        .await
                                }
                            },
                            |_| {},
                        );
                    }
                }
            });
            app.add_action(&call_action);
        }
    }

    // Faces update their weakly registered Avatar widgets directly. Resources
    // that alter a message's widget tree rebind only the rows that reference
    // that cache key; they must never replace the whole ListStore, because
    // doing so destroys GtkListView's scroll anchor.
    avatars.connect_loaded({
        let ui = ui.clone();
        move |key| {
            if !key.contains(':') {
                ui.refresh_completion_avatars();
                return;
            }
            if ui.avatar_redraw_pending.replace(true) {
                return;
            }
            let ui = ui.clone();
            let key = key.to_string();
            glib::timeout_add_local_once(AVATAR_REDRAW_WAIT, move || {
                ui.avatar_redraw_pending.set(false);
                let actions = ui.message_actions();
                ui.chat
                    .refresh_resource(&key, &ui.state, &ui.avatars, &actions);
                ui.right.refresh(&ui.state, &ui.avatars, &actions);
            });
        }
    });

    glib::spawn_future_local({
        let ui = ui.clone();
        async move {
            while let Ok(action) = rx.recv().await {
                ui.handle(action);
            }
        }
    });

    ui
}

fn add_breakpoint(
    window: &adw::ApplicationWindow,
    max_width_sp: f64,
    splits: &[(&adw::NavigationSplitView, bool)],
    overlays: &[(&adw::OverlaySplitView, bool)],
) {
    let condition = adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        max_width_sp,
        adw::LengthUnit::Sp,
    );
    let breakpoint = adw::Breakpoint::new(condition);
    for (view, collapsed) in splits {
        breakpoint.add_setter(*view, "collapsed", Some(&(*collapsed).to_value()));
    }
    for (view, collapsed) in overlays {
        breakpoint.add_setter(*view, "collapsed", Some(&(*collapsed).to_value()));
    }
    window.add_breakpoint(breakpoint);
}

/// Everything the action loop needs.
struct Ui {
    window: adw::ApplicationWindow,
    state: SharedState,
    channels: Rc<ChannelSidebar>,
    dock: Rc<CallDock>,
    chat: Rc<ChatView>,
    right: Rc<RightPanel>,
    overlay: adw::OverlaySplitView,
    /// Where remote screens and cameras are shown, keyed by the media session
    /// they belong to.
    videos: gtk::Box,
    video_views: RefCell<HashMap<String, video::RemoteView>>,
    split: adw::NavigationSplitView,
    avatars: Avatars,
    toasts: adw::ToastOverlay,
    /// Set while the window is too narrow for a static thread column.
    narrow: Rc<std::cell::Cell<bool>>,
    sidebar_reload_pending: std::cell::Cell<bool>,
    typing_sweep_pending: std::cell::Cell<bool>,
    draft_save_pending: std::cell::Cell<bool>,
    thread_draft_pending: std::cell::Cell<bool>,
    loading_older: std::cell::Cell<bool>,
    snapshot_pending: std::cell::Cell<bool>,
    pruned_at: std::cell::Cell<Option<std::time::Instant>>,
    /// The local message store, once it has opened.
    store: RefCell<Option<crate::store::Store>>,
    typing_sent_recently: std::cell::Cell<bool>,
    /// Bumped on every completion query, so a slow answer for a term the
    /// person has already typed past is discarded rather than replacing the
    /// list under them.
    completion_generation: std::cell::Cell<u64>,
    /// Set while a redraw for freshly-landed pictures is already booked.
    avatar_redraw_pending: std::cell::Cell<bool>,
    /// Set while a debounced `@mention` network lookup is scheduled; further
    /// keystrokes just overwrite `mention_query` instead of scheduling again.
    mention_query_pending: std::cell::Cell<bool>,
    /// The term/team/channel/client the pending lookup above will use — always
    /// the latest keystroke's, not the one that started the debounce.
    mention_query: RefCell<Option<(String, String, String, mattermost_api::Client)>>,
    /// The candidates currently shown in the completion popover, kept around
    /// so a picture landing later can redraw them without asking again.
    last_completions: RefCell<Vec<autocomplete::Candidate>>,
    dock_in_chat: std::cell::Cell<bool>,
    dock_visible: std::cell::Cell<bool>,
    tx: async_channel::Sender<Action>,
}

/// What one websocket event did to one post, so the feed can redraw that row
/// instead of every row.
enum PostChange {
    Added(Post),
    Changed(Post),
    Removed(String),
}

impl Ui {
    fn toast(&self, message: &str) {
        self.toasts.add_toast(plain_toast(message));
    }

    /// The start failed. With last time's channels on screen that is a state
    /// of the connection, said by its banner, with a Retry on the toast; with
    /// nothing saved there is nothing to show, so the whole window says why
    /// and offers to try again.
    fn show_load_failure(self: &Rc<Self>, error: &mattermost_api::Error, had_cache: bool) {
        let reason = login::describe_failure(error);
        let retry = {
            let ui = self.clone();
            move || bootstrap(ui.clone(), None)
        };
        if had_cache {
            self.chat
                .set_connection_problem(Some("Offline — showing saved messages"));
            let toast = adw::Toast::builder()
                .title(format!("Could not load your account: {reason}"))
                .use_markup(false)
                .button_label("Retry")
                .timeout(0)
                .build();
            toast.connect_button_clicked(move |_| retry());
            self.toasts.add_toast(toast);
            return;
        }

        let Some(session) = self.toasts.child() else {
            return;
        };
        let button = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .build();
        button.add_css_class("pill");
        button.add_css_class("suggested-action");
        let page = adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("Could Not Load Your Account")
            .description(glib::markup_escape_text(&reason))
            .child(&button)
            .vexpand(true)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        self.toasts.set_child(Some(&view));
        button.connect_clicked({
            let toasts = self.toasts.clone();
            move |_| {
                toasts.set_child(Some(&session));
                retry();
            }
        });
        button.grab_focus();
    }

    /// The account's "Notification sound" switch; on unless turned off.
    fn notification_sound(&self) -> bool {
        self.state
            .borrow()
            .me
            .notify_props
            .get("desktop_sound")
            .map(String::as_str)
            != Some("false")
    }

    /// A failure, in words rather than as the error's own debug text.
    fn toast_error(&self, what: &str, error: &(dyn std::error::Error + 'static)) {
        self.toast(&format!("{what}: {}", human_error(error)));
    }

    fn dispatch(&self, action: Action) {
        let _ = self.tx.send_blocking(action);
    }

    /// Answers an outstanding completion query, and remembers the answer so a
    /// picture landing later can redraw the popover without re-querying it.
    fn set_completions(&self, items: Vec<autocomplete::Candidate>) {
        *self.last_completions.borrow_mut() = items.clone();
        self.chat.set_completions(items);
    }

    /// A picture that finished downloading after the popover opened belongs
    /// to one of the rows already drawn — redraw it with whatever pictures
    /// are available now. No query, no network: same candidates, same order.
    fn refresh_completion_avatars(&self) {
        let mut items = self.last_completions.borrow().clone();
        if items.is_empty() {
            return;
        }
        let mut changed = false;
        for item in &mut items {
            if item.image.is_some() {
                continue;
            }
            if let Some(id) = &item.user_id {
                if let Some(texture) = self.avatars.texture(id) {
                    item.image = Some(texture);
                    changed = true;
                }
            }
        }
        if changed {
            self.set_completions(items);
        }
    }

    /// The callbacks a message row needs. Rebuilt per redraw, which is cheap —
    /// they are three `Rc` clones.
    fn message_actions(self: &Rc<Self>) -> MessageActions {
        MessageActions {
            open_thread: {
                let ui = self.clone();
                Rc::new(move |root| ui.dispatch(Action::OpenThread(root)))
            },
            toggle_reaction: {
                let ui = self.clone();
                Rc::new(move |post, emoji| ui.dispatch(Action::ToggleReaction(post, emoji)))
            },
            post_action: {
                let ui = self.clone();
                Rc::new(move |post, what| ui.dispatch(Action::Post(post, what)))
            },
            open_permalink: {
                let ui = self.clone();
                Rc::new(move |post_id| ui.open_permalink(post_id))
            },
            show_profile_by_handle: {
                let ui = self.clone();
                Rc::new(move |handle, anchor| ui.show_profile_by_handle(&handle, &anchor))
            },
            show_profile: {
                let ui = self.clone();
                Rc::new(move |user_id, anchor| ui.show_profile(&user_id, &anchor))
            },
        }
    }

    /// Redraws only the message surfaces — used when something cosmetic lands,
    /// such as an avatar finishing its download.
    fn refresh_messages(self: &Rc<Self>) {
        let inbox_count = {
            let state = self.state.borrow();
            state
                .mentions
                .len()
                .max(state.unread_threads().max(0) as usize) as i64
        };
        self.channels.set_inbox_count(inbox_count);
        let actions = self.message_actions();
        self.chat.refresh(&self.state, &self.avatars, &actions);
        self.right.refresh(&self.state, &self.avatars, &actions);
    }

    /// Redraws only the thread panel — used when a thread's own data changes,
    /// which has nothing to do with the channel feed behind it. The feed's
    /// "N replies" footer is kept live separately, by the ordinary post-apply
    /// path that already runs on every incoming reply.
    fn refresh_thread_panel(self: &Rc<Self>) {
        let actions = self.message_actions();
        self.right.refresh(&self.state, &self.avatars, &actions);
    }

    fn refresh_all(self: &Rc<Self>) {
        self.channels.refresh(&self.state, &self.avatars);
        self.refresh_messages();
        self.refresh_call_ui();
        self.refresh_title();
        self.hydrate_dm_teammates();
    }

    /// Applies a streamed LLM answer.
    ///
    /// `next` carries the whole message so far rather than the new part, so
    /// this replaces the text instead of appending — appending would double
    /// every character.
    fn apply_stream_update(self: &Rc<Self>, data: &mattermost_api::ws::Data) {
        use crate::agents::StreamUpdate;
        match crate::agents::parse_stream(data) {
            StreamUpdate::Text { post_id, message } => {
                let mut st = self.state.borrow_mut();
                let Some(mut post) = st.post(&post_id) else {
                    // The post itself arrives over the ordinary `posted`
                    // event; a stream frame that beats it has nothing to
                    // write into yet, and the next frame will.
                    return;
                };
                post.message = message;
                st.apply_post(post.clone());
                drop(st);
                let actions = self.message_actions();
                if !self
                    .chat
                    .replace_post(&post, &self.state, &self.avatars, &actions)
                {
                    self.refresh_messages();
                } else {
                    // A visible thread/search result can contain this post as
                    // well. The channel feed stays a one-row splice above.
                    self.right.refresh(&self.state, &self.avatars, &actions);
                }
            }
            // The final text already arrived as a Text frame, and the post is
            // updated server-side too; nothing left to do.
            StreamUpdate::Done { .. } | StreamUpdate::Ignored => {}
        }
    }

    /// Announces an incoming call, with the two things you might want to do
    /// about it. Mattermost has no ring signal of its own — a call starting is
    /// the whole event — so this is the client's doing.
    fn ring(self: &Rc<Self>, channel_id: &str) {
        let title = {
            let st = self.state.borrow();
            st.channel(channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_else(|| "Someone".to_string())
        };
        let Some(app) = self.window.application() else {
            return;
        };

        let notification = gtk::gio::Notification::new(&format!("{title} is calling"));
        notification.set_body(Some("Incoming call"));
        notification.set_priority(gtk::gio::NotificationPriority::Urgent);
        notification.add_button_with_target_value(
            "Join",
            "app.join-call",
            Some(&channel_id.to_variant()),
        );
        notification.add_button_with_target_value(
            "Dismiss",
            "app.dismiss-call",
            Some(&channel_id.to_variant()),
        );
        notification.set_default_action_and_target_value(
            "app.open-channel",
            Some(&channel_id.to_variant()),
        );
        // Keyed by channel, so a second call in the same DM replaces the first
        // rather than stacking two doorbells.
        app.send_notification(Some(&format!("call-{channel_id}")), &notification);
        self.state.borrow_mut().ringing.push(channel_id.to_string());
    }

    /// Takes every incoming-call notification back down.
    fn stop_ringing(&self) {
        let ringing = std::mem::take(&mut self.state.borrow_mut().ringing);
        let Some(app) = self.window.application() else {
            return;
        };
        for channel_id in ringing {
            app.withdraw_notification(&format!("call-{channel_id}"));
        }
    }

    /// Applies a host control. These are HTTP routes rather than websocket
    /// messages — the one part of the calls protocol that is.
    fn host_control(self: &Rc<Self>, session_id: String, what: call_dock::HostAction) {
        let Some(session) = self.state.borrow().call.as_ref().map(|c| c.session.clone()) else {
            return;
        };
        let ui = self.clone();
        runtime::spawn(
            async move {
                match what {
                    call_dock::HostAction::MuteOthers => session.host_mute_others().await,
                    call_dock::HostAction::EndCall => session.host_end_call().await,
                    call_dock::HostAction::Mute => session.host_mute(&session_id).await,
                    call_dock::HostAction::StopSharing => {
                        session.host_screen_off(&session_id).await
                    }
                    call_dock::HostAction::LowerHand => session.host_lower_hand(&session_id).await,
                    call_dock::HostAction::MakeHost => session.host_make(&session_id).await,
                    call_dock::HostAction::Remove => session.host_remove(&session_id).await,
                }
            },
            move |result| {
                if let Err(e) = result {
                    ui.toast_error("Could not do that", &e);
                }
            },
        );
    }

    /// Raises or lowers your own hand. The SFU echoes it back as
    /// `user_raise_hand`, which is what actually updates the roster.
    fn toggle_hand(self: &Rc<Self>) {
        let (session, raise) = {
            let st = self.state.borrow();
            let Some(call) = st.call.as_ref() else { return };
            (
                call.session.clone(),
                !call.hands.iter().any(|id| id == &st.me.id),
            )
        };
        let ui = self.clone();
        runtime::spawn(
            async move { session.raise_hand(raise).await },
            move |result| {
                if let Err(e) = result {
                    ui.toast_error("Could not do that", &e);
                }
            },
        );
    }

    /// Looks up people named in messages that we do not hold.
    ///
    /// A mention is only highlighted once the name resolves, and a channel you
    /// have just opened is full of names you may never have seen — without
    /// this, every one of them reads as plain text until they happen to post.
    fn resolve_mentions(self: &Rc<Self>) {
        let (client, wanted) = {
            let st = self.state.borrow();
            let known: HashSet<&str> = st.users.values().map(|u| u.username.as_str()).collect();
            let mut wanted: HashSet<String> = HashSet::new();
            let feed = st.current_channel.as_ref().and_then(|id| st.feeds.get(id));
            for post in feed.into_iter().flat_map(|feed| feed.posts.iter()) {
                for name in mentioned_names(&post.message) {
                    if !known.contains(name.as_str()) {
                        wanted.insert(name);
                    }
                }
            }
            (st.client.clone(), wanted)
        };
        if wanted.is_empty() {
            return;
        }

        let names: Vec<String> = wanted.into_iter().take(50).collect();
        let ui = self.clone();
        runtime::spawn(
            async move { client.users_by_usernames(&names).await },
            move |result| {
                let Ok(users) = result else { return };
                if users.is_empty() {
                    return;
                }
                {
                    let mut st = ui.state.borrow_mut();
                    for user in users {
                        st.users.insert(user.id.clone(), user);
                    }
                }
                ui.refresh_messages();
            },
        );
    }

    /// Quietly fetches the channels most likely to be opened next.
    ///
    /// Switching to a channel that has never been read waits on the network;
    /// the ones with something unread are exactly the ones about to be
    /// clicked, so their first page is fetched before it is asked for. Three
    /// of them, because this is a guess and a wrong guess should be cheap.
    fn preload_unread(self: &Rc<Self>) {
        const PRELOAD: usize = 3;

        let (client, crt, wanted) = {
            let st = self.state.borrow();
            let wanted: Vec<String> = st
                .sidebar_groups()
                .into_iter()
                .flat_map(|(_, channels)| channels)
                .map(|c| c.id)
                .filter(|id| st.unread(id).is_unread() && !st.feeds.contains_key(id))
                .take(PRELOAD)
                .collect();
            (st.client.clone(), st.crt_enabled, wanted)
        };

        for channel_id in wanted {
            let client = client.clone();
            let ui = self.clone();
            runtime::spawn(
                async move {
                    let posts = client
                        .posts_for_channel(&channel_id, 0, INITIAL_POSTS, crt)
                        .await?;
                    let (authors, statuses) = hydrate_authors(&client, &posts).await;
                    Ok::<_, mattermost_api::Error>((channel_id, posts, authors, statuses))
                },
                move |result| {
                    let Ok((channel_id, posts, authors, statuses)) = result else {
                        return;
                    };
                    {
                        let mut st = ui.state.borrow_mut();
                        for user in authors {
                            st.users.insert(user.id.clone(), user);
                        }
                        st.apply_statuses(statuses);
                        // Only if it is still absent: the person may have
                        // opened it while this was in flight, and that copy is
                        // the one being read.
                        st.feeds
                            .entry(channel_id)
                            .or_insert_with(|| ChannelFeed::from_list(&posts));
                    }
                    ui.store_posts(posts.chronological().into_iter().cloned().collect());
                },
            );
        }
    }

    /// Files posts in the local store. Fire and forget: a failure costs a
    /// slower next launch and nothing on this one.
    fn store_posts(&self, posts: Vec<Post>) {
        if posts.is_empty() {
            return;
        }
        let Some(store) = self.store.borrow().clone() else {
            return;
        };
        // Once every ten minutes of running is plenty for a cache that grows a
        // screenful at a time.
        const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(600);
        let prune = self
            .pruned_at
            .get()
            .is_none_or(|at| at.elapsed() >= PRUNE_EVERY);
        if prune {
            self.pruned_at.set(Some(std::time::Instant::now()));
        }
        runtime::spawn(
            async move {
                if let Err(e) = store.save_posts(posts).await {
                    tracing::warn!(error = %e, "could not store those messages");
                }
            },
            |_| {},
        );
    }

    /// Writes the snapshot the next launch will open with.
    ///
    /// Debounced hard: this serialises a chunk of state, and the only thing
    /// that matters is that it ran reasonably recently before the app closed.
    fn schedule_snapshot(self: &Rc<Self>) {
        if self.snapshot_pending.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(5), move || {
            ui.snapshot_pending.set(false);
            ui.save_snapshot();
        });
    }

    fn capture_scroll_anchor(&self) {
        let Some((channel_id, post_id)) = self.chat.current_anchor() else {
            return;
        };
        let mut st = self.state.borrow_mut();
        match post_id {
            Some(post_id) => {
                st.scroll_anchors.insert(channel_id, post_id);
            }
            None => {
                st.scroll_anchors.remove(&channel_id);
            }
        }
    }

    fn save_snapshot(&self) {
        self.capture_scroll_anchor();
        let st = self.state.borrow();
        crate::cache::save(&crate::cache::Snapshot {
            server: st.client.site_url().to_string(),
            current_team: st.current_team.clone(),
            current_channel: st.current_channel.clone(),
            scroll_anchors: st.scroll_anchors.clone(),
        });
        drop(st);

        // The content goes to the store, which keeps every channel rather than
        // the handful a single file could hold.
        let Some(store) = self.store.borrow().clone() else {
            return;
        };
        // Once every ten minutes of running is plenty for a cache that grows a
        // screenful at a time.
        const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(600);
        let prune = self
            .pruned_at
            .get()
            .is_none_or(|at| at.elapsed() >= PRUNE_EVERY);
        if prune {
            self.pruned_at.set(Some(std::time::Instant::now()));
        }
        let (channels, members, users, posts) = {
            let st = self.state.borrow();
            let posts: Vec<Post> = st
                .feeds
                .values()
                .flat_map(|feed| feed.posts.iter().cloned())
                .collect();
            (
                st.channels.values().cloned().collect::<Vec<_>>(),
                st.memberships.values().cloned().collect::<Vec<_>>(),
                st.users.values().cloned().collect::<Vec<_>>(),
                posts,
            )
        };
        runtime::spawn(
            async move {
                // Failures here cost a slower next launch and nothing else, so
                // they are logged rather than surfaced.
                if let Err(e) = store.save_channels(channels, members).await {
                    tracing::warn!(error = %e, "could not store the channel list");
                }
                if let Err(e) = store.save_users(users).await {
                    tracing::warn!(error = %e, "could not store the users");
                }
                if let Err(e) = store.save_posts(posts).await {
                    tracing::warn!(error = %e, "could not store the messages");
                }
                // Occasionally, not on every write: switching channels
                // writes a snapshot each time, and scanning every post to
                // delete nothing is pure work.
                if prune {
                    if let Err(e) = store.prune(crate::store::DEFAULT_KEEP_PER_CHANNEL).await {
                        tracing::warn!(error = %e, "could not trim the store");
                    }
                }
            },
            |_| {},
        );
    }

    /// Sets your own presence, showing it immediately: the server echoes it
    /// back as a status_change, but the click should not wait for a round trip
    /// to look like it landed.
    fn set_status(self: &Rc<Self>, status: String) {
        let (client, me) = {
            let mut st = self.state.borrow_mut();
            let me = st.me.id.clone();
            let presence = mattermost_api::models::Presence::from(status.as_str());
            st.statuses.insert(me.clone(), presence);
            (st.client.clone(), me)
        };
        self.channels.refresh(&self.state, &self.avatars);
        self.refresh_messages();

        let ui = self.clone();
        runtime::spawn(
            async move { client.set_status(&me, &status).await },
            move |result| {
                if let Err(e) = result {
                    ui.toast_error("Could not change your status", &e);
                }
            },
        );
    }

    /// Asks the Agents plugin what bots exist. A server without the plugin
    /// 404s, which is indistinguishable from having no bots.
    fn load_bots(self: &Rc<Self>) {
        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(
            async move { crate::agents::bots(&client).await },
            move |result| match result {
                Ok(bots) => {
                    ui.state.borrow_mut().bots = bots.bots;
                    ui.refresh_agent_actions();
                }
                Err(e) => tracing::debug!(error = %e, "no agents plugin on this server"),
            },
        );
    }

    /// Shows or hides the agent entries, which only make sense when there is
    /// a bot to answer them.
    fn refresh_agent_actions(self: &Rc<Self>) {
        let bots: Vec<(String, String)> = self
            .state
            .borrow()
            .bots
            .iter()
            .map(|bot| {
                let name = if bot.display_name.is_empty() {
                    format!("@{}", bot.username)
                } else {
                    bot.display_name.clone()
                };
                // The DM channel may not exist yet; the bot's user id is
                // enough to make one.
                let target = if bot.dm_channel_id.is_empty() {
                    format!("user:{}", bot.id)
                } else {
                    format!("channel:{}", bot.dm_channel_id)
                };
                (target, format!("Chat with {name}"))
            })
            .collect();

        let ui = self.clone();
        self.chat
            .set_agents(&bots, move |target| match target.split_once(':') {
                Some(("channel", id)) => ui.dispatch(Action::SelectChannel(id.to_string())),
                Some(("user", id)) => ui.dispatch(Action::OpenDirectMessage(id.to_string())),
                _ => {}
            });
    }

    /// "Catch me up": asks the default bot to summarise what you have not read
    /// in this channel. The answer is written into a DM post, so this only
    /// starts it — the text arrives over the socket.
    fn summarise_unreads(self: &Rc<Self>) {
        let (client, channel) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_channel.clone())
        };
        let Some(channel_id) = channel else { return };

        let ui = self.clone();
        self.toast("Asking the agent…");
        runtime::spawn(
            async move { crate::agents::summarise_unreads(&client, &channel_id).await },
            move |result| match result {
                // The summary is a DM from the bot, so go and read it there.
                Ok(target) => ui.dispatch(Action::OpenPost(target.channel_id, target.post_id)),
                Err(e) => ui.toast_error("The agent could not answer", &e),
            },
        );
    }

    /// Handles the reactions-notify plugin, which tells you when somebody
    /// reacts to something you wrote — something core Mattermost does not.
    ///
    /// The plugin creates no posts: the websocket and its own feed endpoint
    /// are the whole client surface.
    fn apply_reaction_notice(self: &Rc<Self>, kind: &str, data: &mattermost_api::ws::Data) {
        let string = |key: &str| {
            data.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match kind {
            "reaction_item" => {
                // The plugin has already applied the push-content policy and
                // decided whether a toast is appropriate — it knows things we
                // do not, like whether you are active in that channel.
                let suppressed = data
                    .get("suppress_desktop")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                self.state.borrow_mut().reaction_unread += 1;
                self.refresh_messages();
                if suppressed {
                    return;
                }
                let channel_id = string("channel_id");
                let title = match string("channel_name").as_str() {
                    "" => format!(
                        ":{}: from {}",
                        string("emoji_name"),
                        string("reactor_username")
                    ),
                    channel => format!("{channel} — :{}:", string("emoji_name")),
                };
                let body = match string("text").as_str() {
                    "" => string("snippet"),
                    text => text.to_string(),
                };
                if let Some(app) = self.window.application() {
                    notify::show(&app, &channel_id, &title, &body, self.notification_sound());
                }
            }
            // Authoritative count, so it replaces ours rather than adjusting it.
            "unread" => {
                let count = data
                    .get("count")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                self.state.borrow_mut().reaction_unread = count;
                self.refresh_messages();
            }
            "item_removed" => {
                let mut st = self.state.borrow_mut();
                st.reaction_unread = (st.reaction_unread - 1).max(0);
                drop(st);
                self.refresh_messages();
            }
            other => tracing::debug!(event = other, "unhandled reactions-notify event"),
        }
    }

    /// Applies an edit. An emptied message means delete, which is what the
    /// other clients do and what the server expects.
    fn submit_edit(self: &Rc<Self>, post_id: String, text: String) {
        self.chat.end_edit();
        if text.trim().is_empty() {
            self.confirm_delete(post_id);
            return;
        }
        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(
            async move { client.update_post(&post_id, &text).await },
            move |result| {
                // Success arrives as post_edited over the socket, so only the
                // failure needs saying.
                if let Err(e) = result {
                    ui.toast_error("Could not save the edit", &e);
                }
            },
        );
    }

    fn menu_action(self: &Rc<Self>, action: MenuAction) {
        match action {
            MenuAction::NewChannel => self.new_channel(),
            MenuAction::BrowseChannels => self.browse_channels(),
            MenuAction::Preferences => self.preferences(),
            MenuAction::EditProfile => self.edit_profile(),
            MenuAction::QuickSwitch => self.quick_switch(),
            MenuAction::SignOut => self.sign_out(),
            MenuAction::ScheduledPosts => self.scheduled_posts(),
            MenuAction::ChannelMembers => self.channel_members(),
            MenuAction::ChannelBookmarks => self.channel_bookmarks(),
            MenuAction::BrowseTeams => self.browse_teams(),
            MenuAction::NewCategory => self.new_category(),
            MenuAction::FocusSearch => self.channels.focus_search(),
            MenuAction::OpenInbox => self.open_inbox(),
            MenuAction::ClosePanel => self.escape_navigation(),
            MenuAction::NextUnread => self.step_unread(true),
            MenuAction::PreviousUnread => self.step_unread(false),
            MenuAction::LeaveTeam => self.leave_team(),
            MenuAction::EditChannel => self.edit_channel(),
            MenuAction::ArchiveChannel => self.archive_channel(),
            MenuAction::CustomStatus => self.custom_status(),
            MenuAction::ChannelNotifications => self.channel_notifications(),
            MenuAction::LeaveChannel => self.leave_channel(),
            MenuAction::PinnedPosts => self.show_pinned(),
        }
    }

    fn new_channel(self: &Rc<Self>) {
        let ui = self.clone();
        dialogs::create_channel(&self.window, move |display_name, url, purpose, private| {
            let (client, team) = {
                let st = ui.state.borrow();
                (st.client.clone(), st.current_team.clone())
            };
            let Some(team_id) = team else { return };
            let kind = if private {
                ChannelType::Private
            } else {
                ChannelType::Open
            };
            let ui = ui.clone();
            let purpose = purpose.clone();
            runtime::spawn(
                async move {
                    let channel = client
                        .create_channel(&team_id, &url, &display_name, kind)
                        .await?;
                    // Purpose is a separate patch; a create that succeeds and
                    // a purpose that does not is still a usable channel.
                    if !purpose.is_empty() {
                        let _ = client.update_channel_header(&channel.id, &purpose).await;
                    }
                    Ok::<_, mattermost_api::Error>(channel)
                },
                move |result| match result {
                    Ok(channel) => {
                        ui.schedule_sidebar_reload();
                        ui.dispatch(Action::SelectChannel(channel.id));
                    }
                    Err(e) => ui.toast_error("Could not create it", &e),
                },
            );
        });
    }

    fn browse_channels(self: &Rc<Self>) {
        let browser = Rc::new(RefCell::new(None::<Rc<dialogs::ChannelBrowser>>));
        let ui = self.clone();
        let search_ui = self.clone();
        let holder = browser.clone();

        let opened = Rc::new(dialogs::ChannelBrowser::present(
            &self.window,
            move |term| {
                let (client, team) = {
                    let st = search_ui.state.borrow();
                    (st.client.clone(), st.current_team.clone())
                };
                let Some(team_id) = team else { return };
                let holder = holder.clone();
                let state = search_ui.state.clone();
                let asked = term.clone();
                runtime::spawn(
                    async move {
                        // An empty box means "show me what is there", which is
                        // the browse list rather than a search for nothing.
                        if term.trim().is_empty() {
                            client.channels_for_team(&team_id, 0, 100).await
                        } else {
                            client.search_channels(&team_id, &term).await
                        }
                    },
                    move |result| {
                        let Ok(channels) = result else { return };
                        let joined = &state.borrow().channels;
                        let rows = channels
                            .into_iter()
                            .filter(|c| c.delete_at == 0)
                            .map(|c| {
                                let member = joined.contains_key(&c.id);
                                (c.id, c.display_name, c.purpose, member)
                            })
                            .collect();
                        if let Some(browser) = holder.borrow().as_ref() {
                            browser.set_results(&asked, rows);
                        }
                    },
                );
            },
            move |channel_id| {
                let (client, me) = {
                    let st = ui.state.borrow();
                    (st.client.clone(), st.me.id.clone())
                };
                let ui = ui.clone();
                runtime::spawn(
                    async move { client.join_channel(&channel_id, &me).await },
                    move |result| match result {
                        Ok(member) => {
                            ui.schedule_sidebar_reload();
                            ui.dispatch(Action::SelectChannel(member.channel_id));
                        }
                        Err(e) => ui.toast_error("Could not join", &e),
                    },
                );
            },
        ));
        // The browser asks for its first page as it opens, and that request
        // needs the handle to fill — which only exists once present returns.
        // Storing it here is what closes that loop.
        *browser.borrow_mut() = Some(opened);
    }

    /// Shows what a message used to say, newest first, each with the time it
    /// was replaced.
    fn show_history(self: &Rc<Self>, versions: Vec<Post>) {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // The current text is the first entry the server returns, so anything
        // after it is something you could go back to.
        for (index, version) in versions.into_iter().enumerate() {
            let when = gtk::Label::builder()
                .label(format!(
                    "{} {}",
                    message::format_day(version.create_at),
                    message::format_time(version.create_at)
                ))
                .xalign(0.0)
                .build();
            when.add_css_class("message-timestamp");
            when.add_css_class("dim-label");

            let text = gtk::Label::builder()
                .label(version.source_text())
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .can_focus(false)
                .build();

            let entry = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();
            entry.append(&when);
            entry.append(&text);

            if index > 0 {
                let restore = gtk::Button::builder()
                    .label("Restore this version")
                    .halign(gtk::Align::Start)
                    .margin_top(4)
                    .build();
                restore.add_css_class("pill");
                restore.connect_clicked({
                    let ui = self.clone();
                    let post_id = version.original_id.clone();
                    let version_id = version.id.clone();
                    move |button| {
                        button.set_sensitive(false);
                        let client = ui.state.borrow().client.clone();
                        let post_id = post_id.clone();
                        let version_id = version_id.clone();
                        let ui = ui.clone();
                        runtime::spawn(
                            async move { client.restore_post_version(&post_id, &version_id).await },
                            move |result| match result {
                                Ok(_) => ui.toast("Restored"),
                                Err(e) => ui.toast_error("Could not restore it", &e),
                            },
                        );
                    }
                });
                entry.append(&restore);
            }

            list.append(&entry);
        }

        let dialog = dialogs::list_dialog("Edit History", None, &list);
        dialog.set_content_width(520);
        dialog.present(Some(&self.window));
    }

    /// Sends what is in the composer at a chosen time.
    ///
    /// The server keeps it and posts it for you, so this works with the app
    /// closed — which is the only reason to use it over waiting.
    fn schedule_message(self: &Rc<Self>) {
        let text = self.chat.composer_text();
        if text.trim().is_empty() {
            self.toast("Write the message first");
            return;
        }
        let (client, channel_id) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_channel.clone())
        };
        let Some(channel_id) = channel_id else { return };

        let ui = self.clone();
        dialogs::schedule_message(&self.window, move |when_ms| {
            let scheduled = mattermost_api::models::ScheduledPost {
                post: Post {
                    channel_id: channel_id.clone(),
                    message: text.clone(),
                    ..Default::default()
                },
                scheduled_at: when_ms,
                error_code: String::new(),
            };
            let client = client.clone();
            let ui = ui.clone();
            runtime::spawn(
                async move { client.create_scheduled_post(&scheduled).await },
                move |result| match result {
                    Ok(_) => {
                        // The composer is empty now, so the draft goes too.
                        ui.chat.set_composer_text("");
                        ui.save_draft();
                        ui.toast("Scheduled");
                    }
                    Err(e) => ui.toast_error("Could not schedule it", &e),
                },
            );
        });
    }

    /// Muting a channel, or filing it under a different category.
    fn row_action(self: &Rc<Self>, channel_id: String, what: RowAction) {
        let (client, me, team_id) = {
            let st = self.state.borrow();
            (
                st.client.clone(),
                st.me.id.clone(),
                st.current_team.clone().unwrap_or_default(),
            )
        };

        match what {
            RowAction::MarkRead => {
                let crt = self.state.borrow().crt_enabled;
                let ui = self.clone();
                runtime::spawn(
                    async move { client.view_channel(&channel_id, "", crt).await },
                    move |result| match result {
                        // The server broadcasts multiple_channels_viewed, which
                        // is what actually clears the badge.
                        Ok(_) => ui.schedule_sidebar_reload(),
                        Err(e) => ui.toast_error("Could not mark it read", &e),
                    },
                );
            }
            RowAction::MarkUnread => {
                let crt = self.state.borrow().crt_enabled;
                let ui = self.clone();
                let target = channel_id.clone();
                runtime::spawn(
                    async move { client.mark_channel_unread(&me, &channel_id, crt).await },
                    move |result| match result {
                        Ok(()) => {
                            // Reading it again on the way out would undo this.
                            if ui.state.borrow().current_channel.as_deref() == Some(target.as_str())
                            {
                                ui.state.borrow_mut().current_channel = None;
                                ui.refresh_messages();
                            }
                            ui.schedule_sidebar_reload();
                        }
                        Err(e) => ui.toast_error("Could not mark it unread", &e),
                    },
                );
            }
            RowAction::SetMuted(muted) => {
                // Muted is stored as mark_unread: "mention" — the same field
                // the notification dialog writes, so it goes the same way.
                let mut props = mattermost_api::models::StringMap::new();
                props.insert(
                    "mark_unread".into(),
                    if muted { "mention" } else { "all" }.into(),
                );
                let ui = self.clone();
                runtime::spawn(
                    async move {
                        client
                            .set_channel_notify_props(&channel_id, &me, &props)
                            .await
                    },
                    move |result| match result {
                        Ok(()) => ui.schedule_sidebar_reload(),
                        Err(e) => ui.toast_error("Could not change that", &e),
                    },
                );
            }
            RowAction::RenameCategory => {
                let current = self
                    .state
                    .borrow()
                    .categories
                    .categories
                    .iter()
                    .find(|c| c.id == channel_id)
                    .map(|c| c.display_name.clone())
                    .unwrap_or_default();
                let ui = self.clone();
                dialogs::name_category(&self.window, "Rename Category", &current, move |name| {
                    let Some(mut category) = ui
                        .state
                        .borrow()
                        .categories
                        .categories
                        .iter()
                        .find(|c| c.id == channel_id)
                        .cloned()
                    else {
                        return;
                    };
                    category.display_name = name;
                    let client = ui.state.borrow().client.clone();
                    let me = ui.state.borrow().me.id.clone();
                    let team_id = ui.state.borrow().current_team.clone().unwrap_or_default();
                    let ui = ui.clone();
                    runtime::spawn(
                        async move {
                            client
                                .update_categories(&me, &team_id, std::slice::from_ref(&category))
                                .await
                        },
                        move |result| match result {
                            Ok(_) => ui.schedule_sidebar_reload(),
                            Err(e) => ui.toast_error("Could not rename it", &e),
                        },
                    );
                });
            }
            RowAction::DeleteCategory => {
                // The channels in it are not deleted — they fall back to the
                // default category — so this needs no confirmation.
                let ui = self.clone();
                runtime::spawn(
                    async move { client.delete_category(&me, &team_id, &channel_id).await },
                    move |result| match result {
                        Ok(()) => ui.schedule_sidebar_reload(),
                        Err(e) => ui.toast_error("Could not delete it", &e),
                    },
                );
            }
            RowAction::MoveTo(category_id) => {
                // The categories route replaces membership wholesale, so both
                // the old and the new category have to be sent together.
                let mut categories = self.state.borrow().categories.categories.clone();
                for category in categories.iter_mut() {
                    category.channel_ids.retain(|id| id != &channel_id);
                    if category.id == category_id {
                        category.channel_ids.insert(0, channel_id.clone());
                    }
                }
                let ui = self.clone();
                runtime::spawn(
                    async move { client.update_categories(&me, &team_id, &categories).await },
                    move |result| match result {
                        Ok(_) => ui.schedule_sidebar_reload(),
                        Err(e) => ui.toast_error("Could not move it", &e),
                    },
                );
            }
        }
    }

    fn new_category(self: &Rc<Self>) {
        let ui = self.clone();
        dialogs::name_category(&self.window, "New Category", "", move |name| {
            let (client, me, team_id) = {
                let st = ui.state.borrow();
                (
                    st.client.clone(),
                    st.me.id.clone(),
                    st.current_team.clone().unwrap_or_default(),
                )
            };
            let category = mattermost_api::models::SidebarCategory {
                display_name: name,
                team_id: team_id.clone(),
                user_id: me.clone(),
                r#type: mattermost_api::models::CategoryType::Custom,
                ..Default::default()
            };
            let ui = ui.clone();
            runtime::spawn(
                async move { client.create_category(&me, &team_id, &category).await },
                move |result| match result {
                    Ok(_) => ui.schedule_sidebar_reload(),
                    Err(e) => ui.toast_error("Could not create it", &e),
                },
            );
        });
    }

    fn leave_team(self: &Rc<Self>) {
        let (client, me, team_id, name) = {
            let st = self.state.borrow();
            let Some(team_id) = st.current_team.clone() else {
                return;
            };
            let name = st
                .teams
                .iter()
                .find(|t| t.id == team_id)
                .map(|t| t.display_name.clone())
                .unwrap_or_default();
            (st.client.clone(), st.me.id.clone(), team_id, name)
        };

        let ui = self.clone();
        dialogs::confirm_leave_team(&self.window, &name, move || {
            let client = client.clone();
            let me = me.clone();
            let team_id = team_id.clone();
            let ui = ui.clone();
            runtime::spawn(
                async move { client.leave_team(&team_id, &me).await },
                move |result| match result {
                    Ok(()) => {
                        // Land somewhere real rather than on a team we just
                        // left.
                        ui.reload_teams();
                        let next = ui.state.borrow().teams.first().map(|t| t.id.clone());
                        if let Some(team_id) = next {
                            ui.dispatch(Action::SelectTeam(team_id));
                        }
                    }
                    Err(e) => ui.toast_error("Could not leave", &e),
                },
            );
        });
    }

    /// The channel's name and topic.
    fn edit_channel(self: &Rc<Self>) {
        let (client, channel_id, current) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let Some(channel) = st.channel(&channel_id) else {
                return;
            };
            (
                st.client.clone(),
                channel_id,
                (channel.display_name.clone(), channel.header.clone()),
            )
        };

        let ui = self.clone();
        dialogs::edit_channel(&self.window, current, move |name, header| {
            let client = client.clone();
            let channel_id = channel_id.clone();
            let ui = ui.clone();
            runtime::spawn(
                async move {
                    // Two routes, because the server patches these separately.
                    if !name.is_empty() {
                        client.rename_channel(&channel_id, &name).await?;
                    }
                    client.update_channel_header(&channel_id, &header).await
                },
                move |result| match result {
                    // channel_updated comes back over the socket and repaints.
                    Ok(_) => {}
                    Err(e) => ui.toast_error("Could not save that", &e),
                },
            );
        });
    }

    fn archive_channel(self: &Rc<Self>) {
        let (client, channel_id, name) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let name = st
                .channel(&channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            (st.client.clone(), channel_id, name)
        };

        let ui = self.clone();
        dialogs::confirm_archive(&self.window, &name, move || {
            let client = client.clone();
            let channel_id = channel_id.clone();
            let ui = ui.clone();
            runtime::spawn(
                async move { client.archive_channel(&channel_id).await },
                move |result| match result {
                    Ok(()) => {
                        ui.state.borrow_mut().current_channel = None;
                        ui.schedule_sidebar_reload();
                        ui.refresh_messages();
                    }
                    Err(e) => ui.toast_error("Could not archive it", &e),
                },
            );
        });
    }

    /// Who is in this channel, and a way to add or remove people.
    fn channel_members(self: &Rc<Self>) {
        let (client, channel_id, name, team_id) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let name = st
                .channel(&channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            (
                st.client.clone(),
                channel_id,
                name,
                st.current_team.clone().unwrap_or_default(),
            )
        };

        let holder: Rc<RefCell<Option<Rc<dialogs::MemberList>>>> = Rc::new(RefCell::new(None));
        let search_holder = holder.clone();
        let search_client = client.clone();
        let search_channel = channel_id.clone();
        let search_ui = self.clone();
        let add_ui = self.clone();
        let add_client = client.clone();
        let add_channel = channel_id.clone();
        let remove_ui = self.clone();
        let remove_channel = channel_id.clone();

        let opened = Rc::new(dialogs::MemberList::present(
            &self.window,
            &name,
            move |term| {
                if term.trim().is_empty() {
                    return;
                }
                let client = search_client.clone();
                let team_id = team_id.clone();
                let channel_id = search_channel.clone();
                let holder = search_holder.clone();
                let state = search_ui.state.clone();
                let asked = term.clone();
                runtime::spawn(
                    // Not-in-channel only: offering someone already here is an
                    // add that does nothing.
                    async move { client.search_users(&term, &team_id, "", &channel_id).await },
                    move |result| {
                        let Ok(users) = result else { return };
                        let display = state.borrow().teammate_name_display().to_string();
                        let rows = users
                            .into_iter()
                            .map(|u| {
                                (
                                    u.id.clone(),
                                    u.display_name(&display),
                                    format!("@{}", u.username),
                                )
                            })
                            .collect();
                        if let Some(list) = holder.borrow().as_ref() {
                            list.set_candidates(&asked, rows);
                        }
                    },
                );
            },
            move |user_id| {
                let client = add_client.clone();
                let channel_id = add_channel.clone();
                let ui = add_ui.clone();
                runtime::spawn(
                    async move { client.join_channel(&channel_id, &user_id).await },
                    move |result| match result {
                        Ok(_) => ui.toast("Added"),
                        Err(e) => ui.toast_error("Could not add them", &e),
                    },
                );
            },
            move |user_id| {
                // Removing someone is not undoable and is visible to them, so
                // it asks first.
                let ui = remove_ui.clone();
                let name = ui.user_name(&user_id);
                let client = client.clone();
                let channel_id = remove_channel.clone();
                dialogs::confirm_remove_member(&ui.window.clone(), &name, move || {
                    let client = client.clone();
                    let channel_id = channel_id.clone();
                    let user_id = user_id.clone();
                    let ui = ui.clone();
                    runtime::spawn(
                        async move { client.leave_channel(&channel_id, &user_id).await },
                        move |result| match result {
                            Ok(()) => ui.toast("Removed"),
                            Err(e) => ui.toast_error("Could not remove them", &e),
                        },
                    );
                });
            },
        ));
        *holder.borrow_mut() = Some(opened.clone());

        // Fill the current members in.
        let ui = self.clone();
        let fill_client = self.state.borrow().client.clone();
        runtime::spawn(
            async move {
                // Two requests rather than three: the users route can filter
                // by channel directly, and the memberships are only needed to
                // find out who the channel admins are.
                let (members, users) = tokio::try_join!(
                    fill_client.channel_members(&channel_id, 0, 200),
                    fill_client.users_in_channel(&channel_id, 0, 200),
                )?;
                Ok::<_, mattermost_api::Error>((members, users))
            },
            move |result| {
                let Ok((members, users)) = result else { return };
                let display = ui.state.borrow().teammate_name_display().to_string();
                let rows = users
                    .into_iter()
                    .map(|user| {
                        let admin = members
                            .iter()
                            .find(|m| m.user_id == user.id)
                            .is_some_and(|m| m.roles.contains("channel_admin"));
                        let name = user.display_name(&display);
                        (user.id, name, format!("@{}", user.username), admin)
                    })
                    .collect();
                opened.set_members(rows);
            },
        );
    }

    /// A channel's bookmarks. Servers older than 9.4 have no such route, so a
    /// failure here says the feature is missing rather than that it broke.
    fn channel_bookmarks(self: &Rc<Self>) {
        let (client, channel_id) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_channel.clone())
        };
        let Some(channel_id) = channel_id else { return };

        let holder: Rc<RefCell<Option<Rc<dialogs::BookmarkList>>>> = Rc::new(RefCell::new(None));
        let refill = {
            let holder = holder.clone();
            let client = client.clone();
            let channel_id = channel_id.clone();
            let ui = self.clone();
            move || {
                let holder = holder.clone();
                let client = client.clone();
                let channel_id = channel_id.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move { client.list_bookmarks(&channel_id).await },
                    move |result| match result {
                        Ok(bookmarks) => {
                            let rows = bookmarks
                                .into_iter()
                                .map(|b| (b.id, b.display_name, b.link_url))
                                .collect();
                            if let Some(list) = holder.borrow().as_ref() {
                                list.set_bookmarks(rows);
                            }
                        }
                        Err(e) => ui.toast_error("Bookmarks are not available here", &e),
                    },
                );
            }
        };

        let add_client = client.clone();
        let add_channel = channel_id.clone();
        let add_refill = refill.clone();
        let delete_refill = refill.clone();
        let ui = self.clone();
        let opened = Rc::new(dialogs::BookmarkList::present(
            &self.window,
            move |display_name, link_url| {
                let bookmark = mattermost_api::models::ChannelBookmark {
                    channel_id: add_channel.clone(),
                    display_name: if display_name.is_empty() {
                        link_url.clone()
                    } else {
                        display_name
                    },
                    link_url,
                    r#type: "link".into(),
                    ..Default::default()
                };
                let client = add_client.clone();
                let channel_id = add_channel.clone();
                let refill = add_refill.clone();
                runtime::spawn(
                    async move { client.create_bookmark(&channel_id, &bookmark).await },
                    move |_| refill(),
                );
            },
            move |link_url| {
                let _ = gtk::gio::AppInfo::launch_default_for_uri(
                    &link_url,
                    None::<&gtk::gio::AppLaunchContext>,
                );
            },
            move |bookmark_id| {
                let client = client.clone();
                let channel_id = channel_id.clone();
                let refill = delete_refill.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move { client.delete_bookmark(&channel_id, &bookmark_id).await },
                    move |result| {
                        if let Err(e) = result {
                            ui.toast_error("Could not remove it", &e);
                        }
                        refill();
                    },
                );
            },
        ));
        *holder.borrow_mut() = Some(opened);
        refill();
    }

    /// Teams on this server you are not in yet.
    fn browse_teams(self: &Rc<Self>) {
        let (client, me) = {
            let st = self.state.borrow();
            (st.client.clone(), st.me.id.clone())
        };

        let join_client = client.clone();
        let ui = self.clone();
        let browser = Rc::new(dialogs::TeamBrowser::present(
            &self.window,
            move |team_id| {
                let client = join_client.clone();
                let me = me.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move { client.join_team(&team_id, &me).await },
                    move |result| match result {
                        Ok(member) => {
                            ui.reload_teams();
                            ui.dispatch(Action::SelectTeam(member.team_id));
                        }
                        Err(e) => ui.toast_error("Could not join", &e),
                    },
                );
            },
        ));

        let mine = self.state.borrow().teams.clone();
        runtime::spawn(
            async move { client.all_teams(0, 100).await },
            move |result| {
                let Ok(teams) = result else { return };
                let rows = teams
                    .into_iter()
                    .filter(|t| t.delete_at == 0)
                    .map(|team| {
                        let member = mine.iter().any(|m| m.id == team.id);
                        (team.id, team.display_name, team.description, member)
                    })
                    .collect();
                browser.set_teams(rows);
            },
        );
    }

    /// Messages waiting to be sent later, with a way to call them off.
    ///
    /// The response is a bucket map keyed by team, plus a separate one for
    /// direct messages, so this walks the values rather than assuming a shape.
    fn scheduled_posts(self: &Rc<Self>) {
        let (client, team) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_team.clone())
        };
        let Some(team_id) = team else { return };

        let ui = self.clone();
        runtime::spawn(
            async move { client.scheduled_posts_for_team(&team_id).await },
            move |result| match result {
                Ok(value) => ui.show_scheduled(value),
                Err(e) => ui.toast_error("Could not load scheduled messages", &e),
            },
        );
    }

    fn show_scheduled(self: &Rc<Self>, scheduled: mattermost_api::models::TeamScheduledPosts) {
        // Every bucket, team and direct alike: they are all messages this
        // person has waiting, and separating them here would be a
        // distinction without a difference.
        let mut posts: Vec<mattermost_api::models::ScheduledPost> =
            scheduled.0.into_values().flatten().collect();
        posts.sort_by_key(|p| p.scheduled_at);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        if posts.is_empty() {
            list.append(
                &adw::StatusPage::builder()
                    .icon_name("alarm-symbolic")
                    .title("No Scheduled Messages")
                    .description("Messages you send later wait here")
                    .css_classes(["compact"])
                    .build(),
            );
        }

        // Deleting is undone from a toast, so the toasts live in the dialog
        // where they can be seen, and the delete itself waits for the toast
        // to go away.
        let toasts = adw::ToastOverlay::new();
        let pending: Rc<RefCell<Vec<adw::Toast>>> = Rc::default();

        for scheduled in posts {
            let when = gtk::Label::builder()
                .label(format!(
                    "{} {}",
                    message::format_day(scheduled.scheduled_at),
                    message::format_time(scheduled.scheduled_at)
                ))
                .xalign(0.0)
                .build();
            when.add_css_class("message-timestamp");
            when.add_css_class("dim-label");

            let text = gtk::Label::builder()
                .label(&scheduled.post.message)
                .xalign(0.0)
                .wrap(true)
                .hexpand(true)
                .build();

            // Rescheduling rather than cancel-and-retype: the message is
            // already written, and the usual reason to touch one of these is
            // that the time was wrong.
            let reschedule = gtk::Button::builder()
                .icon_name("alarm-symbolic")
                .tooltip_text("Reschedule")
                .valign(gtk::Align::Center)
                .build();
            reschedule.add_css_class("flat");
            reschedule.add_css_class("circular");
            reschedule.connect_clicked({
                let ui = self.clone();
                let scheduled = scheduled.clone();
                move |_| {
                    let ui = ui.clone();
                    let scheduled = scheduled.clone();
                    dialogs::schedule_message(&ui.window.clone(), move |when_ms| {
                        let client = ui.state.borrow().client.clone();
                        let mut updated = scheduled.clone();
                        updated.scheduled_at = when_ms;
                        let id = updated.post.id.clone();
                        let ui = ui.clone();
                        runtime::spawn(
                            async move { client.update_scheduled_post(&id, &updated).await },
                            move |result| match result {
                                Ok(_) => ui.toast("Rescheduled"),
                                Err(e) => ui.toast_error("Could not reschedule it", &e),
                            },
                        );
                    });
                }
            });

            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Delete Scheduled Message")
                .valign(gtk::Align::Center)
                .build();
            delete.add_css_class("flat");
            delete.add_css_class("circular");

            let body = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .build();
            body.append(&when);
            body.append(&text);

            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(&body);
            row.append(&reschedule);
            row.append(&delete);
            list.append(&row);

            delete.connect_clicked({
                let ui = self.clone();
                let id = scheduled.post.id.clone();
                let toasts = toasts.clone();
                let pending = pending.clone();
                let row = row.clone();
                move |_| {
                    row.set_visible(false);
                    let toast = adw::Toast::builder()
                        .title("Scheduled message deleted")
                        .button_label("Undo")
                        .build();
                    let undone = Rc::new(std::cell::Cell::new(false));
                    toast.connect_button_clicked({
                        let undone = undone.clone();
                        let row = row.clone();
                        move |_| {
                            undone.set(true);
                            row.set_visible(true);
                        }
                    });
                    toast.connect_dismissed({
                        let ui = ui.clone();
                        let id = id.clone();
                        let row = row.clone();
                        let pending = pending.clone();
                        move |toast| {
                            pending.borrow_mut().retain(|t| t != toast);
                            if undone.get() {
                                return;
                            }
                            let client = ui.state.borrow().client.clone();
                            let id = id.clone();
                            let ui = ui.clone();
                            let row = row.clone();
                            runtime::spawn(
                                async move { client.delete_scheduled_post(&id).await },
                                move |result| {
                                    if let Err(e) = result {
                                        row.set_visible(true);
                                        ui.toast_error("Could not delete it", &e);
                                    }
                                },
                            );
                        }
                    });
                    pending.borrow_mut().push(toast.clone());
                    toasts.add_toast(toast);
                }
            });
        }

        toasts.set_child(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&adw::Clamp::builder().maximum_size(560).child(&list).build())
                .build(),
        ));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&toasts));
        let dialog = adw::Dialog::builder()
            .title("Scheduled Messages")
            .content_width(520)
            .content_height(440)
            .child(&view)
            .build();
        // Closing the dialog is not undoing anything: deletes still waiting on
        // their toast go through now rather than being lost with it.
        dialog.connect_closed(move |_| {
            let waiting: Vec<adw::Toast> = pending.borrow().clone();
            for toast in waiting {
                toast.dismiss();
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Signs out of this server, forgetting its token and its cached
    /// messages, and leaves any other server signed in.
    fn sign_out(self: &Rc<Self>) {
        let ui = self.clone();
        dialogs::confirm(
            &self.window,
            "Sign Out?",
            "This device will forget the session, and anything unsent is lost.",
            "Sign Out",
            move || {
                let client = ui.state.borrow().client.clone();
                let server = client.site_url().to_string();
                crate::cache::clear();
                let stored = server.clone();
                runtime::spawn(
                    async move {
                        if let Err(e) = crate::store::Store::clear(&stored).await {
                            tracing::warn!(error = %e, "could not delete the local messages");
                        }
                    },
                    |_| {},
                );
                runtime::spawn(
                    async move {
                        // Revoke it server-side too, so a copy that leaked with
                        // the file is useless rather than merely forgotten.
                        let _ = client.logout().await;
                        crate::session::forget_async(&server).await;
                    },
                    |_| {},
                );
                ui.window.close();
            },
        );
    }

    /// Moves to the next or previous channel with something unread, in
    /// sidebar order. Wraps, because the alternative is a shortcut that
    /// silently stops working at the end of the list.
    fn step_unread(self: &Rc<Self>, forwards: bool) {
        let next = {
            let st = self.state.borrow();
            let ordered: Vec<String> = st
                .sidebar_groups()
                .into_iter()
                .flat_map(|(_, channels)| channels)
                .map(|c| c.id)
                .collect();
            let unread: Vec<String> = ordered
                .iter()
                .filter(|id| st.unread(id).is_unread())
                .cloned()
                .collect();
            if unread.is_empty() {
                None
            } else {
                let here = st
                    .current_channel
                    .as_ref()
                    .and_then(|id| ordered.iter().position(|other| other == id))
                    .unwrap_or(0);
                let mut candidates: Vec<&String> = unread.iter().collect();
                if !forwards {
                    candidates.reverse();
                }
                candidates
                    .iter()
                    .find(|id| {
                        let position = ordered.iter().position(|other| &other == *id).unwrap_or(0);
                        if forwards {
                            position > here
                        } else {
                            position < here
                        }
                    })
                    .or(candidates.first())
                    .map(|id| (*id).clone())
            }
        };
        if let Some(channel_id) = next {
            self.dispatch(Action::SelectChannel(channel_id));
        }
    }

    /// Asks which channel, using the same switcher as Ctrl+K. Picking a
    /// destination is the same act as picking one to read, and a second list
    /// would be a second thing to keep working.
    fn pick_channel(self: &Rc<Self>, on_pick: impl Fn(String) + 'static) {
        let holder: Rc<RefCell<Option<Rc<switcher::Switcher>>>> = Rc::new(RefCell::new(None));
        let search_ui = self.clone();
        let search_holder = holder.clone();
        let opened = Rc::new(switcher::Switcher::present(
            &self.window,
            move |term| {
                let lowered = term.to_lowercase();
                let st = search_ui.state.borrow();
                let mut rows: Vec<(switcher::Target, String, String, String)> = st
                    .channels
                    .values()
                    .filter(|c| c.delete_at == 0)
                    .filter_map(|channel| {
                        let title = st.channel_title(channel);
                        (lowered.is_empty() || title.to_lowercase().contains(&lowered)).then(|| {
                            (
                                switcher::Target::Channel(channel.id.clone()),
                                title,
                                String::new(),
                                channel_icon_name(channel),
                            )
                        })
                    })
                    .collect();
                rows.sort_by_key(|row| row.1.to_lowercase());
                rows.truncate(QUICK_SWITCH_ROWS);
                if let Some(switcher) = search_holder.borrow().as_ref() {
                    switcher.set_results(rows);
                }
            },
            move |target| {
                if let switcher::Target::Channel(channel_id) = target {
                    on_pick(channel_id);
                }
            },
        ));
        *holder.borrow_mut() = Some(opened);
    }

    /// Ctrl+K: jump to a channel or a person by typing a few letters.
    ///
    /// Channels are matched locally against the sidebar — instant, and the
    /// list is small. People have to be searched for, because the client only
    /// knows the ones it has seen.
    fn quick_switch(self: &Rc<Self>) {
        let holder: Rc<RefCell<Option<Rc<switcher::Switcher>>>> = Rc::new(RefCell::new(None));
        let search_ui = self.clone();
        let pick_ui = self.clone();
        let search_holder = holder.clone();

        let opened = Rc::new(switcher::Switcher::present(
            &self.window,
            move |term| {
                let lowered = term.to_lowercase();
                let (client, team_id, mut rows) = {
                    let st = search_ui.state.borrow();
                    let mut rows: Vec<(switcher::Target, String, String, String)> =
                        st.channels
                            .values()
                            .filter(|c| c.delete_at == 0)
                            .filter_map(|channel| {
                                let title = st.channel_title(channel);
                                (lowered.is_empty() || title.to_lowercase().contains(&lowered))
                                    .then(|| {
                                        (
                                            switcher::Target::Channel(channel.id.clone()),
                                            title,
                                            String::new(),
                                            channel_icon_name(channel),
                                        )
                                    })
                            })
                            .collect();
                    rows.sort_by_key(|row| row.1.to_lowercase());
                    rows.truncate(QUICK_SWITCH_ROWS);
                    (
                        st.client.clone(),
                        st.current_team.clone().unwrap_or_default(),
                        rows,
                    )
                };

                if let Some(switcher) = search_holder.borrow().as_ref() {
                    switcher.set_results(rows.clone());
                }
                if lowered.is_empty() {
                    return;
                }

                // People arrive after the channels rather than instead of
                // them: the local answer should never wait on the network.
                let holder = search_holder.clone();
                let state = search_ui.state.clone();
                runtime::spawn(
                    async move { client.search_users(&term, &team_id, "", "").await },
                    move |result| {
                        let Ok(users) = result else { return };
                        let display = state.borrow().teammate_name_display().to_string();
                        rows.extend(users.into_iter().take(QUICK_SWITCH_ROWS).map(|user| {
                            (
                                switcher::Target::User(user.id.clone()),
                                user.display_name(&display),
                                format!("@{}", user.username),
                                "avatar-default-symbolic".to_string(),
                            )
                        }));
                        if let Some(switcher) = holder.borrow().as_ref() {
                            switcher.set_results(rows.clone());
                        }
                    },
                );
            },
            move |target| match target {
                switcher::Target::Channel(id) => pick_ui.dispatch(Action::SelectChannel(id)),
                switcher::Target::User(id) => pick_ui.dispatch(Action::OpenDirectMessage(id)),
            },
        ));
        *holder.borrow_mut() = Some(opened);
    }

    /// Your own name, nickname, position and picture.
    fn edit_profile(self: &Rc<Self>) {
        let (client, me, current) = {
            let st = self.state.borrow();
            let me = &st.me;
            (
                st.client.clone(),
                me.id.clone(),
                (
                    me.first_name.clone(),
                    me.last_name.clone(),
                    me.nickname.clone(),
                    me.position.clone(),
                ),
            )
        };
        let avatar = self.avatars.texture(&me);

        let save_ui = self.clone();
        let avatar_ui = self.clone();
        let save_client = client.clone();
        let save_me = me.clone();
        account::edit_profile(
            &self.window,
            current,
            avatar,
            move |first, last, nickname, position| {
                let patch = serde_json::json!({
                    "first_name": first,
                    "last_name": last,
                    "nickname": nickname,
                    "position": position,
                });
                let client = save_client.clone();
                let me = save_me.clone();
                let ui = save_ui.clone();
                runtime::spawn(
                    async move { client.patch_user(&me, &patch).await },
                    move |result| match result {
                        Ok(user) => {
                            ui.state
                                .borrow_mut()
                                .users
                                .insert(user.id.clone(), user.clone());
                            ui.state.borrow_mut().me = user;
                            ui.refresh_all();
                        }
                        Err(e) => ui.toast_error("Could not save that", &e),
                    },
                );
            },
            move |path| {
                let client = client.clone();
                let me = me.clone();
                let ui = avatar_ui.clone();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "avatar.png".into());
                runtime::spawn(
                    async move {
                        let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
                        client
                            .set_profile_image(&me, &name, bytes)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    move |result| match result {
                        Ok(()) => {
                            // The cached texture is now wrong everywhere it is
                            // drawn, so drop it and let it refetch.
                            let me = ui.state.borrow().me.id.clone();
                            ui.avatars.forget(&me);
                            ui.refresh_all();
                        }
                        Err(e) => ui.toast(&format!("Could not upload that: {e}")),
                    },
                );
            },
        );
    }

    /// The emoji-and-a-line status that shows next to your name.
    fn custom_status(self: &Rc<Self>) {
        let (client, current) = {
            let st = self.state.borrow();
            let status = st.me.custom_status();
            (
                st.client.clone(),
                status
                    .map(|s| (s.emoji.clone(), s.text.clone()))
                    .unwrap_or_default(),
            )
        };

        // The last few, which the server keeps as a preference so every client
        // offers the same list.
        let recents: Vec<(String, String)> = self
            .state
            .borrow()
            .preferences
            .iter()
            .find(|p| p.category == "custom_status" && p.name == "recentCustomStatuses")
            .and_then(|p| serde_json::from_str::<Vec<serde_json::Value>>(&p.value).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| {
                let emoji = entry.get("emoji")?.as_str()?.to_string();
                let text = entry.get("text")?.as_str()?.to_string();
                (!emoji.is_empty() || !text.is_empty()).then_some((emoji, text))
            })
            .take(5)
            .collect();

        let set_client = client.clone();
        let set_ui = self.clone();
        let clear_ui = self.clone();
        account::custom_status(
            &self.window,
            current,
            recents,
            move |emoji, text, expires_at| {
                let status = mattermost_api::models::CustomStatus {
                    emoji,
                    text,
                    duration: if expires_at > 0 {
                        "date_and_time".into()
                    } else {
                        String::new()
                    },
                    // RFC3339, unlike every other time in this API — and it
                    // is decoded into a Go time.Time, which insists on the
                    // colon in the zone offset. `format_iso8601` writes
                    // "+0300", which fails to parse and comes back as
                    // "invalid or missing custom_status", so the format is
                    // spelled out.
                    expires_at: (expires_at > 0)
                        .then(|| {
                            glib::DateTime::from_unix_local(expires_at / 1000)
                                .ok()
                                .and_then(|d| d.format("%Y-%m-%dT%H:%M:%S%:z").ok())
                                .map(|s| s.to_string())
                        })
                        .flatten(),
                };
                let client = set_client.clone();
                let ui = set_ui.clone();
                runtime::spawn(
                    async move { client.set_custom_status(&status).await },
                    move |result| match result {
                        Ok(()) => ui.reload_me(),
                        Err(e) => ui.toast_error("Could not set that", &e),
                    },
                );
            },
            move || {
                let client = client.clone();
                let ui = clear_ui.clone();
                runtime::spawn(
                    async move { client.clear_custom_status().await },
                    move |result| match result {
                        Ok(()) => ui.reload_me(),
                        Err(e) => ui.toast_error("Could not clear that", &e),
                    },
                );
            },
        );
    }

    /// Refetches our own user after changing something the server owns the
    /// canonical version of.
    fn reload_me(self: &Rc<Self>) {
        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(async move { client.me().await }, move |result| {
            if let Ok(user) = result {
                let mut st = ui.state.borrow_mut();
                st.users.insert(user.id.clone(), user.clone());
                st.me = user;
                drop(st);
                ui.refresh_all();
            }
        });
    }

    /// Per-channel notification overrides. The dialog is shown with what the
    /// membership currently says, and only what changed is written.
    fn channel_notifications(self: &Rc<Self>) {
        let (client, me, channel_id, name, current) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let name = st
                .channel(&channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            let props = st
                .memberships
                .get(&channel_id)
                .map(|m| m.notify_props.clone())
                .unwrap_or_default();
            let desktop = props
                .get("desktop")
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            // "mark_unread: mention" is how Mattermost stores a muted channel,
            // so the switch is the inverse of it.
            let all_activity = props.get("mark_unread").map(String::as_str) != Some("mention");
            let ignore_mentions =
                props.get("ignore_channel_mentions").map(String::as_str) == Some("on");
            (
                st.client.clone(),
                st.me.id.clone(),
                channel_id,
                name,
                (desktop, all_activity, ignore_mentions),
            )
        };

        let ui = self.clone();
        dialogs::channel_notifications(
            &self.window,
            &name,
            current,
            move |desktop, all_activity, ignore_mentions| {
                let mut props = mattermost_api::models::StringMap::new();
                props.insert("desktop".into(), desktop.clone());
                props.insert(
                    "mark_unread".into(),
                    if all_activity { "all" } else { "mention" }.into(),
                );
                props.insert(
                    "ignore_channel_mentions".into(),
                    if ignore_mentions { "on" } else { "off" }.into(),
                );

                let client = client.clone();
                let me = me.clone();
                let channel_id = channel_id.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move {
                        client
                            .set_channel_notify_props(&channel_id, &me, &props)
                            .await
                    },
                    move |result| match result {
                        // The server broadcasts channel_member_updated, which
                        // is what refreshes the muted styling in the sidebar.
                        Ok(()) => ui.schedule_sidebar_reload(),
                        Err(e) => ui.toast_error("Could not save that", &e),
                    },
                );
            },
        );
    }

    /// Ctrl+comma: the account's notifications, running in the background,
    /// and the media cache, each applied the moment it changes.
    ///
    /// The notification settings live on the user object's notify_props, not
    /// in preferences — a distinction that trips up most third-party clients.
    fn preferences(self: &Rc<Self>) {
        let current = {
            let st = self.state.borrow();
            let props = &st.me.notify_props;
            let desktop = props
                .get("desktop")
                .cloned()
                .unwrap_or_else(|| "mention".to_string());
            let sound = props.get("desktop_sound").map(String::as_str) != Some("false");
            let keys = props.get("mention_keys").cloned().unwrap_or_default();
            let first_name = props.get("first_name").map(String::as_str) == Some("true");
            (desktop, sound, keys, first_name)
        };

        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        dialog.add(&page);

        let ui = self.clone();
        let toasts = dialog.downgrade();
        page.add(&dialogs::account_notifications(
            current,
            move |key, value, revert| {
                // PATCH replaces notify_props whole, so the change goes out
                // with every other key the account already has.
                let (client, me, mut props) = {
                    let st = ui.state.borrow();
                    (
                        st.client.clone(),
                        st.me.id.clone(),
                        st.me.notify_props.clone(),
                    )
                };
                props.insert(key.to_string(), value);
                let patch = serde_json::json!({ "notify_props": props });
                let ui = ui.clone();
                let toasts = toasts.clone();
                runtime::spawn(
                    async move { client.patch_user(&me, &patch).await },
                    move |result| match result {
                        // These decide every future toast, so the local copy
                        // has to be the server's answer, not ours.
                        Ok(user) => ui.state.borrow_mut().me = user,
                        Err(e) => {
                            revert();
                            let message = format!("Could not save that: {}", human_error(&e));
                            match toasts.upgrade() {
                                Some(dialog) => dialog.add_toast(plain_toast(&message)),
                                None => ui.toast(&message),
                            }
                        }
                    },
                );
            },
        ));

        // Mirrors `app.background`, which owns the setting and its effects.
        let background = adw::SwitchRow::builder()
            .title("Keep running in background")
            .subtitle("Closing the window leaves notifications and calls working")
            .active(crate::background::Background::enabled())
            .build();
        background.connect_active_notify({
            let window = self.window.clone();
            move |row| {
                if crate::background::Background::enabled() != row.is_active() {
                    WidgetExt::activate_action(&window, "app.background", None).ok();
                }
            }
        });
        let app_group = adw::PreferencesGroup::builder().title("Background").build();
        app_group.add(&background);
        page.add(&app_group);

        page.add(&storage::group(self.avatars.resources()));
        dialog.present(Some(&self.window));
    }

    fn leave_channel(self: &Rc<Self>) {
        let (client, me, channel_id, name) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let name = st
                .channel(&channel_id)
                .map(|c| st.channel_title(c))
                .unwrap_or_default();
            (st.client.clone(), st.me.id.clone(), channel_id, name)
        };

        let ui = self.clone();
        dialogs::confirm_leave(&self.window, &name, move || {
            let client = client.clone();
            let me = me.clone();
            let channel_id = channel_id.clone();
            let ui = ui.clone();
            runtime::spawn(
                async move { client.leave_channel(&channel_id, &me).await },
                move |result| match result {
                    Ok(()) => {
                        ui.state.borrow_mut().current_channel = None;
                        ui.schedule_sidebar_reload();
                        ui.refresh_messages();
                    }
                    Err(e) => ui.toast_error("Could not leave", &e),
                },
            );
        });
    }

    /// Pinned messages, in the right panel. They are a property of the channel
    /// rather than of any list we already hold, so they are fetched.
    fn show_pinned(self: &Rc<Self>) {
        let (client, channel_id) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_channel.clone())
        };
        let Some(channel_id) = channel_id else { return };

        let ui = self.clone();
        runtime::spawn(
            async move {
                let posts = client.pinned_posts(&channel_id).await?;
                let (authors, statuses) = hydrate_authors(&client, &posts).await;
                Ok::<_, mattermost_api::Error>((posts, authors, statuses))
            },
            move |result| match result {
                Ok((posts, authors, statuses)) => {
                    {
                        let mut st = ui.state.borrow_mut();
                        for user in authors {
                            st.users.insert(user.id.clone(), user);
                        }
                        st.apply_statuses(statuses);
                        st.search_results = ChannelFeed::from_list(&posts).posts;
                        st.search_results.reverse();
                        st.searching = false;
                    }
                    ui.right
                        .set_mode(rhs::PanelMode::Search("Pinned messages".into()));
                    ui.refresh_panel_mode();
                    ui.overlay.set_show_sidebar(true);
                    ui.refresh_messages();
                }
                Err(e) => ui.toast_error("Could not load the pinned messages", &e),
            },
        );
    }

    /// Answers the composer's completion query.
    ///
    /// Emoji come from the built-in table, which is local and therefore
    /// instant. Mentions have to be asked for, because who is in a channel is
    /// not something the client holds in full.
    fn complete(self: &Rc<Self>, query: Option<autocomplete::Query>) {
        use autocomplete::Query;
        let Some(query) = query else {
            // Closing the list has to cancel what is in flight as well, or a
            // late answer reopens it over a composer nobody is completing in.
            self.completion_generation
                .set(self.completion_generation.get() + 1);
            self.mention_query.borrow_mut().take();
            self.set_completions(Vec::new());
            return;
        };

        match query {
            Query::Emoji(term) => {
                let term = term.to_lowercase();
                let mut items: Vec<autocomplete::Candidate> = emojis::iter()
                    .filter_map(|e| {
                        let name = e.shortcode()?;
                        name.contains(&term).then(|| autocomplete::Candidate {
                            insert: format!(":{name}:"),
                            primary: format!("{}  :{name}:", e.as_str()),
                            secondary: String::new(),
                            image: None,
                            user_id: None,
                        })
                    })
                    .take(COMPLETIONS)
                    .collect();
                // Exact prefixes first: typing ":sm" wants "smile", not
                // "cosmic".
                items.sort_by_key(|candidate| {
                    !candidate.insert.trim_start_matches(':').starts_with(&term)
                });
                self.set_completions(items.clone());

                // The server's own emoji are in no local table, so they have
                // to be asked for — after the instant ones are already up, and
                // only once there is something to search with.
                if term.is_empty() {
                    return;
                }
                let client = self.state.borrow().client.clone();
                let ui = self.clone();
                runtime::spawn(
                    async move { client.search_emoji(&term).await },
                    move |result| {
                        let Ok(custom) = result else { return };
                        if custom.is_empty() {
                            return;
                        }
                        let mut items = items;
                        items.extend(custom.into_iter().take(COMPLETIONS).map(|emoji| {
                            autocomplete::Candidate {
                                insert: format!(":{}:", emoji.name),
                                primary: format!(":{}:", emoji.name),
                                secondary: "custom".to_string(),
                                image: None,
                                user_id: None,
                            }
                        }));
                        ui.set_completions(items);
                    },
                );
            }
            Query::Mention(term) => {
                let lowered = term.to_lowercase();
                let (client, team_id, channel_id, local) = {
                    let st = self.state.borrow();
                    let display = st.teammate_name_display().to_string();

                    // Answered from memory first, before anything touches the
                    // network. This is what keeps the list under a frame: a
                    // round trip is tens of milliseconds at best, and the
                    // names most likely to be wanted are already here.
                    let local = local_mentions(st.users.values(), &lowered, &display, &|id| {
                        self.avatars.texture(id)
                    });

                    (
                        st.client.clone(),
                        st.current_team.clone().unwrap_or_default(),
                        st.current_channel.clone().unwrap_or_default(),
                        local,
                    )
                };
                self.set_completions(local.clone());

                if channel_id.is_empty() {
                    return;
                }

                // Every keystroke invalidates whatever is already in flight.
                self.completion_generation
                    .set(self.completion_generation.get() + 1);

                // Only the last keystroke of a burst is asked about. Typing a
                // name is half a dozen letters, and the server is answering
                // the word, not each letter of it — without this, six round
                // trips race each other and five of them are thrown away.
                // The local list is already on screen, so the wait costs
                // nothing anyone can see.
                *self.mention_query.borrow_mut() = Some((term, team_id, channel_id, client));
                if self.mention_query_pending.get() {
                    return;
                }
                self.mention_query_pending.set(true);
                let ui = self.clone();
                glib::timeout_add_local_once(MENTION_DEBOUNCE, move || {
                    ui.mention_query_pending.set(false);
                    // Whatever the latest keystroke left behind, not the one
                    // that started the timer.
                    let pending = ui.mention_query.borrow_mut().take();
                    if let Some((term, team_id, channel_id, client)) = pending {
                        ui.fetch_mentions(term, team_id, channel_id, client);
                    }
                });
            }
        }
    }

    /// The server's half of `@mention` completion: everyone in the channel
    /// this client has never heard of, plus the groups. Debounced by its
    /// caller, so this runs once per typed word rather than once per letter.
    fn fetch_mentions(
        self: &Rc<Self>,
        term: String,
        team_id: String,
        channel_id: String,
        client: mattermost_api::Client,
    ) {
        let generation = self.completion_generation.get();
        let ui = self.clone();
        let groups_client = client.clone();
        let group_term = term.clone();
        let group_ui = self.clone();
        runtime::spawn(
            async move {
                client
                    .autocomplete_users(&term, &team_id, &channel_id)
                    .await
            },
            move |result| {
                if ui.completion_generation.get() != generation {
                    return;
                }
                let Ok(found) = result else { return };
                let display = ui.state.borrow().teammate_name_display().to_string();
                // People in the channel first; the server already
                // separates them, and suggesting someone who is not
                // here would post a mention that notifies nobody.
                let items: Vec<autocomplete::Candidate> = found
                    .users
                    .iter()
                    .chain(found.out_of_channel.iter())
                    .take(COMPLETIONS)
                    .map(|user| autocomplete::Candidate {
                        insert: format!("@{}", user.username),
                        // The name first: it is what somebody is
                        // looking for, and the handle is how the
                        // account is spelled.
                        primary: user.display_name(&display),
                        secondary: format!("@{}", user.username),
                        image: ui.avatars.texture(&user.id),
                        user_id: Some(user.id.clone()),
                    })
                    .collect();
                if items.is_empty() {
                    return;
                }
                ui.set_completions(items.clone());

                runtime::spawn(
                    async move { groups_client.mentionable_groups(&group_term).await },
                    move |result| {
                        if group_ui.completion_generation.get() != generation {
                            return;
                        }
                        let Ok(groups) = result else { return };
                        if groups.is_empty() {
                            return;
                        }
                        let mut items = items;
                        items.extend(groups.into_iter().map(|group| autocomplete::Candidate {
                            insert: format!("@{}", group.name),
                            primary: format!("@{}", group.name),
                            secondary: match group.member_count {
                                Some(n) => {
                                    format!("{} · {n} people", group.display_name)
                                }
                                None => group.display_name,
                            },
                            image: None,
                            user_id: None,
                        }));
                        group_ui.set_completions(items);
                    },
                );
            },
        );
    }

    /// Asks for files and uploads them straight away.
    ///
    /// Uploading on pick rather than on send is what the other clients do, and
    /// it is the reason sending feels instant: by the time a message goes out
    /// its attachments are already on the server.
    fn pick_attachment(self: &Rc<Self>) {
        let Some(channel_id) = self.state.borrow().current_channel.clone() else {
            return;
        };
        let dialog = gtk::FileDialog::builder().title("Attach files").build();
        let ui = self.clone();
        dialog.open_multiple(
            Some(&self.window),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                let Ok(files) = result else { return };
                let paths: Vec<std::path::PathBuf> = files
                    .iter::<gtk::gio::File>()
                    .flatten()
                    .filter_map(|f| f.path())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                ui.chat.set_uploading(paths.len());
                for path in paths {
                    ui.upload(&channel_id, path);
                }
            },
        );
    }

    /// Above this a file goes up in chunks through an upload session, so a
    /// dropped connection resumes instead of starting the whole thing again.
    /// Below it, one multipart request is fewer round trips.
    const CHUNKED_ABOVE: u64 = 8 * 1024 * 1024;
    const CHUNK: usize = 4 * 1024 * 1024;

    fn upload(self: &Rc<Self>, channel_id: &str, path: std::path::PathBuf) {
        let client = self.state.borrow().client.clone();
        let channel_id = channel_id.to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());

        let ui = self.clone();
        let label = name.clone();
        runtime::spawn(
            async move {
                // Reading in the worker: a large file would otherwise block
                // the frame this was started from.
                let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
                if bytes.len() as u64 <= Self::CHUNKED_ABOVE {
                    return client
                        .upload_file(&channel_id, &name, bytes, None)
                        .await
                        .map_err(|e| e.to_string());
                }

                // Chunked: create a session, then send from wherever the
                // server says it got to. There is no Content-Range here — the
                // body simply starts at file_offset.
                let session = client
                    .create_upload_session(&channel_id, &name, bytes.len() as i64)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut offset = session.file_offset as usize;
                loop {
                    let end = (offset + Self::CHUNK).min(bytes.len());
                    let info = client
                        .upload_data(&session.id, bytes[offset..end].to_vec())
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Some(info) = info {
                        return Ok(mattermost_api::models::FileUploadResponse {
                            file_infos: vec![info],
                            ..Default::default()
                        });
                    }
                    if end >= bytes.len() {
                        return Err("the server never finished the upload".to_string());
                    }
                    // Trust the server's idea of where it got to rather than
                    // our own arithmetic: a partial write is its to report.
                    offset = client
                        .upload_session(&session.id)
                        .await
                        .map_err(|e| e.to_string())?
                        .file_offset as usize;
                }
            },
            move |result| {
                match result {
                    Ok(response) => {
                        let mut st = ui.state.borrow_mut();
                        for info in response.file_infos {
                            st.pending_files.push((info.id, info.name.clone()));
                        }
                    }
                    Err(e) => ui.toast(&format!("Could not attach {label}: {e}")),
                }
                ui.refresh_attachments();
            },
        );
    }

    fn refresh_attachments(self: &Rc<Self>) {
        let files = self.state.borrow().pending_files.clone();
        let ui = self.clone();
        self.chat.set_attachments(&files, move |file_id| {
            ui.dispatch(Action::DropAttachment(file_id))
        });
    }

    /// Runs a search and shows the hits in the right panel.
    ///
    /// Search is one of the routes the server refuses while it is busy, so a
    /// failure here is worth saying out loud rather than showing as "no
    /// results" — those mean very different things to whoever is looking.
    fn search(self: &Rc<Self>, terms: String) {
        // "file:" scopes the same box to attachments. A second search field
        // would be a second thing to find; Mattermost's own syntax already
        // works this way for `in:` and `from:`.
        if let Some(rest) = terms.strip_prefix("file:") {
            self.search_files(rest.trim().to_string());
            return;
        }
        let (client, team) = {
            let mut st = self.state.borrow_mut();
            st.searching = true;
            st.search_results.clear();
            (st.client.clone(), st.current_team.clone())
        };
        let Some(team_id) = team else { return };

        let shown = rhs::PanelMode::Search(terms.clone());
        self.right.set_mode(shown.clone());
        self.refresh_panel_mode();
        self.overlay.set_show_sidebar(true);
        self.refresh_messages();

        let ui = self.clone();
        runtime::spawn(
            async move {
                let hits = client.search_posts(&team_id, &terms, false).await?;
                let (authors, statuses) = hydrate_authors(&client, &hits.posts).await;
                Ok::<_, mattermost_api::Error>((hits.posts, authors, statuses))
            },
            move |result| {
                // Live search sends a request per pause in typing; an answer
                // to an older query must not replace the newer one's.
                if ui.right.mode() != shown {
                    return;
                }
                {
                    let mut st = ui.state.borrow_mut();
                    st.searching = false;
                    match result {
                        Ok((posts, authors, statuses)) => {
                            for user in authors {
                                st.users.insert(user.id.clone(), user);
                            }
                            st.apply_statuses(statuses);
                            st.search_results = ChannelFeed::from_list(&posts).posts;
                            // Newest first reads better for a search than the
                            // oldest-first order a channel wants.
                            st.search_results.reverse();
                        }
                        Err(e) => {
                            drop(st);
                            ui.toast_error("Search failed", &e);
                            ui.refresh_messages();
                            return;
                        }
                    }
                }
                ui.refresh_messages();
            },
        );
    }

    /// Attachments matching a search, as a list of names to open.
    fn search_files(self: &Rc<Self>, terms: String) {
        let (client, team) = {
            let mut st = self.state.borrow_mut();
            st.searching = true;
            st.search_results.clear();
            (st.client.clone(), st.current_team.clone())
        };
        let Some(team_id) = team else { return };

        let shown = rhs::PanelMode::Search(format!("files: {terms}"));
        self.right.set_mode(shown.clone());
        self.refresh_panel_mode();
        self.overlay.set_show_sidebar(true);
        self.refresh_messages();

        let ui = self.clone();
        runtime::spawn(
            async move { client.search_files(&team_id, &terms).await },
            move |result| {
                if ui.right.mode() != shown {
                    return;
                }
                match result {
                    Ok(files) => {
                        // A file hit names the post it is attached to, so the
                        // posts are what gets listed — the same rows as any
                        // other search, and clicking one goes to the message.
                        let ids: Vec<String> = files.ordered().map(|f| f.post_id.clone()).collect();
                        ui.load_posts_by_id(ids);
                    }
                    Err(e) => {
                        ui.state.borrow_mut().searching = false;
                        ui.toast_error("File search failed", &e);
                        ui.refresh_messages();
                    }
                }
            },
        );
    }

    /// Fetches posts by id and shows them as the current search results.
    fn load_posts_by_id(self: &Rc<Self>, ids: Vec<String>) {
        if ids.is_empty() {
            self.state.borrow_mut().searching = false;
            self.refresh_messages();
            return;
        }
        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(
            async move {
                let posts = client.posts_by_ids(&ids).await?;
                let (authors, statuses) = hydrate_authors(&client, &posts).await;
                Ok::<_, mattermost_api::Error>((posts, authors, statuses))
            },
            move |result| {
                {
                    let mut st = ui.state.borrow_mut();
                    st.searching = false;
                    if let Ok((posts, authors, statuses)) = result {
                        for user in authors {
                            st.users.insert(user.id.clone(), user);
                        }
                        st.apply_statuses(statuses);
                        st.search_results = ChannelFeed::from_list(&posts).posts;
                        st.search_results.reverse();
                    }
                }
                ui.refresh_messages();
            },
        );
    }

    /// Everything a message's own menu can ask for.
    fn post_action(self: &Rc<Self>, post_id: String, what: PostAction) {
        let (client, me, post) = {
            let st = self.state.borrow();
            (st.client.clone(), st.me.id.clone(), st.post(&post_id))
        };
        let Some(post) = post else {
            self.toast("That message is no longer here");
            return;
        };

        match what {
            PostAction::CopyText => {
                self.window.clipboard().set_text(&post.message);
                self.toast("Message copied");
            }
            PostAction::CopyLink => {
                let team = self
                    .state
                    .borrow()
                    .current_team_name()
                    .unwrap_or_else(|| "_redirect".to_string());
                let link = format!("{}/{team}/pl/{post_id}", client.site_url());
                self.window.clipboard().set_text(&link);
                self.toast("Link copied");
            }
            PostAction::Summarise => {
                if self.state.borrow().bots.is_empty() {
                    self.toast("This server has no agent to ask");
                    return;
                }
                let ui = self.clone();
                self.toast("Asking the agent…");
                runtime::spawn(
                    async move { crate::agents::summarise_thread(&client, &post_id).await },
                    move |result| match result {
                        Ok(target) => {
                            ui.dispatch(Action::OpenPost(target.channel_id, target.post_id))
                        }
                        Err(e) => ui.toast_error("The agent could not answer", &e),
                    },
                );
            }
            PostAction::Remind => {
                let ui = self.clone();
                let me = me.clone();
                dialogs::post_reminder(&self.window, move |when_ms| {
                    let client = client.clone();
                    let me = me.clone();
                    let post_id = post_id.clone();
                    let ui = ui.clone();
                    runtime::spawn(
                        // The reminder route takes seconds, unlike everything
                        // else in this API.
                        async move {
                            client
                                .set_post_reminder(&me, &post_id, when_ms / 1000)
                                .await
                        },
                        move |result| match result {
                            Ok(()) => ui.toast("You will be reminded"),
                            Err(e) => ui.toast_error("Could not set that", &e),
                        },
                    );
                });
            }
            PostAction::History => {
                let ui = self.clone();
                runtime::spawn(
                    async move { client.post_edit_history(&post_id).await },
                    move |result| match result {
                        Ok(versions) if versions.is_empty() => {
                            ui.toast("No earlier versions are kept for this message")
                        }
                        Ok(versions) => ui.show_history(versions),
                        Err(e) => ui.toast_error("Could not load the history", &e),
                    },
                );
            }
            PostAction::Acknowledge | PostAction::Unacknowledge => {
                let ack = what == PostAction::Acknowledge;
                let ui = self.clone();
                runtime::spawn(
                    async move {
                        if ack {
                            client.acknowledge_post(&me, &post_id).await.map(|_| ())
                        } else {
                            client.unacknowledge_post(&me, &post_id).await
                        }
                    },
                    move |result| {
                        // The server broadcasts the change, which is what
                        // redraws the row; only a failure needs saying.
                        if let Err(e) = result {
                            ui.toast_error("Could not do that", &e);
                        }
                    },
                );
            }
            PostAction::Forward => {
                // Forwarding is a new message carrying a permalink: the server
                // resolves that back into the original, which is how the other
                // clients do it and why the quoted post stays live rather than
                // becoming a stale copy.
                let team = self
                    .state
                    .borrow()
                    .current_team_name()
                    .unwrap_or_else(|| "_redirect".to_string());
                let link = format!("{}/{team}/pl/{post_id}", client.site_url());
                let ui = self.clone();
                self.pick_channel(move |channel_id| {
                    let client = client.clone();
                    let link = link.clone();
                    let ui = ui.clone();
                    runtime::spawn(
                        async move { client.send_message(&channel_id, &link, None).await },
                        move |result| match result {
                            Ok(post) => {
                                ui.dispatch(Action::OpenPost(post.channel_id, String::new()))
                            }
                            Err(e) => ui.toast_error("Could not forward it", &e),
                        },
                    );
                });
            }
            PostAction::MoveThread => {
                let ui = self.clone();
                self.pick_channel(move |channel_id| {
                    let client = client.clone();
                    let post_id = post_id.clone();
                    let ui = ui.clone();
                    runtime::spawn(
                        async move { client.move_thread(&post_id, &channel_id).await },
                        move |result| match result {
                            Ok(()) => ui.toast("Thread moved"),
                            Err(e) => ui.toast_error("Could not move it", &e),
                        },
                    );
                });
            }
            PostAction::Edit => self.chat.begin_edit(&post_id, post.source_text()),
            PostAction::Delete => self.confirm_delete(post_id),
            PostAction::Pin | PostAction::Unpin => {
                let pin = what == PostAction::Pin;
                let ui = self.clone();
                runtime::spawn(
                    async move { client.pin_post(&post_id, pin).await },
                    move |result| match result {
                        // The server echoes the change as post_edited, so
                        // there is nothing to apply here.
                        Ok(()) => ui.toast(if pin {
                            "Message pinned"
                        } else {
                            "Message unpinned"
                        }),
                        Err(e) => ui.toast_error("Could not change the pin", &e),
                    },
                );
            }
            PostAction::Save | PostAction::Unsave => {
                let save = what == PostAction::Save;
                self.set_saved(post_id, save);
            }
            PostAction::MarkUnread => {
                let (crt, channel) = {
                    let st = self.state.borrow();
                    (st.crt_enabled, post.channel_id.clone())
                };
                let ui = self.clone();
                runtime::spawn(
                    async move { client.set_post_unread(&me, &post_id, crt).await },
                    move |result| match result {
                        Ok(()) => {
                            // Nothing is being read here any more, so stop
                            // marking it read on the way out.
                            if ui.state.borrow().current_channel.as_deref()
                                == Some(channel.as_str())
                            {
                                ui.state.borrow_mut().current_channel = None;
                                ui.chat.set_composer_text("");
                            }
                            ui.schedule_sidebar_reload();
                        }
                        Err(e) => ui.toast_error("Could not mark it unread", &e),
                    },
                );
            }
        }
    }

    /// Deleting is destructive and has no undo, so it asks first.
    fn confirm_delete(self: &Rc<Self>, post_id: String) {
        let ui = self.clone();
        dialogs::confirm(
            &self.window,
            "Delete Message?",
            "It will be removed for everyone and cannot be restored.",
            "Delete",
            move || {
                let client = ui.state.borrow().client.clone();
                let post_id = post_id.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move { client.delete_post(&post_id).await },
                    move |result| {
                        if let Err(e) = result {
                            ui.toast_error("Could not delete it", &e);
                        }
                    },
                );
            },
        );
    }

    /// Saving a post is a preference, not a post field, so it is written and
    /// mirrored locally rather than waiting for an echo that never comes.
    fn set_saved(self: &Rc<Self>, post_id: String, save: bool) {
        let (client, me) = {
            let mut st = self.state.borrow_mut();
            if save {
                st.saved_posts.insert(post_id.clone());
            } else {
                st.saved_posts.remove(&post_id);
            }
            (st.client.clone(), st.me.id.clone())
        };
        self.refresh_messages();

        let ui = self.clone();
        let id = post_id.clone();
        runtime::spawn(
            async move {
                let pref = mattermost_api::models::Preference {
                    user_id: me.clone(),
                    category: "flagged_post".into(),
                    name: id.clone(),
                    value: "true".into(),
                };
                if save {
                    client.save_preferences(&me, &[pref]).await
                } else {
                    client.delete_preferences(&me, &[pref]).await
                }
            },
            move |result| {
                if let Err(e) = result {
                    // Put the local view back where the server still has it.
                    let mut st = ui.state.borrow_mut();
                    if save {
                        st.saved_posts.remove(&post_id);
                    } else {
                        st.saved_posts.insert(post_id.clone());
                    }
                    drop(st);
                    ui.refresh_messages();
                    ui.toast_error("Could not change that", &e);
                }
            },
        );
    }

    /// Follows or unfollows the open thread. Following is what keeps a thread
    /// in the inbox after you stop being mentioned in it.
    fn follow_thread(self: &Rc<Self>, following: bool) {
        let PanelMode::Thread(root_id) = self.right.mode() else {
            return;
        };
        let (client, team_id) = {
            let st = self.state.borrow();
            (
                st.client.clone(),
                st.current_team.clone().unwrap_or_default(),
            )
        };
        let ui = self.clone();
        runtime::spawn(
            async move { client.follow_thread(&team_id, &root_id, following).await },
            move |result| match result {
                Ok(()) => ui.load_inbox(),
                Err(e) => ui.toast_error("Could not change that", &e),
            },
        );
    }

    /// Shows an integration's form and posts the answers back.
    ///
    /// The server does not interpret the submission — it forwards it to the
    /// integration's own URL, which is why that URL has to be echoed back
    /// exactly as it arrived.
    fn open_dialog(self: &Rc<Self>, request: mattermost_api::models::dialog::OpenDialogRequest) {
        let (client, me, channel_id, team_id) = {
            let st = self.state.borrow();
            (
                st.client.clone(),
                st.me.id.clone(),
                st.current_channel.clone().unwrap_or_default(),
                st.current_team.clone().unwrap_or_default(),
            )
        };

        let dialog = request.dialog.clone();
        let submit_ctx = (
            client.clone(),
            request.url.clone(),
            dialog.callback_id.clone(),
            dialog.state.clone(),
            me.clone(),
            channel_id.clone(),
            team_id.clone(),
        );
        let cancel_ctx = submit_ctx.clone();
        let notify_on_cancel = dialog.notify_on_cancel;
        let ui = self.clone();
        let cancel_ui = self.clone();

        interactive::present(
            &self.window,
            &dialog,
            move |submission| {
                let (client, url, callback_id, state, user_id, channel_id, team_id) =
                    submit_ctx.clone();
                let ui = ui.clone();
                runtime::spawn(
                    async move {
                        let request = mattermost_api::models::dialog::SubmitDialogRequest {
                            url,
                            callback_id,
                            state,
                            user_id,
                            channel_id,
                            team_id,
                            submission,
                            cancelled: false,
                        };
                        client.submit_dialog(&request).await
                    },
                    move |result| match result {
                        // Errors come back inside a 200 as well, since the
                        // integration is what validated the form.
                        Ok(response) => {
                            if let Some(error) = response
                                .get("error")
                                .and_then(|v| v.as_str())
                                .filter(|e| !e.is_empty())
                            {
                                ui.toast(error);
                            }
                        }
                        Err(e) => ui.toast_error("That form was not accepted", &e),
                    },
                );
            },
            move || {
                // Only when the dialog asked to be told; most do not care.
                if !notify_on_cancel {
                    return;
                }
                let (client, url, callback_id, state, user_id, channel_id, team_id) =
                    cancel_ctx.clone();
                let _ = &cancel_ui;
                runtime::spawn(
                    async move {
                        let request = mattermost_api::models::dialog::SubmitDialogRequest {
                            url,
                            callback_id,
                            state,
                            user_id,
                            channel_id,
                            team_id,
                            submission: Default::default(),
                            cancelled: true,
                        };
                        client.submit_dialog(&request).await
                    },
                    |_| {},
                );
            },
        );
    }

    /// Runs a slash command. Its output arrives as a post or an ephemeral
    /// message, so there is usually nothing to show from the response itself.
    fn run_command(self: &Rc<Self>, channel_id: String, command: String) {
        let client = self.state.borrow().client.clone();
        self.chat.set_composer_text("");
        let ui = self.clone();
        runtime::spawn(
            async move { client.execute_command(&channel_id, &command).await },
            move |result| match result {
                Ok(response) => {
                    // Some commands answer with somewhere to go rather than
                    // something to say.
                    if let Some(location) = response
                        .get("goto_location")
                        .and_then(|v| v.as_str())
                        .filter(|l| !l.is_empty())
                    {
                        let _ = gtk::gio::AppInfo::launch_default_for_uri(
                            location,
                            None::<&gtk::gio::AppLaunchContext>,
                        );
                    }
                }
                Err(e) => ui.toast_error("That command failed", &e),
            },
        );
    }

    /// Follows a link to another message on this server.
    ///
    /// The link names only the post, so the channel has to be looked up —
    /// from what is loaded when possible, and from the server when not. A
    /// reminder about a message in a channel you have not opened this session
    /// is exactly the case that would otherwise fail.
    fn open_permalink(self: &Rc<Self>, post_id: String) {
        let (client, known) = {
            let st = self.state.borrow();
            (st.client.clone(), st.post(&post_id).map(|p| p.channel_id))
        };
        if let Some(channel_id) = known {
            self.jump_to_post(channel_id, post_id);
            return;
        }

        let ui = self.clone();
        runtime::spawn(
            async move { client.post(&post_id).await },
            move |result| match result {
                Ok(post) => {
                    let channel_id = post.channel_id.clone();
                    let post_id = post.id.clone();
                    ui.state.borrow_mut().apply_post(post);
                    ui.jump_to_post(channel_id, post_id);
                }
                Err(e) => ui.toast_error("Could not open that message", &e),
            },
        );
    }

    /// Opens a channel and puts one message on screen.
    ///
    /// Whether it is loaded decides what happens: if it is, scroll to it; if
    /// it is not, fetch the page around it, because a hit from three months
    /// ago is not reachable by paging back from today.
    fn jump_to_post(self: &Rc<Self>, channel_id: String, post_id: String) {
        self.select_channel_inner(channel_id.clone(), true);

        // After the channel's own load and layout have had their turn.
        let ui = self.clone();
        glib::idle_add_local_once(move || {
            if ui.chat.scroll_to_post(&post_id) {
                return;
            }
            let (client, crt) = {
                let st = ui.state.borrow();
                (st.client.clone(), st.crt_enabled)
            };
            runtime::spawn(
                async move {
                    // Half a page either side, so the message lands in the
                    // middle with its context rather than at an edge.
                    let before = client
                        .posts_before(&channel_id, &post_id, INITIAL_POSTS / 2, crt)
                        .await?;
                    let after = client
                        .posts_after(&channel_id, &post_id, INITIAL_POSTS / 2, crt)
                        .await?;
                    let target = client.post(&post_id).await?;
                    let (authors, statuses) = hydrate_authors(&client, &before).await;
                    Ok::<_, mattermost_api::Error>((
                        channel_id, before, after, target, authors, statuses,
                    ))
                },
                move |result| {
                    let Ok((channel_id, before, after, target, authors, statuses)) = result else {
                        ui.toast("That message could not be loaded");
                        return;
                    };
                    let post_id = target.id.clone();
                    {
                        let mut st = ui.state.borrow_mut();
                        for user in authors {
                            st.users.insert(user.id.clone(), user);
                        }
                        st.apply_statuses(statuses);
                        let feed = st.feeds.entry(channel_id).or_default();
                        for post in ChannelFeed::from_list(&before).posts {
                            feed.upsert(post);
                        }
                        for post in ChannelFeed::from_list(&after).posts {
                            feed.upsert(post);
                        }
                        feed.upsert(target);
                        // The feed no longer runs to the newest post, so the
                        // view must not claim it does.
                        feed.at_latest = false;
                    }
                    ui.refresh_messages();
                    let ui = ui.clone();
                    glib::idle_add_local_once(move || {
                        ui.chat.scroll_to_post(&post_id);
                    });
                },
            );
        });
    }

    /// Fetches everything posted in a channel *after* the newest post we
    /// hold. Used when the socket has been away long enough that the feed has
    /// a hole in it — scrolling up finds older messages, and nothing else
    /// would find the ones in the middle.
    fn load_newer(self: &Rc<Self>, channel_id: String) {
        let (client, crt, newest) = {
            let st = self.state.borrow();
            let Some(feed) = st.feeds.get(&channel_id) else {
                return;
            };
            let Some(newest) = feed.posts.last().map(|p| p.id.clone()) else {
                return;
            };
            (st.client.clone(), st.crt_enabled, newest)
        };

        let ui = self.clone();
        runtime::spawn(
            async move {
                let posts = client
                    .posts_after(&channel_id, &newest, INITIAL_POSTS, crt)
                    .await?;
                let (authors, statuses) = hydrate_authors(&client, &posts).await;
                Ok::<_, mattermost_api::Error>((channel_id, posts, authors, statuses))
            },
            move |result| {
                let Ok((channel_id, posts, authors, statuses)) = result else {
                    return;
                };
                {
                    let mut st = ui.state.borrow_mut();
                    for user in authors {
                        st.users.insert(user.id.clone(), user);
                    }
                    st.apply_statuses(statuses);
                    let newer = ChannelFeed::from_list(&posts);
                    if let Some(feed) = st.feeds.get_mut(&channel_id) {
                        for post in newer.posts {
                            feed.upsert(post);
                        }
                    }
                }
                ui.refresh_messages();
            },
        );
    }

    /// Fetches the page of messages before the oldest one we hold.
    ///
    /// Only one at a time, and never past the beginning: reaching the top of a
    /// short channel would otherwise ask for the same empty page on every
    /// scroll event.
    fn load_older(self: &Rc<Self>) {
        if self.loading_older.get() {
            if chat::scroll_trace_enabled() {
                tracing::info!(
                    target: "matras::scroll",
                    event = "pagination-suppressed",
                    reason = "already-loading",
                    "scroll trace"
                );
            }
            return;
        }
        let (client, crt, channel_id, oldest) = {
            let st = self.state.borrow();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let Some(feed) = st.feeds.get(&channel_id) else {
                return;
            };
            if feed.at_oldest {
                if chat::scroll_trace_enabled() {
                    tracing::info!(
                        target: "matras::scroll",
                        event = "pagination-suppressed",
                        reason = "at-oldest",
                        channel_id,
                        "scroll trace"
                    );
                }
                return;
            }
            let Some(oldest) = feed.posts.first().map(|p| p.id.clone()) else {
                return;
            };
            (st.client.clone(), st.crt_enabled, channel_id, oldest)
        };

        self.loading_older.set(true);
        if chat::scroll_trace_enabled() {
            tracing::info!(
                target: "matras::scroll",
                event = "pagination-request",
                channel_id,
                oldest,
                "scroll trace"
            );
        }
        self.chat.set_loading_older(true);
        let ui = self.clone();
        runtime::spawn(
            async move {
                let posts = client
                    .posts_before(&channel_id, &oldest, INITIAL_POSTS, crt)
                    .await?;
                let (authors, statuses) = hydrate_authors(&client, &posts).await;
                Ok::<_, mattermost_api::Error>((channel_id, posts, authors, statuses))
            },
            move |result| {
                ui.chat.set_loading_older(false);
                let Ok((channel_id, posts, authors, statuses)) = result else {
                    if chat::scroll_trace_enabled() {
                        tracing::warn!(
                            target: "matras::scroll",
                            event = "pagination-error",
                            "scroll trace"
                        );
                    }
                    ui.loading_older.set(false);
                    ui.chat.retry_older_on_next_edge_change();
                    return;
                };
                if chat::scroll_trace_enabled() {
                    tracing::info!(
                        target: "matras::scroll",
                        event = "pagination-response",
                        channel_id,
                        posts = posts.posts.len(),
                        "scroll trace"
                    );
                }
                let kept;
                // Only built when the channel this page belongs to is still
                // the one on screen: a `prepend_older` for a feed nobody is
                // looking at would splice rows into the wrong pane.
                let mut prepend = None;
                {
                    let mut st = ui.state.borrow_mut();
                    for user in authors {
                        st.users.insert(user.id.clone(), user);
                    }
                    st.apply_statuses(statuses);
                    let older = ChannelFeed::from_list(&posts);
                    // An empty page means there is nothing before this, and
                    // the feed should stop asking.
                    let exhausted = older.posts.is_empty();
                    kept = older.posts.clone();
                    let now_at_oldest = exhausted || older.at_oldest;
                    // Computed before the mutable borrow below so this can
                    // still call the ordinary (immutable) state accessors.
                    // The title is only fetched when it will actually be
                    // shown — a channel that is not yet exhausted, or is not
                    // even the one on screen, never needs it.
                    let showing = st.current_channel.as_deref() == Some(channel_id.as_str());
                    let at_oldest_title = (showing && now_at_oldest)
                        .then(|| st.channel(&channel_id).map(|c| st.channel_title(c)))
                        .flatten();
                    if let Some(feed) = st.feeds.get_mut(&channel_id) {
                        for post in older.posts {
                            feed.upsert(post);
                        }
                        feed.at_oldest = now_at_oldest;
                        if showing {
                            prepend = Some((feed.posts.clone(), at_oldest_title));
                        }
                    }
                }
                // Splices in only the new rows rather than rebuilding the
                // whole feed (see `ChatView::prepend_older`) — the fix for
                // the scrollback stall, which used to redraw everything ever
                // paged into this channel on every page turn.
                if let Some((merged, at_oldest_title)) = prepend {
                    let actions = ui.message_actions();
                    let restored_ui = ui.clone();
                    ui.chat.prepend_older(
                        &merged,
                        at_oldest_title.as_deref(),
                        &ui.state,
                        &ui.avatars,
                        &actions,
                        move || restored_ui.loading_older.set(false),
                    );
                } else {
                    ui.loading_older.set(false);
                }
                ui.store_posts(kept);
            },
        );
    }

    /// A thread's reply box has its own draft, keyed by the thread root —
    /// which is how the server stores them too, so they sync with the other
    /// clients rather than only surviving locally.
    fn schedule_thread_draft_save(self: &Rc<Self>) {
        if self.thread_draft_pending.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
            ui.thread_draft_pending.set(false);
            ui.save_thread_draft();
        });
    }

    fn save_thread_draft(self: &Rc<Self>) {
        let PanelMode::Thread(root_id) = self.right.mode() else {
            return;
        };
        let text = self.right.composer_text();
        let (client, channel_id, synced, changed) = {
            let mut st = self.state.borrow_mut();
            let Some(channel_id) = st.find_post(&root_id).map(|p| p.channel_id.clone()) else {
                return;
            };
            let trimmed = text.trim().to_string();
            let changed = if trimmed.is_empty() {
                st.thread_drafts.remove(&root_id).is_some()
            } else {
                st.thread_drafts.insert(root_id.clone(), trimmed.clone()) != Some(trimmed)
            };
            (st.client.clone(), channel_id, st.drafts_synced, changed)
        };
        if !changed || !synced {
            return;
        }

        let body = text.trim().to_string();
        let draft = mattermost_api::models::Draft::new(&channel_id, &root_id, &body);
        runtime::spawn(
            async move {
                if body.is_empty() {
                    client.delete_draft(&channel_id, &root_id).await
                } else {
                    client.upsert_draft(&draft).await.map(|_| ())
                }
            },
            move |result| {
                if let Err(e) = result {
                    tracing::warn!(error = %e, "could not save the thread draft");
                }
            },
        );
    }

    fn restore_thread_draft(&self) {
        let text = match self.right.mode() {
            PanelMode::Thread(root_id) => self
                .state
                .borrow()
                .thread_drafts
                .get(&root_id)
                .cloned()
                .unwrap_or_default(),
            _ => return,
        };
        self.right.set_composer_text(&text);
    }

    /// Saves the composer as a draft shortly after typing stops.
    ///
    /// Debounced rather than per-keystroke: a draft is worth one request when
    /// someone pauses, not one per character.
    fn schedule_draft_save(self: &Rc<Self>) {
        if self.draft_save_pending.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
            ui.draft_save_pending.set(false);
            ui.save_draft();
        });
    }

    /// Stores the current composer text for the channel it belongs to, locally
    /// and — when the server keeps drafts — there too.
    fn save_draft(self: &Rc<Self>) {
        let text = self.chat.composer_text();
        let (client, channel_id, synced, changed) = {
            let mut st = self.state.borrow_mut();
            let Some(channel_id) = st.current_channel.clone() else {
                return;
            };
            let trimmed = text.trim().to_string();
            let changed = if trimmed.is_empty() {
                st.drafts.remove(&channel_id).is_some()
            } else {
                st.drafts.insert(channel_id.clone(), trimmed.clone()) != Some(trimmed)
            };
            (st.client.clone(), channel_id, st.drafts_synced, changed)
        };
        if !changed {
            return;
        }
        // The pencil in the sidebar has to follow.
        self.channels.refresh(&self.state, &self.avatars);
        if !synced {
            return;
        }

        let ui = self.clone();
        let body = text.trim().to_string();
        let draft = mattermost_api::models::Draft::new(&channel_id, "", &body);
        runtime::spawn(
            async move {
                // An empty upsert deletes server-side, but the explicit route
                // says what is meant and does not depend on that behaviour
                // staying true.
                if body.is_empty() {
                    client.delete_draft(&channel_id, "").await
                } else {
                    client.upsert_draft(&draft).await.map(|_| ())
                }
            },
            move |result| {
                if let Err(e) = result {
                    // 501 means the admin turned synced drafts off. Stop
                    // asking; the local copy still works.
                    if matches!(&e, mattermost_api::Error::Api(a) if a.status_code == 501) {
                        tracing::info!("synced drafts are disabled on this server");
                        ui.state.borrow_mut().drafts_synced = false;
                    } else {
                        tracing::warn!(error = %e, "could not save the draft");
                    }
                }
            },
        );
    }

    /// Pulls this team's drafts and puts the current channel's back.
    fn load_drafts(self: &Rc<Self>) {
        let (client, team) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_team.clone())
        };
        let Some(team_id) = team else { return };

        let ui = self.clone();
        runtime::spawn(
            async move { client.my_drafts(&team_id).await },
            move |result| match result {
                Ok(drafts) => {
                    {
                        let mut st = ui.state.borrow_mut();
                        for draft in drafts {
                            if draft.message.is_empty() {
                                continue;
                            }
                            if draft.root_id.is_empty() {
                                st.drafts.insert(draft.channel_id, draft.message);
                            } else {
                                st.thread_drafts.insert(draft.root_id, draft.message);
                            }
                        }
                    }
                    ui.restore_draft();
                    ui.channels.refresh(&ui.state, &ui.avatars);
                }
                Err(e) => {
                    if matches!(&e, mattermost_api::Error::Api(a) if a.status_code == 501) {
                        ui.state.borrow_mut().drafts_synced = false;
                    }
                }
            },
        );
    }

    /// Puts the current channel's draft into the composer.
    fn restore_draft(&self) {
        let text = {
            let st = self.state.borrow();
            st.current_channel
                .as_ref()
                .and_then(|id| st.drafts.get(id))
                .cloned()
                .unwrap_or_default()
        };
        self.chat.set_composer_text(&text);
    }

    /// Repaints the "someone is typing" line, and schedules the repaint that
    /// will clear it. The server never says anyone stopped, so the line has to
    /// expire on its own clock.
    fn refresh_typing(self: &Rc<Self>) {
        let names = {
            let mut st = self.state.borrow_mut();
            let Some(channel_id) = st.current_channel.clone() else {
                drop(st);
                self.chat.set_typing(&[]);
                return;
            };
            let ids = st.typing_in(&channel_id);
            let display = st.teammate_name_display().to_string();
            ids.iter()
                .filter_map(|id| st.users.get(id).map(|u| u.display_name(&display)))
                .collect::<Vec<_>>()
        };
        let anyone = !names.is_empty();
        self.chat.set_typing(&names);

        // One pending sweep at a time, or every keystroke would add a timer.
        if anyone && !self.typing_sweep_pending.replace(true) {
            let ui = self.clone();
            glib::timeout_add_local_once(AppState::TYPING_TTL, move || {
                ui.typing_sweep_pending.set(false);
                ui.refresh_typing();
            });
        }
    }

    /// Tells the server we are typing, at most once every few seconds. The
    /// server repeats to other clients on its own schedule, so sending on every
    /// keystroke would be pure noise.
    fn notify_typing(self: &Rc<Self>) {
        if self.typing_sent_recently.replace(true) {
            return;
        }
        {
            let st = self.state.borrow();
            if let (Some(ws), Some(channel)) = (st.ws.as_ref(), st.current_channel.as_ref()) {
                let _ = ws.typing(channel, "");
            }
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            ui.typing_sent_recently.set(false);
        });
    }

    /// Refetches the channel list, its memberships and the categories for the
    /// current team.
    ///
    /// Debounced: joining a team, or an admin reorganising channels, produces a
    /// burst of these events, and one reload after the burst is as correct as
    /// nine during it.
    fn schedule_sidebar_reload(self: &Rc<Self>) {
        if self.sidebar_reload_pending.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            ui.sidebar_reload_pending.set(false);
            ui.reload_sidebar();
        });
    }

    fn reload_sidebar(self: &Rc<Self>) {
        let (client, team) = {
            let st = self.state.borrow();
            (st.client.clone(), st.current_team.clone())
        };
        let Some(team_id) = team else { return };

        let ui = self.clone();
        runtime::spawn(
            async move {
                tokio::try_join!(
                    client.my_channels(&team_id, false, 0),
                    client.my_channel_members(&team_id),
                    client.sidebar_categories(&team_id),
                )
            },
            move |result| {
                let Ok((channels, members, categories)) = result else {
                    // A failed refresh is not worth interrupting anyone over:
                    // the next event, or the next resync, tries again.
                    return;
                };
                {
                    let mut st = ui.state.borrow_mut();
                    st.channels.clear();
                    st.memberships.clear();
                    for c in channels {
                        st.channels.insert(c.id.clone(), c);
                    }
                    for m in members {
                        st.memberships.insert(m.channel_id.clone(), m);
                    }
                    st.categories = categories;
                }
                ui.channels.refresh(&ui.state, &ui.avatars);
                ui.refresh_title();
                ui.hydrate_dm_teammates();
            },
        );
    }

    /// Unread and mention counts for every team, so the switcher can show
    /// where something is waiting rather than only what is in front of you.
    fn load_team_unreads(self: &Rc<Self>) {
        let (client, crt) = {
            let st = self.state.borrow();
            (st.client.clone(), st.crt_enabled)
        };
        let ui = self.clone();
        runtime::spawn(
            async move { client.my_team_unreads(crt).await },
            move |result| {
                let Ok(unreads) = result else { return };
                {
                    let mut st = ui.state.borrow_mut();
                    st.team_unreads = unreads
                        .into_iter()
                        .map(|u| (u.team_id, (u.msg_count, u.mention_count)))
                        .collect();
                }
                ui.channels.refresh(&ui.state, &ui.avatars);
            },
        );
    }

    /// Being added to or removed from a team changes the switcher, not the
    /// channel list.
    fn reload_teams(self: &Rc<Self>) {
        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(async move { client.my_teams().await }, move |result| {
            if let Ok(teams) = result {
                ui.state.borrow_mut().teams = teams;
                ui.channels.refresh(&ui.state, &ui.avatars);
            }
        });
    }

    /// A DM channel carries no display name — just `"<idA>__<idB>"` — so until
    /// the other person is in `users` the sidebar row is blank. Nothing in the
    /// startup sequence fetches them, so do it here: after any load that
    /// replaces the channel list.
    fn hydrate_dm_teammates(self: &Rc<Self>) {
        let (client, ids) = {
            let st = self.state.borrow();
            let ids: Vec<String> = st
                .channels
                .values()
                .filter(|c| c.r#type == ChannelType::Direct)
                .filter_map(|c| c.dm_teammate_id(&st.me.id))
                // Missing either half is a reason to ask: a teammate known
                // from a message may still have no presence.
                .filter(|id| !st.users.contains_key(*id) || !st.statuses.contains_key(*id))
                .map(str::to_string)
                .collect();
            (st.client.clone(), ids)
        };
        if ids.is_empty() {
            return;
        }

        let ui = self.clone();
        runtime::spawn(
            async move {
                (
                    client.users_by_ids(&ids).await.unwrap_or_default(),
                    client.statuses_by_ids(&ids).await.unwrap_or_default(),
                )
            },
            move |(users, statuses)| {
                {
                    let mut st = ui.state.borrow_mut();
                    for user in users {
                        st.users.insert(user.id.clone(), user);
                    }
                    st.apply_statuses(statuses);
                }
                ui.channels.refresh(&ui.state, &ui.avatars);
                ui.refresh_title();
            },
        );
    }

    /// Mirrors what the desktop app puts in its tray badge: a single mention
    /// count for the whole account.
    fn refresh_title(&self) {
        let mentions = self.state.borrow().total_mentions();
        self.window.set_title(Some(&if mentions > 0 {
            format!("({mentions}) Matras")
        } else {
            "Matras".to_string()
        }));
    }

    fn refresh_call_ui(&self) {
        let st = self.state.borrow();
        let available = st.calls.is_some() && st.current_channel.is_some();
        let reason = if st.calls.is_none() {
            Some("Calls are not enabled on this server")
        } else {
            None
        };
        // The call we are in only shows as "ours" on its own channel; switching
        // away leaves it running, exactly as the web client does — which is why
        // the dock, not the header, carries the controls.
        let in_call = st
            .call
            .as_ref()
            .is_some_and(|c| Some(&c.channel_id) == st.current_channel.as_ref());
        let in_progress = st
            .current_channel
            .as_ref()
            .and_then(|id| st.active_calls.get(id))
            .map(Vec::len);
        drop(st);

        self.chat.set_calls_available(available, reason);
        self.chat.set_call_in_progress(in_progress);
        self.chat.set_in_call(in_call, in_progress.is_some());
        let visible = self.dock.refresh(&self.state, &self.avatars);
        self.dock_visible.set(visible);
        self.host_toolbar().set_reveal_bottom_bars(visible);
    }

    /// Whichever toolbar view currently holds the call dock.
    fn host_toolbar(&self) -> &adw::ToolbarView {
        if self.dock_in_chat.get() {
            &self.chat.widget
        } else {
            &self.channels.widget
        }
    }

    /// Moves the dock between the sidebar and the conversation. Reparenting is
    /// the only option: a widget cannot be in two places, and duplicating it
    /// would duplicate its state along with it.
    fn place_dock(&self, into_chat: bool) {
        if self.dock_in_chat.get() == into_chat {
            return;
        }
        self.host_toolbar().remove(&self.dock.widget);
        self.host_toolbar().set_reveal_bottom_bars(false);
        self.dock_in_chat.set(into_chat);
        self.host_toolbar().add_bottom_bar(&self.dock.widget);
        self.host_toolbar()
            .set_reveal_bottom_bars(self.dock_visible.get());
    }

    /// A thread is a place you read alongside the conversation, so it earns a
    /// static column when there is room. The inbox is a stack you glance at and
    /// dismiss, so it always overlays — pushing the conversation aside for it
    /// would be a heavier gesture than the content deserves.
    fn refresh_panel_mode(&self) {
        let overlays =
            rhs::panel_should_overlay(self.narrow.get(), self.window.width(), &self.right.mode());
        self.overlay.set_collapsed(overlays);
        self.right.set_overlaid(overlays);
    }

    /// Opens the card for a mention, which names a handle rather than an id.
    ///
    /// The special ones — here, channel, all — address everybody and have no
    /// account behind them, so there is nothing to open.
    fn show_profile_by_handle(self: &Rc<Self>, handle: &str, anchor: &gtk::Widget) {
        if matches!(handle, "here" | "channel" | "all") {
            return;
        }
        let known = self
            .state
            .borrow()
            .users
            .values()
            .find(|u| u.username == handle)
            .map(|u| u.id.clone());
        if let Some(user_id) = known {
            self.show_profile(&user_id, anchor);
            return;
        }

        // Somebody mentioned in a message we are reading but who has never
        // posted here — worth one lookup rather than a dead link.
        let client = self.state.borrow().client.clone();
        let handles = vec![handle.to_string()];
        let ui = self.clone();
        let anchor = anchor.clone();
        runtime::spawn(
            async move { client.users_by_usernames(&handles).await },
            move |result| {
                let Ok(users) = result else { return };
                let Some(user) = users.into_iter().next() else {
                    return;
                };
                let id = user.id.clone();
                ui.state.borrow_mut().users.insert(id.clone(), user);
                ui.show_profile(&id, &anchor);
            },
        );
    }

    fn show_profile(self: &Rc<Self>, user_id: &str, anchor: &gtk::Widget) {
        let popover = profile::popover(user_id, &self.state, &self.avatars, anchor, {
            let ui = self.clone();
            move |id| ui.dispatch(Action::OpenDirectMessage(id))
        });
        let Some(popover) = popover else {
            // Not held yet — fetch them and try again rather than telling
            // someone to wait for something they cannot make happen.
            let client = self.state.borrow().client.clone();
            let id = user_id.to_string();
            let ui = self.clone();
            let anchor = anchor.clone();
            runtime::spawn(
                async move { client.user(&id).await },
                move |result| match result {
                    Ok(user) => {
                        let id = user.id.clone();
                        ui.state.borrow_mut().users.insert(id.clone(), user);
                        ui.show_profile(&id, &anchor);
                    }
                    Err(_) => ui.toast("That person could not be looked up"),
                },
            );
            return;
        };
        // The popover parents itself onto the anchor, so it has to unparent
        // when it closes or the anchor keeps a widget it no longer shows.
        popover.connect_closed(|p| p.unparent());
        popover.popup();
    }

    fn handle(self: &Rc<Self>, action: Action) {
        match action {
            Action::SelectTeam(team_id) => self.select_team(team_id),
            Action::SelectChannel(channel_id) => self.select_channel(channel_id),
            Action::Send(text) => {
                // The composer is shared with editing, so what "send" means
                // depends on which mode it is in.
                match self.chat.editing() {
                    Some(post_id) => self.submit_edit(post_id, text),
                    None => self.send_message(text, None),
                }
                // The composer is empty now, so the draft has to go with it —
                // and immediately, not on the debounce.
                self.save_draft();
            }
            Action::ToggleCall => self.toggle_call(),
            Action::ToggleMute => self.toggle_mute(),
            Action::ToggleRecording => self.toggle_recording(),
            Action::ToggleScreen => self.toggle_screen(),
            Action::ToggleCamera => self.toggle_camera(),
            Action::OpenThread(root_id) => self.open_thread(root_id),
            Action::ReplyInThread(text) => {
                // The reply box is empty after this, and the draft has to go
                // with it — immediately, not on the debounce.
                let flush = self.clone();
                glib::idle_add_local_once(move || flush.save_thread_draft());
                if let PanelMode::Thread(root) = self.right.mode() {
                    self.send_message(text, Some(root));
                }
            }
            Action::OpenInbox => self.open_inbox(),
            Action::CloseRightPanel => {
                self.right.set_mode(PanelMode::Hidden);
                self.overlay.set_show_sidebar(false);
            }
            Action::ToggleReaction(post_id, emoji) => self.toggle_reaction(post_id, emoji),
            Action::OpenPost(channel_id, root_id) => {
                self.select_channel(channel_id);
                if !root_id.is_empty() {
                    self.open_thread(root_id);
                }
            }
            Action::JumpToPost(channel_id, post_id) => self.jump_to_post(channel_id, post_id),
            Action::OpenDirectMessage(user_id) => self.open_direct_message(user_id),
            Action::Post(post_id, what) => self.post_action(post_id, what),
            Action::Search(terms) => self.search(terms),
            Action::Complete(query) => self.complete(query),
            Action::ScheduleMessage => self.schedule_message(),
            Action::ThreadDraftChanged => self.schedule_thread_draft_save(),
            Action::LoadOlder => self.load_older(),
            Action::FollowThread(following) => self.follow_thread(following),
            Action::PickAttachment => self.pick_attachment(),
            Action::AttachFiles(paths) => {
                let Some(channel_id) = self.state.borrow().current_channel.clone() else {
                    return;
                };
                self.chat.set_uploading(paths.len());
                for path in paths {
                    self.upload(&channel_id, path);
                }
            }
            Action::SummariseUnreads => self.summarise_unreads(),
            Action::SetStatus(status) => self.set_status(status),
            Action::Row(channel_id, what) => self.row_action(channel_id, what),
            Action::ToggleHand => self.toggle_hand(),
            Action::HostControl(session_id, what) => self.host_control(session_id, what),
            Action::CallReaction(name, glyph) => {
                let Some(session) = self.state.borrow().call.as_ref().map(|c| c.session.clone())
                else {
                    return;
                };
                let ui = self.clone();
                runtime::spawn(
                    async move {
                        session
                            .react(&mattermost_calls::CallReaction {
                                name: name.clone(),
                                literal: glyph,
                                ..Default::default()
                            })
                            .await
                    },
                    move |result| {
                        if let Err(e) = result {
                            ui.toast_error("Could not react", &e);
                        }
                    },
                );
            }
            Action::DropAttachment(file_id) => {
                self.state
                    .borrow_mut()
                    .pending_files
                    .retain(|(id, _)| id != &file_id);
                self.refresh_attachments();
            }
            Action::ComposerChanged(has_text) => {
                if has_text {
                    self.notify_typing();
                }
                // Emptying the composer is a draft change too — that is how a
                // draft gets deleted.
                self.schedule_draft_save();
            }
            Action::OpenCallChannel => {
                let channel = self
                    .state
                    .borrow()
                    .call
                    .as_ref()
                    .map(|c| c.channel_id.clone());
                if let Some(id) = channel {
                    self.select_channel(id);
                }
            }
        }
    }

    // ------------------------------------------------------------- navigation

    fn select_team(self: &Rc<Self>, team_id: String) {
        // On a collapsed window, picking a team should land on that team's
        // channel list rather than straight into a conversation.
        self.split.set_show_content(false);
        {
            let mut st = self.state.borrow_mut();
            if st.current_team.as_deref() == Some(team_id.as_str()) {
                return;
            }
            st.current_team = Some(team_id.clone());
            st.current_channel = None;
        }

        let client = self.state.borrow().client.clone();
        let ui = self.clone();
        runtime::spawn(
            async move {
                tokio::try_join!(
                    client.my_channels(&team_id, false, 0),
                    client.my_channel_members(&team_id),
                    client.sidebar_categories(&team_id),
                )
            },
            move |result| match result {
                Ok((channels, members, categories)) => {
                    let first = {
                        let mut st = ui.state.borrow_mut();
                        st.channels.clear();
                        st.memberships.clear();
                        for c in channels {
                            st.channels.insert(c.id.clone(), c);
                        }
                        for m in members {
                            st.memberships.insert(m.channel_id.clone(), m);
                        }
                        st.categories = categories;
                        st.sidebar_groups()
                            .into_iter()
                            .flat_map(|(_, cs)| cs)
                            .next()
                            .map(|c| c.id)
                    };
                    ui.refresh_all();
                    ui.load_inbox();
                    ui.load_drafts();
                    ui.load_bots();
                    if let Some(id) = first {
                        ui.dispatch(Action::SelectChannel(id));
                    }
                }
                Err(e) => ui.toast_error("Could not load that team", &e),
            },
        );
    }

    fn select_channel(self: &Rc<Self>, channel_id: String) {
        self.select_channel_inner(channel_id, false);
    }

    fn escape_navigation(self: &Rc<Self>) {
        use shortcuts::EscapeAction;
        let action = {
            let channel_open = self.state.borrow().current_channel.is_some();
            shortcuts::escape_action(
                self.right.mode() != PanelMode::Hidden,
                channel_open,
                self.split.is_collapsed(),
                self.split.shows_content(),
            )
        };
        match action {
            EscapeAction::ClosePanel => self.dispatch(Action::CloseRightPanel),
            EscapeAction::Home => {
                self.save_draft();
                self.state.borrow_mut().current_channel = None;
                self.right.set_mode(PanelMode::Hidden);
                self.overlay.set_show_sidebar(false);
                self.split.set_show_content(true);
                self.chat.set_loading(false);
                self.refresh_messages();
                // The row keeps its ListBox selection otherwise, and clicking
                // the channel just left would not emit `row-selected` again.
                self.channels.refresh(&self.state, &self.avatars);
                self.refresh_call_ui();
                self.schedule_snapshot();
            }
            EscapeAction::ShowChannels => self.split.set_show_content(false),
            EscapeAction::None => {}
        }
    }

    fn select_channel_inner(self: &Rc<Self>, channel_id: String, from_post_jump: bool) {
        self.split.set_show_content(true);
        let already_open = self.state.borrow().current_channel.as_deref() == Some(&channel_id);
        // Nothing is being left when the same channel is asked for a second
        // time, and where the restore has got to so far is not where the
        // reader was: capturing it here overwrote the saved anchor with the
        // bottom of the cached block, moments before it was to be put back.
        if !already_open {
            self.capture_scroll_anchor();
            // Priority belongs to the message being written here, not to the
            // next one written anywhere.
            self.chat.reset_priority();
        }
        // Flush the outgoing channel's draft *before* the composer is pointed
        // at a new one, or the text would be filed under the wrong channel.
        self.save_draft();
        {
            let mut st = self.state.borrow_mut();
            // Asking for the channel already on screen is a no-op only once it
            // has a feed that runs to the newest message. The cache restore
            // makes a channel current before the server has said a word about
            // it, and that selection still has a fetch owed to it — swallowing
            // it here is what left a restored channel showing last session's
            // messages until somebody happened to post.
            if already_open && st.feeds.get(&channel_id).is_some_and(|feed| feed.at_latest) {
                let (client, crt) = (st.client.clone(), st.crt_enabled);
                drop(st);
                if from_post_jump
                    && inbox_post_reselect_needs_read_sync(Some(channel_id.as_str()), &channel_id)
                {
                    self.mark_channel_viewed(channel_id, client, crt);
                }
                return;
            }
            st.current_channel = Some(channel_id.clone());
        }
        self.refresh_messages();
        if let Some(anchor) = self.state.borrow().scroll_anchors.get(&channel_id).cloned() {
            self.chat.restore_anchor(&anchor);
        }
        self.refresh_call_ui();
        self.refresh_typing();
        self.restore_draft();
        self.schedule_snapshot();

        let (client, crt, have_feed) = {
            let st = self.state.borrow();
            (
                st.client.clone(),
                st.crt_enabled,
                // Holding *a* block is not holding the current one. Only a
                // feed that reaches the newest message spares us the fetch;
                // one restored from the cache, or left behind by a jump into
                // history, has to be brought up to date.
                st.feeds.get(&channel_id).is_some_and(|feed| feed.at_latest),
            )
        };

        self.chat.set_loading(!have_feed);
        // A channel with unread messages opens *at* them rather than at the
        // newest post — the same fetch the other clients use, so the page
        // arrives centred on where reading stopped instead of needing a scroll
        // back to find it.
        let unread = self.state.borrow().unread(&channel_id).is_unread();

        if !have_feed {
            // Repaint now that the pane knows it is waiting; the fetch below
            // may take a while and the reader should not be looking at "this
            // is the beginning of…" in the meantime.
            self.refresh_messages();
            let id = channel_id.clone();
            let fetch_client = client.clone();
            runtime::spawn(
                async move {
                    let posts = if unread {
                        fetch_client
                            .posts_around_unread(&id, INITIAL_POSTS / 2, INITIAL_POSTS / 2, crt)
                            .await?
                    } else {
                        fetch_client
                            .posts_for_channel(&id, 0, INITIAL_POSTS, crt)
                            .await?
                    };
                    let (authors, statuses) = hydrate_authors(&fetch_client, &posts).await;
                    Ok::<_, mattermost_api::Error>((posts, authors, statuses))
                },
                {
                    let ui = self.clone();
                    let channel_id = channel_id.clone();
                    move |result| match result {
                        Ok((posts, authors, statuses)) => {
                            {
                                let mut st = ui.state.borrow_mut();
                                for user in authors {
                                    st.users.insert(user.id.clone(), user);
                                }
                                st.apply_statuses(statuses);
                                st.feeds
                                    .insert(channel_id.clone(), ChannelFeed::from_list(&posts));
                            }
                            ui.chat.set_loading(false);
                            ui.refresh_messages();
                            if let Some(anchor) =
                                ui.state.borrow().scroll_anchors.get(&channel_id).cloned()
                            {
                                ui.chat.restore_anchor(&anchor);
                            }
                            ui.chat.focus_composer();
                            ui.resolve_mentions();
                            // Straight into the store, so the next launch has
                            // this channel without asking for it again.
                            ui.store_posts(posts.chronological().into_iter().cloned().collect());
                        }
                        Err(e) => {
                            ui.chat.set_loading(false);
                            ui.refresh_messages();
                            ui.toast_error("Could not load messages", &e);
                        }
                    }
                },
            );
        } else {
            self.chat.focus_composer();
        }

        // How many people are here. Cheap, cached by the server, and the
        // question "who can see this" comes up constantly in a channel you
        // have just walked into.
        {
            let stats_client = client.clone();
            let id = channel_id.clone();
            let ui = self.clone();
            runtime::spawn(
                async move { stats_client.channel_stats(&id).await },
                move |result| {
                    if let Ok(stats) = result {
                        ui.chat.set_member_count(Some(stats.member_count));
                    }
                },
            );
        }

        // Tell the server we are looking at this channel so read state syncs to
        // our other sessions.
        self.mark_channel_viewed(channel_id, client, crt);
    }

    /// Sync the current read position to Mattermost and clear the local unread
    /// counters. Inbox post jumps use this even when their channel was already
    /// selected, because that path does not otherwise re-enter channel setup.
    fn mark_channel_viewed(
        self: &Rc<Self>,
        channel_id: String,
        client: mattermost_api::Client,
        crt: bool,
    ) {
        let id = channel_id.clone();
        runtime::spawn(
            async move { client.view_channel(&id, "", crt).await },
            |_| {},
        );
        if let Some(app) = self.window.application() {
            notify::withdraw(&app, &channel_id);
        }

        {
            let mut st = self.state.borrow_mut();
            let total = st
                .channels
                .get(&channel_id)
                .map(|c| (c.total_msg_count, c.total_msg_count_root));
            if let (Some((total, total_root)), Some(member)) =
                (total, st.memberships.get_mut(&channel_id))
            {
                member.msg_count = total;
                member.msg_count_root = total_root;
                member.mention_count = 0;
                member.mention_count_root = 0;
                member.urgent_mention_count = 0;
            }
        }
        self.channels.refresh(&self.state, &self.avatars);
        self.refresh_title();
    }

    fn open_direct_message(self: &Rc<Self>, user_id: String) {
        let (client, me) = {
            let st = self.state.borrow();
            (st.client.clone(), st.me.id.clone())
        };
        let ui = self.clone();
        runtime::spawn(
            async move { client.create_direct_channel(&me, &user_id).await },
            move |result| match result {
                Ok(channel) => {
                    let id = channel.id.clone();
                    ui.state.borrow_mut().channels.insert(id.clone(), channel);
                    ui.channels.refresh(&ui.state, &ui.avatars);
                    ui.dispatch(Action::SelectChannel(id));
                }
                Err(e) => ui.toast_error("Could not open that conversation", &e),
            },
        );
    }

    // ---------------------------------------------------------------- threads

    fn open_thread(self: &Rc<Self>, root_id: String) {
        // Flush the previous thread's reply before the box is reused.
        self.save_thread_draft();
        self.right.set_mode(PanelMode::Thread(root_id.clone()));
        self.refresh_panel_mode();
        self.overlay.set_show_sidebar(true);
        self.restore_thread_draft();
        let following = {
            let st = self.state.borrow();
            st.thread_inbox
                .iter()
                .any(|t| t.id == root_id && t.is_following)
        };
        self.right.set_following(following);

        // Opening a thread is reading it, so its unread count should go —
        // the inbox badge only ever grew before.
        {
            let (client, team_id, unread) = {
                let st = self.state.borrow();
                let unread = st
                    .thread_inbox
                    .iter()
                    .any(|t| t.id == root_id && t.unread_replies > 0);
                (
                    st.client.clone(),
                    st.current_team.clone().unwrap_or_default(),
                    unread,
                )
            };
            if unread && !team_id.is_empty() {
                let id = root_id.clone();
                let ui = self.clone();
                let now = glib::real_time() / 1000;
                runtime::spawn(
                    async move { client.mark_thread_read(&team_id, &id, now).await },
                    move |result| {
                        if result.is_ok() {
                            ui.load_inbox();
                        }
                    },
                );
            }
        }

        // Seed the panel from the root we already hold, so it draws the
        // message immediately and fills in the replies when they arrive.
        // Waiting for the round trip is what made opening a thread feel slow
        // and, on a slow link, look like nothing had happened.
        {
            let mut st = self.state.borrow_mut();
            if !st.threads.contains_key(&root_id) {
                if let Some(root) = st.post(&root_id) {
                    st.threads
                        .insert(root_id.clone(), ChannelFeed::from_posts(vec![root]));
                }
            }
        }
        self.refresh_thread_panel();

        // The demo already carries the complete thread. Do not ask the fake
        // demo endpoint for it: an expected network failure would cover the
        // UI with a toast and make a state screenshot misleading.
        if std::env::var_os("MATRAS_DEMO").is_some() {
            return;
        }

        // Always refetch. A thread we opened earlier may have grown, and the
        // root's reply count is not enough to tell which replies we hold.
        let (client, crt) = {
            let st = self.state.borrow();
            (st.client.clone(), st.crt_enabled)
        };
        let id = root_id.clone();
        let ui = self.clone();
        runtime::spawn(
            async move {
                let list = client.post_thread(&id, crt).await?;
                let (authors, statuses) = hydrate_authors(&client, &list).await;
                Ok::<_, mattermost_api::Error>((list, authors, statuses))
            },
            move |result| match result {
                Ok((list, authors, statuses)) => {
                    {
                        let mut st = ui.state.borrow_mut();
                        for user in authors {
                            st.users.insert(user.id.clone(), user);
                        }
                        st.apply_statuses(statuses);
                        st.threads
                            .insert(root_id.clone(), ChannelFeed::from_list(&list));
                    }
                    // Both panes here: a fetched thread can change the reply
                    // footer in the feed behind it. Off the click's critical
                    // path, so the cost does not show.
                    ui.refresh_messages();
                    // A slow fetch can lose the race to a click elsewhere —
                    // only steal focus into the reply box if the panel is
                    // still showing the thread this answer is for.
                    if ui.right.mode() == PanelMode::Thread(root_id) {
                        ui.right.focus_composer();
                    }
                }
                Err(e) => {
                    ui.toast_error("Could not load the thread", &e);
                    // Stop the panel claiming it is still loading. Whatever we
                    // hold of the root is better than a spinner that never
                    // resolves.
                    let root = ui.state.borrow().find_post(&root_id).cloned();
                    if let Some(root) = root {
                        ui.state.borrow_mut().threads.insert(
                            root_id,
                            ChannelFeed {
                                posts: vec![root],
                                ..Default::default()
                            },
                        );
                    }
                    ui.refresh_messages();
                }
            },
        );
    }

    fn open_inbox(self: &Rc<Self>) {
        if inbox_requires_chat_navigation(self.split.is_collapsed(), self.split.shows_content()) {
            self.split.set_show_content(true);
        }
        self.right.set_mode(PanelMode::Inbox);
        self.refresh_panel_mode();
        self.overlay.set_show_sidebar(true);

        // Land on whichever tab has something to show: unread threads with no
        // mentions would otherwise open onto an empty list.
        let st = self.state.borrow();
        let threads_first = st.mentions.is_empty() && st.unread_threads() > 0;
        drop(st);
        if threads_first {
            self.right.show_threads_tab();
        }

        self.refresh_messages();
        if std::env::var_os("MATRAS_DEMO").is_none() {
            self.load_inbox();
        }
    }

    /// Fetches recent mentions and the thread inbox for the current team.
    fn load_inbox(self: &Rc<Self>) {
        let (client, team_id, username, crt, me) = {
            let st = self.state.borrow();
            let Some(team) = st.current_team.clone() else {
                return;
            };
            (
                st.client.clone(),
                team,
                st.me.username.clone(),
                st.crt_enabled,
                st.me.id.clone(),
            )
        };

        let ui = self.clone();
        runtime::spawn(
            async move {
                // Mattermost's "Recent Mentions" is literally a search for your
                // mention keys; @username is the one every account has.
                let mentions = client
                    .search_posts(&team_id, &format!("@{username}"), false)
                    .await
                    .ok();
                let threads = if crt {
                    client.my_threads(&team_id, false, INBOX_PAGE).await.ok()
                } else {
                    None
                };

                // Saved posts are a preference list of ids; the posts
                // themselves have to be fetched, or the saved tab can only
                // show whatever a channel happened to load.
                let saved = client.flagged_posts(&me, INBOX_PAGE).await.ok();

                let mut ids: HashSet<String> = HashSet::new();
                if let Some(s) = &saved {
                    ids.extend(s.posts.values().map(|p| p.user_id.clone()));
                }
                if let Some(m) = &mentions {
                    ids.extend(m.posts.posts.values().map(|p| p.user_id.clone()));
                }
                if let Some(t) = &threads {
                    ids.extend(t.threads.iter().map(|t| t.post.user_id.clone()));
                }
                let ids: Vec<String> = ids.into_iter().filter(|i| !i.is_empty()).collect();
                let authors = if ids.is_empty() {
                    Vec::new()
                } else {
                    client.users_by_ids(&ids).await.unwrap_or_default()
                };
                (mentions, threads, saved, authors)
            },
            move |(mentions, threads, saved, authors)| {
                {
                    let mut st = ui.state.borrow_mut();
                    for user in authors {
                        st.users.insert(user.id.clone(), user);
                    }
                    if let Some(results) = mentions {
                        st.mentions = results.posts.ordered().cloned().collect();
                    }
                    if let Some(threads) = threads {
                        st.thread_inbox = threads.threads;
                    }
                    // Filed under their own channels, so the saved tab and the
                    // conversation agree about what a post says.
                    if let Some(saved) = saved {
                        for post in saved.posts.values() {
                            st.apply_post(post.clone());
                        }
                    }
                }
                ui.refresh_messages();
            },
        );
    }

    // ---------------------------------------------------------------- posting

    fn send_message(self: &Rc<Self>, text: String, root_id: Option<String>) {
        let (client, channel_id, me, file_ids) = {
            let mut st = self.state.borrow_mut();
            let channel = match &root_id {
                // A reply belongs to the root's channel, which is not
                // necessarily the one on screen.
                Some(root) => st
                    .find_post(root)
                    .map(|p| p.channel_id.clone())
                    .or_else(|| st.current_channel.clone()),
                None => st.current_channel.clone(),
            };
            let Some(id) = channel else { return };
            // Attachments leave the queue with the message they go out on.
            let files: Vec<String> = st.pending_files.drain(..).map(|(id, _)| id).collect();
            (st.client.clone(), id, st.me.id.clone(), files)
        };
        // An empty message with nothing attached is not a message.
        if text.trim().is_empty() && file_ids.is_empty() {
            return;
        }
        // A slash command is an instruction to the server, not a message. It
        // was being posted as literal text, which is how "/away" ended up in
        // channels as a joke about the client.
        if text.starts_with('/') && !text.starts_with("//") && file_ids.is_empty() {
            self.run_command(channel_id, text);
            return;
        }
        let priority = self.chat.priority();
        self.chat.reset_priority();
        self.refresh_attachments();

        // Show it immediately. The websocket echo replaces this copy — matched
        // on `pending_post_id` — so a slow round trip never looks like a
        // dropped message.
        let pending_id = format!("pending{}", glib::monotonic_time());
        let optimistic = Post {
            id: pending_id.clone(),
            pending_post_id: pending_id.clone(),
            channel_id: channel_id.clone(),
            user_id: me,
            message: text.clone(),
            root_id: root_id.clone().unwrap_or_default(),
            create_at: glib::real_time() / 1000,
            file_ids: file_ids.clone(),
            ..Default::default()
        };
        self.state.borrow_mut().apply_post(optimistic.clone());
        self.refresh_messages();
        // Both send paths — the channel composer and the thread panel's —
        // come through here, so the rule that your own message brings you to
        // the bottom lives here rather than at either call site.
        self.chat.follow_own_post(&optimistic, &self.state);
        self.right.follow_own_reply(&optimistic);

        let ui = self.clone();
        let reply_to = root_id.clone();
        let placeholder_id = pending_id.clone();
        runtime::spawn(
            async move {
                let mut post = Post {
                    channel_id,
                    message: text,
                    root_id: reply_to.unwrap_or_default(),
                    pending_post_id: pending_id,
                    file_ids,
                    ..Default::default()
                };
                if !priority.is_empty() {
                    // Priority travels in the post's metadata, which the
                    // server reads on create and echoes back on the result.
                    post.metadata.get_or_insert_with(Default::default).priority =
                        Some(mattermost_api::models::PostPriority {
                            priority: Some(priority),
                            requested_ack: None,
                            persistent_notifications: None,
                        });
                }
                client.create_post(&post).await
            },
            move |result| match result {
                Ok(post) => {
                    {
                        let mut st = ui.state.borrow_mut();
                        // Retire the optimistic copy explicitly rather than
                        // trusting the response to carry `pending_post_id`
                        // back — otherwise a server that drops it leaves the
                        // message on screen twice.
                        st.remove_post(&placeholder_id);
                        st.apply_post(post);
                    }
                    ui.refresh_messages();
                }
                Err(e) => {
                    ui.toast_error("Message not sent", &e);
                    // Take the optimistic copy back down: leaving it there
                    // would claim the message was sent.
                    ui.state.borrow_mut().remove_post(&placeholder_id);
                    ui.refresh_messages();
                }
            },
        );
    }

    fn toggle_reaction(self: &Rc<Self>, post_id: String, emoji: String) {
        let (client, me, currently_mine) = {
            let st = self.state.borrow();
            (
                st.client.clone(),
                st.me.id.clone(),
                st.has_my_reaction(&post_id, &emoji),
            )
        };

        // Optimistic, like the send path: a reaction is the one interaction
        // where a round trip is very visible.
        let reaction = Reaction {
            user_id: me.clone(),
            post_id: post_id.clone(),
            emoji_name: emoji.clone(),
            ..Default::default()
        };
        self.state
            .borrow_mut()
            .apply_reaction(&reaction, !currently_mine);
        self.refresh_messages();

        let ui = self.clone();
        let request_post = post_id.clone();
        let request_emoji = emoji.clone();
        runtime::spawn(
            async move {
                if currently_mine {
                    client
                        .remove_reaction(&me, &request_post, &request_emoji)
                        .await
                } else {
                    client
                        .add_reaction(&me, &request_post, &request_emoji)
                        .await
                        .map(|_| ())
                }
            },
            move |result| {
                if let Err(e) = result {
                    ui.toast_error("Reaction failed", &e);
                    // Put it back the way it was.
                    ui.state
                        .borrow_mut()
                        .apply_reaction(&reaction, currently_mine);
                    ui.refresh_messages();
                }
            },
        );
    }

    /// The call button: join the current channel's call, or hang up.
    fn toggle_call(self: &Rc<Self>) {
        let st = self.state.borrow();
        if let Some(call) = &st.call {
            let session = call.session.clone();
            drop(st);
            // Dropping `ActiveCall` stops the microphone; the SFU is told
            // separately, and either way we are out of the call.
            self.close_videos();
            self.state.borrow_mut().call = None;
            self.refresh_call_ui();
            runtime::spawn(async move { session.leave().await }, |_| {});
            return;
        }
        let Some(channel_id) = st.current_channel.clone() else {
            return;
        };
        let (Some(discovery), Some(ws)) = (st.calls.clone(), st.ws.clone()) else {
            drop(st);
            self.toast("Calls are not enabled on this server");
            return;
        };
        let client = st.client.clone();
        drop(st);

        let ui = self.clone();
        tracing::info!(%channel_id, "joining the call");
        runtime::spawn(
            async move {
                let opts = JoinOptions {
                    channel_id,
                    ..Default::default()
                };
                CallSession::join(&client, ws, opts, &discovery).await
            },
            move |result| match result {
                Ok(session) => {
                    tracing::info!("join returned a session; starting audio");
                    ui.call_joined(session)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not join the call");
                    ui.toast_error("Could not join the call", &e)
                }
            },
        );
    }

    /// Starts audio for a freshly joined call and subscribes to its updates.
    fn call_joined(self: &Rc<Self>, session: Arc<CallSession>) {
        // A late safety net if the up-front init at window map never ran:
        // ndk_context must be set before cpal touches it, and the mic must be
        // granted before the capture stream opens.
        #[cfg(target_os = "android")]
        if let Some(surface) = self.window.surface() {
            android_context::ensure(&surface);
            android_permissions::ensure_record_audio(&surface);
        }
        let audio = match AudioIo::start(session.clone()) {
            Ok(audio) => {
                tracing::info!("audio started; the call is live");
                audio
            }
            Err(e) => {
                tracing::warn!(error = %e, "audio failed to start; leaving the call");
                self.toast(&format!("No audio for this call: {e}"));
                runtime::spawn(async move { session.leave().await }, |_| {});
                return;
            }
        };
        self.state.borrow_mut().call = Some(ActiveCall {
            channel_id: session.channel_id().to_string(),
            recording: false,
            roster: HashMap::new(),
            speaking: Vec::new(),
            sharing: Vec::new(),
            host_id: String::new(),
            sessions: HashMap::new(),
            muted_users: HashSet::new(),
            hands: Vec::new(),
            screen: None,
            camera: None,
            audio,
            // Mattermost clients join muted, and so does the session itself.
            muted: session.is_muted(),
            session: session.clone(),
        });

        let updates = session.subscribe();
        runtime::spawn_stream(
            move |tx| async move {
                let mut rx = updates;
                loop {
                    match rx.recv().await {
                        Ok(update) => {
                            if tx.send(update).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            },
            {
                let ui = self.clone();
                move |update| ui.apply_call_update(update)
            },
        );

        // The roster arrived before that subscription existed.
        if let Some(state) = session.last_state() {
            self.apply_call_update(CallUpdate::State(state));
        }
        self.refresh_call_ui();
    }

    fn toggle_mute(self: &Rc<Self>) {
        let st = self.state.borrow();
        let Some(call) = &st.call else {
            return;
        };
        let session = call.session.clone();
        let muted = call.muted;
        drop(st);

        let ui = self.clone();
        if muted {
            runtime::spawn(
                async move { session.unmute().await },
                move |result| match result {
                    // The capture loop discards frames until it has this track.
                    Ok(track) => {
                        if let Some(call) = ui.state.borrow().call.as_ref() {
                            call.audio.set_track(track);
                        }
                    }
                    Err(e) => ui.toast_error("Could not unmute", &e),
                },
            );
        } else {
            runtime::spawn(async move { session.mute().await }, move |result| {
                if let Err(e) = result {
                    ui.toast_error("Could not mute", &e);
                }
            });
        }
    }

    fn toggle_recording(self: &Rc<Self>) {
        let st = self.state.borrow();
        let Some(call) = &st.call else {
            return;
        };
        let (client, channel_id, on) =
            (st.client.clone(), call.channel_id.clone(), !call.recording);
        drop(st);

        let ui = self.clone();
        runtime::spawn(
            async move { mattermost_calls::set_recording(&client, &channel_id, on).await },
            move |result| {
                if let Err(e) = result {
                    // Only the host may record, and only when the server
                    // allows it at all; both come back as a plain refusal.
                    ui.toast_error("Could not change the recording", &e);
                }
            },
        );
    }

    fn toggle_screen(self: &Rc<Self>) {
        let st = self.state.borrow();
        let Some(call) = &st.call else {
            return;
        };
        let session = call.session.clone();
        if call.screen.is_some() {
            drop(st);
            if let Some(call) = self.state.borrow_mut().call.as_mut() {
                // Dropping the sender stops the encoder and the portal capture.
                call.screen = None;
            }
            self.refresh_call_ui();
            runtime::spawn(async move { session.stop_screen_share().await }, |_| {});
            return;
        }
        drop(st);

        let ui = self.clone();
        runtime::spawn(
            async move {
                // The portal picker comes first: there is no point announcing a
                // share the user is about to cancel.
                let (node_id, fd) = video::pick_screen().await?;
                let track = session
                    .start_screen_share()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>((track, node_id, fd))
            },
            move |result| {
                let sender = result.and_then(|(track, node_id, fd)| {
                    video::VideoSender::screen(track, node_id, fd)
                });
                match sender {
                    Ok(sender) => {
                        if let Some(call) = ui.state.borrow_mut().call.as_mut() {
                            call.screen = Some(sender);
                        }
                        ui.refresh_call_ui();
                    }
                    Err(e) => ui.toast(&format!("Could not share the screen: {e}")),
                }
            },
        );
    }

    fn toggle_camera(self: &Rc<Self>) {
        let st = self.state.borrow();
        let Some(call) = &st.call else {
            return;
        };
        let session = call.session.clone();
        if call.camera.is_some() {
            drop(st);
            if let Some(call) = self.state.borrow_mut().call.as_mut() {
                call.camera = None;
            }
            self.refresh_call_ui();
            runtime::spawn(async move { session.stop_video().await }, |_| {});
            return;
        }
        drop(st);

        let ui = self.clone();
        runtime::spawn(async move { session.start_video().await }, move |result| {
            let sender = result
                .map_err(|e| e.to_string())
                .and_then(video::VideoSender::camera);
            match sender {
                Ok(sender) => {
                    if let Some(call) = ui.state.borrow_mut().call.as_mut() {
                        call.camera = Some(sender);
                    }
                    ui.refresh_call_ui();
                }
                // The server allows camera only on DM channels, and only
                // when EnableVideo is on.
                Err(e) => ui.toast(&format!("Could not start the camera: {e}")),
            }
        });
    }

    /// Removes one remote picture, if it is on screen.
    fn drop_video(&self, session_id: &str, kind: &str) {
        self.video_views
            .borrow_mut()
            .remove(&format!("{session_id}:{kind}"));
        self.videos.set_visible(self.videos.first_child().is_some());
    }

    fn close_videos(&self) {
        self.video_views.borrow_mut().clear();
        self.videos.set_visible(false);
    }

    /// Who a media session belongs to, as far as the roster knows.
    /// A person's display name, or something honest when we do not have them.
    fn user_name(&self, user_id: &str) -> String {
        let st = self.state.borrow();
        st.users
            .get(user_id)
            .map(|user| st.display_name(user))
            .unwrap_or_else(|| "Someone".to_string())
    }

    fn speaker_name(&self, session_id: &str) -> String {
        let st = self.state.borrow();
        st.call
            .as_ref()
            .and_then(|call| call.roster.get(session_id))
            .and_then(|user_id| st.users.get(user_id))
            .map(|user| st.display_name(user))
            .unwrap_or_else(|| "Someone".to_string())
    }

    fn apply_call_update(self: &Rc<Self>, update: CallUpdate) {
        if self.state.borrow().call.is_none() {
            // We hung up; whatever is still arriving belongs to a call that is
            // no longer ours.
            return;
        }
        match update {
            CallUpdate::RemoteTrack {
                session_id,
                track_type,
                track,
            } => {
                use mattermost_calls::protocol::track_type as tt;
                match track_type.as_str() {
                    // Screen audio is audio like any other; it just happens to
                    // come from a share rather than a microphone.
                    tt::VOICE | tt::SCREEN_AUDIO => {
                        if let Some(call) = self.state.borrow().call.as_ref() {
                            call.audio.play(session_id, track);
                        }
                    }
                    tt::SCREEN | tt::VIDEO => {
                        tracing::info!(%session_id, track = %track_type, "a remote video arrived");
                        let who = self.speaker_name(&session_id);
                        let title = if track_type == tt::SCREEN {
                            format!("{who} is sharing a screen")
                        } else {
                            format!("{who} — camera")
                        };
                        match video::show_remote(&self.videos, &title, track) {
                            Ok(view) => {
                                self.video_views
                                    .borrow_mut()
                                    .insert(format!("{session_id}:{track_type}"), view);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "could not show a remote video");
                                self.toast(&format!("Could not show the video: {e}"))
                            }
                        }
                    }
                    other => tracing::debug!(track = other, "ignoring a remote track"),
                }
            }
            // Voice activity and screen shares are what the dock reports, and
            // both arrive per session; the roster turns those into people.
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserSpeaking {
                user_id,
                speaking,
                ..
            }) => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.speaking.retain(|id| id != &user_id);
                    if speaking {
                        call.speaking.insert(0, user_id);
                    }
                }
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserScreenShare {
                user_id,
                session_id,
                sharing,
            }) => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.sharing.retain(|id| id != &user_id);
                    if sharing {
                        call.sharing.push(user_id);
                    }
                }
                // A share ending is also the cue to tear its picture down; the
                // track itself may never close.
                if !sharing {
                    self.drop_video(&session_id, mattermost_calls::protocol::track_type::SCREEN);
                }
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserDismissedNotification {
                user_id,
                ..
            }) => {
                // Answered somewhere else: take the doorbell down here too.
                if user_id == self.state.borrow().me.id {
                    if let (Some(app), Some(channel)) = (
                        self.window.application(),
                        self.state.borrow().current_channel.clone(),
                    ) {
                        app.withdraw_notification(&format!("call-{channel}"));
                    }
                }
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::Caption {
                user_id,
                text,
                ..
            }) => {
                let who = self.user_name(&user_id);
                self.dock.set_caption(&format!("{who}:"), &text);
            }
            // Host controls are advisory: the server asks, and the client is
            // what actually mutes or stops sharing. Ignoring them meant a host
            // muting someone did nothing at all on their machine.
            CallUpdate::Participant(mattermost_calls::CallsEvent::HostMuteRequest { .. }) => {
                let muted = self.state.borrow().call.as_ref().is_some_and(|c| c.muted);
                if !muted {
                    self.toggle_mute();
                    self.toast("The host muted you");
                }
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::HostScreenOffRequest {
                ..
            }) => {
                if self
                    .state
                    .borrow()
                    .call
                    .as_ref()
                    .is_some_and(|c| c.screen.is_some())
                {
                    self.toggle_screen();
                    self.toast("The host stopped your screen share");
                }
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::HostLowerHandRequest {
                ..
            }) => {
                let raised = {
                    let st = self.state.borrow();
                    st.call
                        .as_ref()
                        .is_some_and(|c| c.hands.iter().any(|id| id == &st.me.id))
                };
                if raised {
                    self.toggle_hand();
                    self.toast("The host lowered your hand");
                }
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::HostRemoved { .. }) => {
                self.toast("The host removed you from the call");
                self.toggle_call();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::HostChanged {
                host_id, ..
            }) => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.host_id = host_id;
                }
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserMuted {
                user_id,
                muted,
                ..
            }) => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    if muted {
                        call.muted_users.insert(user_id);
                    } else {
                        call.muted_users.remove(&user_id);
                    }
                }
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserRaisedHand {
                user_id,
                raised_at,
                ..
            }) => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.hands.retain(|id| id != &user_id);
                    // Zero means the hand went back down.
                    if raised_at > 0 {
                        // Appended, not prepended: the queue is who asked
                        // first.
                        call.hands.push(user_id.clone());
                    }
                }
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserReacted {
                user_id,
                reaction,
                ..
            }) => {
                // In the dock rather than as a toast: a reaction is about the
                // call, and it belongs where the call is. It clears itself,
                // because a reaction is a moment and not a state.
                let who = self.user_name(&user_id);
                let glyph = if reaction.literal.is_empty() {
                    crate::emoji::label(&reaction.name)
                } else {
                    reaction.literal.clone()
                };
                self.dock.set_caption(&who, &glyph);
                let ui = self.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                    ui.dock.set_caption("", "");
                });
            }
            CallUpdate::MuteChanged { muted } => {
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.muted = muted;
                }
                self.refresh_call_ui();
            }
            CallUpdate::State(state) => {
                let mut st = self.state.borrow_mut();
                if let Some(call) = st.call.as_mut() {
                    call.recording = is_running(state.recording.as_ref());
                    call.host_id = state.host_id.clone();
                    call.sessions = state
                        .sessions
                        .iter()
                        .map(|s| (s.user_id.clone(), s.session_id.clone()))
                        .collect();
                    call.muted_users = state
                        .sessions
                        .iter()
                        .filter(|s| !s.unmuted)
                        .map(|s| s.user_id.clone())
                        .collect();
                    call.hands = state
                        .sessions
                        .iter()
                        .filter(|s| s.raised_hand > 0)
                        .map(|s| (s.raised_hand, s.user_id.clone()))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|(_, id)| id)
                        .collect();
                    call.roster = state
                        .sessions
                        .iter()
                        .map(|s| (s.session_id.clone(), s.user_id.clone()))
                        .collect();
                    let channel = call.channel_id.clone();
                    st.active_calls.insert(channel, participants(&state));
                }
                drop(st);
                self.refresh_call_ui();
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserVideo {
                session_id,
                on: false,
                ..
            }) => self.drop_video(&session_id, mattermost_calls::protocol::track_type::VIDEO),
            CallUpdate::Participant(mattermost_calls::CallsEvent::UserLeft {
                session_id, ..
            }) => {
                use mattermost_calls::protocol::track_type as tt;
                self.drop_video(&session_id, tt::SCREEN);
                self.drop_video(&session_id, tt::VIDEO);
            }
            CallUpdate::Participant(mattermost_calls::CallsEvent::JobState { state, .. }) => {
                if state.job_type != "recording" {
                    return;
                }
                if let Some(call) = self.state.borrow_mut().call.as_mut() {
                    call.recording = is_running(Some(&state));
                }
                self.refresh_call_ui();
            }
            CallUpdate::Connected(false) | CallUpdate::Ended => {
                self.close_videos();
                self.state.borrow_mut().call = None;
                self.refresh_call_ui();
                self.toast("The call ended");
            }
            CallUpdate::Error(e) => self.toast(&format!("Call error: {e}")),
            _ => {}
        }
    }

    // ------------------------------------------------------------ ws handling

    fn apply_event(self: &Rc<Self>, event: Event) {
        // Calls traffic arrives as plugin-namespaced events and never overlaps
        // with the core event set, so it is cheapest to split it off first.
        if let Some(calls) = mattermost_calls::signaling::parse(&event) {
            self.apply_calls_event(calls);
            return;
        }
        if let Event::Other {
            event: name, data, ..
        } = &event
        {
            if let Some(kind) = name.strip_prefix(REACTION_NOTIFY_PREFIX) {
                self.apply_reaction_notice(kind, data);
                return;
            }
            // An LLM answer being written a token at a time.
            if name.strip_prefix(crate::agents::WS_PREFIX) == Some("postupdate") {
                self.apply_stream_update(data);
                return;
            }
        }

        // A whole-feed rebuild, for the events that really do change every
        // row — somebody's avatar, a presence dot, a channel switch.
        let mut redraw_messages = false;
        // The events that change exactly one post, which is nearly all of
        // them. One row is rebuilt rather than the several hundred behind it;
        // see `ChatView::append_post` and friends.
        let mut touched: Vec<PostChange> = Vec::new();
        let mut redraw_sidebar = false;
        let mut redraw_typing = false;
        let mut redraw_draft = false;
        let mut reload_sidebar = false;
        let mut reload_teams = false;
        let mut reload_inbox = false;
        let mut open_dialog: Option<mattermost_api::models::dialog::OpenDialogRequest> = None;
        let mut notice: Option<String> = None;
        let mut refetch_post: Option<String> = None;
        let mut forget_avatar: Option<String> = None;
        let mut notify_about: Option<mattermost_api::ws::Posted> = None;
        // Channels read on another device, whose notifications are stale now.
        let mut read_elsewhere: Vec<String> = Vec::new();
        // Reading a message as it lands is not something to be told about, but
        // only while the window is actually in front of the person.
        let focused_channel = self
            .window
            .is_active()
            .then(|| self.state.borrow().current_channel.clone())
            .flatten();

        {
            let mut st = self.state.borrow_mut();
            let me = st.me.id.clone();
            let crt = st.crt_enabled;

            match event {
                Event::Posted(posted) => {
                    let post = posted.post.clone();
                    let channel_id = post.channel_id.clone();
                    let is_mine = post.user_id == me;
                    let mentions_me = posted.mentions.contains(&me);
                    let viewing = st.current_channel.as_deref() == Some(channel_id.as_str());
                    let is_reply = post.is_reply();
                    let create_at = post.create_at;
                    let row_post = post.clone();
                    let root_id = post.thread_root().to_string();

                    st.apply_post(post.clone());

                    // The channel's own counter has to move too, or the unread
                    // maths (total − my count) goes negative on the next read.
                    if let Some(channel) = st.channels.get_mut(&channel_id) {
                        channel.total_msg_count += 1;
                        if !is_reply {
                            channel.total_msg_count_root += 1;
                        }
                        channel.last_post_at = create_at;
                    }
                    if let Some(member) = st.memberships.get_mut(&channel_id) {
                        if is_mine || viewing {
                            // Reading it counts as reading it.
                            member.msg_count += 1;
                            if !is_reply {
                                member.msg_count_root += 1;
                            }
                        } else if mentions_me {
                            member.mention_count += 1;
                            if !is_reply || !crt {
                                member.mention_count_root += 1;
                            }
                        }
                    }
                    if mentions_me && !is_mine {
                        st.mentions.insert(0, post);
                        st.mentions.truncate(INBOX_PAGE as usize);
                    }
                    if viewing {
                        // Under collapsed threads a reply never enters the
                        // feed; what changes there is the root's reply
                        // footer, and only if the root is on screen at all.
                        touched.extend(if is_reply && crt {
                            st.post(&root_id).map(PostChange::Changed)
                        } else {
                            Some(PostChange::Added(row_post))
                        });
                    }
                    redraw_sidebar = true;
                    // Decided here, raised below: the decision needs the state
                    // borrow, the toast must not hold it.
                    if notify::should_notify(
                        &posted,
                        &st.me,
                        st.memberships.get(&channel_id),
                        focused_channel.as_deref(),
                    ) {
                        notify_about = Some(posted);
                    }
                }
                // Ephemeral posts are shown like any other, but the server
                // will never mention them again — no edit, no delete, and
                // they are gone on the next fetch. That is the intent.
                // A plugin or slash command asking for a form. Shown outside
                // the state borrow, since it needs the window.
                Event::OpenDialog(request) => open_dialog = Some(*request),
                Event::TeamsChanged => reload_teams = true,
                // The directory is refetched lazily: everyone on screen is
                // already held, and a deactivated user's posts do not vanish.
                Event::UsersChanged | Event::EmojiChanged => {}
                Event::Notice { message } => notice = Some(message),
                Event::ThreadsChanged => reload_inbox = true,
                // Nothing on screen depends on these continuously; they matter
                // when one of those windows is open, and it refills on open.
                Event::ListsChanged => {}
                Event::AcknowledgementChanged { post_id } => refetch_post = Some(post_id),
                Event::EphemeralMessage(post) => {
                    touched.push(PostChange::Added((*post).clone()));
                    st.apply_post(*post);
                }
                Event::PostEdited(post) => {
                    touched.push(PostChange::Changed(post.clone()));
                    st.apply_post(post);
                }
                Event::PostDeleted(post) => {
                    if let Some(feed) = st.feeds.get_mut(&post.channel_id) {
                        feed.remove(&post.id);
                    }
                    let root = post.thread_root().to_string();
                    if let Some(thread) = st.threads.get_mut(&root) {
                        thread.remove(&post.id);
                    }
                    touched.push(PostChange::Removed(post.id.clone()));
                }
                Event::ReactionAdded(reaction) => {
                    st.apply_reaction(&reaction, true);
                    touched.extend(st.post(&reaction.post_id).map(PostChange::Changed));
                }
                Event::ReactionRemoved(reaction) => {
                    st.apply_reaction(&reaction, false);
                    touched.extend(st.post(&reaction.post_id).map(PostChange::Changed));
                }
                Event::UserUpdated(user) => {
                    // A new picture means the cached texture is stale.
                    let changed = st
                        .users
                        .get(&user.id)
                        .map(|old| old.last_picture_update != user.last_picture_update)
                        .unwrap_or(true);
                    if changed {
                        forget_avatar = Some(user.id.clone());
                    }
                    st.users.insert(user.id.clone(), *user);
                    redraw_messages = true;
                }
                Event::StatusChange { user_id, status } => {
                    let presence = Presence::from(status.as_str());
                    // Only redraw when the dot would actually change colour —
                    // the server repeats a user's current status often enough
                    // that rebuilding the feed each time would be noticeable.
                    // Unknown users render as offline already, hence the
                    // default rather than an Option compare.
                    if st.statuses.insert(user_id, presence).unwrap_or_default() != presence {
                        redraw_messages = true;
                        // DM rows and the account button carry the same dot.
                        redraw_sidebar = true;
                    }
                }
                // Another session marked a channel unread; mirror the counters
                // it reported rather than guessing them.
                Event::PostUnread {
                    channel_id,
                    msg_count,
                    mention_count,
                    ..
                } => {
                    let total = st
                        .channels
                        .get(&channel_id)
                        .map(|c| c.total_msg_count)
                        .unwrap_or(msg_count);
                    if let Some(member) = st.memberships.get_mut(&channel_id) {
                        member.msg_count = total - msg_count;
                        member.msg_count_root = member.msg_count;
                        member.mention_count = mention_count;
                        member.mention_count_root = mention_count;
                    }
                    redraw_sidebar = true;
                }
                Event::Typing {
                    channel_id,
                    user_id,
                    ..
                } => {
                    if user_id != me {
                        st.typing_started(channel_id, user_id);
                        redraw_typing = true;
                    }
                }
                Event::ChannelsViewed { channel_times } => {
                    // Another session read something; mirror it.
                    read_elsewhere.extend(channel_times.keys().cloned());
                    for (channel_id, at) in channel_times {
                        let totals = st
                            .channels
                            .get(&channel_id)
                            .map(|c| (c.total_msg_count, c.total_msg_count_root));
                        if let (Some((total, root)), Some(member)) =
                            (totals, st.memberships.get_mut(&channel_id))
                        {
                            member.last_viewed_at = Some(at);
                            member.msg_count = total;
                            member.msg_count_root = root;
                            member.mention_count = 0;
                            member.mention_count_root = 0;
                        }
                    }
                    redraw_sidebar = true;
                }
                // Everything that can reshape the channel list. Nine events,
                // one answer: ask the server for the list again. A delta per
                // event would be nine chances to drift out of sync with it,
                // and these arrive rarely enough that three requests is
                // cheaper than being wrong.
                // A draft written on another device. Our own writes carry a
                // Connection-Id, so the server never echoes these back to us.
                Event::DraftCreated(draft) => {
                    if draft.root_id.is_empty() {
                        st.drafts.insert(draft.channel_id, draft.message);
                    } else {
                        st.thread_drafts.insert(draft.root_id, draft.message);
                    }
                    redraw_draft = true;
                }
                Event::DraftDeleted(draft) => {
                    if draft.root_id.is_empty() {
                        st.drafts.remove(&draft.channel_id);
                    } else {
                        st.thread_drafts.remove(&draft.root_id);
                    }
                    redraw_draft = true;
                }
                Event::ChannelCreated { .. }
                | Event::ChannelUpdated { .. }
                | Event::ChannelDeleted { .. }
                | Event::ChannelMemberUpdated { .. }
                | Event::DirectAdded { .. }
                | Event::PreferencesChanged(_)
                | Event::SidebarCategoriesInvalidated { .. } => reload_sidebar = true,
                // Someone joining a channel only matters to the list when the
                // someone is us.
                Event::UserAdded { user_id, .. } | Event::UserRemoved { user_id, .. }
                    if user_id == me =>
                {
                    reload_sidebar = true
                }
                Event::AddedToTeam { user_id, .. } | Event::LeaveTeam { user_id, .. }
                    if user_id == me =>
                {
                    reload_teams = true
                }
                _ => {}
            }
        }

        if let Some(posted) = notify_about {
            let title = if posted.channel_display_name.is_empty() {
                posted.sender_name.clone()
            } else {
                format!("{} — {}", posted.sender_name, posted.channel_display_name)
            };
            if let Some(app) = self.window.application() {
                notify::show(
                    &app,
                    &posted.post.channel_id,
                    title.trim_start_matches(" — "),
                    &notify::body(&posted),
                    self.notification_sound(),
                );
            }
        }
        if let Some(app) = self.window.application() {
            for channel_id in &read_elsewhere {
                notify::withdraw(&app, channel_id);
            }
        }
        if reload_teams {
            self.reload_teams();
        }
        if reload_inbox {
            self.load_inbox();
        }
        if let Some(request) = open_dialog {
            self.open_dialog(request);
        }
        if let Some(message) = notice.filter(|m| !m.is_empty()) {
            self.toast(&message);
        }
        if let Some(post_id) = refetch_post {
            // The event says which post changed but not to what, and
            // acknowledgements live in the post's metadata.
            let client = self.state.borrow().client.clone();
            let ui = self.clone();
            runtime::spawn(async move { client.post(&post_id).await }, move |result| {
                if let Ok(post) = result {
                    ui.state.borrow_mut().apply_post(post);
                    ui.refresh_messages();
                }
            });
        }
        if reload_sidebar {
            self.schedule_sidebar_reload();
        }
        if redraw_typing {
            self.refresh_typing();
        }
        if redraw_draft {
            self.restore_draft();
            self.restore_thread_draft();
            redraw_sidebar = true;
        }

        if let Some(user_id) = forget_avatar {
            self.avatars.forget(&user_id);
        }
        // Row by row where the row is on screen. `false` means it is not —
        // history not paged in this far, or the panel showing something else
        // — and then there is nothing for it but the full rebuild.
        if !touched.is_empty() && !redraw_messages {
            let actions = self.message_actions();
            for change in touched {
                let done = match change {
                    PostChange::Added(post) => {
                        self.chat
                            .append_post(&post, &self.state, &self.avatars, &actions)
                    }
                    PostChange::Changed(post) => {
                        self.chat
                            .replace_post(&post, &self.state, &self.avatars, &actions)
                    }
                    PostChange::Removed(id) => {
                        self.chat.remove_post(&id);
                        true
                    }
                };
                redraw_messages |= !done;
            }
            // The thread panel holds a screenful, not a channel's history, so
            // it is still redrawn whole — it may be showing the same post.
            if !redraw_messages {
                self.right.refresh(&self.state, &self.avatars, &actions);
            }
        }
        if redraw_messages {
            self.refresh_messages();
        }
        if redraw_sidebar {
            self.channels.refresh(&self.state, &self.avatars);
            self.refresh_title();
        }
    }

    /// Keeps the sidebar's "call in progress" marker and the channel banner
    /// honest. Joining is a separate, explicit action.
    fn apply_calls_event(self: &Rc<Self>, event: mattermost_calls::CallsEvent) {
        use mattermost_calls::CallsEvent as Ev;
        let mut touched = true;
        let mut ring: Option<String> = None;
        let mut stop_ringing = false;
        {
            let mut st = self.state.borrow_mut();
            match event {
                Ev::CallStarted { channel_id, .. } => {
                    // Worth interrupting someone for only where a call is
                    // addressed to them: a DM, or a group they are in. A busy
                    // public channel starting calls all day is not a doorbell.
                    let direct = st.channel(&channel_id).is_some_and(|c| {
                        matches!(c.r#type, ChannelType::Direct | ChannelType::Group)
                    });
                    if direct && st.call.is_none() {
                        ring = Some(channel_id.clone());
                    }
                    st.active_calls.entry(channel_id).or_default();
                }
                Ev::CallEnded { channel_id } => {
                    st.active_calls.remove(&channel_id);
                    // Nothing left to answer.
                    stop_ringing = true;
                }
                // Answered or dismissed on another device. It has to be
                // handled here rather than in the joined-call path: while
                // being rung there is no call of ours, and that path returns
                // early when there is not.
                Ev::UserDismissedNotification { .. } => stop_ringing = true,
                // The server only sends a full roster to the joiner, so the
                // list has to follow the individual comings and goings too —
                // otherwise the banner keeps claiming a call we have left.
                Ev::UserJoined {
                    channel_id,
                    user_id,
                    ..
                } => {
                    let people = st.active_calls.entry(channel_id).or_default();
                    // One person can be in a call from two devices, which is
                    // two sessions but still one face.
                    if !people.contains(&user_id) {
                        people.push(user_id);
                    }
                }
                Ev::UserLeft {
                    channel_id,
                    user_id,
                    ..
                } => {
                    if let Some(people) = st.active_calls.get_mut(&channel_id) {
                        people.retain(|id| id != &user_id);
                        if people.is_empty() {
                            st.active_calls.remove(&channel_id);
                        }
                    }
                }
                Ev::CallState { channel_id, state } => {
                    let people = participants(&state);
                    if people.is_empty() {
                        st.active_calls.remove(&channel_id);
                    } else {
                        st.active_calls.insert(channel_id, people);
                    }
                }
                _ => touched = false,
            }
        }
        if stop_ringing {
            self.stop_ringing();
        }
        if let Some(channel_id) = ring {
            self.ring(&channel_id);
        }
        if touched {
            self.channels.refresh(&self.state, &self.avatars);
            self.refresh_call_ui();
        }
    }
}

/// The distinct people in a call. A roster is keyed by *session*, and one
/// person joining from two devices holds two of them.
fn participants(state: &mattermost_calls::protocol::CallState) -> Vec<String> {
    let mut people: Vec<String> = Vec::with_capacity(state.sessions.len());
    for session in &state.sessions {
        if !people.contains(&session.user_id) {
            people.push(session.user_id.clone());
        }
    }
    people
}

/// Whether a recording/transcription job is actually running right now.
fn is_running(job: Option<&mattermost_calls::protocol::JobState>) -> bool {
    job.is_some_and(|j| j.start_at > 0 && j.end_at == 0 && j.err.is_empty())
}

/// Post authors are not included in a feed response, so hydrate them before
/// rendering or every message reads "unknown". Presence comes along for the
/// ride: the same ids, and the avatars carry a status badge.
async fn hydrate_authors(
    client: &mattermost_api::Client,
    list: &PostList,
) -> (Vec<User>, Vec<Status>) {
    let ids: Vec<String> = list
        .posts
        .values()
        .map(|p| p.user_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return (Vec::new(), Vec::new());
    }
    (
        client.users_by_ids(&ids).await.unwrap_or_default(),
        client.statuses_by_ids(&ids).await.unwrap_or_default(),
    )
}

/// Runs the startup sequence and connects the websocket.
///
/// `on_auth_failure` is only `Some` for a restored session: one whose token
/// came out of the keyring untested (see `restore_session`, which no longer
/// spends a round trip on `client.me()` before drawing anything). If the boot
/// call below is the one that finds out the token is no good, this is what
/// sends the reader to the login form instead of leaving them looking at a
/// window that will never finish loading. A fresh login has no such fallback
/// to offer — its account was already proven live — so it passes `None`.
fn bootstrap(ui: Rc<Ui>, on_auth_failure: Option<Box<dyn FnOnce()>>) {
    let client = ui.state.borrow().client.clone();
    let pointer = crate::cache::load(client.site_url());
    // Whether last time's picture will be on screen if the server is not.
    let had_cache = pointer.is_some();

    // Where the reader was sitting in each channel, put back before anything
    // can open one. Both the store load below and the first `select_channel`
    // consult these, and either may land first, so they cannot wait on a disk
    // read the way they used to.
    if let Some(pointer) = &pointer {
        ui.state
            .borrow_mut()
            .scroll_anchors
            .clone_from(&pointer.scroll_anchors);
    }
    // The channel to open is the one that *was* open. Without this the startup
    // sequence picks a channel for a first-ever visit — town-square, or the
    // first one alphabetically — and the restore below then finds the seat
    // taken and gives up, so the snapshot was read, fetched from, and ignored.
    // `Bootstrap` re-validates both against current membership and falls back
    // on its own, so a team or channel we have since left still lands
    // somewhere sensible.
    let preferred_team = pointer.as_ref().and_then(|p| p.current_team.clone());
    let preferred_channel = pointer.as_ref().and_then(|p| p.current_channel.clone());

    // Draw last time's picture first. Everything here is replaced the moment
    // the real data lands; it is on screen so that launching the app shows
    // your channels instead of an empty window for the length of a round trip.
    {
        let server = client.site_url().to_string();
        let ui = ui.clone();
        runtime::spawn(
            async move {
                let store = crate::store::Store::open(&server).await.ok()?;
                let (channels, members) = store.channels().await.ok()?;
                let users = store.users().await.ok()?;
                Some((store, channels, members, users))
            },
            move |loaded| {
                let Some((store, channels, members, users)) = loaded else {
                    return;
                };
                *ui.store.borrow_mut() = Some(store.clone());
                {
                    let mut st = ui.state.borrow_mut();
                    for channel in channels {
                        st.channels.entry(channel.id.clone()).or_insert(channel);
                    }
                    for member in members {
                        st.memberships
                            .entry(member.channel_id.clone())
                            .or_insert(member);
                    }
                    for user in users {
                        st.users.entry(user.id.clone()).or_insert(user);
                    }
                    if let Some(pointer) = &pointer {
                        // Only if the live data has not already answered.
                        if st.current_team.is_none() {
                            st.current_team = pointer.current_team.clone();
                        }
                    }
                }
                ui.refresh_all();

                // And the messages for wherever we were, so the conversation
                // is there too rather than just the list around it.
                if let Some(channel_id) = pointer
                    .as_ref()
                    .and_then(|p| p.current_channel.clone())
                    .filter(|id| ui.state.borrow().current_channel.as_deref() != Some(id.as_str()))
                {
                    let anchor = pointer
                        .as_ref()
                        .and_then(|p| p.scroll_anchors.get(&channel_id).cloned());
                    let ui = ui.clone();
                    runtime::spawn(
                        async move {
                            let posts = if let Some(post_id) = anchor.as_deref() {
                                let around = store.posts_around(&channel_id, post_id, 10, 10).await;
                                match around {
                                    Ok(posts) if !posts.is_empty() => Ok(posts),
                                    Ok(_) => store.posts(&channel_id, INITIAL_POSTS as usize).await,
                                    Err(error) => Err(error),
                                }
                            } else {
                                store.posts(&channel_id, INITIAL_POSTS as usize).await
                            };
                            (channel_id, anchor, posts)
                        },
                        move |(channel_id, anchor, posts)| {
                            let Ok(posts) = posts else { return };
                            if posts.is_empty() {
                                return;
                            }
                            {
                                let mut st = ui.state.borrow_mut();
                                st.feeds
                                    .entry(channel_id.clone())
                                    .or_insert_with(|| ChannelFeed::from_posts(posts));
                                let current = st.current_channel.take();
                                st.current_channel =
                                    shortcuts::startup_channel(current, Some(channel_id));
                            }
                            ui.refresh_messages();
                            if let Some(anchor) = anchor {
                                ui.chat.restore_anchor(&anchor);
                            }
                        },
                    );
                }
            },
        );
    }

    runtime::spawn(
        async move {
            let boot = Bootstrap::run(
                &client,
                preferred_team.as_deref(),
                preferred_channel.as_deref(),
                0,
            )
            .await?;
            // Calls discovery is optional: the plugin may not be installed.
            let calls = mattermost_calls::discover(&client)
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "calls discovery failed"))
                .ok();
            let active = match &calls {
                Some(_) => mattermost_calls::config::all_channel_states(&client)
                    .await
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            Ok::<_, mattermost_api::Error>((boot, calls, active))
        },
        move |result| {
            let (boot, calls, active) = match result {
                Ok(v) => v,
                Err(e) => {
                    // A refused token is spent; anything else (server down,
                    // no network) leaves it alone so the next launch can
                    // retry — the same split `restore_session` used to make
                    // itself before this call was the first thing to ask.
                    match (on_auth_failure, &e) {
                        (Some(fallback), e) if !matches!(e, mattermost_api::Error::Http(_)) => {
                            tracing::info!("stored session not usable: {e}");
                            runtime::spawn(crate::session::clear_async(), |_| {});
                            crate::cache::clear();
                            fallback();
                        }
                        _ => ui.show_load_failure(&e, had_cache),
                    }
                    return;
                }
            };

            let ws_url;
            let token;
            let initial_channel;
            {
                let mut st = ui.state.borrow_mut();
                st.config = boot.config;
                st.crt_enabled = boot.crt_enabled;
                st.me = boot.me;
                st.teams = boot.teams;
                st.current_team = boot.initial_team.as_ref().map(|t| t.id.clone());
                for c in boot.channels {
                    st.channels.insert(c.id.clone(), c);
                }
                for m in boot.channel_members {
                    st.memberships.insert(m.channel_id.clone(), m);
                }
                st.categories = boot.categories;
                // Saved posts are preferences, and the startup sequence has
                // already fetched those — asking again would be a second
                // request for something we are holding.
                st.preferences = boot.preferences.clone();
                st.saved_posts = boot
                    .preferences
                    .iter()
                    .filter(|p| p.category == "flagged_post" && p.value == "true")
                    .map(|p| p.name.clone())
                    .collect();
                st.users_fetched_at = glib::real_time() / 1000;
                st.calls = calls;
                st.active_calls = active
                    .into_iter()
                    .filter_map(|c| {
                        let call = c.call?;
                        Some((c.channel_id, participants(&call)))
                    })
                    .collect();

                ws_url = st.client.websocket_url();
                token = st.client.token().unwrap_or_default();
                initial_channel = boot.initial_channel.map(|c| c.id);
            }

            ui.refresh_all();
            ui.load_inbox();
            ui.load_drafts();
            ui.load_bots();
            ui.load_team_unreads();
            ui.preload_unread();

            let initial_channel = shortcuts::startup_channel(
                ui.state.borrow().current_channel.clone(),
                initial_channel,
            );
            if let Some(id) = initial_channel {
                ui.dispatch(Action::SelectChannel(id));
            }

            // --- live updates
            // Inside the runtime guard: `connect` spawns its own task, and
            // calling that from a GTK callback panics with "no reactor running".
            let ws = runtime::with_runtime(|| WebSocket::connect(ws_url, token));
            ui.state.borrow_mut().ws = Some(ws.clone());

            poll_presence(&ui);

            let subscription = ws.subscribe();
            runtime::spawn_stream(
                move |tx| async move {
                    let mut rx = subscription;
                    loop {
                        match rx.recv().await {
                            Ok(update) => {
                                if tx.send(update).await.is_err() {
                                    break; // the window went away
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                },
                {
                    let ui = ui.clone();
                    move |update: WsUpdate| match update {
                        WsUpdate::Event(event) => ui.apply_event(event),
                        WsUpdate::MissedMessages => {
                            // The server could not replay its buffer, so
                            // anything could have changed while we were away.
                            ui.chat.set_connection_problem(None);
                            resync(&ui);
                        }
                        WsUpdate::Connected { connection_id, .. } => {
                            // Writes carry this from now on, so the server
                            // leaves us out of their echo.
                            ui.state.borrow().client.set_connection_id(connection_id);
                            ui.chat.set_connection_problem(None);
                        }
                        // Losing the socket is not an event that scrolls past:
                        // it stays true until it stops being true, so it is a
                        // banner, and it says whether it is being worked on.
                        WsUpdate::Disconnected { will_retry, reason } => {
                            ui.chat.set_connection_problem(Some(&if will_retry {
                                "Reconnecting…".to_string()
                            } else {
                                format!("Disconnected: {}", glib::markup_escape_text(&reason))
                            }));
                        }
                        _ => {}
                    }
                },
            );
        },
    );
}

/// How often presence is re-asked for; the webapp's `STATUS_INTERVAL`.
const PRESENCE_POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Keeps other people's presence dots honest.
///
/// The server publishes `status_change` to the affected user's own sessions
/// only (`PlatformService::BroadcastStatus` sets the event's user id), so
/// someone else going away, auto-away included, never reaches this socket.
/// Mattermost's own clients poll `/users/status/ids` once a minute for the
/// same reason; without this a long-running window keeps the green dot it saw
/// at startup. The first round runs straight away: nothing else ever asks
/// for my own status, so the account button would read offline until then.
fn poll_presence(ui: &Rc<Ui>) {
    refresh_presence(ui);
    let weak = Rc::downgrade(ui);
    glib::timeout_add_local(PRESENCE_POLL, move || {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        refresh_presence(&ui);
        glib::ControlFlow::Continue
    });
}

fn refresh_presence(ui: &Rc<Ui>) {
    let (client, ids) = {
        let st = ui.state.borrow();
        (st.client.clone(), st.presence_watch_ids())
    };
    if ids.is_empty() {
        return;
    }
    let ui = ui.clone();
    runtime::spawn(
        async move {
            let mut statuses = Vec::new();
            // Batched only to keep one request body modest; a long session
            // collects every author it has drawn.
            for chunk in ids.chunks(200) {
                statuses.extend(client.statuses_by_ids(chunk).await.unwrap_or_default());
            }
            statuses
        },
        move |statuses| {
            if ui.state.borrow_mut().apply_statuses(statuses) {
                ui.channels.refresh(&ui.state, &ui.avatars);
                ui.refresh_messages();
            }
        },
    );
}

/// The gap-fill after a failed websocket resume.
fn resync(ui: &Rc<Ui>) {
    let (client, team_id, crt, channel_id, since) = {
        let st = ui.state.borrow();
        let Some(team) = st.current_team.clone() else {
            return;
        };
        let channel = st.current_channel.clone();
        let since = channel
            .as_ref()
            .and_then(|id| st.feeds.get(id))
            .map(|f| f.last_fetched_at)
            .unwrap_or(0);
        (st.client.clone(), team, st.crt_enabled, channel, since)
    };

    let ui = ui.clone();
    runtime::spawn(
        async move {
            let channels = client.my_channels(&team_id, false, 0).await?;
            let members = client.my_channel_members(&team_id).await?;
            let missed = match (&channel_id, since) {
                (Some(id), since) if since > 0 => Some(client.posts_since(id, since, crt).await?),
                _ => None,
            };
            Ok::<_, mattermost_api::Error>((channels, members, channel_id, missed))
        },
        move |result| {
            let Ok((channels, members, channel_id, missed)) = result else {
                return;
            };
            {
                let mut st = ui.state.borrow_mut();
                for c in channels {
                    st.channels.insert(c.id.clone(), c);
                }
                for m in members {
                    st.memberships.insert(m.channel_id.clone(), m);
                }
                if let (Some(id), Some(list)) = (channel_id.clone(), missed) {
                    let posts: Vec<Post> = list.chronological().into_iter().cloned().collect();
                    for post in posts {
                        if post.is_deleted() {
                            if let Some(feed) = st.feeds.get_mut(&id) {
                                feed.remove(&post.id);
                            }
                        } else {
                            // `?since=` reports edits and deletes as well as
                            // new posts, so upsert rather than append.
                            st.apply_post(post);
                        }
                    }
                }
            }
            // Names, pictures and positions change while we are away, and
            // nothing else tells us: status_change carries presence only.
            {
                let (client, ids, since) = {
                    let st = ui.state.borrow();
                    let ids: Vec<String> = st.users.keys().cloned().collect();
                    (st.client.clone(), ids, st.users_fetched_at)
                };
                if !ids.is_empty() && since > 0 {
                    let ui = ui.clone();
                    runtime::spawn(
                        async move { client.users_updated_since(&ids, since).await },
                        move |result| {
                            let Ok(users) = result else { return };
                            if users.is_empty() {
                                return;
                            }
                            {
                                let mut st = ui.state.borrow_mut();
                                for user in users {
                                    // A new picture makes the cached texture
                                    // stale, exactly as user_updated does.
                                    let changed = st.users.get(&user.id).is_none_or(|old| {
                                        old.last_picture_update != user.last_picture_update
                                    });
                                    if changed {
                                        ui.avatars.forget(&user.id);
                                    }
                                    st.users.insert(user.id.clone(), user);
                                }
                                st.users_fetched_at = glib::real_time() / 1000;
                            }
                            ui.refresh_all();
                        },
                    );
                }
            }

            ui.refresh_all();
            ui.load_inbox();
            // `?since=` is capped by the server, so a long absence can leave a
            // hole between what it returned and now. Scrolling up finds older
            // messages and nothing finds the ones in the middle, so ask for
            // whatever came after the newest post we hold.
            if let Some(id) = channel_id {
                ui.load_newer(id);
            }
        },
    );
}

/// `MATRAS_SCREENSHOT=out.png` renders the window to a file and exits.
///
/// Wayland will not let anything screenshot another process's window, but a
/// window can always paint itself. Used for the README shots and to eyeball a
/// layout change without a human clicking through demo mode.
fn screenshot_and_quit(window: &adw::ApplicationWindow) {
    let Some(path) = std::env::var_os("MATRAS_SCREENSHOT") else {
        return;
    };
    let window = window.clone();
    // Before the first frame `to_node` yields nothing, so give the compositor
    // a beat rather than racing it.
    glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
        match snapshot.to_node().and_then(|node| {
            window
                .native()
                .and_then(|n| n.renderer())
                .map(|r| r.render_texture(&node, None))
        }) {
            Some(texture) => {
                if let Err(e) = texture.save_to_png(&path) {
                    tracing::error!("screenshot failed: {e}");
                }
            }
            None => tracing::error!("screenshot failed: nothing rendered yet"),
        }
        // Quit, not close: closing hides the window and leaves the app
        // running in the background, which is right for a person and
        // wrong for a one-shot render that is supposed to end.
        match window.application() {
            Some(app) => app.quit(),
            None => window.close(),
        }
    });
}
