# Matras

A native Mattermost client for GNOME — GTK4 + libadwaita, in Rust.

<img src="/docs/screenshots/main.png" />

Four crates:

| Crate | What it is |
|---|---|
| `mattermost-api` | Async Mattermost client: REST v4, models, and a reliable WebSocket with replay-aware reconnect. No GTK dependency. |
| `mattermost-calls` | The Mattermost Calls wire protocol and a WebRTC peer (webrtc-rs). No GTK dependency. |
| `matras` | The application: an adaptive libadwaita window. |
| `matras-testserver` | A fake Mattermost, real enough to exercise the client end to end. Dev-only. |

The two library crates are deliberately independent of the UI — they are usable
for a bot, a CLI, or a different front end.

## Status

**Working**

- Sign in (password + MFA), session token handling, `X-Requested-With`, no cookie jar
- **Single sign-on** through the browser, using Mattermost's desktop-token
  flow — see below
- The session token lives in the **Secret Service**, not a file, because it
  grants the whole account until somebody revokes it. Several servers can be
  stored side by side; a launch with more than one asks which
- Startup sequence in the order the official clients use it (config → teams → channels → categories)
- Adaptive layout: channel sidebar, conversation, and a right-hand panel that
  holds either a thread or the inbox; teams and the signed-in account live in a
  switcher popover in the sidebar header
- A thread earns a static third column when there is room; the inbox always
  overlays, because a stack you glance at should not shove the conversation
  aside
- **Call dock** pinned under the channel list: who is talking, how many are in
  it, the way back to its channel, and every in-call control. A call outlives
  the channel it started in, so its controls cannot live in that channel's
  header. On a phone-sized window the dock moves under the conversation
- Sidebar rows carry the faces of whoever is in a channel's call
- Sidebar categories with the real ordering rules, unread/mention/muted
  styling, and a row menu to mute a channel or move it to another category
- Virtualized message list: only the visible screenful has GTK widgets, while
  author grouping, day separators, edits, attachments, message priority,
  system messages and webhook name overrides remain intact
- **Profile pictures and media**, shared in memory and persisted in an
  expiring disk cache. Storage has a 5 GiB default global limit, LRU eviction,
  and a Storage panel where the limit can be changed or the cache cleared
- **Reactions as emoji**, not `:shortcodes:` — click a chip to toggle your own,
  pick one from the hover menu, or search the whole Unicode set. A server's own
  uploads are drawn as pictures, through the same image cache as avatars
- **Threads**: the reply count opens the thread in the right panel, replies go
  to the root's channel, and collapsed reply threads (CRT) are respected — a
  reply never leaks into the channel feed. The thread's reply box keeps its own
  synced draft, the way the server stores them
- **Profile card** on any avatar or name: picture, presence, position, custom
  status, the person's local time, and a button that opens the DM
- **Inbox**: recent mentions, followed threads and saved posts, with a count on
  the header button
- Sending messages, optimistically, with the server echo retiring the local copy
- **Editing, deleting, pinning, saving and marking unread**, from each
  message's own menu — plus edit history, reminders, and moving a whole thread
  to another channel
- **Message priority** set from the composer, and a button to acknowledge a
  post that asked for one
- **Scheduled messages**, with a list to change your mind before they go
- **Autocomplete** for `@names` and `:emoji`, answering from what is already
  loaded first so a keystroke never waits on a request
- **Markdown**: bold, italic, strikethrough, links, headings, nested lists,
  quotes, inline code, and fenced code blocks on their own panel
- **Permalinks** render as the message they point at, since the server has
  already resolved it and fetching it again would be work for nothing
- **Attachments**: pick, drag in, or paste an image. Anything large goes
  through a resumable upload session, so a stalled upload continues from where
  the server got to instead of starting over. Images draw themselves and open
  full size
- **Drafts**, synced through the server so the same unfinished message is
  waiting on every device
- **Channels**: create, browse, join, leave, rename, edit the header, archive;
  a member list you can add to and remove from; per-channel notification
  settings; bookmarks. Teams can be browsed and joined
