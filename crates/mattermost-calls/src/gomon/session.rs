//! A gomon call: the comms join, then LiveKit.
//!
//! LiveKit runs two peer connections. The *subscriber* is the server's: it
//! offers, we answer, and every remote track arrives on it. The *publisher* is
//! ours: we offer whenever we add a track. Each track is announced over the
//! signalling socket first (`AddTrack`), and the server only accepts media for
//! a track id it has acknowledged (`TrackPublished`). That announce-then-offer
//! order is the one thing LiveKit is strict about.
//!
//! The session speaks the same [`CallUpdate`] language as the Mattermost
//! Calls one, so the call dock, the audio and the video tiles work on either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mattermost_api::tls::tokio_tungstenite::tungstenite::Message as WsMessage;
use prost::Message as _;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use super::api::Comms;
use super::proto::{self, signal_request, signal_response};
use crate::client::{spawn_rtcp_drain, CallUpdate};
use crate::config::IceServerConfig;
use crate::error::{CallsError, Result};
use crate::protocol::{track_type, CallState, SessionState};
use crate::rtc;
use crate::signaling::CallsEvent;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(15);

/// What the UI knows that gomon does not: who the people in the room are on
/// Mattermost. LiveKit only has display names.
#[derive(Debug, Clone, Default)]
pub struct GomonJoin {
    /// Left empty for a ring, which does not say; the call is asked.
    pub channel_id: String,
    /// From the plugin: `start` or `join_url`.
    pub join_url: String,
    /// Display name → Mattermost user id, every spelling the UI could think of.
    pub people: HashMap<String, String>,
    /// Ours, so our own tile is us even if the name lookup misses.
    pub me: String,
}

struct Local {
    track: Arc<TrackLocalStaticSample>,
    /// LiveKit's id for it, from `TrackPublished`.
    sid: String,
}

#[derive(Default)]
struct Roster {
    /// By participant sid.
    people: HashMap<String, proto::ParticipantInfo>,
    /// Remote tracks that arrived before the participant update naming them.
    orphans: Vec<Arc<TrackRemote>>,
    speaking: HashMap<String, bool>,
}

struct Inner {
    channel_id: String,
    call_id: String,
    connection_id: String,
    my_sid: String,
    me: String,
    people: HashMap<String, String>,
    comms: Comms,
    out: mpsc::UnboundedSender<signal_request::Message>,
    publisher: Arc<RTCPeerConnection>,
    subscriber: Arc<RTCPeerConnection>,
    updates: broadcast::Sender<CallUpdate>,
    roster: StdMutex<Roster>,
    last_state: StdMutex<Option<Box<CallState>>>,
    started_at: i64,
    muted: AtomicBool,
    left: AtomicBool,
    audio: Mutex<Option<Local>>,
    video: Mutex<Option<Local>>,
    screen: Mutex<Option<Local>>,
    /// `TrackPublished` replies, by the cid we sent.
    published: StdMutex<HashMap<String, oneshot::Sender<proto::TrackInfo>>>,
    /// The answer to the publisher offer in flight.
    answer: StdMutex<Option<oneshot::Sender<proto::SessionDescription>>>,
    /// One publisher negotiation at a time.
    negotiating: Mutex<()>,
    /// Candidates that came before their connection had a remote description.
    early: StdMutex<[Vec<RTCIceCandidateInit>; 2]>,
}

#[derive(Clone)]
pub struct GomonSession {
    inner: Arc<Inner>,
}

