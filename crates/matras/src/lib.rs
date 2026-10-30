//! A native Mattermost client for GNOME.
//!
//! The window is a three-pane layout — channel sidebar, conversation, and a
//! thread/inbox panel — built from an [`adw::NavigationSplitView`] wrapping an
//! [`adw::OverlaySplitView`], so it collapses the way GNOME expects: the
//! thread panel overlays first, then the sidebar folds away on a phone.
//! Under the sidebar sits the call dock, which is pinned there for as long as
//! a call runs and moves under the conversation once the sidebar folds away.

mod agents;
#[cfg(target_os = "android")]
mod android_entry;
mod audio;
mod avatars;
mod background;
mod cache;
mod demo;
mod emoji;
mod gomon;
mod markdown;
mod resource_cache;
mod runtime;
mod session;
mod state;
mod store;
mod ui;
mod video;

use std::cell::RefCell;

use adw::prelude::*;
use gtk::gio;

use background::Background;

pub const APP_ID: &str = "ru.toxblh.Matras";

/// A base directory that is reliably writable with real POSIX file locking.
///
/// On Android the GTK runtime points `glib::user_data_dir()` at *external*
/// (emulated) storage, whose FUSE layer does not support the locking SQLite
/// needs — every write comes back "attempt to write a readonly database". The
/// runtime also exposes the app's *internal* files dir as the first entry of
/// `XDG_DATA_DIRS`; that is real ext4 and works. Desktop keeps the normal data
/// dir.
pub(crate) fn writable_data_dir() -> std::path::PathBuf {
    #[cfg(target_os = "android")]
    {
        gtk::glib::system_data_dirs()
            .into_iter()
            .next()
            .unwrap_or_else(gtk::glib::user_data_dir)
    }
    #[cfg(not(target_os = "android"))]
    {
        gtk::glib::user_data_dir()
    }
}

thread_local! {
    /// The live hold, if the app is currently running windowless. It has to
    /// outlive every window, so it cannot live in one; GTK is single-threaded,
    /// so a thread-local is the whole of the storage needed.
    static HOLD: RefCell<Option<Background>> = const { RefCell::new(None) };
}