- **Your account**: edit the profile and the avatar, set a status and a custom
  status, and edit the notification settings that decide every toast
- **Search** across a team, with results in the right panel — `file:` in front
  of the terms searches attachments instead, rather than adding a second box to
  find
- **Ctrl+K** jumps to a channel or a person, opening the DM if there is not one
  yet
- **Desktop notifications** following the server's own rules, and a snapshot
  cache so launching draws last session's channels immediately. Closing the
  window keeps the process, the socket and the notifications alive — a chat
  client you have to keep a window open for is not one
- **Calls**: join, mute, share a screen or camera, raise a hand, see who is in
  the call, and a dock that stays put when you read somewhere else. A call in a
  DM or a group rings, with Join and Dismiss on the notification; a host can
  mute or remove someone
- **Plugins**: the Agents LLM module (streamed answers, thread summaries,
  "catch me up") and reaction notifications
- Live updates over the websocket, including a correct reliable-reconnect resync

**Not done yet**

The honest list lives in [docs/PARITY.md](docs/PARITY.md) — what is missing,
ordered by how often it bites, with the routes that would close each gap. The
short version: no scrollback past the newest sixty messages in a channel, no
link previews, no slash commands, non-image attachments cannot be saved,
`:shortcodes:` typed into a message stay as text, and there is a snapshot cache
rather than a real local store.

## Single sign-on

The GitLab button sends the browser to `/oauth/gitlab/login?desktop_token=…`.
The server cannot redirect an OAuth callback to a loopback port — it insists the
redirect matches the site's own scheme and host — so the answer comes back
through a URL scheme instead: Mattermost bounces the browser to a page that
deep-links to `mattermost-dev://…`, which launches this app, and the token in
that URI is traded for a session.

That only works once the desktop file is installed, so the browser knows what
handles the scheme. A packaged build gets this for free. From a source tree:

```sh
cargo build
sed "s|^Exec=matras|Exec=$PWD/target/debug/matras|" \
  data/ru.toxblh.Matras.desktop \
  > ~/.local/share/applications/ru.toxblh.Matras.desktop
update-desktop-database ~/.local/share/applications
xdg-mime default ru.toxblh.Matras.desktop x-scheme-handler/mattermost-dev
```

The scheme is `mattermost-dev`, not `mattermost`, so nothing collides with the
official desktop app; the server picks it because the token starts with `dev-`.
The app must already be running when the callback arrives — the browser hands
the URI to the live instance over D-Bus.

## Install

Flatpak is delivered as an OCI image from the Forgejo Container registry. Add
the stable remote and install the app:

```sh
flatpak remote-add --user --no-gpg-verify matras-oci \
  'oci+https://altlinux.space/api/packages/toxblh/container#stable'
flatpak install --user matras-oci ru.toxblh.Matras
flatpak update --user ru.toxblh.Matras
```

Flatpak selects x86_64 or aarch64 automatically. The GNOME runtime comes from
Flathub. To install system-wide, omit `--user` from these commands.

## Building


Needs Rust 1.85+, GTK 4.12+ and libadwaita 1.5+.

```sh
# Debian/Ubuntu
sudo apt install libgtk-4-dev libadwaita-1-dev libssl-dev pkg-config build-essential

cargo build --release
cargo test --workspace
```

Run it:

```sh
./target/release/matras
```

To look at the layout without a server:

```sh
MATRAS_DEMO=1 ./target/release/matras
```

## Testing against a server

`matras-testserver` is a fake Mattermost with real state: it hands out real ids,
serves avatars, keeps threads and reactions, and pushes the same websocket
events the real server does — including the double-encoded payloads and the
inconsistent key casing, because reproducing those is the point.

```sh
cargo run -p matras-testserver          # 127.0.0.1:8065, logs every request

MATRAS_SERVER=http://127.0.0.1:8065 \
MATRAS_USER=anton MATRAS_PASSWORD=test \
  cargo run -p matras               # skips the sign-in form
```

A background "colleague" posts every 12 seconds (`MM_BOT_SECONDS` to change it)
so live updates are visible with one client open. Lena flips between online
and away every 90 seconds (`MM_AWAY_SECONDS`) without a websocket event, as a
real server does for other people's auto-away. The same three environment
variables work against a real server.