impl GomonSession {
    /// Signs in with the `join_url`, joins the call and connects LiveKit.
    pub async fn join(options: GomonJoin) -> Result<GomonSession> {
        mattermost_api::tls::install_crypto_provider();
        let (comms, call_id) = Comms::redeem(&options.join_url).await?;
        // Per run, as the mobile app does: the server uses it for nothing but
        // knowing which of our devices answered a ring.
        let device_id = format!("matras-desktop-{}", std::process::id());
        let channel_id = match options.channel_id {
            id if !id.is_empty() => id,
            _ => comms.channel_of(&call_id).await?.unwrap_or_default(),
        };
        let joined = comms.join(&call_id, &device_id).await?;
        tracing::info!(call = %call_id, connection = %joined.connection_id, "gomon: joined, connecting LiveKit");

        let url = signal_url(&joined.livekit_url, &joined.token);
        let (stream, _) = mattermost_api::tls::tokio_tungstenite::connect_async_tls_with_config(
            url,
            None,
            false,
            mattermost_api::tls::ws_connector(),
        )
        .await
        .map_err(|e| CallsError::Disconnected(format!("LiveKit: {e}")))?;
        let (mut sink, mut source) = stream.split();

        // The first thing the server says is who is here and how to reach it.
        let join = tokio::time::timeout(SIGNAL_TIMEOUT, async {
            while let Some(frame) = source.next().await {
                let frame = frame.map_err(|e| CallsError::Disconnected(e.to_string()))?;
                if let Some(signal_response::Message::Join(join)) = decode(&frame) {
                    return Ok(join);
                }
            }
            Err(CallsError::Disconnected(
                "LiveKit closed before joining".into(),
            ))
        })
        .await
        .map_err(|_| CallsError::Timeout("LiveKit join"))??;

        let ice: Vec<IceServerConfig> = join
            .ice_servers
            .iter()
            .map(|s| IceServerConfig {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect();
        let publisher = rtc::peer_connection(&ice, false).await?;
        let subscriber = rtc::peer_connection(&ice, false).await?;

        let (out, mut outgoing) = mpsc::unbounded_channel::<signal_request::Message>();
        let (updates, _) = broadcast::channel(256);
        let me = join.participant.clone().unwrap_or_default();

        let inner = Arc::new(Inner {
            channel_id,
            call_id,
            connection_id: joined.connection_id,
            my_sid: me.sid.clone(),
            me: options.me,
            people: options.people,
            comms,
            out,
            publisher,
            subscriber,
            updates,
            roster: StdMutex::default(),
            last_state: StdMutex::default(),
            started_at: now_ms(),
            muted: AtomicBool::new(true),
            left: AtomicBool::new(false),
            audio: Mutex::default(),
            video: Mutex::default(),
            screen: Mutex::default(),
            published: StdMutex::default(),
            answer: StdMutex::default(),
            negotiating: Mutex::default(),
            early: StdMutex::default(),
        });

        // Writer, with the keepalive the server expects at its interval.
        let ping_every = Duration::from_secs(join.ping_interval.max(5) as u64);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(ping_every);
            tick.tick().await;
            loop {
                let message = tokio::select! {
                    m = outgoing.recv() => match m { Some(m) => m, None => break },
                    _ = tick.tick() => signal_request::Message::PingReq(proto::Ping {
                        timestamp: now_ms(),
                        rtt: 0,
                    }),
                };
                let leaving = matches!(message, signal_request::Message::Leave(_));
                let bytes = proto::SignalRequest {
                    message: Some(message),
                }
                .encode_to_vec();
                if sink.send(WsMessage::Binary(bytes.into())).await.is_err() || leaving {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let session = GomonSession {
            inner: inner.clone(),
        };
        session.wire_peer_connections();

        // Reader.
        {
            let session = session.clone();
            tokio::spawn(async move {
                while let Some(frame) = source.next().await {
                    let Ok(frame) = frame else { break };
                    if let Some(message) = decode(&frame) {
                        session.on_signal(message).await;
                    }
                }
                if !session.inner.left.swap(true, Ordering::SeqCst) {
                    tracing::warn!("gomon: LiveKit signalling closed");
                    let _ = session.inner.updates.send(CallUpdate::Ended);
                }
            });
        }

        {
            let mut roster = inner.roster.lock().unwrap();
            for p in std::iter::once(me).chain(join.other_participants) {
                roster.people.insert(p.sid.clone(), p);
            }
        }
        let _ = inner.updates.send(CallUpdate::Joined {
            session_id: inner.connection_id.clone(),
        });
        session.publish_roster();
        Ok(session)
    }

    fn wire_peer_connections(&self) {
        for (pc, target) in [
            (&self.inner.publisher, proto::signal_target::PUBLISHER),
            (&self.inner.subscriber, proto::signal_target::SUBSCRIBER),
        ] {
            let out = self.inner.out.clone();
            pc.on_ice_candidate(Box::new(move |candidate| {
                if let Some(candidate) = candidate {
                    if let Ok(init) = candidate.to_json() {
                        if let Ok(json) = serde_json::to_string(&init) {
                            let _ =
                                out.send(signal_request::Message::Trickle(proto::TrickleRequest {
                                    candidate_init: json,
                                    target,
                                }));
                        }
                    }
                }
                Box::pin(async {})
            }));
        }

        let weak = Arc::downgrade(&self.inner);
        self.inner
            .subscriber
            .on_peer_connection_state_change(Box::new(move |state| {
                if let Some(inner) = weak.upgrade() {
                    let up = state == RTCPeerConnectionState::Connected;
                    if up || state == RTCPeerConnectionState::Failed {
                        let _ = inner.updates.send(CallUpdate::Connected(up));
                    }
                }
                Box::pin(async {})
            }));

        let weak = Arc::downgrade(&self.inner);
        self.inner
            .subscriber
            .on_track(Box::new(move |track, _receiver, _transceiver| {
                if let Some(inner) = weak.upgrade() {
                    GomonSession { inner }.attribute(track);
                }
                Box::pin(async {})
            }));

        // The server's `conf` packets: reactions, and a moderator muting us.
        let weak = Arc::downgrade(&self.inner);
        self.inner.subscriber.on_data_channel(Box::new(move |dc| {
            let weak = weak.clone();
            dc.on_message(Box::new(move |msg| {
                if let Some(inner) = weak.upgrade() {
                    GomonSession { inner }.on_data(&msg.data);
                }
                Box::pin(async {})
            }));
            Box::pin(async {})
        }));
    }

    async fn on_signal(&self, message: signal_response::Message) {
        use signal_response::Message as M;
        let inner = &self.inner;
        match message {
            M::Offer(offer) => {
                if let Err(e) = self.answer_subscriber(offer).await {
                    tracing::warn!("gomon: subscriber offer: {e}");
                }
            }
            M::Answer(answer) => {
                if let Some(tx) = inner.answer.lock().unwrap().take() {
                    let _ = tx.send(answer);
                }
            }
            M::Trickle(trickle) => {
                let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&trickle.candidate_init)
                else {
                    return;
                };
                let (pc, slot) = if trickle.target == proto::signal_target::PUBLISHER {
                    (&inner.publisher, 0)
                } else {
                    (&inner.subscriber, 1)
                };
                if pc.remote_description().await.is_none() {
                    inner.early.lock().unwrap()[slot].push(init);
                } else if let Err(e) = pc.add_ice_candidate(init).await {
                    tracing::debug!("gomon: candidate: {e}");
                }
            }
            M::Update(update) => {
                let orphans = {
                    let mut roster = inner.roster.lock().unwrap();
                    for p in update.participants {
                        let stale = roster
                            .people
                            .get(&p.sid)
                            .is_some_and(|held| held.version > p.version);
                        if stale {
                            continue;
                        }
                        if p.state == proto::participant_state::DISCONNECTED {
                            roster.people.remove(&p.sid);
                        } else {
                            roster.people.insert(p.sid.clone(), p);
                        }
                    }
                    std::mem::take(&mut roster.orphans)
                };
                self.publish_roster();
                for track in orphans {
                    self.attribute(track);
                }
            }
            M::TrackPublished(published) => {
                if let Some(tx) = inner.published.lock().unwrap().remove(&published.cid) {
                    let _ = tx.send(published.track.unwrap_or_default());
                }
            }
            M::Mute(mute) => {
                // A moderator muted one of our tracks; only the microphone has
                // anything on screen that has to follow.
                let ours = inner.audio.lock().await.as_ref().map(|l| l.sid.clone());
                if ours.as_deref() == Some(mute.sid.as_str()) && mute.muted {
                    inner.muted.store(true, Ordering::SeqCst);
                    let _ = inner.updates.send(CallUpdate::MuteChanged { muted: true });
                }
            }
            M::SpeakersChanged(changed) => {
                let events: Vec<CallsEvent> = {
                    let mut roster = inner.roster.lock().unwrap();
                    changed
                        .speakers
                        .into_iter()
                        .filter_map(|s| {
                            let was = roster.speaking.insert(s.sid.clone(), s.active);
                            if was == Some(s.active) {
                                return None;
                            }
                            let p = roster.people.get(&s.sid)?;
                            Some(CallsEvent::UserSpeaking {
                                user_id: self.user_for(p),
                                session_id: p.identity.clone(),
                                speaking: s.active,
                            })
                        })
                        .collect()
                };
                for event in events {
                    let _ = inner.updates.send(CallUpdate::Participant(event));
                }
            }
            M::Leave(leave) => {
                tracing::info!(reason = leave.reason, "gomon: the server ended our session");
                if !inner.left.swap(true, Ordering::SeqCst) {
                    let _ = inner.publisher.close().await;
                    let _ = inner.subscriber.close().await;
                    let _ = inner.updates.send(CallUpdate::Ended);
                }
            }
            M::Join(_)
            | M::RefreshToken(_)
            | M::TrackUnpublished(_)
            | M::Pong(_)
            | M::PongResp(_) => {}
        }
    }

    async fn answer_subscriber(&self, offer: proto::SessionDescription) -> Result<()> {
        let pc = &self.inner.subscriber;
        pc.set_remote_description(RTCSessionDescription::offer(offer.sdp)?)
            .await?;
        self.flush_early(1).await;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer.clone()).await?;
        let _ = self
            .inner
            .out
            .send(signal_request::Message::Answer(proto::SessionDescription {
                r#type: "answer".into(),
                sdp: answer.sdp,
            }));
        Ok(())
    }

    async fn flush_early(&self, slot: usize) {
        let pending = std::mem::take(&mut self.inner.early.lock().unwrap()[slot]);
        let pc = if slot == 0 {
            &self.inner.publisher
        } else {
            &self.inner.subscriber
        };
        for init in pending {
            let _ = pc.add_ice_candidate(init).await;
        }
    }

    /// Offers the publisher connection and waits for the server's answer.
    async fn negotiate(&self) -> Result<()> {
        let _turn = self.inner.negotiating.lock().await;
        let pc = &self.inner.publisher;
        let (tx, rx) = oneshot::channel();
        *self.inner.answer.lock().unwrap() = Some(tx);
        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        let _ = self
            .inner
            .out
            .send(signal_request::Message::Offer(proto::SessionDescription {
                r#type: "offer".into(),
                sdp: offer.sdp,
            }));
        let answer = tokio::time::timeout(SIGNAL_TIMEOUT, rx)
            .await
            .map_err(|_| CallsError::Timeout("LiveKit answer"))?
            .map_err(|_| CallsError::Disconnected("LiveKit closed".into()))?;
        pc.set_remote_description(RTCSessionDescription::answer(answer.sdp)?)
            .await?;
        self.flush_early(0).await;
        Ok(())
    }

    /// Announces a track, adds it and renegotiates.
    async fn publish(&self, kind: &str, source: i32) -> Result<Local> {
        let video = source != proto::track_source::MICROPHONE;
        let cid = format!("{kind}-{}", self.inner.connection_id);
        let track = Arc::new(TrackLocalStaticSample::new(
            if video {
                RTCRtpCodecCapability {
                    mime_type: "video/VP8".to_owned(),
                    clock_rate: 90_000,
                    ..Default::default()
                }
            } else {
                RTCRtpCodecCapability {
                    mime_type: "audio/opus".to_owned(),
                    clock_rate: rtc::OPUS_CLOCK_RATE,
                    channels: rtc::OPUS_CHANNELS,
                    sdp_fmtp_line: rtc::OPUS_FMTP.to_owned(),
                    rtcp_feedback: vec![],
                }
            },
            cid.clone(),
            format!("{}|{kind}", self.inner.connection_id),
        ));

        let (tx, rx) = oneshot::channel();
        self.inner.published.lock().unwrap().insert(cid.clone(), tx);
        let _ = self
            .inner
            .out
            .send(signal_request::Message::AddTrack(proto::AddTrackRequest {
                cid: cid.clone(),
                name: kind.to_string(),
                r#type: if video {
                    proto::track_type::VIDEO
                } else {
                    proto::track_type::AUDIO
                },
                source,
                ..Default::default()
            }));
        let info = tokio::time::timeout(SIGNAL_TIMEOUT, rx)
            .await
            .map_err(|_| CallsError::Timeout("LiveKit track publish"))?
            .map_err(|_| CallsError::Disconnected("LiveKit closed".into()))?;

        let sender = self
            .inner
            .publisher
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        spawn_rtcp_drain(sender);
        self.negotiate().await?;
        Ok(Local {
            track,
            sid: info.sid,
        })
    }

    fn set_track_muted(&self, sid: &str, muted: bool) {
        let _ = self
            .inner
            .out
            .send(signal_request::Message::Mute(proto::MuteTrackRequest {
                sid: sid.to_string(),
                muted,
            }));
    }

    /// Publishes a track the first time, unmutes it after that.
    async fn enable(&self, kind: &str, source: i32) -> Result<Arc<TrackLocalStaticSample>> {
        let slot = match source {
            proto::track_source::MICROPHONE => &self.inner.audio,
            proto::track_source::CAMERA => &self.inner.video,
            _ => &self.inner.screen,
        };
        let mut guard = slot.lock().await;
        if let Some(local) = guard.as_ref() {
            self.set_track_muted(&local.sid, false);
            return Ok(local.track.clone());
        }
        let local = self.publish(kind, source).await?;
        let track = local.track.clone();
        *guard = Some(local);
        Ok(track)
    }

    async fn disable(&self, source: i32) {
        let slot = match source {
            proto::track_source::MICROPHONE => &self.inner.audio,
            proto::track_source::CAMERA => &self.inner.video,
            _ => &self.inner.screen,
        };
        if let Some(local) = slot.lock().await.as_ref() {
            self.set_track_muted(&local.sid, true);
        }
    }

    /// Ties a remote track to a participant. LiveKit names the stream
    /// `<participant sid>|<track sid>`; the roster says what kind it is.
    fn attribute(&self, track: Arc<TrackRemote>) {
        let stream = track.stream_id();
        let (participant, track_sid) = stream
            .split_once('|')
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .unwrap_or_else(|| (stream.clone(), track.id()));
        let found = {
            let mut roster = self.inner.roster.lock().unwrap();
            let found = roster.people.get(&participant).and_then(|p| {
                let info = p.tracks.iter().find(|t| t.sid == track_sid)?;
                Some((p.identity.clone(), info.source))
            });
            if found.is_none() {
                roster.orphans.push(track.clone());
            }
            found
        };
        let Some((session_id, source)) = found else {
            return;
        };
        let track_type = match source {
            proto::track_source::CAMERA => track_type::VIDEO,
            proto::track_source::SCREEN_SHARE => track_type::SCREEN,
            4 => track_type::SCREEN_AUDIO,
            _ => track_type::VOICE,
        };
        let _ = self.inner.updates.send(CallUpdate::RemoteTrack {
            session_id,
            track_type: track_type.to_string(),
            track,
        });
    }

    fn on_data(&self, bytes: &[u8]) {
        let Ok(packet) = proto::DataPacket::decode(bytes) else {
            return;
        };
        // Only the server's own packets, as the web client insists (§3.5).
        let Some(proto::data_packet::Value::User(user)) = packet.value else {
            return;
        };
        if !packet.participant_identity.is_empty() || user.topic.as_deref() != Some("conf") {
            return;
        }
        let Ok(conf) = serde_json::from_slice::<serde_json::Value>(&user.payload) else {
            return;
        };
        if conf.get("type").and_then(|t| t.as_str()) == Some("muted") {
            self.inner.muted.store(true, Ordering::SeqCst);
            let _ = self
                .inner
                .updates
                .send(CallUpdate::MuteChanged { muted: true });
        }
    }

    fn user_for(&self, p: &proto::ParticipantInfo) -> String {
        if p.sid == self.inner.my_sid {
            return self.inner.me.clone();
        }
        let name = participant_name(p);
        self.inner
            .people
            .get(&name)
            .cloned()
            // Somebody we cannot place — a guest, or a name spelled another
            // way. The connection id at least keeps them apart.
            .unwrap_or_else(|| p.identity.clone())
    }

    /// The roster as a Calls `CallState`, which is what the UI draws.
    fn publish_roster(&self) {
        let state = {
            let roster = self.inner.roster.lock().unwrap();
            let mut people: Vec<&proto::ParticipantInfo> = roster.people.values().collect();
            people.sort_by(|a, b| a.sid.cmp(&b.sid));
            let live = |p: &proto::ParticipantInfo, source: i32| {
                p.tracks.iter().any(|t| t.source == source && !t.muted)
            };
            CallState {
                id: self.inner.call_id.clone(),
                start_at: self.inner.started_at,
                sessions: people
                    .iter()
                    .map(|p| SessionState {
                        session_id: p.identity.clone(),
                        user_id: self.user_for(p),
                        unmuted: live(p, proto::track_source::MICROPHONE),
                        raised_hand: 0,
                        video: live(p, proto::track_source::CAMERA),
                    })
                    .collect(),
                screen_sharing_session_id: people
                    .iter()
                    .find(|p| live(p, proto::track_source::SCREEN_SHARE))
                    .map(|p| p.identity.clone())
                    .unwrap_or_default(),
                ..Default::default()
            }
        };
        *self.inner.last_state.lock().unwrap() = Some(Box::new(state.clone()));
        let _ = self.inner.updates.send(CallUpdate::State(Box::new(state)));
    }

    /// LiveKit display names by participant, for tiles of people the UI could
    /// not match to a Mattermost account.
    pub fn display_names(&self) -> HashMap<String, String> {
        self.inner
            .roster
            .lock()
            .unwrap()
            .people
            .values()
            .map(|p| (p.identity.clone(), participant_name(p)))
            .collect()
    }

    // ------------------------------------------------- what the UI calls

    pub fn subscribe(&self) -> broadcast::Receiver<CallUpdate> {
        self.inner.updates.subscribe()
    }

    pub fn session_id(&self) -> &str {
        &self.inner.connection_id
    }

    pub fn channel_id(&self) -> &str {
        &self.inner.channel_id
    }

    pub fn call_id(&self) -> String {
        self.inner.call_id.clone()
    }

    pub fn last_state(&self) -> Option<Box<CallState>> {
        self.inner.last_state.lock().unwrap().clone()
    }

    pub fn is_muted(&self) -> bool {
        self.inner.muted.load(Ordering::SeqCst)
    }

    pub async fn unmute(&self) -> Result<Arc<TrackLocalStaticSample>> {
        let track = self
            .enable(track_type::VOICE, proto::track_source::MICROPHONE)
            .await?;
        self.inner.muted.store(false, Ordering::SeqCst);
        let _ = self
            .inner
            .updates
            .send(CallUpdate::MuteChanged { muted: false });
        Ok(track)
    }

    pub async fn mute(&self) -> Result<()> {
        self.disable(proto::track_source::MICROPHONE).await;
        self.inner.muted.store(true, Ordering::SeqCst);
        let _ = self
            .inner
            .updates
            .send(CallUpdate::MuteChanged { muted: true });
        Ok(())
    }

    pub async fn start_video(&self) -> Result<Arc<TrackLocalStaticSample>> {
        self.enable(track_type::VIDEO, proto::track_source::CAMERA)
            .await
    }

    pub async fn stop_video(&self) -> Result<()> {
        self.disable(proto::track_source::CAMERA).await;
        Ok(())
    }

    pub async fn start_screen_share(&self) -> Result<Arc<TrackLocalStaticSample>> {
        self.enable(track_type::SCREEN, proto::track_source::SCREEN_SHARE)
            .await
    }

    pub async fn stop_screen_share(&self) -> Result<()> {
        self.disable(proto::track_source::SCREEN_SHARE).await;
        Ok(())
    }

    pub async fn raise_hand(&self, raised: bool) -> Result<()> {
        self.inner
            .comms
            .raise_hand(&self.inner.call_id, raised)
            .await
    }

    /// gomon has six reactions; anything else is sent as the closest one.
    pub async fn react(&self, emoji: &str) -> Result<()> {
        let reaction = match emoji {
            "clap" => "clap",
            "heart" | "hearts" => "heart",
            "laughing" | "joy" | "smile" => "laugh",
            "open_mouth" | "astonished" => "wow",
            "tada" => "party",
            _ => "thumbs_up",
        };
        self.inner
            .comms
            .react(&self.inner.call_id, &self.inner.connection_id, reaction)
            .await
    }

    pub async fn leave(&self) -> Result<()> {
        if self.inner.left.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self
            .inner
            .out
            .send(signal_request::Message::Leave(proto::LeaveRequest {
                can_reconnect: false,
                reason: proto::disconnect_reason::CLIENT_INITIATED,
            }));
        if let Err(e) = self.inner.comms.leave(&self.inner.connection_id).await {
            tracing::warn!("gomon: leave: {e}");
        }
        let _ = self.inner.publisher.close().await;
        let _ = self.inner.subscriber.close().await;
        let _ = self.inner.updates.send(CallUpdate::Ended);
        Ok(())
    }
}

fn decode(frame: &WsMessage) -> Option<signal_response::Message> {
    match frame {
        WsMessage::Binary(bytes) => proto::SignalResponse::decode(bytes.as_ref()).ok()?.message,
        _ => None,
    }
}

fn participant_name(p: &proto::ParticipantInfo) -> String {
    serde_json::from_str::<serde_json::Value>(&p.metadata)
        .ok()
        .and_then(|m| m.get("display_name")?.as_str().map(str::to_owned))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| p.name.clone())
}

