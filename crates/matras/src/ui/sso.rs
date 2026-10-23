//! Single sign-on through the browser, using Mattermost's desktop-token flow.
//!
//! The server will not redirect an OAuth callback to a loopback address —
//! `ValidateWebAuthRedirectUrl` insists the redirect matches the site's own
//! scheme and host — so there is no port to listen on. What it will do is hand
//! the result to a registered URL scheme:
//!
//! 1. we invent a `client_token` and send the browser to
//!    `/oauth/<service>/login?desktop_token=<client_token>`,
//! 2. the identity provider authenticates the person,
//! 3. Mattermost mints a `server_token` and bounces the browser to
//!    `/login/desktop`, whose page deep-links to `mattermost-dev://…`,
//! 4. that launches us again; GIO delivers the URI to the running instance,
//!    and we trade the `server_token` for a session.
//!
//! The `dev-` prefix on the client token is load-bearing: it is what makes the
//! server pick the `mattermost-dev` scheme over `mattermost`, which belongs to
//! the official desktop app.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use mattermost_api::Client;

use super::login::LoginResult;
use crate::runtime;

/// Which provider to send people to. `/oauth/{service}/login` is the same
/// route for every one of them.
pub const GITLAB: &str = "gitlab";

struct Pending {
    client_token: String,
    client: Client,
    on_success: Rc<dyn Fn(LoginResult)>,
    on_error: Rc<dyn Fn(&str)>,
}

thread_local! {
    /// At most one sign-in can be in flight, and it only ever touches the GTK
    /// thread, so a thread-local outlives the login view without an `Rc` maze.
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

/// Opens the browser at the provider's login page. Returns the error to show
/// if the browser could not be launched at all.
pub fn start(
    client: Client,
    service: &str,
    on_success: impl Fn(LoginResult) + 'static,
    on_error: impl Fn(&str) + 'static,
) -> Result<(), String> {
    let client_token = new_client_token();
    let url = format!(
        "{}/oauth/{service}/login?desktop_token={client_token}",
        client.site_url().trim_end_matches('/'),
    );

    // Persist before opening the browser: the browser detour can outlive this
    // process (Android kills it in the background, or the callback cold-starts
    // a fresh instance, or the person finishes auth in a different browser than
    // the one we launched). The in-memory Pending below only survives a warm
    // process; the file is what lets any later instance finish the sign-in.
    persist_pending(client.site_url(), &client_token);

    super::browser::open(&url)?;

    PENDING.with(|p| {
        *p.borrow_mut() = Some(Pending {
            client_token,
            client,
            on_success: Rc::new(on_success),
            on_error: Rc::new(on_error),
        })
    });
    Ok(())
}

/// Where the pending sign-in is persisted across a process restart. Next to
/// the session file, i.e. app-private.
fn pending_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join(crate::APP_ID)
        .join("sso-pending.json")
}

fn persist_pending(server_url: &str, client_token: &str) {
    let path = pending_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::json!({ "server": server_url, "client_token": client_token });
    // 0600 like session.json: the token is a sign-in nonce, and the file
    // outlives a browser that failed to open.
    let written = {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(json.to_string().as_bytes()))
    };
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not persist the pending sign-in");
    }
}

/// Reads and removes the persisted pending sign-in, if any.
fn take_persisted() -> Option<(String, String)> {
    let path = pending_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let server = v.get("server")?.as_str()?.to_string();
    let client_token = v.get("client_token")?.as_str()?.to_string();
    Some((server, client_token))
}

fn clear_persisted() {
    let _ = std::fs::remove_file(pending_path());
}