/// Builds the application and runs it to completion. This is the whole
/// program: the desktop `main` and the Android entry point are both one-line
/// callers, so every platform goes through the same code from here on.
pub fn run() -> gtk::glib::ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "matras=debug,mattermost_api=debug,mattermost_calls=debug".into());
    // Android sends stdout to /dev/null, so the desktop's fmt writer would drop
    // every line; route to logcat instead (adb logcat -s Matras). This is the
    // only way to debug anything on-device — a call above all.
    #[cfg(target_os = "android")]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let android = tracing_android::layer("Matras").expect("the android log layer opens");
        tracing_subscriber::registry()
            .with(filter)
            .with(android)
            .init();
    }
    #[cfg(not(target_os = "android"))]
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // A panic on Android otherwise vanishes: the default hook writes to stderr,
    // which is /dev/null there, so a panic inside a glib callback (audio start,
    // say) is silently swallowed and looks like "nothing happened". Route the
    // message and location to the log instead.
    {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            tracing::error!(%location, "panic: {info}");
            default(info);
        }));
    }

    // HANDLES_OPEN is what makes the `mattermost-dev://` SSO callback work: the
    // browser launches a second copy of this binary with the URI, GIO hands it
    // to the already-running one over D-Bus, and it arrives in `connect_open`.
    // Before anything can reach rustls. Two providers are in the tree —
    // aws-lc-rs through reqwest, ring through webrtc's DTLS — so rustls
    // refuses to pick one, and the thread that first needs TLS panics rather
    // than the process failing visibly.
    mattermost_api::tls::install_crypto_provider();

    let mut flags = gio::ApplicationFlags::HANDLES_OPEN;
    // Profiling and screenshot runs need an isolated second process without
    // stealing activation from the person's live client. This is deliberately
    // opt-in: normal launches retain the single-instance contract.
    if std::env::var_os("MATRAS_NON_UNIQUE").is_some() {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();

    app.connect_startup(|app| {
        load_css();
        load_icons();
        add_actions(app);
    });
    app.connect_activate(present_or_build);

    // `matras --new-window`: the launcher's "New Window" action. Handled in
    // the launching process, which asks the running instance over D-Bus.
    app.add_main_option(
        "new-window",
        gtk::glib::Char::from(0u8),
        gtk::glib::OptionFlags::NONE,
        gtk::glib::OptionArg::None,
        "Open a new window",
        None,
    );
    app.connect_handle_local_options(|app, options| {
        use std::ops::ControlFlow;
        if !options.contains("new-window") {
            return ControlFlow::Continue(()); // The normal startup.
        }
        if let Err(e) = app.register(gio::Cancellable::NONE) {
            tracing::error!("could not register the application: {e}");
            return ControlFlow::Break(gtk::glib::ExitCode::FAILURE);
        }
        // Nothing running yet: the ordinary first window is the new one.
        if !app.is_remote() {
            return ControlFlow::Continue(());
        }
        app.activate_action("new-window", None);
        ControlFlow::Break(gtk::glib::ExitCode::SUCCESS)
    });

    // `window-removed` rather than `shutdown`: by the time shutdown runs the
    // application has already decided to exit, which is far too late to keep
    // it alive. This fires the moment a window leaves, which is exactly when
    // the hold has to be in place. GApplication only acts on a use count of
    // zero from the main loop, not inside the emission, so taking the hold
    // here still beats the exit.
    //
    // Android has no windowless-background concept: the OS owns the lifecycle
    // and kills the held process anyway, so the hold just churns PIDs (and
    // loses any in-flight SSO). Let the app end with its window there.
    #[cfg(not(target_os = "android"))]
    app.connect_window_removed(|app, window| {
        if app.windows().iter().any(|other| other != window) {
            return;
        }
        if !Background::enabled() {
            return;
        }
        tracing::info!("last window closed; staying in the background");
        HOLD.with_borrow_mut(|hold| *hold = Some(Background::hold(app)));
    });
    // A window holds the application by itself, so the hold is only wanted
    // between windows.
    app.connect_window_added(|_, _| {
        HOLD.with_borrow_mut(|hold| hold.take());
    });

    app.connect_open(|app, files, _hint| {
        // A cold start from the callback still needs a window to land in.
        if app.active_window().is_none() {
            ui::build_window(app);
        }
        if let Some(window) = app.active_window() {
            window.present();
        }
        for file in files {
            ui::sso::deliver(app, &file.uri());
        }
    });
    app.run()
}

/// Raising the application has to *show* something. Running in the background
/// means there may be no window at all, and a second launch of the binary —
/// clicking the launcher again — arrives here in the running instance. An
/// existing window is presented rather than duplicated: a second window would
/// mean a second session and a second websocket. `app.new-window` is the way
/// to ask for another one on purpose.
fn present_or_build(app: &adw::Application) {
    // `active_window` is the *focused* one, and a window hidden into the
    // background has no focus — so it would look like there is none and a
    // second session would be built beside the one still running.
    match app.windows().first() {
        Some(window) => window.present(),
        None => ui::build_window(app),
    }
}

