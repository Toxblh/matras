//! One call, whichever service carries it. The UI holds this rather than
//! either session, so the dock, audio and video do not care which it is.

use std::sync::Arc;

use tokio::sync::broadcast;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::client::{CallSession, CallUpdate};
use crate::error::{CallsError, Result};
use crate::gomon::GomonSession;
use crate::protocol::{CallReaction, CallState};

#[derive(Clone)]
pub enum AnyCall {
    Calls(Arc<CallSession>),
    Gomon(GomonSession),
}

/// The same call on either session.
macro_rules! either {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            AnyCall::Calls($s) => $e,
            AnyCall::Gomon($s) => $e,
        }
    };
}

/// For the Calls-only host controls: gomon moderates from its own web page.
macro_rules! calls_only {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            AnyCall::Calls($s) => $e,
            AnyCall::Gomon(_) => Err(CallsError::Unavailable(
                "host controls are not available in this call".into(),
            )),
        }
    };
}

impl AnyCall {
    pub fn is_gomon(&self) -> bool {
        matches!(self, AnyCall::Gomon(_))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CallUpdate> {
        either!(self, s => s.subscribe())
    }

    pub fn session_id(&self) -> &str {
        either!(self, s => s.session_id())
    }

    pub fn channel_id(&self) -> &str {
        either!(self, s => s.channel_id())
    }

    pub fn call_id(&self) -> String {
        either!(self, s => s.call_id())
    }

    pub fn last_state(&self) -> Option<Box<CallState>> {
        either!(self, s => s.last_state())
    }

    pub fn is_muted(&self) -> bool {
        either!(self, s => s.is_muted())
    }

    pub async fn unmute(&self) -> Result<Arc<TrackLocalStaticSample>> {
        either!(self, s => s.unmute().await)
    }

    pub async fn mute(&self) -> Result<()> {
        either!(self, s => s.mute().await)
    }

    pub async fn raise_hand(&self, raised: bool) -> Result<()> {
        either!(self, s => s.raise_hand(raised).await)
    }

    pub async fn react(&self, reaction: &CallReaction) -> Result<()> {
        match self {
            AnyCall::Calls(s) => s.react(reaction).await,
            AnyCall::Gomon(s) => s.react(&reaction.name).await,
        }
    }

    pub async fn start_video(&self) -> Result<Arc<TrackLocalStaticSample>> {
        either!(self, s => s.start_video().await)
    }

    pub async fn stop_video(&self) -> Result<()> {
        either!(self, s => s.stop_video().await)
    }

    pub async fn start_screen_share(&self) -> Result<Arc<TrackLocalStaticSample>> {
        either!(self, s => s.start_screen_share().await)
    }

    pub async fn stop_screen_share(&self) -> Result<()> {
        either!(self, s => s.stop_screen_share().await)
    }

    pub async fn leave(&self) -> Result<()> {
        either!(self, s => s.leave().await)
    }

    pub async fn host_mute(&self, session_id: &str) -> Result<()> {
        calls_only!(self, s => s.host_mute(session_id).await)
    }

    pub async fn host_mute_others(&self) -> Result<()> {
        calls_only!(self, s => s.host_mute_others().await)
    }

    pub async fn host_screen_off(&self, session_id: &str) -> Result<()> {
        calls_only!(self, s => s.host_screen_off(session_id).await)
    }

    pub async fn host_lower_hand(&self, session_id: &str) -> Result<()> {
        calls_only!(self, s => s.host_lower_hand(session_id).await)
    }

    pub async fn host_remove(&self, session_id: &str) -> Result<()> {
        calls_only!(self, s => s.host_remove(session_id).await)
    }

    pub async fn host_make(&self, new_host_id: &str) -> Result<()> {
        calls_only!(self, s => s.host_make(new_host_id).await)
    }

    pub async fn host_end_call(&self) -> Result<()> {
        calls_only!(self, s => s.host_end_call().await)
    }
}
