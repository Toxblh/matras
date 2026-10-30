//! gomon, the `ru.corp.comms` call service. The protocol is written up in
//! `docs/GOMON.md`; section numbers below refer to it.

use mattermost_api::models::Post;

/// The plugin's post type for a call card (§4.2).
pub const CARD_POST_TYPE: &str = "custom_comms_call";

/// What a call card says, read from the post's props. The plugin rewrites the
/// same post as the call goes on, so this is the call as of the last edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallCard {
    pub call_id: String,
    /// Somebody can still join: the call is starting, running or waiting out
    /// its empty grace period.
    pub live: bool,
    pub participants: Vec<String>,
    /// People who were rung and did not pick up, once each.
    pub missed: Vec<String>,
    pub recording_url: Option<String>,
    /// The code for dialling in by phone or from a meeting room.
    pub pin: Option<String>,
}

impl CallCard {
    pub fn from_post(post: &Post) -> Option<CallCard> {
        if post.r#type != CARD_POST_TYPE {
            return None;
        }
        let prop = |key: &str| {
            post.props
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let call_id = prop("call_id");
        if call_id.is_empty() {
            return None;
        }
        let names = |s: String| -> Vec<String> {
            s.split(", ")
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let mut missed: Vec<String> = Vec::new();
        // "<name>:<STATE>", and the same person once per ring.
        for entry in names(prop("missed")) {
            let name = entry
                .rsplit_once(':')
                .map_or(entry.as_str(), |(name, _)| name)
                .to_string();
            if !missed.contains(&name) {
                missed.push(name);
            }
        }
        Some(CallCard {
            live: matches!(
                prop("state").as_str(),
                "STARTING" | "ACTIVE" | "EMPTY_GRACE"
            ),
            participants: names(prop("participant_names")),
            missed,
            recording_url: Some(prop("recording_url")).filter(|u| !u.is_empty()),
            pin: Some(prop("pin")).filter(|p| !p.is_empty()),
            call_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(props: serde_json::Value) -> Option<CallCard> {
        let post = Post {
            r#type: CARD_POST_TYPE.into(),
            props: serde_json::from_value(props).unwrap(),
            ..Default::default()
        };
        CallCard::from_post(&post)
    }

    #[test]
    fn reads_an_ended_card_from_the_live_server() {
        // Props as the server sent them, numbers and all.
        let c = card(serde_json::json!({
            "participant_names": "", "from_plugin": "true", "participant_ids": "",
            "pin": "7654", "state": "ENDED", "version": 4,
            "call_id": "call_5689d42b6531b761d266", "participants": 0,
            "missed": "Evgeny Sinelnikov:EXPIRED, Evgeny Sinelnikov:EXPIRED, Андрей Лимачко:DECLINED",
            "recording_url": "https://mm/plugins/ru.corp.comms/login?return=%2Frecordings%2Frec_1"
        }))
        .unwrap();
        assert!(!c.live);
        assert!(c.participants.is_empty());
        assert_eq!(c.missed, ["Evgeny Sinelnikov", "Андрей Лимачко"]);
        assert!(c.recording_url.is_some());
        assert_eq!(c.pin.as_deref(), Some("7654"));
    }

    #[test]
    fn a_running_call_is_joinable() {
        let c = card(serde_json::json!({
            "call_id": "call_1", "state": "ACTIVE", "participant_names": "A B, C D"
        }))
        .unwrap();
        assert!(c.live);
        assert_eq!(c.participants, ["A B", "C D"]);
    }

    #[test]
    fn other_posts_are_not_cards() {
        assert!(card(serde_json::json!({})).is_none());
        let post = Post {
            r#type: String::new(),
            ..Default::default()
        };
        assert!(CallCard::from_post(&post).is_none());
    }
}