/// `wss://sfu` + token → the signalling URL. An `https` URL works too.
fn signal_url(livekit_url: &str, token: &str) -> String {
    let base = livekit_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!(
        "{base}/rtc?access_token={token}&auto_subscribe=1&sdk=rust&protocol=15&adaptive_stream=0"
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_urls_use_websocket_schemes() {
        assert_eq!(
            signal_url("https://sfu.example/", "T"),
            "wss://sfu.example/rtc?access_token=T&auto_subscribe=1&sdk=rust&protocol=15&adaptive_stream=0"
        );
        assert!(signal_url("wss://sfu.example", "T").starts_with("wss://sfu.example/rtc?"));
    }

    #[test]
    fn signal_frames_round_trip() {
        // Encoded the way the server encodes a join, decoded the way we do.
        let response = proto::SignalResponse {
            message: Some(signal_response::Message::Join(proto::JoinResponse {
                participant: Some(proto::ParticipantInfo {
                    sid: "PA_1".into(),
                    identity: "conn_1".into(),
                    ..Default::default()
                }),
                subscriber_primary: true,
                ping_interval: 10,
                ..Default::default()
            })),
        };
        let frame = WsMessage::Binary(response.encode_to_vec().into());
        let Some(signal_response::Message::Join(join)) = decode(&frame) else {
            panic!("not a join");
        };
        assert_eq!(join.participant.unwrap().identity, "conn_1");
        assert_eq!(join.ping_interval, 10);
    }

    #[test]
    fn names_come_from_metadata_first() {
        let p = proto::ParticipantInfo {
            name: "fallback".into(),
            metadata: r#"{"display_name":"Антон Пальгунов","kind":"employee"}"#.into(),
            ..Default::default()
        };
        assert_eq!(participant_name(&p), "Антон Пальгунов");
        let bare = proto::ParticipantInfo {
            name: "fallback".into(),
            ..Default::default()
        };
        assert_eq!(participant_name(&bare), "fallback");
    }
}