fn add_actions(app: &adw::Application) {
    // A stateful action so the menu draws it as a checkbox: leaving a process
    // running with no window is only acceptable if it is visible and stoppable.
    let background =
        gio::SimpleAction::new_stateful("background", None, &Background::enabled().to_variant());
    background.connect_activate({
        let app = app.clone();
        move |action, _| {
            let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(true);
            Background::set_enabled(on);
            action.set_state(&on.to_variant());
            // Turning it off while already backgrounded should take effect
            // now, not at the next close.
            if !on && app.windows().is_empty() {
                HOLD.with_borrow_mut(|hold| *hold = None);
            }
        }
    });
    app.add_action(&background);

    // ponytail: a second window means a second session and a second
    // websocket, because the session is built inside `build_window` and owned
    // by it. That is honest but wasteful; sharing one session across windows
    // means lifting `Ui` out of the window, which is a refactor of every
    // handler. Left as-is because two windows onto one server is rare, and a
    // duplicated socket is correct, just not thrifty.
    let new_window = gio::SimpleAction::new("new-window", None);
    new_window.connect_activate({
        let app = app.clone();
        move |_, _| ui::build_window(&app)
    });
    app.add_action(&new_window);

    let about = gio::SimpleAction::new("about", None);
    about.connect_activate({
        let app = app.clone();
        move |_, _| show_about(&app)
    });
    app.add_action(&about);

    // Closing the window no longer quits, so something else has to. Dropping
    // the hold first is what lets the application actually go.
    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate({
        let app = app.clone();
        move |_, _| {
            HOLD.with_borrow_mut(|hold| hold.take());
            app.quit();
        }
    });
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}

const METAINFO: &str = include_str!("../../../data/ru.toxblh.Matras.metainfo.xml");

/// About Matras. There is no gresource to read the metainfo from, so the
/// fields are filled in here; the release notes are still the metainfo's.
fn show_about(app: &adw::Application) {
    let version = env!("CARGO_PKG_VERSION");
    let about = adw::AboutDialog::builder()
        .application_name("Matras")
        .application_icon(APP_ID)
        .version(version)
        .developer_name("Anton Palgunov")
        .developers(["Anton Palgunov"])
        .copyright("© 2026 Anton Palgunov")
        .license_type(gtk::License::Gpl30Only)
        .website("https://altlinux.space/toxblh/matras")
        .issue_url("https://altlinux.space/toxblh/matras/issues")
        .build();
    if let Some(notes) = release_notes(METAINFO, version) {
        about.set_release_notes_version(version);
        about.set_release_notes(notes);
    }
    about.present(app.active_window().as_ref());
}

/// The `<description>` of `version`'s `<release>` in a metainfo file, which is
/// the markup AdwAboutDialog takes for its release notes.
fn release_notes<'a>(metainfo: &'a str, version: &str) -> Option<&'a str> {
    let start = metainfo.find(&format!("<release version=\"{version}\""))?;
    let release = &metainfo[start..];
    // `<release …/>` has no notes, and the next `</release>` is someone else's.
    if release[..release.find('>')?].ends_with('/') {
        return None;
    }
    let release = &release[..release.find("</release>")?];
    let body = &release[release.find("<description>")? + "<description>".len()..];
    Some(body[..body.find("</description>")?].trim())
}

/// Makes the application icon findable.
///
/// An installed build picks it up from the XDG icon directories; a `cargo run`
/// from the source tree needs the tree's own `data/icons` on the search path.
fn load_icons() {
    gtk::Window::set_default_icon_name(APP_ID);
    // The bundled Adwaita symbolic icons (see build.rs). Registering the
    // gresource is idempotent per process; the icon theme then resolves the
    // named icons from it, which is the only source on Android.
    gtk::gio::resources_register_include!("matras-icons.gresource")
        .expect("the bundled icon resource is valid");
    if let Some(display) = gtk::gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_resource_path("/ru/toxblh/Matras/icons");
        theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/icons"));
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{release_notes, METAINFO};

    #[test]
    fn release_notes_come_from_that_release_only() {
        let xml = r#"<release version="2.0" date="x"/>
            <release version="1.0" date="y"><description><p>One</p></description></release>"#;
        assert_eq!(release_notes(xml, "1.0"), Some("<p>One</p>"));
        // Self-closing: no notes, rather than the next release's.
        assert_eq!(release_notes(xml, "2.0"), None);
        assert_eq!(release_notes(xml, "3.0"), None);
    }

    #[test]
    fn this_version_has_release_notes() {
        assert!(release_notes(METAINFO, env!("CARGO_PKG_VERSION")).is_some());
    }
}