> On a minimal system, install `librsvg2-common` too — without the SVG pixbuf
> loader every symbolic icon renders as a broken-image box, which looks like an
> application bug and is not one.

## Using the libraries

```rust
use mattermost_api::{Client, WebSocket, ws::{WsUpdate, Event}};

let client = Client::new("https://mm.example.com")?;
client.login("alice", "hunter2", None).await?;

let ws = WebSocket::connect(client.websocket_url(), client.token().unwrap());
let mut rx = ws.subscribe();

while let Ok(update) = rx.recv().await {
    match update {
        WsUpdate::Event(Event::Posted(p)) => println!("{}: {}", p.sender_name, p.post.message),
        // The server could not replay its buffer — refetch.
        WsUpdate::MissedMessages => { /* full resync */ }
        _ => {}
    }
}
```

Joining a call:

```rust
use mattermost_calls::{config, CallSession, JoinOptions};

let discovery = config::discover(&client).await?;
let call = CallSession::join(
    &client,
    ws,
    JoinOptions { channel_id: channel_id.into(), ..Default::default() },
    &discovery,
).await?;

let track = call.unmute().await?;   // then write Opus samples into `track`
```

## Notes on the protocol

Mattermost's REST API is documented; its WebSocket payloads and the Calls
protocol largely are not. `docs/PROTOCOL.md` records what was found by reading
`mattermost/mattermost`, `mattermost-plugin-calls`, `rtcd` and `calls-common`,
including several behaviours that fail *silently* if you get them wrong.

## Android APK

The [Android build instructions](build-aux/android/README.md) and
`.forgejo/workflows/android.yml` build an arm64 APK on pushes to `android` or
`master`. Forgejo stores the unsigned APK as a workflow artifact and, when
signing secrets are configured, also uploads a signed APK.

## Forgejo Packages

The CI workflows publish downloads to the [Matras Packages tab](https://altlinux.space/toxblh/matras/packages):

- Signed Android arm64 APK: Generic package `matras-apk`.
- x86_64 and aarch64 Flatpak OCI images: Container packages
  `ru.toxblh.matras-x86_64-stable` and `ru.toxblh.matras-aarch64-stable`.
- ALT Sisyphus and p11 RPMs: ALT package `matras`, in separate `sisyphus` and
  `p11` groups. The RPM source is [matras.spec](matras.spec); its locked Rust
  dependencies are vendored by CI before the isolated Gear build.

Forgejo packages belong to the `toxblh` account. After each upload,
`build-aux/publish-package.sh` links the package to this repository so that it
appears on the Matras Packages tab. Install Flatpak from the OCI remote above
so `flatpak update` checks the published `stable` image.

To enable publishing, create a personal access token in
[Forgejo Settings → Applications](https://altlinux.space/user/settings/applications)
with `write:package` scope and **Public only** access. Save it as the
`PACKAGES_TOKEN` secret in [Matras Actions settings](https://altlinux.space/toxblh/matras/settings/actions/secrets).
Without this secret, CI still builds and uploads workflow artifacts but skips
Packages. Publishing the Android APK also requires the signing secrets listed
in [the Android instructions](build-aux/android/README.md); unsigned APKs are
kept only as workflow artifacts.

## Publishing the Flatpak

`.forgejo/workflows/flatpak.yml` builds a Flatpak OCI image for each supported
architecture and publishes `stable` plus `oci-test` tags through Forgejo's
Container registry. `build-aux/publish-oci-flatpak.py` uploads the OCI blobs and
manifest directly to the registry. The `PACKAGES_TOKEN` Actions secret needs
`write:package` scope. The published registry index carries Flatpak ref and
commit labels, so clients can discover and update the app through
`flatpak update`.

## Licence

GPL-3.0-only. See `LICENSE`.

Matras is an independent client. It is not affiliated with, endorsed by, or
sponsored by Mattermost, Inc.; "Mattermost" is used only to name the server
protocol it speaks.
