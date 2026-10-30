//! A native client for [Mattermost Calls].
//!
//! Calls has no published protocol specification. Everything here was derived
//! by reading three repositories: `mattermost-plugin-calls` (the plugin),
//! `rtcd` (the SFU — and, crucially, its `client/` package, which is a working
//! headless Go client of this exact protocol), and `calls-common` (the shared
//! TypeScript types). Where the code and the folklore disagree, the code wins;
//! the surprises are called out in module docs.
//!
//! # Shape of the thing
//!
//! ```text
//! this crate ──HTTPS──► /plugins/com.mattermost.calls/…      discovery
//!            ──WSS───► /api/v4/websocket                     signalling
//!                        (custom_com.mattermost.calls_* actions)
//!                              │
//!                        Calls plugin
//!                              ▼
//!                        rtcd SFU (pion)
//!            ◄────────DTLS/SRTP/ICE────────►                 media
//! ```
//!
//! A third-party client never speaks to the SFU's signalling directly — it has
//! no credentials for it. Everything goes through Mattermost's own websocket.
//!
//! # Three things that fail silently
//!
//! 1. **SDP must be a binary msgpack frame** carrying zlib-compressed JSON. Send
//!    it as JSON and the plugin's `[]byte` type assertion fails; your offer
//!    disappears with no error.
//! 2. **Renegotiation must hold the signalling lock.** The SFU is the impolite
//!    peer: it drops an offer that races its own, again without an error.
//! 3. **Audio level must be negotiated.** Voice activity is computed
//!    server-side from `ssrc-audio-level`; without that header extension you
//!    are audible but never appear to be speaking.
//!
//! # Usage
//!
//! ```no_run
//! use mattermost_api::{Client, WebSocket};
//! use mattermost_calls::{config, CallSession, JoinOptions};
//!
//! # async fn run() -> mattermost_calls::Result<()> {
//! let client = Client::new("https://mm.example.com")?;
//! client.set_token("…");
//! let ws = WebSocket::connect(client.websocket_url(), client.token().unwrap());
//!
//! let discovery = config::discover(&client).await?;
//! let call = CallSession::join(
//!     &client,
//!     ws,
//!     JoinOptions { channel_id: "…".into(), ..Default::default() },
//!     &discovery,
//! )
//! .await?;
//!
//! call.unmute().await?;
//! # Ok(())
//! # }
//! ```
//!
//! [Mattermost Calls]: https://github.com/mattermost/mattermost-plugin-calls

pub mod active;
pub mod client;
pub mod config;
pub mod dc;
pub mod error;
pub mod gomon;
pub mod protocol;
pub mod rtc;
pub mod signaling;

pub use active::AnyCall;
pub use client::{write_audio_sample, CallSession, CallUpdate, JoinOptions};
pub use config::{
    decline, discover, dismiss_notification, host_control, set_recording, CallsConfig,
    ChannelCallState, Discovery, VersionInfo,
};
pub use error::{CallsError, Result};
pub use protocol::{CallReaction, CallState, SessionState, PLUGIN_ID};
pub use signaling::CallsEvent;

// Re-exported so a UI can drive audio without depending on `webrtc` directly,
// and without risking a version skew with the one we negotiate with.
pub use webrtc::media::Sample;
pub use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
pub use webrtc::track::track_remote::TrackRemote;
/// `marshal()` on an RTP packet, for feeding a decoder the bytes as they came.
pub use webrtc::util::Marshal;
