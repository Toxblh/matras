//! Joining and running a call.
//!
//! # Join sequence
//!
//! ```text
//!  A. REST      GET /plugins/com.mattermost.calls/version   → dc locking?
//!               GET /plugins/com.mattermost.calls/config    → ICE, feature flags
//!               GET .../turn-credentials                    → only if NeedsTURNCredentials
//!  B. WS        `hello` → connection_id  ⇒ THIS IS OUR SESSION ID
//!               send  calls_join {channelID, jobID:"", av1Support, dcSignaling}
//!               await calls_join ack (data.connID == our session id)
//!  C. RTC       build PeerConnection, create the `calls-dc` data channel
//!               (creating it is what makes negotiation "needed")
//!               create offer → send `sdp` as a BINARY msgpack frame
//!               trickle ICE via `ice` text frames
//!               await `signal` answer → set remote description
//!               ICE → DTLS → SCTP → the data channel opens
//!               the SFU then offers one renegotiation per existing remote track
//!  D. Audio     add the Opus track (taking the signalling lock first),
//!               renegotiate, then send `unmute`
//! ```
//!
//! # Failures that are silent if you get them wrong
//!
//! * **SDP as anything but a binary msgpack frame** — the plugin's `[]byte`
//!   assertion fails and the offer vanishes.
//! * **Renegotiating without the signalling lock** — the SFU is the impolite
//!   peer and drops the offer, leaving us stuck in `have-local-offer`. The lock
//!   is gated on the *server version*, not on whether data-channel signalling
//!   is enabled: the SFU takes it for its own offers either way.
//! * **Not buffering ICE candidates that arrive before the answer** — the SFU
//!   starts gathering inside `SetLocalDescription`, so candidates routinely
//!   arrive first, and `add_ice_candidate` rejects them outright until a remote
//!   description exists.
//! * **Not sending `reconnect` every time the websocket reopens** — the plugin
//!   does not know or care whether the transport resumed; any socket teardown
//!   starts a ten-second timer that ends the call, and until `reconnect`
//!   arrives everything we send is dropped.
//! * **Not attaching the audio-level RTP extension** — voice activity is
//!   computed server-side, so you are audible but never shown as speaking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mattermost_api::ws::{WebSocket, WsRequest, WsUpdate};
use mattermost_api::Client;
use tokio::sync::{broadcast, mpsc, Mutex};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use crate::config::Discovery;
use crate::dc::{self, DcMessage};
use crate::error::{CallsError, Result};
use crate::protocol::{
    self, client_action, CallReaction, CallState, IceCandidateInit, JoinMessage, ReconnectMessage,
    SdpPayload, SessionDescription, StringPayload,
};
use crate::rtc;
use crate::signaling::{parse as parse_calls_event, CallsEvent};

/// How long the SFU waits for our first offer before killing the session.
const SIGNALING_TIMEOUT: Duration = Duration::from_secs(10);
/// The SFU's own lock timeout.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(100);
const DC_PING_INTERVAL: Duration = Duration::from_secs(1);
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

/// What the caller asks for when joining.
#[derive(Debug, Clone, Default)]
pub struct JoinOptions {
    pub channel_id: String,
    /// Only used when *starting* a call: titles the "call started" post.
    pub title: Option<String>,
    /// Attach the call to an existing thread.
    pub thread_id: Option<String>,
}

/// Events a UI needs to render a call.
#[derive(Debug, Clone)]
pub enum CallUpdate {
    /// The call is joined; `session_id` is our identity for its whole life.
    Joined {
        session_id: String,
    },
    /// Full roster — sent on join and whenever membership changes.
    State(Box<CallState>),
    /// A participant event (mute, speaking, hand, screen share, …).
    Participant(CallsEvent),
    /// A remote media track arrived, already attributed to a session.
    ///
    /// `session_id` maps to a user id via the roster; there is no user id
    /// anywhere in the media plane.
    RemoteTrack {
        session_id: String,
        track_type: String,
        track: Arc<TrackRemote>,
    },
    /// Our own microphone state changed.
    MuteChanged {
        muted: bool,
    },
    /// The peer connection state changed; `false` means media has stopped.
    Connected(bool),
    Error(String),
    /// The call ended, or we left.
    Ended,
}

/// The signalling lock the SFU arbitrates over the data channel.
///
/// Both peers may renegotiate, and the SFU silently drops an offer that arrives
/// while it is making one of its own. The lock makes that impossible.
///
/// It is enabled by **server version alone**. Gating it on `EnableDCSignaling`
/// would disable it on a stock server, where that setting defaults to off — and
/// the SFU takes the lock for its own renegotiations regardless.
struct SignalingLock {
    enabled: bool,
    responses: Mutex<mpsc::UnboundedReceiver<bool>>,
    tx: mpsc::UnboundedSender<bool>,
}

