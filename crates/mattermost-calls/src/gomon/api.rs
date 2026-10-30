//! gomon's two REST surfaces: the Mattermost plugin, under the ordinary MM
//! session (§2 of `docs/GOMON.md`), and the comms API, under a token of its
//! own that the plugin hands over as a one-time code (§3).

use mattermost_api::Client;
use serde::Deserialize;
use serde_json::json;

use crate::error::{CallsError, Result};

pub const PLUGIN_ID: &str = "ru.corp.comms";

fn plugin(client: &Client, path: &str) -> String {
    client.plugin_url(PLUGIN_ID, &format!("/api/v1{path}"))
}

/// Whether the server runs gomon at all (§1).
pub async fn available(client: &Client) -> Result<bool> {
    let manifests: Vec<serde_json::Value> = client
        .get_url(
            &format!("{}/api/v4/plugins/webapp", client.site_url()),
            "webapp plugins",
        )
        .await?;
    Ok(manifests
        .iter()
        .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(PLUGIN_ID)))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Started {
    pub call_id: String,
    /// The channel already had a call, and this is it.
    #[serde(default)]
    pub reused: bool,
    pub join_url: String,
}

/// Starts a call in a channel, or hands back the one already running there.
/// In a DM or group DM this rings the others; in a channel it rings nobody.
pub async fn start(client: &Client, channel_id: &str, video: bool) -> Result<Started> {
    Ok(client
        .post_url(
            &plugin(client, "/calls"),
            Some(&json!({ "channel_id": channel_id, "video": video })),
            "start a call",
        )
        .await?)
}

/// A fresh `join_url` for a running call. Each one is good for a single join
/// within two minutes, so this is asked for right before joining.
pub async fn join_url(client: &Client, call_id: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Reply {
        join_url: String,
    }
    let reply: Reply = client
        .post_url(
            &plugin(client, &format!("/join/{call_id}")),
            Option::<&()>::None,
            "join a call",
        )
        .await?;
    Ok(reply.join_url)
}

/// The call running in a channel, as the plugin sees it.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelCall {
    pub call_id: String,
    #[serde(default)]
    pub participant_ids: String,
    #[serde(default)]
    pub in_call: bool,
}

pub async fn channel_call(client: &Client, channel_id: &str) -> Result<Option<ChannelCall>> {
    #[derive(Deserialize)]
    struct Reply {
        call: Option<ChannelCall>,
    }
    let reply: Reply = client
        .get_url(
            &plugin(client, &format!("/channels/{channel_id}/call")),
            "channel call",
        )
        .await?;
    Ok(reply.call)
}

/// Every live call in the user's channels, with who is in each: channel id →
/// Mattermost user ids. Startup only; the plugin scans its whole store for it.
pub async fn active(client: &Client) -> Result<Vec<(String, Vec<String>)>> {
    #[derive(Deserialize)]
    struct Live {
        channel_id: String,
        #[serde(default)]
        participant_ids: String,
    }
    #[derive(Deserialize)]
    struct Reply {
        calls: Vec<Live>,
    }
    let reply: Reply = client
        .get_url(&plugin(client, "/active"), "active calls")
        .await?;
    Ok(reply
        .calls
        .into_iter()
        .map(|c| (c.channel_id, split_ids(&c.participant_ids)))
        .collect())
}