/// Handles a `mattermost-dev://…` callback. Anything that is not one, or does
/// not match the sign-in we started, is dropped: this URI arrives from outside
/// the process and a stray one must not be able to log anybody in.
pub fn deliver(app: &adw::Application, uri: &str) {
    let Some((client_token, server_token)) = parse_callback(uri) else {
        tracing::warn!("ignoring an SSO callback that carried no tokens");
        return;
    };

    // Warm path: this is the same process that started the sign-in, so the
    // login view's callbacks are still live — use them for inline feedback.
    if let Some(pending) = PENDING.with(|p| p.borrow_mut().take()) {
        clear_persisted();
        if pending.client_token != client_token {
            tracing::warn!("an SSO callback carried a client token we did not issue");
            (pending.on_error)("That sign-in did not match the one this window started.");
            return;
        }
        let client = pending.client.clone();
        let on_success = pending.on_success.clone();
        let on_error = pending.on_error.clone();
        runtime::spawn(
            async move {
                client
                    .login_with_desktop_token(&server_token)
                    .await
                    .map(|me| (client, me))
            },
            move |result| match result {
                Ok((client, me)) => on_success(LoginResult { client, me }),
                // A 401 here is a spent or expired desktop token, not a bad
                // password — `describe` would otherwise say the wrong thing.
                Err(mattermost_api::Error::Api(app)) if app.status_code == 401 => {
                    on_error("That sign-in did not complete. Try again.")
                }
                Err(e) => on_error(&super::login::describe(&e)),
            },
        );
        return;
    }

    // Cold path: a fresh process (the callback cold-started us, or we were
    // killed in the background) with no live callbacks. Complete from the
    // persisted request: exchange the token with a new client, save the
    // session, and rebuild the window so it lands in the chat.
    let Some((server_url, saved_client_token)) = take_persisted() else {
        tracing::warn!("an SSO callback arrived with no sign-in in flight (memory or disk)");
        return;
    };
    if saved_client_token != client_token {
        tracing::warn!("an SSO callback carried a client token we did not persist");
        return;
    }

    let app = app.clone();
    runtime::spawn(
        async move {
            let client = Client::new(&server_url).map_err(|e| e.to_string())?;
            client
                .login_with_desktop_token(&server_token)
                .await
                .map_err(|e| super::login::describe(&e))?;
            let server = client.site_url().to_string();
            let token = client.token().unwrap_or_default();
            crate::session::save_async(&server, &token).await;
            Ok::<(), String>(())
        },
        move |result| match result {
            Ok(()) => {
                // The saved session is what the rebuilt window reads to restore
                // straight into the chat rather than the login form.
                for window in app.windows() {
                    window.close();
                }
                super::build_window(&app);
            }
            Err(e) => {
                tracing::warn!(error = %e, "SSO completion from the persisted request failed")
            }
        },
    );
}

/// Pulls the two tokens out of `mattermost-dev://host/login/desktop?…`.
fn parse_callback(uri: &str) -> Option<(String, String)> {
    let uri = uri.strip_prefix("mattermost-dev://")?;
    let query = uri.split_once('?')?.1;
    let mut client_token = None;
    let mut server_token = None;
    // Both tokens are alphanumeric by construction — the server mints them with
    // `NewRandomString`, we mint ours as hex — so there is nothing to decode.
    for (key, value) in query.split('&').filter_map(|p| p.split_once('=')) {
        match key {
            "client_token" => client_token = Some(value.to_string()),
            "server_token" => server_token = Some(value.to_string()),
            _ => {}
        }
    }
    Some((client_token?, server_token?))
}

/// 64 characters, `dev-` and then hex — the shape the web app uses, and the
/// prefix the server keys the `mattermost-dev` scheme off.
fn new_client_token() -> String {
    let mut token = String::from("dev-");
    while token.len() < 64 {
        token.push_str(&gtk::glib::uuid_string_random().replace('-', ""));
    }
    token.truncate(64);
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_parsing_and_token_shape() {
        let uri = "mattermost-dev://mm.example.com/login/desktop\
                   ?client_token=dev-abc&server_token=xyz&isDesktopDev=true";
        assert_eq!(
            parse_callback(uri),
            Some(("dev-abc".to_string(), "xyz".to_string()))
        );
        // A callback missing either half must not be treated as a sign-in.
        assert_eq!(
            parse_callback("mattermost-dev://h/login/desktop?client_token=a"),
            None
        );
        assert_eq!(
            parse_callback("https://mm.example.com/login/desktop?a=b"),
            None
        );

        let token = new_client_token();
        assert_eq!(token.len(), 64);
        assert!(token.starts_with("dev-"));
    }
}