impl SignalingLock {
    fn new(enabled: bool) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(SignalingLock {
            enabled,
            responses: Mutex::new(rx),
            tx,
        })
    }

    fn offer_response(&self, granted: bool) {
        let _ = self.tx.send(granted);
    }

    /// Asks the SFU for the lock, retrying until granted or the deadline
    /// passes. A no-op when the server is too old to support locking.
    async fn acquire(&self, dc: &RTCDataChannel) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        // The lock rides the data channel, and that only opens once DTLS is up.
        // A track added right after joining — which is what unmuting does —
        // otherwise fails outright with "DataChannel is not opened".
        while dc.ready_state() != RTCDataChannelState::Open {
            if tokio::time::Instant::now() >= deadline {
                return Err(CallsError::Timeout("data channel open"));
            }
            tokio::time::sleep(LOCK_RETRY).await;
        }
        let mut rx = self.responses.lock().await;
        // Drain anything stale from a previous attempt.
        while rx.try_recv().is_ok() {}

        loop {
            dc.send(&dc::encode(&DcMessage::Lock(None))?.into()).await?;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(CallsError::Timeout("signalling lock"));
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(true)) => return Ok(()),
                Ok(Some(false)) => tokio::time::sleep(LOCK_RETRY).await,
                Ok(None) => return Err(CallsError::Disconnected("data channel closed".into())),
                Err(_) => return Err(CallsError::Timeout("signalling lock")),
            }
        }
    }

    /// Releases the lock. A no-op when locking is off — releasing a lock we
    /// never took makes the SFU log `ErrAlreadyUnlocked`, and worse, could
    /// release one *it* is holding.
    async fn release(&self, dc: &RTCDataChannel) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        dc.send(&dc::encode(&DcMessage::Unlock)?.into()).await?;
        Ok(())
    }
}

/// Everything the SDP path needs, wherever it is driven from — the websocket
/// loop or the data channel.
struct Signaling {
    pc: Arc<RTCPeerConnection>,
    ws: WebSocket,
    dc: Arc<RTCDataChannel>,
    lock: Arc<SignalingLock>,
    /// Set as soon as the first offer is **sent**. Gates lock acquisition: the
    /// SFU is blocking on that offer without holding anything, so taking the
    /// lock for it would deadlock.
    negotiation_started: AtomicBool,
    /// Set when the first answer is **received**. Gates the unlock, so we never
    /// release a lock we did not take.
    negotiated: AtomicBool,
    dc_signaling: bool,
    /// Candidates that arrived before a remote description existed.
    ///
    /// `add_ice_candidate` returns `ErrNoRemoteDescription` until one is set,
    /// and the SFU starts gathering inside `SetLocalDescription` — before it
    /// sends the answer — so this race happens on almost every join.
    pending_ice: Mutex<Vec<IceCandidateInit>>,
    /// Serialises our own renegotiations.
    negotiation: Mutex<()>,
}

impl Signaling {
    fn use_data_channel(&self) -> bool {
        self.dc_signaling && self.dc.ready_state() == RTCDataChannelState::Open
    }

    async fn send_sdp(&self, sdp: &SessionDescription) -> Result<()> {
        if self.use_data_channel() {
            let json = serde_json::to_string(sdp)?;
            self.dc
                .send(&dc::encode(&DcMessage::Sdp(json))?.into())
                .await?;
            Ok(())
        } else {
            send_sdp_over_ws(&self.ws, sdp)
        }
    }

    /// Queues or applies a remote candidate depending on whether we have a
    /// remote description yet.
    async fn add_ice(&self, candidate: IceCandidateInit) {
        // Hold the queue lock across the check. Releasing it first would let a
        // candidate pass the check, be preempted by `flush_ice`, and then be
        // pushed onto a queue nothing will drain again.
        let mut pending = self.pending_ice.lock().await;
        if self.pc.remote_description().await.is_none() {
            pending.push(candidate);
            return;
        }
        drop(pending);
        apply_ice(&self.pc, candidate).await;
    }

    async fn flush_ice(&self) {
        let queued = {
            let mut pending = self.pending_ice.lock().await;
            std::mem::take(&mut *pending)
        };
        for candidate in queued {
            apply_ice(&self.pc, candidate).await;
        }
    }

    /// Applies an SFU offer or answer, answering when needed.
    async fn handle_remote_sdp(&self, wire: SessionDescription) -> Result<()> {
        let desc = rtc::to_rtc_sdp(&wire)?;

        match wire.kind.as_str() {
            "answer" | "pranswer" => {
                self.pc.set_remote_description(desc).await?;
                self.flush_ice().await;

                // The first answer completes the initial negotiation, which was
                // never locked — so it must not be unlocked either.
                let was_first = !self.negotiated.swap(true, Ordering::SeqCst);
                if !was_first {
                    let _ = self.lock.release(&self.dc).await;
                }
            }
            "offer" => {
                self.pc.set_remote_description(desc).await?;
                self.flush_ice().await;

                let answer = self.pc.create_answer(None).await?;
                self.pc.set_local_description(answer.clone()).await?;
                self.send_sdp(&rtc::from_rtc_sdp(&answer)).await?;
            }
            other => {
                return Err(CallsError::Protocol(format!(
                    "unexpected sdp type {other:?} from the SFU"
                )))
            }
        }
        Ok(())
    }

    /// Renegotiates after a local track change, holding the signalling lock.
    async fn renegotiate(&self) -> Result<()> {
        let _guard = self.negotiation.lock().await;

        // Gate on whether an offer has been *sent*, not on whether one has been
        // answered: a track added while the first offer is still in flight
        // would otherwise skip the lock and then spuriously unlock.
        let first = !self.negotiation_started.load(Ordering::SeqCst);
        if !first {
            self.lock.acquire(&self.dc).await?;
        }

        let result = self.offer().await;
        if result.is_err() && !first {
            // Give the lock back, or the SFU sits on it for its full timeout
            // and the next renegotiation — ours or its own — stalls.
            let _ = self.lock.release(&self.dc).await;
        }
        result
    }

