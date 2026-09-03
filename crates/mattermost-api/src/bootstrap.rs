//! The startup sequence, in the order the official clients perform it.
//!
//! mattermost-mobile is the model to copy here, not the webapp: it renders from
//! local state immediately and syncs in the background, whereas the webapp
//! loads everything eagerly. What this module provides is the *synchronous
//! prefix* — the calls whose results you need before you can draw a useful
//! window — with the rest left to the caller to schedule.
//!
//! ```text
//! 1. parallel  GET /config/client   GET /license/client   GET /users/me/preferences
//!              → derive crt_enabled
//! 2. parallel  GET /users/me        GET /users/me/teams   GET /users/me/teams/members
//!              → pick the initial team
//! 3. parallel  GET /users/me/teams/{team}/channels[?last_delete_at=since]
//!              GET /users/me/teams/{team}/channels/members
//!              GET /users/me/teams/{team}/channels/categories
//!              → pick the initial channel
//! 4. open the websocket, then render
//! 5. deferred: other teams, profiles, unread channels' posts, roles, threads
//! ```

use crate::error::Result;
use crate::models::*;
use crate::rest::Client;

/// Everything needed to draw the first frame of the UI.
#[derive(Debug, Clone)]
pub struct Bootstrap {
    pub config: ClientConfig,
    pub license: StringMap,
    pub me: User,
    pub preferences: Vec<Preference>,
    pub crt_enabled: bool,
    pub teams: Vec<Team>,
    pub team_members: Vec<TeamMember>,
    pub initial_team: Option<Team>,
    pub channels: Vec<Channel>,
    pub channel_members: Vec<ChannelMember>,
    pub categories: OrderedSidebarCategories,
    pub initial_channel: Option<Channel>,
}

impl Bootstrap {
    /// Runs the synchronous prefix of the startup sequence.
    ///
    /// `preferred_team` / `preferred_channel` are what the user was last
    /// looking at; both are validated against current membership and fall back
    /// gracefully. `since` is the last successful full-sync timestamp (0 on a
    /// cold start), used as `last_delete_at` so the caller can reconcile
    /// channels deleted while we were away.
    pub async fn run(
        client: &Client,
        preferred_team: Option<&str>,
        preferred_channel: Option<&str>,
        since: Millis,
    ) -> Result<Bootstrap> {
        // Step 1.
        let (config, license, preferences) = tokio::try_join!(
            client.client_config(),
            client.client_license(),
            client.my_preferences(),
        )?;

        let crt_pref = preferences
            .iter()
            .find(|p| {
                p.category == preference_category::DISPLAY_SETTINGS
                    && p.name == "collapsed_reply_threads"
            })
            .map(|p| p.value.as_str());
        let crt_enabled = is_crt_enabled(&config, crt_pref);

        // Step 2.
        let (me, teams, team_members) =
            tokio::try_join!(client.me(), client.my_teams(), client.my_team_members(),)?;

        let teams_order = preferences
            .iter()
            .find(|p| p.category == preference_category::TEAMS_ORDER)
            .map(|p| p.value.clone())
            .unwrap_or_default();

        let initial_team = select_team(&teams, &teams_order, preferred_team);

        // Step 3.
        let (channels, channel_members, categories) = match &initial_team {
            Some(team) => tokio::try_join!(
                client.my_channels(&team.id, false, since),
                client.my_channel_members(&team.id),
                client.sidebar_categories(&team.id),
            )?,
            None => (Vec::new(), Vec::new(), OrderedSidebarCategories::default()),
        };

        let initial_channel = select_channel(&channels, &channel_members, preferred_channel);

        Ok(Bootstrap {
            config,
            license,
            me,
            preferences,
            crt_enabled,
            teams,
            team_members,
            initial_team,
            channels,
            channel_members,
            categories,
            initial_channel,
        })
    }
}

/// Picks the team to open: the requested one if still joined, else the first
/// entry of the `teams_order` preference that is still joined, else the first
/// team alphabetically by display name.
pub fn select_team(teams: &[Team], teams_order: &str, preferred: Option<&str>) -> Option<Team> {
    if let Some(id) = preferred {
        if let Some(t) = teams.iter().find(|t| t.id == id) {
            return Some(t.clone());
        }
    }
    for id in teams_order
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(t) = teams.iter().find(|t| t.id == id) {
            return Some(t.clone());
        }
    }
    let mut sorted: Vec<&Team> = teams.iter().filter(|t| t.delete_at == 0).collect();
    sorted.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    sorted.first().map(|t| (*t).clone())
}

