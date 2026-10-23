//! User, status and custom-status models.
//!
//! Field names mirror `server/public/model/user.go`, `status.go`,
//! `custom_status.go` verbatim.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::Millis;

pub type StringMap = HashMap<String, String>;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub create_at: Millis,
    #[serde(default)]
    pub update_at: Millis,
    #[serde(default)]
    pub delete_at: Millis,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub auth_service: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub position: String,
    /// Space-separated, e.g. `"system_user system_admin"`. Use [`User::has_role`].
    #[serde(default)]
    pub roles: String,
    #[serde(default)]
    pub props: StringMap,
    #[serde(default)]
    pub notify_props: StringMap,
    #[serde(default)]
    pub last_picture_update: Millis,
    #[serde(default)]
    pub locale: String,
    /// Keys: `useAutomaticTimezone` (the string `"true"`/`"false"`),
    /// `automaticTimezone`, `manualTimezone`.
    #[serde(default)]
    pub timezone: StringMap,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub bot_description: String,
    #[serde(default)]
    pub last_activity_at: Millis,
}

impl User {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.split_whitespace().any(|r| r == role)
    }

    pub fn is_deleted(&self) -> bool {
        self.delete_at != 0
    }

    /// The custom status lives in `props["customStatus"]` as a *JSON-encoded
    /// string*, not as a nested object.
    /// The custom status, unless it has expired. The server clears an expired
    /// one lazily, so the props keep it after `expires_at` has passed.
    pub fn custom_status(&self) -> Option<CustomStatus> {
        let raw = self.props.get("customStatus")?;
        let status: CustomStatus = serde_json::from_str(raw).ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        (!status.expired_at(now)).then_some(status)
    }

    /// Best display name for the given teammate-name-display preference value
    /// (`username` | `nickname_full_name` | `full_name`).
    pub fn display_name(&self, setting: &str) -> String {
        let full = format!("{} {}", self.first_name, self.last_name);
        let full = full.trim();
        match setting {
            "nickname_full_name" if !self.nickname.is_empty() => self.nickname.clone(),
            "nickname_full_name" | "full_name" if !full.is_empty() => full.to_string(),
            _ => self.username.clone(),
        }
    }
}

/// `server/public/model/custom_status.go`.
///
/// Note `expires_at` is RFC3339, **not** milliseconds — it is the only
/// timestamp in the API shaped that way besides `Status::dnd_end_time`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CustomStatus {
    #[serde(default)]
    pub emoji: String,
    #[serde(default)]
    pub text: String,
    /// `thirty_minutes` | `one_hour` | `four_hours` | `today` | `this_week` |
    /// `date_and_time`
    #[serde(default)]
    pub duration: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl CustomStatus {
    /// Same rule as the webapp's `isCustomStatusExpired`: "don't clear", or
    /// no expiry at all, never expires. An expiry we cannot read keeps the
    /// status rather than hiding something the user set.
    pub fn expired_at(&self, now_unix: i64) -> bool {
        if self.duration == "don't_clear" {
            return false;
        }
        self.expires_at
            .as_deref()
            .and_then(parse_rfc3339)
            .is_some_and(|expiry| now_unix >= expiry)
    }
}

/// Unix seconds from an RFC 3339 timestamp as Go writes it:
/// `2026-10-23T20:59:59.999Z` or with a `+03:00` offset.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let h = rest.get(1..3)?.parse::<i64>().ok()?;
            let m = rest.get(4..6)?.parse::<i64>().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    // Days from civil (Howard Hinnant), proleptic Gregorian.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - offset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Presence {
    Online,
    Away,
    #[default]
    Offline,
    Dnd,
    OutOfOffice,
}

impl Presence {
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Online => "online",
            Presence::Away => "away",
            Presence::Offline => "offline",
            Presence::Dnd => "dnd",
            Presence::OutOfOffice => "ooo",
        }
    }
}

impl From<&str> for Presence {
    fn from(s: &str) -> Self {
        match s {
            "online" => Presence::Online,
            "away" => Presence::Away,
            "dnd" => Presence::Dnd,
            "ooo" => Presence::OutOfOffice,
            _ => Presence::Offline,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Status {
    pub user_id: String,
    /// `online` | `away` | `offline` | `dnd` | `ooo`
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub manual: bool,
    #[serde(default)]
    pub last_activity_at: Millis,
    /// **Seconds**, not milliseconds — the one exception in the whole API.
    #[serde(default)]
    pub dnd_end_time: i64,
}

impl Status {
    pub fn presence(&self) -> Presence {
        Presence::from(self.status.as_str())
    }
}

/// `GET /users/autocomplete`
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserAutocomplete {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub out_of_channel: Vec<User>,
}

#[cfg(test)]
mod custom_status_tests {
    use super::*;

    fn status(duration: &str, expires_at: Option<&str>) -> CustomStatus {
        CustomStatus {
            emoji: "palm_tree".into(),
            duration: duration.into(),
            expires_at: expires_at.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn rfc3339_as_go_writes_it() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-23T10:00:00Z"), Some(1_792_749_600));
        assert_eq!(
            parse_rfc3339("2026-10-23T10:00:00.999Z"),
            Some(1_792_749_600)
        );
        assert_eq!(
            parse_rfc3339("2026-10-23T13:00:00+03:00"),
            Some(1_792_749_600)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn a_passed_expiry_hides_the_status() {
        let now = 1_792_749_600;
        let s = status("today", Some("2026-10-23T09:59:59Z"));
        assert!(s.expired_at(now));
        assert!(!status("today", Some("2026-10-23T10:00:01Z")).expired_at(now));
    }

    #[test]
    fn dont_clear_missing_or_unreadable_expiry_keeps_it() {
        let now = 1_792_749_600;
        assert!(!status("don't_clear", Some("0001-01-01T00:00:00Z")).expired_at(now));
        assert!(!status("today", None).expired_at(now));
        assert!(!status("today", Some("soon")).expired_at(now));
    }
}
