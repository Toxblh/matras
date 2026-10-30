//! The slice of LiveKit's signalling protocol (`livekit_rtc.proto`,
//! `livekit_models.proto`) a call participant needs.
//!
//! Written by hand rather than taken from the `livekit-protocol` crate, which
//! drags in pbjson, chrono and a megabyte of generated code for the few
//! messages used here. Protobuf skips fields it does not know, so leaving most
//! of each message out is safe; the tags are copied from livekit-protocol 0.8
//! and must stay exactly as they are.

use prost::{Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct SignalRequest {
    #[prost(oneof = "signal_request::Message", tags = "1, 2, 3, 4, 5, 8, 14, 16")]
    pub message: Option<signal_request::Message>,
}

pub mod signal_request {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        /// Our offer for the publisher connection.
        #[prost(message, tag = "1")]
        Offer(SessionDescription),
        /// Our answer to the server's subscriber offer.
        #[prost(message, tag = "2")]
        Answer(SessionDescription),
        #[prost(message, tag = "3")]
        Trickle(TrickleRequest),
        #[prost(message, tag = "4")]
        AddTrack(AddTrackRequest),
        #[prost(message, tag = "5")]
        Mute(MuteTrackRequest),
        #[prost(message, tag = "8")]
        Leave(LeaveRequest),
        /// The old ping, kept because a 1.x server still answers it.
        #[prost(int64, tag = "14")]
        Ping(i64),
        #[prost(message, tag = "16")]
        PingReq(Ping),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SignalResponse {
    #[prost(
        oneof = "signal_response::Message",
        tags = "1, 2, 3, 4, 5, 6, 8, 9, 10, 16, 17, 18, 20"
    )]
    pub message: Option<signal_response::Message>,
}