/// The plugin joins user ids with commas.
pub fn split_ids(ids: &str) -> Vec<String> {
    ids.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// What to do with a ring (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// "This device is ringing": must be sent within 10 s of `incoming`, or
    /// the server gives up on delivering the call.
    Ack,
    Accept,
    Decline,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Answered {
    #[serde(default)]
    pub state: String,
    /// Present on accept.
    #[serde(default)]
    pub join_url: Option<String>,
}

pub async fn answer(client: &Client, invitation_id: &str, how: Answer) -> Result<Answered> {
    let verb = match how {
        Answer::Ack => "ack",
        Answer::Accept => "accept",
        Answer::Decline => "decline",
    };
    Ok(client
        .post_url(
            &plugin(client, &format!("/invitations/{invitation_id}/{verb}")),
            Option::<&()>::None,
            "answer a call",
        )
        .await?)
}

/// The comms API, signed in for one join.
pub struct Comms {
    client: Client,
}

/// Where LiveKit is and how to get in.
#[derive(Debug, Clone, Deserialize)]
pub struct Joined {
    pub connection_id: String,
    pub livekit_url: String,
    pub token: String,
}

impl Comms {
    /// Redeems the code in a `join_url`. The comms API lives on the same origin
    /// as the call page the URL points at.
    pub async fn redeem(join_url: &str) -> Result<(Comms, String)> {
        let (origin, call_id, code) = split_join_url(join_url)
            .ok_or_else(|| CallsError::Protocol(format!("unexpected join url {join_url:?}")))?;
        let client = Client::new(&origin)?;
        #[derive(Deserialize)]
        struct Reply {
            token: String,
        }
        let reply: Reply = client
            .post_url(
                &format!("{origin}/v1/handoff/redeem"),
                Some(&json!({ "code": code })),
                "call sign-in",
            )
            .await?;
        client.set_token(&reply.token);
        Ok((Comms { client }, call_id))
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1{path}", self.client.site_url())
    }

    /// The Mattermost channel a call belongs to, from its context key
    /// `mm:<channel id>`. A ring does not say; the call does.
    pub async fn channel_of(&self, call_id: &str) -> Result<Option<String>> {
        let call: serde_json::Value = self
            .client
            .get_url(&self.url(&format!("/calls/{call_id}")), "call")
            .await?;
        Ok(call
            .pointer("/context/key")
            .and_then(|k| k.as_str())
            .and_then(|k| k.strip_prefix("mm:"))
            .map(str::to_owned))
    }

    pub async fn join(&self, call_id: &str, device_id: &str) -> Result<Joined> {
        Ok(self
            .client
            .post_url(
                &self.url(&format!("/calls/{call_id}/join")),
                Some(&json!({ "idempotency_key": idempotency_key(), "device_id": device_id })),
                "join the call",
            )
            .await?)
    }

    /// Leaves for good. Without it the server waits out the connection's
    /// grace period before telling everyone we are gone.
    pub async fn leave(&self, connection_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .client
            .post_url(
                &self.url(&format!("/connections/{connection_id}/leave")),
                Some(&json!({ "idempotency_key": idempotency_key() })),
                "leave the call",
            )
            .await?;
        Ok(())
    }

    pub async fn raise_hand(&self, call_id: &str, raised: bool) -> Result<()> {
        let _: serde_json::Value = self
            .client
            .post_url(
                &self.url(&format!("/calls/{call_id}/hand")),
                Some(&json!({ "idempotency_key": idempotency_key(), "raised": raised })),
                "raise a hand",
            )
            .await?;
        Ok(())
    }

    pub async fn react(&self, call_id: &str, connection_id: &str, reaction: &str) -> Result<()> {
        let _: serde_json::Value = self
            .client
            .post_url(
                &self.url(&format!("/calls/{call_id}/reactions")),
                Some(&json!({ "reaction": reaction, "connection_id": connection_id })),
                "react",
            )
            .await?;
        Ok(())
    }
}

/// Every comms command needs a fresh one, 8 to 128 characters.
fn idempotency_key() -> String {
    format!("matras-{:032x}", rand_u128())
}

fn rand_u128() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    // Two independently seeded SipHashers: random enough for a key that only
    // has to be unique per user, without a dependency for it.
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        h.finish() as u128
    };
    half() << 64 | half()
}

/// `https://host/call/<id>#code=<hex>` → (`https://host`, id, code).
fn split_join_url(url: &str) -> Option<(String, String, String)> {
    let (path, fragment) = url.split_once('#')?;
    let code = fragment
        .split('&')
        .find_map(|kv| kv.strip_prefix("code="))?;
    let (origin, call_id) = path.split_once("/call/")?;
    let call_id = call_id.split(['?', '/']).next()?;
    (!code.is_empty() && !call_id.is_empty())
        .then(|| (origin.to_string(), call_id.to_string(), code.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_urls_split_into_origin_call_and_code() {
        assert_eq!(
            split_join_url("https://meet.example/call/call_4594#code=abcdef0123"),
            Some((
                "https://meet.example".into(),
                "call_4594".into(),
                "abcdef0123".into()
            ))
        );
        assert_eq!(
            split_join_url("https://meet.example/call/call_1?autojoin=1#code=ff"),
            Some(("https://meet.example".into(), "call_1".into(), "ff".into()))
        );
        assert_eq!(split_join_url("https://meet.example/call/call_1"), None);
    }

    #[test]
    fn idempotency_keys_are_long_enough_and_differ() {
        let (a, b) = (idempotency_key(), idempotency_key());
        assert!((8..=128).contains(&a.len()));
        assert_ne!(a, b);
    }
}
