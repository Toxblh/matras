//! REST client for the Mattermost v4 API.
//!
//! # Authentication
//!
//! Mattermost's [`ParseAuthTokenFromRequest`] checks the `MMAUTHTOKEN` **cookie
//! before** the `Authorization` header. If a cookie jar is active *and* you set
//! a bearer token, the cookie wins and you are silently moved onto the
//! CSRF-enforced code path. This client therefore disables reqwest's cookie
//! store entirely and authenticates with `Authorization: Bearer` only — which
//! also means CSRF never applies to us (`web/handlers.go: checkCSRFToken`
//! requires `tokenLocation == TokenLocationCookie`).
//!
//! [`ParseAuthTokenFromRequest`]: https://github.com/mattermost/mattermost/blob/master/server/channels/app/authentication.go

use std::sync::{Arc, RwLock};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE, RANGE};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{AppError, Error, Result};
use crate::models::*;

/// Server-imposed maximum for `per_page` / `limit_*` (`web/params.go`).
/// Larger values are silently **clamped**, so never infer "end of list" from a
/// short page.
pub const PER_PAGE_MAX: u32 = 200;
/// The default the server applies when `per_page` is omitted.
pub const PER_PAGE_DEFAULT: u32 = 60;
/// What the official clients request for a channel feed page.
pub const POST_CHUNK_SIZE: u32 = 60;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// Site URL with any trailing slash trimmed, e.g. `https://mm.example.com`.
    base: Arc<str>,
    token: Arc<RwLock<Option<String>>>,
    /// Populated from the `X-Version-Id` response header.
    server_version: Arc<RwLock<Option<String>>>,
    /// The live websocket's connection id, sent on writes so the server can
    /// leave this connection out of the echo.
    connection_id: Arc<RwLock<Option<String>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .field("authenticated", &self.token.read().unwrap().is_some())
            .finish()
    }
}

impl Client {
    /// Builds a client for `site_url` (e.g. `https://mm.example.com`).
    pub fn new(site_url: &str) -> Result<Self> {
        Self::with_http(site_url, Self::default_http()?)
    }

    fn default_http() -> Result<reqwest::Client> {
        let mut headers = HeaderMap::new();
        // The server uses this for (a) attaching session cookies on login and
        // (b) the legacy CSRF fallback. Harmless, and the official clients
        // always send it.
        headers.insert(
            "X-Requested-With",
            HeaderValue::from_static("XMLHttpRequest"),
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("matras/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers);
        if crate::tls::insecure() {
            tracing::warn!("MM_INSECURE_TLS: certificate verification disabled");
            builder = builder.danger_accept_invalid_certs(true);
        } else {
            // On Android the default verifier (rustls-platform-verifier) hangs
            // with no Java shim present; verify against webpki roots instead.
            // See tls::android_rustls_config. Desktop keeps reqwest's default.
            #[cfg(target_os = "android")]
            {
                builder = builder.use_preconfigured_tls(crate::tls::android_rustls_config());
            }
        }

        Ok(builder
            // No cookie jar: reqwest's `cookies` feature is deliberately not
            // enabled. See the module docs — the MMAUTHTOKEN cookie takes
            // precedence over the Authorization header server-side, which would
            // silently move us onto the CSRF-enforced path.
            .build()?)
    }

    pub fn with_http(site_url: &str, http: reqwest::Client) -> Result<Self> {
        let base = site_url.trim_end_matches('/').to_string();
        // Validate early so a typo fails at construction, not at first call.
        let _ = url::Url::parse(&base)?;
        Ok(Client {
            http,
            base: base.into(),
            token: Arc::new(RwLock::new(None)),
            server_version: Arc::new(RwLock::new(None)),
            connection_id: Arc::new(RwLock::new(None)),
        })
    }

    pub fn site_url(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<String> {
        self.token.read().unwrap().clone()
    }

    /// Sets a session token or personal access token.
    pub fn set_token(&self, token: impl Into<String>) {
        *self.token.write().unwrap() = Some(token.into());
    }

    pub fn clear_token(&self) {
        *self.token.write().unwrap() = None;
    }

    /// The websocket's connection id, once it has one.
    ///
    /// Sent as `Connection-Id` on writes so the server leaves *this* socket
    /// out of the resulting broadcast: we already know what we just did, and
    /// applying our own echo would fight whatever the user typed next.
    pub fn set_connection_id(&self, id: impl Into<String>) {
        *self.connection_id.write().unwrap() = Some(id.into());
    }

    /// `{version}.{build}.{config_hash}.{enterprise}` from `X-Version-Id`.
    pub fn server_version(&self) -> Option<String> {
        self.server_version.read().unwrap().clone()
    }

    /// The websocket URL derived from the site URL, as every official client
    /// does it: `http` → `ws`, `https` → `wss`, path + `/api/v4/websocket`.
    pub fn websocket_url(&self) -> String {
        let ws = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            format!("wss://{}", self.base)
        };
        format!("{ws}/api/v4/websocket")
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v4{}", self.base, path)
    }

    /// Absolute URL for a plugin route, e.g.
    /// `plugin_url("com.mattermost.calls", "/config")`.
    pub fn plugin_url(&self, plugin_id: &str, path: &str) -> String {
        format!("{}/plugins/{}{}", self.base, plugin_id, path)
    }

    // ---------------------------------------------------------------- plumbing

    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url);
        if let Some(token) = self.token() {
            req = req.bearer_auth(token);
        }
        if let Some(id) = self.connection_id.read().unwrap().as_deref() {
            req = req.header("Connection-Id", id);
        }
        req
    }

    /// Sends a request, records `X-Version-Id`, and turns any non-2xx into
    /// [`Error::Api`] carrying the server's [`AppError`].
    pub async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req.send().await?;

        // Only trust the version header when the response was not cached — a
        // CDN-cached body carries a stale one (`client4.ts`).
        if resp.headers().get(reqwest::header::CACHE_CONTROL).is_none() {
            if let Some(v) = resp
                .headers()
                .get("X-Version-Id")
                .and_then(|v| v.to_str().ok())
            {
                *self.server_version.write().unwrap() = Some(v.to_string());
            }
        }

        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_MODIFIED {
            return Ok(resp);
        }