pub mod signal_response {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        Join(JoinResponse),
        /// The server's answer to our publisher offer.
        #[prost(message, tag = "2")]
        Answer(SessionDescription),
        /// The server offering the subscriber connection.
        #[prost(message, tag = "3")]
        Offer(SessionDescription),
        #[prost(message, tag = "4")]
        Trickle(TrickleRequest),
        #[prost(message, tag = "5")]
        Update(ParticipantUpdate),
        #[prost(message, tag = "6")]
        TrackPublished(TrackPublishedResponse),
        #[prost(message, tag = "8")]
        Leave(LeaveRequest),
        /// A moderator muted one of our tracks.
        #[prost(message, tag = "9")]
        Mute(MuteTrackRequest),
        #[prost(message, tag = "10")]
        SpeakersChanged(SpeakersChanged),
        #[prost(string, tag = "16")]
        RefreshToken(String),
        #[prost(message, tag = "17")]
        TrackUnpublished(TrackUnpublishedResponse),
        #[prost(int64, tag = "18")]
        Pong(i64),
        #[prost(message, tag = "20")]
        PongResp(Pong),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct JoinResponse {
    #[prost(message, optional, tag = "2")]
    pub participant: Option<ParticipantInfo>,
    #[prost(message, repeated, tag = "3")]
    pub other_participants: Vec<ParticipantInfo>,
    #[prost(string, tag = "4")]
    pub server_version: String,
    #[prost(message, repeated, tag = "5")]
    pub ice_servers: Vec<IceServer>,
    #[prost(bool, tag = "6")]
    pub subscriber_primary: bool,
    #[prost(int32, tag = "10")]
    pub ping_timeout: i32,
    #[prost(int32, tag = "11")]
    pub ping_interval: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct IceServer {
    #[prost(string, repeated, tag = "1")]
    pub urls: Vec<String>,
    #[prost(string, tag = "2")]
    pub username: String,
    #[prost(string, tag = "3")]
    pub credential: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionDescription {
    /// "offer" or "answer".
    #[prost(string, tag = "1")]
    pub r#type: String,
    #[prost(string, tag = "2")]
    pub sdp: String,
}

/// Which of the two peer connections a candidate belongs to.
pub mod signal_target {
    pub const PUBLISHER: i32 = 0;
    pub const SUBSCRIBER: i32 = 1;
}

#[derive(Clone, PartialEq, Message)]
pub struct TrickleRequest {
    /// `RTCIceCandidateInit` as JSON.
    #[prost(string, tag = "1")]
    pub candidate_init: String,
    #[prost(int32, tag = "2")]
    pub target: i32,
}

pub mod track_type {
    pub const AUDIO: i32 = 0;
    pub const VIDEO: i32 = 1;
}

pub mod track_source {
    pub const CAMERA: i32 = 1;
    pub const MICROPHONE: i32 = 2;
    pub const SCREEN_SHARE: i32 = 3;
}

#[derive(Clone, PartialEq, Message)]
pub struct AddTrackRequest {
    /// Our own id for the track, echoed back in `TrackPublishedResponse`. It
    /// has to be the media track id that goes into the SDP.
    #[prost(string, tag = "1")]
    pub cid: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int32, tag = "3")]
    pub r#type: i32,
    #[prost(uint32, tag = "4")]
    pub width: u32,
    #[prost(uint32, tag = "5")]
    pub height: u32,
    #[prost(bool, tag = "6")]
    pub muted: bool,
    #[prost(int32, tag = "8")]
    pub source: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TrackPublishedResponse {
    #[prost(string, tag = "1")]
    pub cid: String,
    #[prost(message, optional, tag = "2")]
    pub track: Option<TrackInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TrackUnpublishedResponse {
    #[prost(string, tag = "1")]
    pub track_sid: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct MuteTrackRequest {
    #[prost(string, tag = "1")]
    pub sid: String,
    #[prost(bool, tag = "2")]
    pub muted: bool,
}

pub mod disconnect_reason {
    pub const CLIENT_INITIATED: i32 = 1;
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveRequest {
    #[prost(bool, tag = "1")]
    pub can_reconnect: bool,
    #[prost(int32, tag = "2")]
    pub reason: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ParticipantUpdate {
    #[prost(message, repeated, tag = "1")]
    pub participants: Vec<ParticipantInfo>,
}

pub mod participant_state {
    pub const DISCONNECTED: i32 = 3;
}

#[derive(Clone, PartialEq, Message)]
pub struct ParticipantInfo {
    #[prost(string, tag = "1")]
    pub sid: String,
    /// The gomon connection id.
    #[prost(string, tag = "2")]
    pub identity: String,
    #[prost(int32, tag = "3")]
    pub state: i32,
    #[prost(message, repeated, tag = "4")]
    pub tracks: Vec<TrackInfo>,
    /// JSON: `principal_id`, `kind`, `display_name`, `call_id`, `avatar_url`.
    #[prost(string, tag = "5")]
    pub metadata: String,
    #[prost(string, tag = "9")]
    pub name: String,
    /// Grows with every change; an update older than what we hold is stale.
    #[prost(uint32, tag = "10")]
    pub version: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TrackInfo {
    #[prost(string, tag = "1")]
    pub sid: String,
    #[prost(int32, tag = "2")]
    pub r#type: i32,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(bool, tag = "4")]
    pub muted: bool,
    #[prost(int32, tag = "9")]
    pub source: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SpeakersChanged {
    #[prost(message, repeated, tag = "1")]
    pub speakers: Vec<SpeakerInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SpeakerInfo {
    /// Participant sid.
    #[prost(string, tag = "1")]
    pub sid: String,
    #[prost(float, tag = "2")]
    pub level: f32,
    #[prost(bool, tag = "3")]
    pub active: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ping {
    #[prost(int64, tag = "1")]
    pub timestamp: i64,
    #[prost(int64, tag = "2")]
    pub rtt: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Pong {
    #[prost(int64, tag = "1")]
    pub last_ping_timestamp: i64,
    #[prost(int64, tag = "2")]
    pub timestamp: i64,
}

/// What arrives on the subscriber's data channels.
#[derive(Clone, PartialEq, Message)]
pub struct DataPacket {
    #[prost(string, tag = "4")]
    pub participant_identity: String,
    #[prost(oneof = "data_packet::Value", tags = "2")]
    pub value: Option<data_packet::Value>,
}

pub mod data_packet {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(message, tag = "2")]
        User(UserPacket),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct UserPacket {
    #[prost(bytes = "vec", tag = "2")]
    pub payload: Vec<u8>,
    #[prost(string, optional, tag = "4")]
    pub topic: Option<String>,
}