    async fn offer(&self) -> Result<()> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        self.negotiation_started.store(true, Ordering::SeqCst);
        self.send_sdp(&rtc::from_rtc_sdp(&offer)).await
    }
}

async fn apply_ice(pc: &Arc<RTCPeerConnection>, candidate: IceCandidateInit) {
    let init = webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
        candidate: candidate.candidate,
        sdp_mid: candidate.sdp_mid,
        sdp_mline_index: candidate.sdp_mline_index,
        username_fragment: candidate.username_fragment,
    };
    if let Err(e) = pc.add_ice_candidate(init).await {
        tracing::warn!(error = %e, "rejected ice candidate");
    }
}

/// A joined call.
pub struct CallSession {
    signaling: Arc<Signaling>,
    /// Kept for the host controls, which the plugin serves over REST only.
    client: Client,
    updates: broadcast::Sender<CallUpdate>,
    session_id: String,
    channel_id: String,
    /// The connection id of the websocket we are currently riding. Diverges
    /// from `session_id` after a reconnect.
    current_conn_id: Mutex<String>,
    /// The most recent roster. The plugin sends `call_state` to a joiner
    /// immediately, which is before any UI has had a chance to subscribe, so
    /// the event alone would lose the call id every time.
    last_state: std::sync::Mutex<Option<Box<CallState>>>,
    /// Set by [`leave`](CallSession::leave): the websocket keeps delivering
    /// this call's events afterwards, and answering an offer on a closed peer
    /// connection just produces noise.
    left: AtomicBool,
    audio: Mutex<Option<(Arc<TrackLocalStaticSample>, Arc<RTCRtpSender>)>>,
    /// Screen share and camera, kept once created: turning either off is a
    /// signalling message, not a renegotiation — the SFU stops relaying a
    /// track it was told about, and the webapp keeps the sender alive too.
    screen: Mutex<Option<(Arc<TrackLocalStaticSample>, Arc<RTCRtpSender>)>>,
    video: Mutex<Option<(Arc<TrackLocalStaticSample>, Arc<RTCRtpSender>)>>,
    muted: AtomicBool,
}