/// Picks the channel to open: the requested one if still a member, else the
/// team's default channel (`town-square`), else the first open channel by
/// display name.
pub fn select_channel(
    channels: &[Channel],
    members: &[ChannelMember],
    preferred: Option<&str>,
) -> Option<Channel> {
    let is_member = |id: &str| members.iter().any(|m| m.channel_id == id);

    if let Some(id) = preferred {
        if let Some(c) = channels.iter().find(|c| c.id == id && is_member(&c.id)) {
            return Some(c.clone());
        }
    }
    if let Some(c) = channels
        .iter()
        .find(|c| c.name == "town-square" && is_member(&c.id))
    {
        return Some(c.clone());
    }
    let mut open: Vec<&Channel> = channels
        .iter()
        .filter(|c| !c.is_archived() && is_member(&c.id))
        .collect();
    open.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    open.first().map(|c| (*c).clone())
}

/// Orders channels for the sidebar within one category, mirroring
/// `app/utils/categories.ts`.
///
/// `sorting` is the category's `sorting` field: `recent`, `manual`, or
/// alphabetical (anything else). Alphabetical ordering forces muted channels to
/// the bottom, which is what makes a noisy sidebar readable.
pub fn sort_channels(
    channels: &mut [Channel],
    members: &[ChannelMember],
    sorting: &str,
    order_hint: &[String],
) {
    let muted = |id: &str| {
        members
            .iter()
            .find(|m| m.channel_id == id)
            .map(ChannelMember::is_muted)
            .unwrap_or(false)
    };
    match sorting {
        "recent" => channels.sort_by(|a, b| {
            let ka = a.last_post_at.max(a.create_at);
            let kb = b.last_post_at.max(b.create_at);
            kb.cmp(&ka)
        }),
        "manual" => {
            let index = |id: &String| {
                order_hint
                    .iter()
                    .position(|c| c == id)
                    .unwrap_or(usize::MAX)
            };
            channels.sort_by_key(|c| index(&c.id));
        }
        _ => channels.sort_by(|a, b| {
            muted(&a.id).cmp(&muted(&b.id)).then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(id: &str, name: &str) -> Team {
        Team {
            id: id.into(),
            display_name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn preferred_team_wins_when_still_joined() {
        let teams = vec![team("a", "Alpha"), team("b", "Beta")];
        assert_eq!(select_team(&teams, "", Some("b")).unwrap().id, "b");
    }

    #[test]
    fn falls_back_to_teams_order_then_alphabetical() {
        let teams = vec![team("a", "Zulu"), team("b", "Alpha")];
        // Not a member of "zzz" any more → fall through to the order pref.
        assert_eq!(select_team(&teams, "zzz,a", Some("gone")).unwrap().id, "a");
        // No order pref either → alphabetical by display name.
        assert_eq!(select_team(&teams, "", None).unwrap().id, "b");
    }

    fn chan(id: &str, name: &str, display: &str) -> Channel {
        Channel {
            id: id.into(),
            name: name.into(),
            display_name: display.into(),
            ..Default::default()
        }
    }

    fn member(id: &str) -> ChannelMember {
        ChannelMember {
            channel_id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn channel_selection_prefers_town_square_over_alphabetical() {
        let channels = vec![
            chan("c1", "aaa", "AAA"),
            chan("c2", "town-square", "Town Square"),
        ];
        let members = vec![member("c1"), member("c2")];
        assert_eq!(select_channel(&channels, &members, None).unwrap().id, "c2");
    }

    /// The channel the reader had open outranks the one a first visit would
    /// get. The UI passes its saved pointer here; when it passed `None`
    /// instead, every launch opened town-square and the restore that followed
    /// found the seat taken and dropped what it had loaded.
    #[test]
    fn channel_selection_prefers_where_we_were_over_town_square() {
        let channels = vec![
            chan("c1", "aaa", "AAA"),
            chan("c2", "town-square", "Town Square"),
        ];
        let members = vec![member("c1"), member("c2")];
        assert_eq!(
            select_channel(&channels, &members, Some("c1")).unwrap().id,
            "c1"
        );
    }

    #[test]
    fn channel_selection_ignores_channels_we_are_not_in() {
        let channels = vec![chan("c1", "aaa", "AAA")];
        let members = vec![];
        assert!(select_channel(&channels, &members, Some("c1")).is_none());
    }

    #[test]
    fn alphabetical_sorting_pushes_muted_channels_last() {
        let mut channels = vec![chan("c1", "aaa", "AAA"), chan("c2", "bbb", "BBB")];
        let mut muted = member("c1");
        muted
            .notify_props
            .insert("mark_unread".into(), "mention".into());
        let members = vec![muted, member("c2")];
        sort_channels(&mut channels, &members, "alpha", &[]);
        assert_eq!(channels[0].id, "c2", "muted AAA should sink below BBB");
    }
}