        let body = resp.text().await.unwrap_or_default();
        let mut app: AppError = serde_json::from_str(&body).unwrap_or_else(|_| AppError {
            message: if body.is_empty() {
                status.to_string()
            } else {
                body.clone()
            },
            status_code: status.as_u16(),
            ..Default::default()
        });
        if app.status_code == 0 {
            app.status_code = status.as_u16();
        }
        Err(Error::Api(app))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        context: &'static str,
    ) -> Result<T> {
        let body = self.send(req).await?.text().await?;
        serde_json::from_str(&body).map_err(|source| Error::Decode { context, source })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, context: &'static str) -> Result<T> {
        self.json(self.request(Method::GET, &self.api(path)), context)
            .await
    }

    async fn get_q<T, Q>(&self, path: &str, query: &Q, context: &'static str) -> Result<T>
    where
        T: DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        self.json(
            self.request(Method::GET, &self.api(path)).query(query),
            context,
        )
        .await
    }

    async fn post_json<T, B>(&self, path: &str, body: &B, context: &'static str) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.json(
            self.request(Method::POST, &self.api(path)).json(body),
            context,
        )
        .await
    }

    async fn put_json<T, B>(&self, path: &str, body: &B, context: &'static str) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        self.json(
            self.request(Method::PUT, &self.api(path)).json(body),
            context,
        )
        .await
    }

    async fn delete_ok(&self, path: &str) -> Result<()> {
        self.send(self.request(Method::DELETE, &self.api(path)))
            .await?;
        Ok(())
    }

    // -------------------------------------------------------------------- auth

    /// `POST /api/v4/users/login`.
    ///
    /// The request body is decoded into a `map[string]string` server-side, so
    /// every value must be a JSON *string* — including booleans. The session
    /// token comes back in the `Token` **response header**, not the body; it is
    /// stored on this client automatically.
    pub async fn login(
        &self,
        login_id: &str,
        password: &str,
        mfa_token: Option<&str>,
    ) -> Result<User> {
        let mut body = std::collections::HashMap::new();
        body.insert("login_id", login_id);
        body.insert("password", password);
        if let Some(t) = mfa_token {
            // Note: the field is `token`, not `mfa_token`.
            body.insert("token", t);
        }

        let resp = self
            .send(
                self.request(Method::POST, &self.api("/users/login"))
                    .json(&body),
            )
            .await?;
        self.finish_login(resp).await
    }

    /// `POST /api/v4/users/login/desktop_token` — the tail of the SSO flow.
    ///
    /// `token` is the **server** token the browser handed back through the
    /// `mattermost-dev://` deep link, not the one we generated. The server only
    /// accepts it for an OAuth or SAML account, and burns it on use, so a
    /// second attempt with the same token is a 401 rather than a fresh session.
    pub async fn login_with_desktop_token(&self, token: &str) -> Result<User> {
        let mut body = std::collections::HashMap::new();
        body.insert("token", token);

        let resp = self
            .send(
                self.request(Method::POST, &self.api("/users/login/desktop_token"))
                    .json(&body),
            )
            .await?;
        self.finish_login(resp).await
    }

    /// The half every login route shares: the session token rides in the
    /// `Token` response header, and the body is the user.
    async fn finish_login(&self, resp: reqwest::Response) -> Result<User> {
        let token = resp
            .headers()
            .get("Token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or(Error::MissingToken)?;
        self.set_token(token);

        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|source| Error::Decode {
            context: "login user",
            source,
        })
    }

    /// `POST /api/v4/users/logout` — revokes the current session.
    pub async fn logout(&self) -> Result<()> {
        let r = self
            .send(self.request(Method::POST, &self.api("/users/logout")))
            .await
            .map(|_| ());
        self.clear_token();
        r
    }

    /// `GET /api/v4/config/client?format=old`
    pub async fn client_config(&self) -> Result<ClientConfig> {
        self.get_q("/config/client", &[("format", "old")], "client config")
            .await
    }

    /// `GET /api/v4/license/client?format=old`
    pub async fn client_license(&self) -> Result<StringMap> {
        self.get_q("/license/client", &[("format", "old")], "client license")
            .await
    }

    // ------------------------------------------------------------------- users

    pub async fn me(&self) -> Result<User> {
        self.get("/users/me", "me").await
    }

    pub async fn user(&self, user_id: &str) -> Result<User> {
        self.get(&format!("/users/{user_id}"), "user").await
    }

    /// `POST /api/v4/users/ids` — the batching endpoint the official clients
    /// use to hydrate post authors. Chunk your ids at 100.
    pub async fn users_by_ids(&self, ids: &[String]) -> Result<Vec<User>> {
        self.post_json("/users/ids", &ids, "users by ids").await
    }

    pub async fn users_by_usernames(&self, names: &[String]) -> Result<Vec<User>> {
        self.post_json("/users/usernames", &names, "users by usernames")
            .await
    }

    /// Users updated since `since` (ms) — the reconnect gap-fill for profiles.
    pub async fn users_updated_since(&self, ids: &[String], since: Millis) -> Result<Vec<User>> {
        self.json(
            self.request(Method::POST, &self.api("/users/ids"))
                .query(&[("since", since.to_string())])
                .json(&ids),
            "users since",
        )
        .await
    }

    pub async fn users_in_channel(
        &self,
        channel_id: &str,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<User>> {
        self.get_q(
            "/users",
            &[
                ("in_channel", channel_id),
                ("page", &page.to_string()),
                ("per_page", &per_page.min(PER_PAGE_MAX).to_string()),
            ],
            "users in channel",
        )
        .await
    }

    pub async fn statuses_by_ids(&self, ids: &[String]) -> Result<Vec<Status>> {
        self.post_json("/users/status/ids", &ids, "statuses").await
    }

    /// `PUT /api/v4/users/{user}/status` — online, away, dnd or offline.
    ///
    /// A status set this way is "manual" and the server stops moving it on
    /// your behalf until you go back to online.
    pub async fn set_status(&self, user_id: &str, status: &str) -> Result<Status> {
        let mut body = std::collections::HashMap::new();
        body.insert("user_id", user_id);
        body.insert("status", status);
        self.json(
            self.request(Method::PUT, &self.api(&format!("/users/{user_id}/status")))
                .json(&body),
            "set status",
        )
        .await
    }

    pub async fn set_custom_status(&self, status: &CustomStatus) -> Result<()> {
        self.send(
            self.request(Method::PUT, &self.api("/users/me/status/custom"))
                .json(status),
        )
        .await?;
        Ok(())
    }

    pub async fn clear_custom_status(&self) -> Result<()> {
        self.delete_ok("/users/me/status/custom").await
    }

    /// URL of a user's avatar. `last_picture_update` is used as a cache buster,
    /// exactly like the webapp does.
    pub fn user_avatar_url(&self, user_id: &str, last_picture_update: Millis) -> String {
        format!(
            "{}/users/{}/image?_={}",
            self.api(""),
            user_id,
            last_picture_update
        )
    }

    /// `POST /api/v4/users/search`.
    ///
    /// Every filter is a plain string where `""` means "do not filter", and the
    /// server picks a different query per filter rather than combining them:
    /// `in_channel` wins over `not_in_channel`, which itself is rejected
    /// without a `team_id`.
    pub async fn search_users(
        &self,
        term: &str,
        team_id: &str,
        in_channel: &str,
        not_in_channel: &str,
    ) -> Result<Vec<User>> {
        #[derive(Serialize)]
        struct Search<'a> {
            term: &'a str,
            team_id: &'a str,
            in_channel_id: &'a str,
            not_in_channel_id: &'a str,
        }
        self.post_json(
            "/users/search",
            &Search {
                term,
                team_id,
                in_channel_id: in_channel,
                not_in_channel_id: not_in_channel,
            },
            "search users",
        )
        .await
    }

    /// `GET /api/v4/users/autocomplete` — the @-mention picker's backend.
    ///
    /// The query parameter is `name`, not `term`. With `channel_id` set the
    /// response also carries `out_of_channel`: users who match but are not in
    /// the channel, which the webapp shows greyed out so mentioning them can
    /// offer to add them.
    pub async fn autocomplete_users(
        &self,
        term: &str,
        team_id: &str,
        channel_id: &str,
    ) -> Result<UserAutocomplete> {
        self.get_q(
            "/users/autocomplete",
            &[
                ("name", term),
                ("team_id", team_id),
                ("in_channel", channel_id),
            ],
            "autocomplete users",
        )
        .await
    }

    /// `PUT /api/v4/users/{user}/patch` — profile fields and `notify_props`.
    ///
    /// Untyped on purpose: a patch must omit what it does not change, and
    /// sending a full [`User`] with defaults would blank half the profile.
    /// `notify_props` is the exception to *that* — it is replaced wholesale, so
    /// read the current bag, change one key, and send it back whole.
    pub async fn patch_user(&self, user_id: &str, patch: &serde_json::Value) -> Result<User> {
        self.put_json(&format!("/users/{user_id}/patch"), patch, "patch user")
            .await
    }

    /// `POST /api/v4/users/{user}/image` (multipart, field `image`).
    ///
    /// Answers `{"status":"OK"}` rather than the user, and `last_picture_update`
    /// only changes on the *next* fetch of the profile — so refetch the user if
    /// you cache avatar URLs by that timestamp.
    pub async fn set_profile_image(
        &self,
        user_id: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new().part("image", part);
        self.send(
            self.request(Method::POST, &self.api(&format!("/users/{user_id}/image")))
                .multipart(form),
        )
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------- teams

    pub async fn my_teams(&self) -> Result<Vec<Team>> {
        self.get("/users/me/teams", "my teams").await
    }

    pub async fn my_team_members(&self) -> Result<Vec<TeamMember>> {
        self.get("/users/me/teams/members", "my team members").await
    }

    pub async fn my_team_unreads(&self, crt_enabled: bool) -> Result<Vec<TeamUnread>> {
        self.get_q(
            "/users/me/teams/unread",
            &[("include_collapsed_threads", crt_enabled.to_string())],
            "team unreads",
        )
        .await
    }

    /// `GET /api/v4/teams` — the teams you could join.
    ///
    /// Not "all teams" for a normal user: without `manage_system` the server
    /// filters this down to open teams with `allow_open_invite`, which is
    /// exactly what a join-a-team picker wants and why there is no separate
    /// endpoint for one.
    pub async fn all_teams(&self, page: u32, per_page: u32) -> Result<Vec<Team>> {
        self.get_q(
            "/teams",
            &[
                ("page", page.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
            ],
            "all teams",
        )
        .await
    }

    /// `POST /api/v4/teams/{team}/members` — adds `user_id` to the team.
    ///
    /// Joining yourself and being added by someone else are the same call; the
    /// server decides which permission to check from whether `user_id` is you.
    pub async fn join_team(&self, team_id: &str, user_id: &str) -> Result<TeamMember> {
        let mut body = std::collections::HashMap::new();
        body.insert("team_id", team_id);
        body.insert("user_id", user_id);
        self.post_json(&format!("/teams/{team_id}/members"), &body, "join team")
            .await
    }

    pub async fn leave_team(&self, team_id: &str, user_id: &str) -> Result<()> {
        self.delete_ok(&format!("/teams/{team_id}/members/{user_id}"))
            .await
    }

    // ---------------------------------------------------------------- channels

    /// `GET /users/me/teams/{team}/channels`.
    ///
    /// `last_delete_at` is the sync watermark: pass your last successful sync
    /// time to also receive channels deleted since then, so you can reconcile
    /// removals. Reconcile deletions only *after* every team has been fetched,
    /// or you will drop channels you are about to re-add.
    pub async fn my_channels(
        &self,
        team_id: &str,
        include_deleted: bool,
        last_delete_at: Millis,
    ) -> Result<Vec<Channel>> {
        self.get_q(
            &format!("/users/me/teams/{team_id}/channels"),
            &[
                ("include_deleted", include_deleted.to_string()),
                ("last_delete_at", last_delete_at.to_string()),
            ],
            "my channels",
        )
        .await
    }

    pub async fn my_channel_members(&self, team_id: &str) -> Result<Vec<ChannelMember>> {
        self.get(
            &format!("/users/me/teams/{team_id}/channels/members"),
            "my channel members",
        )
        .await
    }

    pub async fn channel(&self, channel_id: &str) -> Result<Channel> {
        self.get(&format!("/channels/{channel_id}"), "channel")
            .await
    }

    pub async fn channel_members(
        &self,
        channel_id: &str,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<ChannelMember>> {
        self.get_q(
            &format!("/channels/{channel_id}/members"),
            &[
                ("page", page.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
            ],
            "channel members",
        )
        .await
    }

    pub async fn channel_stats(&self, channel_id: &str) -> Result<ChannelStats> {
        self.get(&format!("/channels/{channel_id}/stats"), "channel stats")
            .await
    }

    /// `POST /channels/members/me/view` — marks a channel read and tells other
    /// sessions, which arrive as a `multiple_channels_viewed` websocket event.
    pub async fn view_channel(
        &self,
        channel_id: &str,
        prev_channel_id: &str,
        crt_enabled: bool,
    ) -> Result<ChannelViewResponse> {
        let body = ChannelView {
            channel_id: channel_id.to_string(),
            prev_channel_id: prev_channel_id.to_string(),
            collapsed_threads_supported: crt_enabled,
        };
        self.post_json("/channels/members/me/view", &body, "view channel")
            .await
    }

    /// Creates (or fetches) the DM channel between two users.
    pub async fn create_direct_channel(&self, me: &str, other: &str) -> Result<Channel> {
        self.post_json("/channels/direct", &[me, other], "direct channel")
            .await
    }

    /// `POST /channels`.
    ///
    /// `name` is the URL slug and must be lowercase — the server 400s rather
    /// than slugifying `display_name` for you. Creating a channel does *not*
    /// join you to it as a member; the creator is added automatically only for
    /// [`ChannelType::Open`] and [`ChannelType::Private`].
    pub async fn create_channel(
        &self,
        team_id: &str,
        name: &str,
        display_name: &str,
        r#type: ChannelType,
    ) -> Result<Channel> {
        let body = Channel {
            team_id: team_id.to_string(),
            name: name.to_string(),
            display_name: display_name.to_string(),
            r#type,
            ..Default::default()
        };
        self.post_json("/channels", &body, "create channel").await
    }

    /// `POST /channels/{channel}/members`.
    ///
    /// Joining yourself and inviting someone else are the same call, so the
    /// membership that comes back is the one for `user_id`, not for you.
    pub async fn join_channel(&self, channel_id: &str, user_id: &str) -> Result<ChannelMember> {
        let mut body = std::collections::HashMap::new();
        body.insert("user_id", user_id);
        self.post_json(
            &format!("/channels/{channel_id}/members"),
            &body,
            "join channel",
        )
        .await
    }

    pub async fn leave_channel(&self, channel_id: &str, user_id: &str) -> Result<()> {
        self.delete_ok(&format!("/channels/{channel_id}/members/{user_id}"))
            .await
    }

    /// `PUT /channels/{channel}/patch`.
    ///
    /// A patch leaves out what it does not send; the plain `PUT /channels/{id}`
    /// replaces the whole record and would blank the purpose and display name.
    pub async fn update_channel_header(&self, channel_id: &str, header: &str) -> Result<Channel> {
        #[derive(Serialize)]
        struct Patch<'a> {
            header: &'a str,
        }
        self.put_json(
            &format!("/channels/{channel_id}/patch"),
            &Patch { header },
            "update channel header",
        )
        .await
    }

    /// Renames the channel's *displayed* name only — the slug in `name`, and so
    /// every permalink, is left alone.
    pub async fn rename_channel(&self, channel_id: &str, display_name: &str) -> Result<Channel> {
        #[derive(Serialize)]
        struct Patch<'a> {
            display_name: &'a str,
        }
        self.put_json(
            &format!("/channels/{channel_id}/patch"),
            &Patch { display_name },
            "rename channel",
        )
        .await
    }

    /// `DELETE /channels/{channel}` — archives, it does not delete.
    ///
    /// The channel keeps its posts and stays readable; only with
    /// `ServiceSettings.EnableAPIChannelDeletion` does this destroy anything.
    pub async fn archive_channel(&self, channel_id: &str) -> Result<()> {
        self.delete_ok(&format!("/channels/{channel_id}")).await
    }

    /// `GET /teams/{team}/channels` — the public channels of a team, for a
    /// browse-and-join dialog.
    ///
    /// Public only, and it includes ones you are already in, so subtract your
    /// own membership list yourself.
    pub async fn channels_for_team(
        &self,
        team_id: &str,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<Channel>> {
        self.get_q(
            &format!("/teams/{team_id}/channels"),
            &[
                ("page", page.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
            ],
            "team channels",
        )
        .await
    }

    /// `POST /teams/{team}/channels/search` — same public-channel scope as
    /// [`channels_for_team`](Client::channels_for_team), matched on both the
    /// slug and the display name.
    pub async fn search_channels(&self, team_id: &str, term: &str) -> Result<Vec<Channel>> {
        #[derive(Serialize)]
        struct Search<'a> {
            term: &'a str,
        }
        self.post_json(
            &format!("/teams/{team_id}/channels/search"),
            &Search { term },
            "search channels",
        )
        .await
    }

    /// `PUT /channels/{channel}/members/{user}/notify_props`.
    ///
    /// A merge, not a replacement: the server copies over only the keys it
    /// recognises (`mark_unread`, `desktop`, `push`, `email`, …) and leaves the
    /// rest of the bag as it was. Answers `{"status":"OK"}`, so refetch the
    /// membership if you need the merged result.
    pub async fn set_channel_notify_props(
        &self,
        channel_id: &str,
        user_id: &str,
        props: &StringMap,
    ) -> Result<()> {
        self.send(
            self.request(
                Method::PUT,
                &self.api(&format!(
                    "/channels/{channel_id}/members/{user_id}/notify_props"
                )),
            )
            .json(props),
        )
        .await?;
        Ok(())
    }

    /// Marks a channel unread from its most recent post.
    ///
    /// There is no channel-level route for this — the server only knows how to
    /// mark a *post* unread — so this costs one extra round trip to find that
    /// post. Call [`set_post_unread`](Client::set_post_unread) directly when
    /// you already have one.
    pub async fn mark_channel_unread(
        &self,
        user_id: &str,
        channel_id: &str,
        crt_enabled: bool,
    ) -> Result<()> {
        let list = self
            .posts_for_channel(channel_id, 0, 1, crt_enabled)
            .await?;
        match list.newest() {
            Some(post) => self.set_post_unread(user_id, &post.id, crt_enabled).await,
            // Nothing was ever posted here, so there is nothing to be unread.
            None => Ok(()),
        }
    }

    // ------------------------------------------------------ sidebar categories

    pub async fn sidebar_categories(&self, team_id: &str) -> Result<OrderedSidebarCategories> {
        self.get(
            &format!("/users/me/teams/{team_id}/channels/categories"),
            "sidebar categories",
        )
        .await
    }

    /// `POST /users/{user}/teams/{team}/channels/categories`.
    ///
    /// Categories are per user *and* per team, so the same channel sits in a
    /// different category on each team. Send the new category with an empty
    /// `id`; the server assigns one.
    pub async fn create_category(
        &self,
        user_id: &str,
        team_id: &str,
        category: &SidebarCategory,
    ) -> Result<SidebarCategory> {
        self.post_json(
            &format!("/users/{user_id}/teams/{team_id}/channels/categories"),
            category,
            "create category",
        )
        .await
    }

    /// `PUT /users/{user}/teams/{team}/channels/categories`.
    ///
    /// This is also how a channel is *moved*: a channel belongs to exactly one
    /// category per team, so listing it in one silently removes it from its
    /// old one. Send both affected categories in the same call, or the server
    /// resolves the conflict on its own terms.
    pub async fn update_categories(
        &self,
        user_id: &str,
        team_id: &str,
        categories: &[SidebarCategory],
    ) -> Result<Vec<SidebarCategory>> {
        self.put_json(
            &format!("/users/{user_id}/teams/{team_id}/channels/categories"),
            categories,
            "update categories",
        )
        .await
    }

    /// Deletes a custom category; its channels fall back to the default
    /// Channels or Direct Messages category rather than disappearing.
    pub async fn delete_category(
        &self,
        user_id: &str,
        team_id: &str,
        category_id: &str,
    ) -> Result<()> {
        self.delete_ok(&format!(
            "/users/{user_id}/teams/{team_id}/channels/categories/{category_id}"
        ))
        .await
    }

    // ------------------------------------------------------- channel bookmarks

    /// `GET /channels/{channel}/bookmarks` — **server 9.4+**.
    ///
    /// Older servers 404 the whole route, and the client config carries no flag
    /// for the feature, so a 404 here is the feature detection.
    pub async fn list_bookmarks(&self, channel_id: &str) -> Result<Vec<ChannelBookmark>> {
        self.get(
            &format!("/channels/{channel_id}/bookmarks"),
            "channel bookmarks",
        )
        .await
    }

    /// Set `link_url` for a link bookmark or `file_id` for a file one — the
    /// server rejects both at once, and `type` must agree with whichever you
    /// filled in.
    pub async fn create_bookmark(
        &self,
        channel_id: &str,
        bookmark: &ChannelBookmark,
    ) -> Result<ChannelBookmark> {
        self.post_json(
            &format!("/channels/{channel_id}/bookmarks"),
            bookmark,
            "create bookmark",
        )
        .await
    }

    pub async fn delete_bookmark(&self, channel_id: &str, bookmark_id: &str) -> Result<()> {
        self.delete_ok(&format!("/channels/{channel_id}/bookmarks/{bookmark_id}"))
            .await
    }

    // ------------------------------------------------------------------- posts

    /// Page through a channel feed, newest first.
    pub async fn posts_for_channel(
        &self,
        channel_id: &str,
        page: u32,
        per_page: u32,
        crt_enabled: bool,
    ) -> Result<PostList> {
        self.get_q(
            &format!("/channels/{channel_id}/posts"),
            &[
                ("page", page.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "channel posts",
        )
        .await
    }

    /// Posts *modified* since `since` (ms).
    ///
    /// Three caveats the official clients all handle:
    /// 1. this is the only feed call that reports deletions and edits;
    /// 2. results are capped at 1000 and are **not guaranteed consecutive**, so
    ///    reconcile against what you have and re-page if you detect a hole;
    /// 3. `since` cannot be combined with `before`/`after`/`page`/`per_page`.
    pub async fn posts_since(
        &self,
        channel_id: &str,
        since: Millis,
        crt_enabled: bool,
    ) -> Result<PostList> {
        self.get_q(
            &format!("/channels/{channel_id}/posts"),
            &[
                ("since", since.to_string()),
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "channel posts since",
        )
        .await
    }

    /// Older posts. The anchor post is included in the response so blocks
    /// overlap and can be merged.
    pub async fn posts_before(
        &self,
        channel_id: &str,
        post_id: &str,
        per_page: u32,
        crt_enabled: bool,
    ) -> Result<PostList> {
        self.get_q(
            &format!("/channels/{channel_id}/posts"),
            &[
                ("before", post_id.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "posts before",
        )
        .await
    }

    pub async fn posts_after(
        &self,
        channel_id: &str,
        post_id: &str,
        per_page: u32,
        crt_enabled: bool,
    ) -> Result<PostList> {
        self.get_q(
            &format!("/channels/{channel_id}/posts"),
            &[
                ("after", post_id.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "posts after",
        )
        .await
    }

    /// `GET /users/me/channels/{id}/posts/unread` — the call the official
    /// clients make on first open of a channel, so the "new messages" line
    /// lands correctly.
    pub async fn posts_around_unread(
        &self,
        channel_id: &str,
        limit_before: u32,
        limit_after: u32,
        crt_enabled: bool,
    ) -> Result<PostList> {
        self.get_q(
            &format!("/users/me/channels/{channel_id}/posts/unread"),
            &[
                ("limit_before", limit_before.min(PER_PAGE_MAX).to_string()),
                // limit_after=0 is rejected by the server.
                (
                    "limit_after",
                    limit_after.clamp(1, PER_PAGE_MAX).to_string(),
                ),
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "unread posts",
        )
        .await
    }

    pub async fn post(&self, post_id: &str) -> Result<Post> {
        self.get(&format!("/posts/{post_id}"), "post").await
    }

    /// `POST /api/v4/posts/ids` — several posts at once.
    ///
    /// The natural way to turn a list of ids into posts; a file search answers
    /// with post ids rather than the posts themselves.
    pub async fn posts_by_ids(&self, ids: &[String]) -> Result<PostList> {
        let posts: Vec<Post> = self.post_json("/posts/ids", &ids, "posts by ids").await?;
        Ok(PostList::from_posts(posts))
    }

    pub async fn post_thread(&self, post_id: &str, crt_enabled: bool) -> Result<PostList> {
        self.get_q(
            &format!("/posts/{post_id}/thread"),
            &[
                ("collapsedThreads", crt_enabled.to_string()),
                ("collapsedThreadsExtended", crt_enabled.to_string()),
            ],
            "post thread",
        )
        .await
    }

    /// Creates a post. Set `root_id` to reply, `file_ids` to attach uploads.
    pub async fn create_post(&self, post: &Post) -> Result<Post> {
        self.post_json("/posts", post, "create post").await
    }

    /// Convenience wrapper for the common case.
    pub async fn send_message(
        &self,
        channel_id: &str,
        message: &str,
        root_id: Option<&str>,
    ) -> Result<Post> {
        let post = Post {
            channel_id: channel_id.to_string(),
            message: message.to_string(),
            root_id: root_id.unwrap_or_default().to_string(),
            ..Default::default()
        };
        self.create_post(&post).await
    }

    pub async fn update_post(&self, post_id: &str, message: &str) -> Result<Post> {
        #[derive(Serialize)]
        struct Patch<'a> {
            message: &'a str,
        }
        self.put_json(
            &format!("/posts/{post_id}/patch"),
            &Patch { message },
            "update post",
        )
        .await
    }

    pub async fn delete_post(&self, post_id: &str) -> Result<()> {
        self.delete_ok(&format!("/posts/{post_id}")).await
    }

    /// `POST /api/v4/users/{user}/posts/{post}/set_unread`.
    ///
    /// The body flag is not optional in practice: without it the server marks
    /// threads by the old, pre-CRT rules and the unread counts come back wrong.
    pub async fn set_post_unread(&self, user_id: &str, post_id: &str, crt: bool) -> Result<()> {
        let mut body = std::collections::HashMap::new();
        body.insert("collapsed_threads_supported", crt);
        self.send(
            self.request(
                Method::POST,
                &self.api(&format!("/users/{user_id}/posts/{post_id}/set_unread")),
            )
            .json(&body),
        )
        .await?;
        Ok(())
    }

    pub async fn pin_post(&self, post_id: &str, pinned: bool) -> Result<()> {
        let path = if pinned { "pin" } else { "unpin" };
        self.send(self.request(Method::POST, &self.api(&format!("/posts/{post_id}/{path}"))))
            .await?;
        Ok(())
    }

    pub async fn add_reaction(
        &self,
        user_id: &str,
        post_id: &str,
        emoji_name: &str,
    ) -> Result<Reaction> {
        let body = Reaction {
            user_id: user_id.to_string(),
            post_id: post_id.to_string(),
            emoji_name: emoji_name.to_string(),
            ..Default::default()
        };
        self.post_json("/reactions", &body, "add reaction").await
    }

    pub async fn remove_reaction(
        &self,
        user_id: &str,
        post_id: &str,
        emoji_name: &str,
    ) -> Result<()> {
        self.delete_ok(&format!(
            "/users/{user_id}/posts/{post_id}/reactions/{emoji_name}"
        ))
        .await
    }

    pub async fn search_posts(
        &self,
        team_id: &str,
        terms: &str,
        is_or_search: bool,
    ) -> Result<PostSearchResults> {
        #[derive(Serialize)]
        struct Search<'a> {
            terms: &'a str,
            is_or_search: bool,
        }
        self.post_json(
            &format!("/teams/{team_id}/posts/search"),
            &Search {
                terms,
                is_or_search,
            },
            "search posts",
        )
        .await
    }

    /// `GET /posts/{post}/edit_history` — the *previous* versions of a post.
    ///
    /// Old versions are stored as ordinary posts pointing at the live one
    /// through `original_id`, which is why this is a plain `Vec<Post>`. The
    /// current version is not in the list, and an unedited post yields an empty
    /// one rather than a 404.
    pub async fn post_edit_history(&self, post_id: &str) -> Result<Vec<Post>> {
        self.get(&format!("/posts/{post_id}/edit_history"), "edit history")
            .await
    }

    /// Restores `post_id` to one of the versions from
    /// [`post_edit_history`](Client::post_edit_history).
    ///
    /// This is itself an edit: the version being replaced is appended to the
    /// history rather than dropped.
    pub async fn restore_post_version(&self, post_id: &str, version_id: &str) -> Result<Post> {
        self.json(
            self.request(
                Method::POST,
                &self.api(&format!("/posts/{post_id}/restore/{version_id}")),
            ),
            "restore post version",
        )
        .await
    }

    /// `POST /posts/{post}/move` — moves a whole thread to another channel.
    ///
    /// `post_id` must be a thread *root*; the replies follow it. The posts are
    /// recreated in the target channel, so their ids change and any permalink
    /// to the old ones dies.
    pub async fn move_thread(&self, post_id: &str, channel_id: &str) -> Result<()> {
        let mut body = std::collections::HashMap::new();
        body.insert("channel_id", channel_id);
        self.send(
            self.request(Method::POST, &self.api(&format!("/posts/{post_id}/move")))
                .json(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /users/{user}/posts/{post}/reminder`.
    ///
    /// `target_time` is Unix **seconds**, not the milliseconds every other
    /// timestamp in this API uses — pass milliseconds and the reminder lands
    /// somewhere in the year 56000.
    pub async fn set_post_reminder(
        &self,
        user_id: &str,
        post_id: &str,
        target_time: i64,
    ) -> Result<()> {
        let mut body = std::collections::HashMap::new();
        body.insert("target_time", target_time);
        self.send(
            self.request(
                Method::POST,
                &self.api(&format!("/users/{user_id}/posts/{post_id}/reminder")),
            )
            .json(&body),
        )
        .await?;
        Ok(())
    }

    /// Acknowledges a post that was sent with `requested_ack`.
    pub async fn acknowledge_post(
        &self,
        user_id: &str,
        post_id: &str,
    ) -> Result<PostAcknowledgement> {
        self.json(
            self.request(
                Method::POST,
                &self.api(&format!("/users/{user_id}/posts/{post_id}/ack")),
            ),
            "acknowledge post",
        )
        .await
    }

    pub async fn unacknowledge_post(&self, user_id: &str, post_id: &str) -> Result<()> {
        self.delete_ok(&format!("/users/{user_id}/posts/{post_id}/ack"))
            .await
    }

    /// `GET /channels/{channel}/pinned` — every pinned post, unpaged.
    pub async fn pinned_posts(&self, channel_id: &str) -> Result<PostList> {
        self.get(&format!("/channels/{channel_id}/pinned"), "pinned posts")
            .await
    }

    // --------------------------------------------------------- scheduled posts

    /// `POST /posts/schedule`.
    ///
    /// The post is stored on the side and only becomes a real post at
    /// `scheduled_at`, so the id you get back is a *scheduled post* id and
    /// cannot be fed to any `/posts/{id}` route.
    pub async fn create_scheduled_post(&self, post: &ScheduledPost) -> Result<ScheduledPost> {
        self.post_json("/posts/schedule", post, "create scheduled post")
            .await
    }

    /// Rescheduling and editing are the same call — send the whole scheduled
    /// post back, not a patch.
    pub async fn update_scheduled_post(
        &self,
        scheduled_post_id: &str,
        post: &ScheduledPost,
    ) -> Result<ScheduledPost> {
        self.put_json(
            &format!("/posts/schedule/{scheduled_post_id}"),
            post,
            "update scheduled post",
        )
        .await
    }

    pub async fn delete_scheduled_post(&self, scheduled_post_id: &str) -> Result<()> {
        self.delete_ok(&format!("/posts/schedule/{scheduled_post_id}"))
            .await
    }

    /// `GET /api/v4/posts/scheduled/team/{team}` — messages waiting to go out.
    ///
    /// The response is a map keyed by team id, plus a `directChannels` bucket
    /// when asked for — direct messages belong to no team, and treating that
    /// key as one would count them as a team you are in.
    pub async fn scheduled_posts_for_team(&self, team_id: &str) -> Result<TeamScheduledPosts> {
        self.get_q(
            &format!("/posts/scheduled/team/{team_id}"),
            &[("includeDirectChannels", "true".to_string())],
            "scheduled posts",
        )
        .await
    }

    // ------------------------------------------------------------------- files

    /// Absolute URL for a file's contents. `update_at` busts caches.
    pub fn file_url(&self, file_id: &str, update_at: Millis) -> String {
        format!("{}/files/{}?_={}", self.api(""), file_id, update_at)
    }

    pub fn file_thumbnail_url(&self, file_id: &str, update_at: Millis) -> String {
        format!(
            "{}/files/{}/thumbnail?_={}",
            self.api(""),
            file_id,
            update_at
        )
    }

    pub fn file_preview_url(&self, file_id: &str, update_at: Millis) -> String {
        format!("{}/files/{}/preview?_={}", self.api(""), file_id, update_at)
    }

    /// Downloads a file's bytes.
    pub async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(self.request(Method::GET, &self.api(&format!("/files/{file_id}"))))
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `GET /api/v4/files/{file}` with `Range: bytes=0-{len-1}` — just the
    /// head of a file, for building a video poster from its container's
    /// index without downloading the whole clip (see
    /// `ui::media::head_playable` in the GTK client).
    ///
    /// The route is served by Go's `http.ServeContent`, which honours `Range`
    /// and answers `206 Partial Content`; a proxy that strips the header
    /// falls back to the whole file, which still decodes, just costs more.
    pub async fn download_file_range(&self, file_id: &str, len: u64) -> Result<Vec<u8>> {
        self.download_file_range_from(file_id, 0, len).await
    }

    /// Up to `len` bytes of a file from `start` — the end of an MP4 whose
    /// index was written last, say.
    pub async fn download_file_range_from(
        &self,
        file_id: &str,
        start: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        let end = start + len.saturating_sub(1);
        let resp = self
            .send(
                self.request(Method::GET, &self.api(&format!("/files/{file_id}")))
                    .header(RANGE, format!("bytes={start}-{end}")),
            )
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `GET /api/v4/files/{file}/thumbnail` — the server-side thumbnail
    /// (120×100), for the rare image with no preview to fall back to.
    pub async fn file_thumbnail_bytes(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(self.request(
                Method::GET,
                &self.api(&format!("/files/{file_id}/thumbnail")),
            ))
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `GET /api/v4/files/{file}/preview` — the server-side preview (capped
    /// at 1920px wide), for showing an attached image inline without pulling
    /// the full original. Big enough that a HiDPI display does not need to
    /// fall back on the tiny thumbnail to look sharp.
    pub async fn file_preview_bytes(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(self.request(Method::GET, &self.api(&format!("/files/{file_id}/preview"))))
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `POST /api/v4/files` (multipart). `client_id` is echoed back so you can
    /// correlate the resulting [`FileInfo`] with a local upload.
    pub async fn upload_file(
        &self,
        channel_id: &str,
        filename: &str,
        bytes: Vec<u8>,
        client_id: Option<&str>,
    ) -> Result<FileUploadResponse> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("channel_id", channel_id.to_string())
            .part("files", part);
        if let Some(cid) = client_id {
            form = form.text("client_ids", cid.to_string());
        }
        self.json(
            self.request(Method::POST, &self.api("/files"))
                .multipart(form),
            "upload file",
        )
        .await
    }

    /// `POST /teams/{team}/files/search` — the same search grammar as
    /// [`search_posts`](Client::search_posts), scoped to attachments.
    pub async fn search_files(&self, team_id: &str, terms: &str) -> Result<FileInfoList> {
        #[derive(Serialize)]
        struct Search<'a> {
            terms: &'a str,
            is_or_search: bool,
        }
        self.post_json(
            &format!("/teams/{team_id}/files/search"),
            &Search {
                terms,
                is_or_search: false,
            },
            "search files",
        )
        .await
    }

    /// `POST /uploads` — opens a resumable upload for a file of `file_size`
    /// bytes, to be filled with [`upload_data`](Client::upload_data).
    ///
    /// The size is fixed here and checked on every write, so it has to be the
    /// real one. Use this instead of [`upload_file`](Client::upload_file) for
    /// anything large enough that a dropped connection matters.
    pub async fn create_upload_session(
        &self,
        channel_id: &str,
        filename: &str,
        file_size: i64,
    ) -> Result<UploadSession> {
        #[derive(Serialize)]
        struct New<'a> {
            channel_id: &'a str,
            filename: &'a str,
            file_size: i64,
        }
        self.post_json(
            "/uploads",
            &New {
                channel_id,
                filename,
                file_size,
            },
            "create upload session",
        )
        .await
    }

    /// Re-reads a session to find out how much of it the server already has.
    pub async fn upload_session(&self, upload_id: &str) -> Result<UploadSession> {
        self.get(&format!("/uploads/{upload_id}"), "upload session")
            .await
    }

    /// `POST /uploads/{id}` — appends raw bytes to the session.
    ///
    /// There is no `Content-Range` header in this protocol: the body is always
    /// taken as starting at the session's current `file_offset`, and the server
    /// rejects a body longer than `file_size - file_offset`. So to resume,
    /// re-read the session with [`upload_session`](Client::upload_session) and
    /// send the remainder from that offset.
    ///
    /// `None` until the last byte lands — the server answers `204 No Content`
    /// for a partial write and only returns the [`FileInfo`] once the file is
    /// complete.
    pub async fn upload_data(&self, upload_id: &str, bytes: Vec<u8>) -> Result<Option<FileInfo>> {
        let resp = self
            .send(
                self.request(Method::POST, &self.api(&format!("/uploads/{upload_id}")))
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(bytes),
            )
            .await?;
        let body = resp.text().await?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|source| Error::Decode {
                context: "upload data",
                source,
            })
    }

    // ----------------------------------------------------------------- threads

    /// `GET /users/me/teams/{team}/threads` — the collapsed-reply-threads inbox.
    ///
    /// Only meaningful when CRT is on. `unread` fetches just the threads that
    /// need attention, which is what an inbox view wants; the official clients
    /// ask for those *every* sync because unread state does not bump
    /// `last_reply_at`.
    pub async fn my_threads(
        &self,
        team_id: &str,
        unread_only: bool,
        per_page: u32,
    ) -> Result<UserThreads> {
        self.get_q(
            &format!("/users/me/teams/{team_id}/threads"),
            &[
                ("unread", unread_only.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
                ("extended", "true".to_string()),
            ],
            "threads",
        )
        .await
    }

    /// Marks a thread read up to `timestamp` (ms).
    pub async fn mark_thread_read(
        &self,
        team_id: &str,
        thread_id: &str,
        timestamp: Millis,
    ) -> Result<()> {
        self.send(self.request(
            Method::PUT,
            &self.api(&format!(
                "/users/me/teams/{team_id}/threads/{thread_id}/read/{timestamp}"
            )),
        ))
        .await?;
        Ok(())
    }

    /// Follows or unfollows a thread.
    pub async fn follow_thread(
        &self,
        team_id: &str,
        thread_id: &str,
        following: bool,
    ) -> Result<()> {
        let method = if following {
            Method::PUT
        } else {
            Method::DELETE
        };
        self.send(self.request(
            method,
            &self.api(&format!(
                "/users/me/teams/{team_id}/threads/{thread_id}/following"
            )),
        ))
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------ images

    /// Raw bytes of a user's profile picture.
    ///
    /// The server always answers — it renders default initials when the user
    /// has not uploaded anything — so a client cannot tell "no picture" from
    /// "a picture" without inspecting the bytes. `last_picture_update` is the
    /// cache key; when it is 0 the user has never set one.
    pub async fn user_image_bytes(&self, user_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(self.request(Method::GET, &self.api(&format!("/users/{user_id}/image"))))
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `GET /api/v4/groups` — the groups that can be @-mentioned.
    ///
    /// Licensed servers only: Team Edition answers 501, which is the same as
    /// having no groups and needs no special handling beyond ignoring it.
    pub async fn mentionable_groups(&self, term: &str) -> Result<Vec<Group>> {
        self.get_q(
            "/groups",
            &[
                ("filter_allow_reference", "true".to_string()),
                ("q", term.to_string()),
                ("per_page", "10".to_string()),
            ],
            "groups",
        )
        .await
    }

    /// `POST /api/v4/actions/dialogs/submit` — answers an interactive dialog.
    ///
    /// The `url` in the request is the integration's own callback, taken from
    /// the `open_dialog` event: the server forwards the submission there
    /// rather than interpreting it, which is why it has to be echoed back.
    pub async fn submit_dialog(
        &self,
        request: &crate::models::dialog::SubmitDialogRequest,
    ) -> Result<serde_json::Value> {
        self.post_json("/actions/dialogs/submit", request, "submit dialog")
            .await
    }

    /// `POST /api/v4/commands/execute` — runs a slash command.
    ///
    /// The command's own output comes back as a post or an ephemeral message,
    /// not in this response, so the interesting part of the reply is usually
    /// `goto_location` and nothing else.
    pub async fn execute_command(
        &self,
        channel_id: &str,
        command: &str,
    ) -> Result<serde_json::Value> {
        let mut body = std::collections::HashMap::new();
        body.insert("channel_id", channel_id);
        body.insert("command", command);
        self.post_json("/commands/execute", &body, "execute command")
            .await
    }

    /// `GET /api/v4/emoji/{id}/image` — a custom emoji's picture.
    pub async fn emoji_image_bytes(&self, emoji_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .send(self.request(Method::GET, &self.api(&format!("/emoji/{emoji_id}/image"))))
            .await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Looks a custom emoji up by name, for shortcodes that are not standard
    /// Unicode ones.
    pub async fn emoji_by_name(&self, name: &str) -> Result<Emoji> {
        self.get(&format!("/emoji/name/{name}"), "emoji by name")
            .await
    }

    // ------------------------------------------------------------------ drafts

    /// `GET /api/v4/users/me/teams/{team}/drafts`.
    ///
    /// Gated on `ServiceSettings.AllowSyncedDrafts`: with it off the server
    /// answers 501 rather than an empty list, so treat that as "the feature is
    /// not here" rather than as "you have no drafts".
    pub async fn my_drafts(&self, team_id: &str) -> Result<Vec<Draft>> {
        self.get(&format!("/users/me/teams/{team_id}/drafts"), "drafts")
            .await
    }

    /// `POST /api/v4/drafts`.
    ///
    /// An empty message **deletes** the draft server-side rather than storing a
    /// blank one — exactly what should happen when a composer is cleared, so
    /// there is no separate case for it.
    pub async fn upsert_draft(&self, draft: &Draft) -> Result<Draft> {
        self.post_json("/drafts", draft, "upsert draft").await
    }

    /// `DELETE /api/v4/users/me/channels/{channel}/drafts[/{root}]`.
    ///
    /// Deleting a draft that is not there is a 200, not a 404.
    pub async fn delete_draft(&self, channel_id: &str, root_id: &str) -> Result<()> {
        let mut path = format!("/users/me/channels/{channel_id}/drafts");
        if !root_id.is_empty() {
            path.push('/');
            path.push_str(root_id);
        }
        self.send(self.request(Method::DELETE, &self.api(&path)))
            .await
            .map(|_| ())
    }

    // ------------------------------------------------------------- preferences

    pub async fn my_preferences(&self) -> Result<Vec<Preference>> {
        self.get("/users/me/preferences", "preferences").await
    }

    pub async fn save_preferences(&self, user_id: &str, prefs: &[Preference]) -> Result<()> {
        self.send(
            self.request(
                Method::PUT,
                &self.api(&format!("/users/{user_id}/preferences")),
            )
            .json(&prefs),
        )
        .await?;
        Ok(())
    }

    /// `POST /api/v4/users/{user}/preferences/delete`.
    ///
    /// Deleting is a POST with the preferences to remove, not a DELETE — only
    /// `category` and `name` are read, so `value` can be anything.
    pub async fn delete_preferences(&self, user_id: &str, prefs: &[Preference]) -> Result<()> {
        self.send(
            self.request(
                Method::POST,
                &self.api(&format!("/users/{user_id}/preferences/delete")),
            )
            .json(&prefs),
        )
        .await?;
        Ok(())
    }

    /// `GET /api/v4/users/{user}/posts/flagged` — the posts you saved.
    pub async fn flagged_posts(&self, user_id: &str, per_page: u32) -> Result<PostList> {
        self.get_q(
            &format!("/users/{user_id}/posts/flagged"),
            &[("per_page", per_page.min(PER_PAGE_MAX).to_string())],
            "flagged posts",
        )
        .await
    }

    // ------------------------------------------------------------------- emoji

    pub async fn custom_emoji(&self, page: u32, per_page: u32) -> Result<Vec<Emoji>> {
        self.get_q(
            "/emoji",
            &[
                ("page", page.to_string()),
                ("per_page", per_page.min(PER_PAGE_MAX).to_string()),
                ("sort", "name".to_string()),
            ],
            "custom emoji",
        )
        .await
    }

    /// `POST /emoji/search` — custom emoji only; the Unicode set is client-side
    /// and never reaches the server.
    pub async fn search_emoji(&self, term: &str) -> Result<Vec<Emoji>> {
        #[derive(Serialize)]
        struct Search<'a> {
            term: &'a str,
        }
        self.post_json("/emoji/search", &Search { term }, "search emoji")
            .await
    }

    pub fn custom_emoji_url(&self, emoji_id: &str) -> String {
        format!("{}/emoji/{}/image", self.api(""), emoji_id)
    }

    // ------------------------------------------------------- generic escape hatch

    /// GET an arbitrary absolute URL with this client's auth applied — used for
    /// plugin routes such as `/plugins/com.mattermost.calls/config`.
    pub async fn get_url<T: DeserializeOwned>(
        &self,
        url: &str,
        context: &'static str,
    ) -> Result<T> {
        self.json(self.request(Method::GET, url), context).await
    }

    /// POST an arbitrary absolute URL with this client's auth applied.
    pub async fn post_url<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: Option<&B>,
        context: &'static str,
    ) -> Result<T> {
        let mut req = self.request(Method::POST, url);
        match body {
            Some(b) => req = req.json(b),
            None => req = req.header(CONTENT_TYPE, "application/json").body("{}"),
        }
        self.json(req, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_is_derived_from_site_url() {
        let c = Client::new("https://mm.example.com/").unwrap();
        assert_eq!(c.websocket_url(), "wss://mm.example.com/api/v4/websocket");

        let c = Client::new("http://localhost:8065").unwrap();
        assert_eq!(c.websocket_url(), "ws://localhost:8065/api/v4/websocket");
    }

    #[test]
    fn plugin_url_is_not_under_api_v4() {
        let c = Client::new("https://mm.example.com").unwrap();
        assert_eq!(
            c.plugin_url("com.mattermost.calls", "/config"),
            "https://mm.example.com/plugins/com.mattermost.calls/config"
        );
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        let c = Client::new("https://mm.example.com///").unwrap();
        assert_eq!(c.site_url(), "https://mm.example.com");
    }
}

/// The portable LDAP/AD auth plugin (`mattermost-plugin-ldapauth`), which
/// provides directory sign-in on Team Edition builds where Mattermost's own
/// LDAP is an Enterprise feature.
pub const LDAPAUTH_PLUGIN_ID: &str = "ru.toxblh.ldapauth";

/// The plugin's one-shot CSRF cookie, bound to the token in the login form.
const LDAPAUTH_TX_COOKIE: &str = "ldapauth_login_tx";

impl Client {
    /// Signs in through [`mattermost-plugin-ldapauth`][LDAPAUTH_PLUGIN_ID],
    /// storing the session token on this client exactly as [`login`] does.
    ///
    /// The plugin has no JSON login endpoint; it serves an HTML form. So this
    /// walks the same two steps a browser does, then takes the **mobile SSO**
    /// exit the plugin offers for native clients: with `mobile=1` it answers
    /// with an `mmauth://` deep link carrying the session token, instead of
    /// redirecting into the web app.
    ///
    /// The GET issues a CSRF token *and* a cookie holding that token's hash;
    /// the POST must present both. That cookie is carried by hand rather than
    /// by enabling reqwest's jar — see the module docs for why a jar would
    /// break bearer auth for every other call.
    ///
    /// Requires HTTPS: the plugin refuses the token handoff over plaintext.
    ///
    /// [`login`]: Client::login
    pub async fn login_ldapauth(&self, login: &str, password: &str) -> Result<User> {
        let url = self.plugin_url(LDAPAUTH_PLUGIN_ID, "/login");

        let page = self.send(self.request(Method::GET, &url)).await?;
        let cookie = page
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| cookie_value(v, LDAPAUTH_TX_COOKIE))
            .ok_or_else(|| Error::Other(format!("{LDAPAUTH_TX_COOKIE} cookie missing")))?
            .to_string();
        let csrf = form_value(&page.text().await?, "csrf")
            .ok_or_else(|| Error::Other("no csrf field on the plugin login page".into()))?
            .to_string();

        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("login", login)
            .append_pair("password", password)
            .append_pair("csrf", &csrf)
            // Ask for the deep-link handoff rather than a redirect into the
            // web app; `app` must be an allowlisted scheme or it is ignored.
            .append_pair("mobile", "1")
            .append_pair("app", "mmauth://callback")
            .finish();
        // Origin is not optional: the handler rejects a POST it cannot tie to
        // the site URL before it ever looks at the CSRF token. The content type
        // is checked just as strictly — anything but a form is a 415.
        let resp = self
            .send(
                self.request(Method::POST, &url)
                    .header(reqwest::header::ORIGIN, self.site_url())
                    .header(
                        reqwest::header::COOKIE,
                        format!("{LDAPAUTH_TX_COOKIE}={cookie}"),
                    )
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(form),
            )
            .await?;

        // `Accept: application/json` is a default header on this client, which
        // is what makes the plugin answer with the link instead of a 303.
        let body = resp.text().await?;
        let handoff: LdapAuthHandoff =
            serde_json::from_str(&body).map_err(|source| Error::Decode {
                context: "ldapauth handoff",
                source,
            })?;
        let token = url::Url::parse(&handoff.redirect_to)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "MMAUTHTOKEN")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or(Error::MissingToken)?;
        self.set_token(token);

        self.me().await
    }
}

#[derive(serde::Deserialize)]
struct LdapAuthHandoff {
    redirect_to: String,
}

/// Value of `name` in a `Set-Cookie` header, ignoring the attributes after it.
fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let rest = header.strip_prefix(name)?.strip_prefix('=')?;
    Some(rest.split(';').next().unwrap_or(rest))
}

/// Value of a hidden `<input name="…">` on the plugin's login page. The form is
/// a fixed server-side template, so matching the attribute order it emits is
/// enough — no HTML parser for one field.
fn form_value<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let at = html.find(&format!("name=\"{name}\""))?;
    let rest = &html[at..];
    let start = rest.find("value=\"")? + "value=\"".len();
    let end = rest[start..].find('"')?;
    Some(&rest[start..start + end])
}

#[cfg(test)]
mod ldapauth_tests {
    use super::*;

    #[test]
    fn reads_the_cookie_and_the_csrf_field() {
        let set = "ldapauth_login_tx=abc123; Path=/; HttpOnly; Secure; SameSite=Lax";
        assert_eq!(cookie_value(set, LDAPAUTH_TX_COOKIE), Some("abc123"));
        // A different cookie on the same response must not match.
        assert_eq!(
            cookie_value("MMAUTHTOKEN=zzz; Path=/", LDAPAUTH_TX_COOKIE),
            None
        );
        // Prefix overlap is not a match either.
        assert_eq!(
            cookie_value("ldapauth_login_tx_other=zzz", LDAPAUTH_TX_COOKIE),
            None
        );

        let html = r#"<form id="form" method="post" action="/plugins/ru.toxblh.ldapauth/login"><input type="hidden" name="csrf" value="tok-42"><input type="hidden" name="redirect_to" value="/"><input id="login" name="login" required></form>"#;
        assert_eq!(form_value(html, "csrf"), Some("tok-42"));
        assert_eq!(form_value(html, "redirect_to"), Some("/"));
        assert_eq!(form_value(html, "nope"), None);
    }

    #[test]
    fn pulls_the_session_token_out_of_the_deep_link() {
        let link = "mmauth://callback?MMAUTHTOKEN=sess-token&MMCSRF=csrf-val&srv=https%3A%2F%2Fmm.example.com";
        let u = url::Url::parse(link).unwrap();
        let token = u
            .query_pairs()
            .find(|(k, _)| k == "MMAUTHTOKEN")
            .map(|(_, v)| v.into_owned());
        assert_eq!(token.as_deref(), Some("sess-token"));
    }
}