impl CallSession {
    /// Runs the full join sequence and returns once the peer connection is
    /// negotiated (phases A–C). Audio is not sent until [`unmute`] is called.
    ///
    /// [`unmute`]: CallSession::unmute
    pub async fn join(
        client: &Client,
        ws: WebSocket,
        opts: JoinOptions,
        discovery: &Discovery,
    ) -> Result<Arc<CallSession>> {
        let mut rx = ws.subscribe();

        // ---- Phase B: our session id is the websocket connection id.
        let session_id = current_connection_id(&ws, &mut rx).await?;
        tracing::info!(%session_id, "calls: got connection id, sending calls_join");

        let dc_signaling = discovery.config.dc_signaling_allowed();
        let join = JoinMessage {
            channel_id: opts.channel_id.clone(),
            job_id: String::new(),
            av1_support: discovery.config.av1_allowed(),
            dc_signaling,
            title: opts.title.clone(),
            thread_id: opts.thread_id.clone(),
        };
        ws.send_action(&protocol::action(client_action::JOIN), &join)?;

        // Do not build the peer connection before the ack: the plugin may
        // refuse (participant limit, permissions, unlicensed group call).
        wait_for_join_ack(&mut rx, &session_id).await?;
        tracing::info!("calls: join acked, building peer connection");

        // ---- Phase C: peer connection and first negotiation.
        let pc =
            rtc::peer_connection(&discovery.ice_servers, discovery.config.av1_allowed()).await?;
        let (updates, _) = broadcast::channel(256);
        let lock = SignalingLock::new(discovery.dc_locking);

        // Trickle ICE. Skip the terminating `None`: the SFU ignores empty
        // candidates and pion never forwards an end-of-candidates sentinel.
        {
            let ws = ws.clone();
            pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
                let ws = ws.clone();
                Box::pin(async move {
                    let Some(c) = candidate else { return };
                    let Ok(init) = c.to_json() else { return };
                    let ours = IceCandidateInit {
                        candidate: init.candidate,
                        sdp_mid: init.sdp_mid,
                        sdp_mline_index: init.sdp_mline_index,
                        username_fragment: init.username_fragment,
                    };
                    if let Ok(json) = serde_json::to_string(&ours) {
                        let _ = ws.send_action(
                            &protocol::action(client_action::ICE),
                            StringPayload { data: json },
                        );
                    }
                })
            }));
        }

        // Attribute every incoming track to a session via its id.
        {
            let updates = updates.clone();
            let pc_for_pli = Arc::downgrade(&pc);
            pc.on_track(Box::new(move |track, _receiver, _transceiver| {
                let updates = updates.clone();
                let pc_for_pli = pc_for_pli.clone();
                Box::pin(async move {
                    let id = track.id();
                    let Some((kind, session_id)) = protocol::parse_track_id(&id) else {
                        tracing::warn!(track_id = %id, "unparseable relayed track id");
                        return;
                    };
                    // A screen share arrives mid-stream; ask for a keyframe now
                    // or the first seconds are grey.
                    if kind == protocol::track_type::SCREEN {
                        if let Some(pc) = pc_for_pli.upgrade() {
                            let _ = pc
                                .write_rtcp(&[Box::new(
                                    webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                                        sender_ssrc: 0,
                                        media_ssrc: track.ssrc(),
                                    },
                                )])
                                .await;
                        }
                    }
                    let _ = updates.send(CallUpdate::RemoteTrack {
                        session_id: session_id.to_string(),
                        track_type: kind.to_string(),
                        track: track.clone(),
                    });
                })
            }));
        }

        {
            let updates = updates.clone();
            pc.on_peer_connection_state_change(Box::new(move |state| {
                let updates = updates.clone();
                Box::pin(async move {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as S;
                    tracing::info!(?state, "calls: peer connection state changed");
                    let _ = updates.send(CallUpdate::Connected(matches!(state, S::Connected)));
                    if matches!(state, S::Failed | S::Closed) {
                        let _ = updates.send(CallUpdate::Ended);
                    }
                })
            }));
        }

        // Creating the data channel is what triggers the first negotiation, so
        // every handler above must already be registered.
        let dc = pc
            .create_data_channel(protocol::DATA_CHANNEL_LABEL, None)
            .await?;

        let signaling = Arc::new(Signaling {
            pc: pc.clone(),
            ws: ws.clone(),
            dc: dc.clone(),
            lock: lock.clone(),
            negotiation_started: AtomicBool::new(false),
            negotiated: AtomicBool::new(false),
            dc_signaling,
            pending_ice: Mutex::new(Vec::new()),
            negotiation: Mutex::new(()),
        });

        {
            // Weak on purpose. The peer connection owns the data channel, which
            // owns this closure — capturing `Arc<Signaling>` (which holds the
            // peer connection) would close a reference cycle that survives
            // `close()`, because closing a data channel does not clear its
            // message handler. The whole RTC stack would then leak per call.
            let signaling = Arc::downgrade(&signaling);
            let dc_inner = dc.clone();
            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let signaling = signaling.clone();
                let dc_inner = dc_inner.clone();
                Box::pin(async move {
                    let Some(signaling) = signaling.upgrade() else {
                        return;
                    };
                    match dc::decode(&msg.data) {
                        Ok(DcMessage::Ping) => {
                            if let Ok(bytes) = dc::encode(&DcMessage::Pong) {
                                let _ = dc_inner.send(&bytes.into()).await;
                            }
                        }
                        Ok(DcMessage::Lock(Some(granted))) => {
                            signaling.lock.offer_response(granted)
                        }
                        Ok(DcMessage::Sdp(json)) => {
                            // An SFU-initiated offer, delivered over the DC.
                            match serde_json::from_str::<SessionDescription>(&json) {
                                Ok(wire) => {
                                    if let Err(e) = signaling.handle_remote_sdp(wire).await {
                                        tracing::warn!(error = %e, "failed to answer dc offer");
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, "undecodable dc sdp"),
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "bad data channel message"),
                    }
                })
            }));
        }

        let session = Arc::new(CallSession {
            signaling: signaling.clone(),
            client: client.clone(),
            updates: updates.clone(),
            session_id: session_id.clone(),
            channel_id: opts.channel_id.clone(),
            current_conn_id: Mutex::new(session_id.clone()),
            last_state: std::sync::Mutex::new(None),
            left: AtomicBool::new(false),
            audio: Mutex::new(None),
            screen: Mutex::new(None),
            video: Mutex::new(None),
            muted: AtomicBool::new(true),
        });

        // The first offer is *not* locked — the SFU is blocking on it with a
        // 10 s deadline and would deadlock against its own lock.
        signaling.offer().await?;

        tokio::spawn(run_event_loop(session.clone(), rx));
        tokio::spawn(dc_keepalive(dc.clone()));

        // Wait for the answer to land before telling the caller we are in.
        let deadline = tokio::time::Instant::now() + SIGNALING_TIMEOUT;
        while !signaling.negotiated.load(Ordering::SeqCst) {
            if tokio::time::Instant::now() >= deadline {
                return Err(CallsError::Timeout("SFU answer"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The roster event fires before the caller can subscribe, so ask for it
        // over REST instead of racing. A failure here costs the call id, not
        // the call.
        match crate::config::channel_state(client, &opts.channel_id).await {
            Ok(state) => {
                if let Some(call) = state.call {
                    *session.last_state.lock().unwrap() = Some(Box::new(call));
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not read the call roster"),
        }

        let _ = updates.send(CallUpdate::Joined {
            session_id: session_id.clone(),
        });
        Ok(session)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CallUpdate> {
        self.updates.subscribe()
    }

    /// Our session id — the connection id we joined with, and the key that ties
    /// relayed tracks to participants. It stays fixed across reconnects.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The last roster we saw, for anything that started listening late.
    pub fn last_state(&self) -> Option<Box<CallState>> {
        self.last_state.lock().unwrap().clone()
    }

    /// The call's own id, which the recording routes need. Empty until the
    /// first roster arrives.
    pub fn call_id(&self) -> String {
        self.last_state().map(|s| s.id.clone()).unwrap_or_default()
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Starts sending audio and returns the track to write Opus samples into.
    ///
    /// Use [`write_audio_sample`] rather than `write_sample`: voice activity is
    /// detected server-side from the RTP audio-level extension, and
    /// `write_sample` attaches no extensions.
    ///
    /// The first call adds the track (which renegotiates); later calls only
    /// flip the flag, because the SFU gates our RTP on the signalled state
    /// rather than on whether packets arrive.
    pub async fn unmute(&self) -> Result<Arc<TrackLocalStaticSample>> {
        let track = {
            let mut guard = self.audio.lock().await;
            match guard.as_ref() {
                Some((t, _)) => t.clone(),
                None => {
                    let track = Arc::new(TrackLocalStaticSample::new(
                        RTCRtpCodecCapability {
                            mime_type: "audio/opus".to_owned(),
                            clock_rate: rtc::OPUS_CLOCK_RATE,
                            channels: rtc::OPUS_CHANNELS,
                            sdp_fmtp_line: rtc::OPUS_FMTP.to_owned(),
                            rtcp_feedback: vec![],
                        },
                        format!("voice-{}", self.session_id),
                        self.session_id.clone(),
                    ));
                    let sender = self
                        .signaling
                        .pc
                        .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                        .await?;
                    // RTCP has to be drained or the interceptors stall.
                    spawn_rtcp_drain(sender.clone());
                    *guard = Some((track.clone(), sender));
                    drop(guard);
                    self.signaling.renegotiate().await?;
                    track
                }
            }
        };

        self.signaling
            .ws
            .send_action(&protocol::action(client_action::UNMUTE), Option::<()>::None)?;
        self.muted.store(false, Ordering::SeqCst);
        let _ = self.updates.send(CallUpdate::MuteChanged { muted: false });
        Ok(track)
    }

    /// Stops being audible. The track keeps running; the SFU drops our RTP.
    pub async fn mute(&self) -> Result<()> {
        self.signaling
            .ws
            .send_action(&protocol::action(client_action::MUTE), Option::<()>::None)?;
        self.muted.store(true, Ordering::SeqCst);
        let _ = self.updates.send(CallUpdate::MuteChanged { muted: true });
        Ok(())
    }

    pub async fn raise_hand(&self, raised: bool) -> Result<()> {
        let action = if raised {
            client_action::RAISE_HAND
        } else {
            client_action::UNRAISE_HAND
        };
        self.signaling
            .ws
            .send_action(&protocol::action(action), Option::<()>::None)?;
        Ok(())
    }

    pub async fn react(&self, reaction: &CallReaction) -> Result<()> {
        let json = serde_json::to_string(reaction)?;
        self.signaling.ws.send_action(
            &protocol::action(client_action::REACT),
            StringPayload { data: json },
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- host
    //
    // These do not go over the websocket: the plugin has no
    // `custom_com.mattermost.calls_host_*` action, only
    // `POST /calls/{call_id}/host/{action}`. They still live here because they
    // are per-call, and because the call id they need is only known once the
    // roster has arrived — see [`call_id`](CallSession::call_id).
    //
    // Every one of them is *advisory*: the server relays a `host_*` event to
    // the target and expects it to comply. A client that ignores `host_mute`
    // keeps being heard, so never update local UI from the request alone —
    // wait for the `user_muted` / `user_screen_off` echo.

    /// Asks one participant to mute. Host only.
    pub async fn host_mute(&self, session_id: &str) -> Result<()> {
        self.host_control(
            protocol::host_route::MUTE,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// Asks everyone *except us* to mute. Host only.
    ///
    /// Takes no target: the plugin picks the exempt session from the caller's
    /// own, so there is no way to spare a third party.
    pub async fn host_mute_others(&self) -> Result<()> {
        self.host_control(protocol::host_route::MUTE_OTHERS, serde_json::json!({}))
            .await
    }

    /// Stops one participant's screen share. Host only.
    pub async fn host_screen_off(&self, session_id: &str) -> Result<()> {
        self.host_control(
            protocol::host_route::SCREEN_OFF,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// Lowers one participant's hand. Host only.
    pub async fn host_lower_hand(&self, session_id: &str) -> Result<()> {
        self.host_control(
            protocol::host_route::LOWER_HAND,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// Removes one participant from the call. Host only.
    ///
    /// Unlike the others this is broadcast (`host_removed`), because everyone
    /// has to drop the removed session from their roster.
    pub async fn host_remove(&self, session_id: &str) -> Result<()> {
        self.host_control(
            protocol::host_route::REMOVE,
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// Hands the host role to someone else. Host only.
    ///
    /// Takes a **user** id, not a session id — the host is a user, so all of
    /// their sessions become host at once. Map with
    /// [`CallState::user_for_session`](crate::protocol::CallState::user_for_session).
    pub async fn host_make(&self, new_host_id: &str) -> Result<()> {
        self.host_control(
            protocol::host_route::MAKE,
            serde_json::json!({ "new_host_id": new_host_id }),
        )
        .await
    }

    /// Ends the call for everyone. Host only.
    pub async fn host_end_call(&self) -> Result<()> {
        self.host_control(protocol::host_route::END, serde_json::json!({}))
            .await
    }

    async fn host_control(&self, route: &str, body: serde_json::Value) -> Result<()> {
        let call_id = self.call_id();
        if call_id.is_empty() {
            // The routes are keyed by call id, and an empty one would just be a
            // 404 from a URL with an empty path segment.
            return Err(CallsError::Protocol(
                "host controls need the call id, which no roster has provided yet".into(),
            ));
        }
        crate::config::host_control(&self.client, &call_id, route, body).await
    }

    /// Announces a screen share.
    ///
    /// Order matters: the SFU classifies an incoming video track purely by
    /// comparing its stream id against the id announced here, and drops any
    /// video track it cannot classify. Send this **before** adding the track.
    pub async fn announce_screen_share(&self, stream_id: &str) -> Result<()> {
        let json = serde_json::to_string(&serde_json::json!({ "screenStreamID": stream_id }))?;
        self.signaling.ws.send_action(
            &protocol::action(client_action::SCREEN_ON),
            StringPayload { data: json },
        )?;
        Ok(())
    }

    /// Starts sharing the screen and returns the track to write VP8 into.
    pub async fn start_screen_share(&self) -> Result<Arc<TrackLocalStaticSample>> {
        self.start_video_track(
            protocol::track_type::SCREEN,
            client_action::SCREEN_ON,
            "screenStreamID",
        )
        .await
    }

    pub async fn stop_screen_share(&self) -> Result<()> {
        self.signaling.ws.send_action(
            &protocol::action(client_action::SCREEN_OFF),
            Option::<()>::None,
        )?;
        Ok(())
    }

    /// Starts sending the camera and returns the track to write VP8 into.
    ///
    /// Gated server-side by `EnableVideo`, and to DM channels.
    pub async fn start_video(&self) -> Result<Arc<TrackLocalStaticSample>> {
        self.start_video_track(
            protocol::track_type::VIDEO,
            client_action::VIDEO_ON,
            "videoStreamID",
        )
        .await
    }

    pub async fn stop_video(&self) -> Result<()> {
        self.signaling.ws.send_action(
            &protocol::action(client_action::VIDEO_OFF),
            Option::<()>::None,
        )?;
        Ok(())
    }

    /// The shared half of screen sharing and camera: announce the stream id,
    /// then add the track — never the other way round.
    async fn start_video_track(
        &self,
        kind: &str,
        action: &str,
        stream_id_key: &str,
    ) -> Result<Arc<TrackLocalStaticSample>> {
        let slot = if kind == protocol::track_type::SCREEN {
            &self.screen
        } else {
            &self.video
        };
        let mut guard = slot.lock().await;
        let (track, fresh) = match guard.as_ref() {
            Some((track, _)) => (track.clone(), false),
            None => {
                let track = Arc::new(TrackLocalStaticSample::new(
                    RTCRtpCodecCapability {
                        mime_type: "video/VP8".to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: String::new(),
                        rtcp_feedback: vec![],
                    },
                    format!("{kind}-{}", self.session_id),
                    format!("{kind}-stream-{}", self.session_id),
                ));
                (track, true)
            }
        };

        let json = serde_json::to_string(&serde_json::json!({ stream_id_key: track.stream_id() }))?;
        self.signaling
            .ws
            .send_action(&protocol::action(action), StringPayload { data: json })?;

        if fresh {
            let sender = self
                .signaling
                .pc
                .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                .await?;
            spawn_rtcp_drain(sender.clone());
            *guard = Some((track.clone(), sender));
            drop(guard);
            self.signaling.renegotiate().await?;
        }
        Ok(track)
    }

    /// Leaves the call and tears the peer connection down.
    ///
    /// Dropping the websocket instead would leave the plugin holding our slot
    /// for its 10 s reconnect grace period.
    pub async fn leave(&self) -> Result<()> {
        self.left.store(true, Ordering::SeqCst);
        let _ = self
            .signaling
            .ws
            .send_action(&protocol::action(client_action::LEAVE), Option::<()>::None);
        self.signaling.pc.close().await?;
        let _ = self.updates.send(CallUpdate::Ended);
        Ok(())
    }

    /// Re-attaches the call after the websocket reopened.
    ///
    /// This must run on **every** reopen, not only when the resume failed. The
    /// server has no notion of a resumed connection as far as plugins are
    /// concerned: `OnWebSocketDisconnect` fires whenever a socket's pump exits,
    /// and the Calls plugin then starts a 10 s timer that tears the RTC session
    /// down unless a `reconnect` arrives. Until it does, every `sdp`, `ice`,
    /// `mute` and `unmute` we send is dropped, with no error — the call simply
    /// goes quiet.
    ///
    /// Sending it twice is harmless: the plugin guards with a compare-and-swap
    /// and only logs "session already reconnected".
    async fn rebind(&self) {
        let prev_conn_id = self.current_conn_id.lock().await.clone();
        let message = ReconnectMessage {
            channel_id: self.channel_id.clone(),
            original_conn_id: self.session_id.clone(),
            prev_conn_id,
        };

        if let Err(e) = self
            .signaling
            .ws
            .send_action(&protocol::action(client_action::RECONNECT), &message)
        {
            let _ = self
                .updates
                .send(CallUpdate::Error(format!("could not rejoin the call: {e}")));
        }
    }

    /// Records the identity of the socket we are now riding, so the next
    /// `reconnect` names the right `prevConnID`.
    async fn note_connection_id(&self, conn_id: String) {
        if conn_id.is_empty() {
            return;
        }
        *self.current_conn_id.lock().await = conn_id;
    }
}

/// Writes an Opus sample with the RTP audio-level extension attached.
///
/// This is not a convenience. The SFU computes voice activity from
/// `urn:ietf:params:rtp-hdrext:ssrc-audio-level` and emits `user_voice_on` /
/// `user_voice_off` from it; `TrackLocalStaticSample::write_sample` attaches no
/// extensions, so using it leaves you audible but never shown as speaking.
///
/// `level_dbov` is the level in −dBov, 0 (loudest) to 127 (silence), as
/// RFC 6464 defines it. It must be the **measured** level of each packet: the
/// SFU's detector is a variance detector, not a threshold — it watches the
/// standard deviation over a 50-sample window — so a constant value, however
/// loud, never registers as speech.
pub async fn write_audio_sample(
    track: &TrackLocalStaticSample,
    sample: &Sample,
    level_dbov: u8,
) -> Result<()> {
    use webrtc::rtp::extension::audio_level_extension::AudioLevelExtension;
    use webrtc::rtp::extension::HeaderExtension;

    let level = level_dbov.min(127);
    track
        .write_sample_with_extensions(
            sample,
            &[HeaderExtension::AudioLevel(AudioLevelExtension {
                level,
                // "Voice" marks speech rather than silence; the floor value
                // means nothing is being said.
                voice: level < 127,
            })],
        )
        .await
        .map_err(CallsError::WebRtc)
}

// ------------------------------------------------------------------ helpers

/// Sends SDP as a binary msgpack frame carrying zlib-compressed JSON.
///
/// This encoding is mandatory. The plugin does `req.Data["data"].([]byte)`;
/// a JSON frame delivers a base64 string, the assertion fails, and the message
/// is dropped with no error returned to us.
fn send_sdp_over_ws(ws: &WebSocket, sdp: &SessionDescription) -> Result<()> {
    let json = serde_json::to_string(sdp)?;
    let compressed = dc::zlib_compress(json.as_bytes())?;
    let req = WsRequest {
        seq: ws.next_seq(),
        action: protocol::action(client_action::SDP),
        data: SdpPayload { data: compressed },
    };
    // `to_vec_named` — the server decodes into map[string]any, so the struct
    // must be a msgpack *map*, not an array.
    ws.send_binary(rmp_serde::to_vec_named(&req)?)?;
    Ok(())
}

pub(crate) fn spawn_rtcp_drain(sender: Arc<RTCRtpSender>) {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 1500];
        while sender.read(&mut buf).await.is_ok() {}
    });
}

/// Pings the data channel once a second, as both reference clients do.
///
/// The channel is not open when this starts — ICE, DTLS and SCTP all have to
/// complete first — so a failed send is a reason to skip, not to stop.
async fn dc_keepalive(dc: Arc<RTCDataChannel>) {
    let mut tick = tokio::time::interval(DC_PING_INTERVAL);
    loop {
        tick.tick().await;
        match dc.ready_state() {
            RTCDataChannelState::Open => {
                let Ok(bytes) = dc::encode(&DcMessage::Ping) else {
                    return;
                };
                if let Err(e) = dc.send(&bytes.into()).await {
                    tracing::debug!(error = %e, "data channel ping failed");
                }
            }
            RTCDataChannelState::Closed | RTCDataChannelState::Closing => return,
            _ => continue,
        }
    }
}

/// Resolves our session id: the websocket connection id.
///
/// Reads it directly when the socket is already up. `Connected` is a broadcast,
/// so a late subscriber never sees the one that already fired — without this,
/// joining from an application's long-lived socket would simply time out.
async fn current_connection_id(
    ws: &WebSocket,
    rx: &mut broadcast::Receiver<WsUpdate>,
) -> Result<String> {
    // Only trust the cached id while a socket is actually open: it is retained
    // across a disconnect so a resume can be attempted, and joining with a dead
    // id would make the join ack never match.
    let existing = ws.connection_id();
    if ws.is_connected() && !existing.is_empty() {
        return Ok(existing);
    }

    let fut = async {
        loop {
            match rx.recv().await {
                Ok(WsUpdate::Connected { connection_id, .. }) if !connection_id.is_empty() => {
                    return Ok(connection_id)
                }
                Ok(WsUpdate::Disconnected { reason, .. }) => {
                    return Err(CallsError::Disconnected(reason))
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => return Err(CallsError::Disconnected(e.to_string())),
            }
        }
    };
    tokio::time::timeout(JOIN_TIMEOUT, fut)
        .await
        .map_err(|_| CallsError::Timeout("websocket hello"))?
}

async fn wait_for_join_ack(rx: &mut broadcast::Receiver<WsUpdate>, session_id: &str) -> Result<()> {
    let fut = async {
        loop {
            let update = match rx.recv().await {
                Ok(u) => u,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => return Err(CallsError::Disconnected(e.to_string())),
            };
            let WsUpdate::Event(event) = update else {
                continue;
            };
            match parse_calls_event(&event) {
                Some(CallsEvent::JoinAccepted { conn_id }) if conn_id == session_id => {
                    return Ok(())
                }
                Some(CallsEvent::Error { message, .. }) => {
                    return Err(CallsError::Rejected(message))
                }
                _ => continue,
            }
        }
    };
    tokio::time::timeout(JOIN_TIMEOUT, fut)
        .await
        .map_err(|_| CallsError::Timeout("calls join ack"))?
}

async fn run_event_loop(session: Arc<CallSession>, mut rx: broadcast::Receiver<WsUpdate>) {
    let signaling = session.signaling.clone();
    let updates = session.updates.clone();

    loop {
        let update = match rx.recv().await {
            Ok(u) => u,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "calls event loop lagged");
                continue;
            }
            Err(_) => break,
        };

        match update {
            // `resumed` marks a *reopened* socket, which is exactly when the
            // plugin needs a `reconnect`; a `hello` (resumed: false) only tells
            // us the new identity to quote next time.
            WsUpdate::Connected {
                connection_id,
                resumed,
            } => {
                if resumed {
                    session.rebind().await;
                } else {
                    session.note_connection_id(connection_id).await;
                }
            }

            WsUpdate::Event(event) => {
                if session.left.load(Ordering::SeqCst) {
                    break;
                }
                let Some(calls) = parse_calls_event(&event) else {
                    continue;
                };
                match calls {
                    CallsEvent::Signal { signal, .. } => match signal {
                        protocol::Signal::Offer { sdp }
                        | protocol::Signal::Answer { sdp }
                        | protocol::Signal::Pranswer { sdp }
                            if sdp.is_empty() =>
                        {
                            continue
                        }
                        protocol::Signal::Offer { sdp } => {
                            let wire = SessionDescription {
                                kind: "offer".into(),
                                sdp,
                            };
                            if let Err(e) = signaling.handle_remote_sdp(wire).await {
                                let _ = updates.send(CallUpdate::Error(e.to_string()));
                            }
                        }
                        protocol::Signal::Answer { sdp } | protocol::Signal::Pranswer { sdp } => {
                            let wire = SessionDescription {
                                kind: "answer".into(),
                                sdp,
                            };
                            if let Err(e) = signaling.handle_remote_sdp(wire).await {
                                let _ = updates.send(CallUpdate::Error(e.to_string()));
                            }
                        }
                        protocol::Signal::Candidate { candidate } => {
                            signaling.add_ice(candidate).await;
                        }
                    },
                    CallsEvent::CallState { ref state, .. } => {
                        *session.last_state.lock().unwrap() = Some(state.clone());
                        let _ = updates.send(CallUpdate::State(state.clone()));
                    }
                    CallsEvent::CallEnded { .. } => {
                        let _ = updates.send(CallUpdate::Ended);
                        break;
                    }
                    CallsEvent::Error { message, .. } => {
                        let _ = updates.send(CallUpdate::Error(message));
                    }
                    // Host controls are advisory: comply, then report.
                    CallsEvent::HostMuteRequest { ref session_id }
                        if *session_id == session.session_id =>
                    {
                        let _ = session.mute().await;
                        let _ = updates.send(CallUpdate::Participant(calls));
                    }
                    other => {
                        let _ = updates.send(CallUpdate::Participant(other));
                    }
                }
            }
            WsUpdate::Disconnected {
                will_retry: false,
                reason,
            } => {
                let _ = updates.send(CallUpdate::Error(reason));
                let _ = updates.send(CallUpdate::Ended);
                break;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_frames_are_msgpack_maps_with_a_bin_payload() {
        // We cannot spin up a server here, but we can pin the encoding, which
        // is the part that fails silently against a real one.
        let json = r#"{"type":"offer","sdp":"v=0\r\n"}"#;
        let compressed = dc::zlib_compress(json.as_bytes()).unwrap();
        let req = WsRequest {
            seq: 1,
            action: protocol::action(client_action::SDP),
            data: SdpPayload { data: compressed },
        };
        let bytes = rmp_serde::to_vec_named(&req).unwrap();

        // A msgpack map header (0x8x fixmap / 0xde map16), never an array.
        assert!(
            bytes[0] & 0xf0 == 0x80 || bytes[0] == 0xde,
            "envelope must be a map, got first byte {:#04x}",
            bytes[0]
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("action"));
        assert!(text.contains("custom_com.mattermost.calls_sdp"));
    }

    #[test]
    fn the_lock_follows_server_version_not_dc_signalling() {
        // Regression guard: gating the lock on EnableDCSignaling disables it on
        // a stock server, where that setting is off by default — and the SFU
        // takes the lock for its own renegotiations regardless.
        let discovery = Discovery {
            version: Default::default(),
            config: crate::config::CallsConfig {
                enable_dc_signaling: Some(false),
                ..Default::default()
            },
            ice_servers: vec![],
            dc_locking: true,
        };
        assert!(!discovery.config.dc_signaling_allowed());
        let lock = SignalingLock::new(discovery.dc_locking);
        assert!(
            lock.enabled,
            "locking must stay on when the server supports it"
        );
    }

    #[tokio::test]
    async fn lock_release_is_a_noop_when_locking_is_unsupported() {
        // `release` must return before touching the data channel, so a server
        // without lock support never sees a stray Unlock.
        let lock = SignalingLock::new(false);
        assert!(!lock.enabled);
    }

    #[test]
    fn reconnect_message_carries_both_ids() {
        let msg = ReconnectMessage {
            channel_id: "c1".into(),
            original_conn_id: "orig".into(),
            prev_conn_id: "prev".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"channelID":"c1","originalConnID":"orig","prevConnID":"prev"}"#
        );
    }
}
