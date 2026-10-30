//! Client-side state.
//!
//! This is the in-memory shape mattermost-mobile persists to SQLite. Two splits
//! from that design are worth keeping even before there is a database:
//!
//! * the **channel** and **my membership** live in separate maps, because the
//!   membership is rewritten on nearly every websocket event while the channel
//!   object almost never changes;
//! * **posts are stored per channel in a single ordered block**. Real clients
//!   keep several blocks with gaps between them (`postsInChannel` in the
//!   webapp, `PostsInChannel` in mobile) — see the note on [`ChannelFeed`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mattermost_api::models::*;
use mattermost_api::{Client, WebSocket};
use mattermost_calls::{AnyCall, Discovery};

/// Posts for one channel.
///
/// Deliberately a single contiguous block for now. The moment we add
/// permalink jumps or "jump to first unread", this has to become a list of
/// blocks with `recent` / `oldest` flags, merged on overlap — otherwise a jump
/// into old history silently appends unrelated posts to the live feed. Both
/// official clients learned this the hard way; the merge rule is that two
/// blocks join when the older one's newest post is not older than the newer
/// one's oldest post.
#[derive(Debug, Default)]
pub struct ChannelFeed {
    /// Oldest first — the order a chat renders in.
    pub posts: Vec<Post>,
    /// True when the newest post here is the newest post in the channel.
    pub at_latest: bool,
    /// True when this block reaches the beginning of the channel.
    pub at_oldest: bool,
    /// `create_at` of the newest post we hold; the `since` watermark.
    pub last_fetched_at: Millis,
}

impl ChannelFeed {
    pub fn from_list(list: &PostList) -> Self {
        let mut posts: Vec<Post> = list.chronological().into_iter().cloned().collect();
        // `order` is only newest-first for *channel* feeds. `/posts/{id}/thread`
        // applies no ORDER BY at all unless the request names a direction, so
        // its order is whatever the query plan happened to yield — reversing
        // that produces a scrambled thread. Sorting here rather than in the
        // thread panel also keeps `upsert`'s invariant: it binary-walks this
        // vec assuming it is sorted, so an unsorted feed misplaces every
        // websocket reply too.
        //
        // The sort is stable, so posts sharing a millisecond keep the order the
        // server listed them in, and the panel still hoists the root to the top.
        posts.sort_by_key(|p| p.create_at);
        let last_fetched_at = posts.last().map(|p| p.create_at).unwrap_or(0);
        ChannelFeed {
            posts,
            at_latest: list.is_recent(),
            at_oldest: list.is_oldest(),
            last_fetched_at,
        }
    }

    /// Builds a feed from posts that are already in order — the cache stores
    /// them the way this holds them, so there is nothing to sort or reverse.
    ///
    /// `at_latest` is false: a restored feed is a snapshot from last time and
    /// almost certainly has messages after it, which is exactly what stops the
    /// view claiming it is up to date before the network says so.
    pub fn from_posts(posts: Vec<Post>) -> Self {
        let last_fetched_at = posts.last().map(|p| p.create_at).unwrap_or(0);
        ChannelFeed {
            posts,
            at_latest: false,
            at_oldest: false,
            last_fetched_at,
        }
    }

    /// Inserts or replaces a post, keeping chronological order.
    ///
    /// Used for both `posted` and `post_edited`: the server sends whole posts,
    /// never diffs.
    pub fn upsert(&mut self, post: Post) {
        if let Some(existing) = self.posts.iter_mut().find(|p| p.id == post.id) {
            *existing = post;
            return;
        }
        // Replace our optimistic copy, if this is the echo of our own send.
        if !post.pending_post_id.is_empty() {
            if let Some(pending) = self
                .posts
                .iter_mut()
                .find(|p| p.id == post.pending_post_id || p.pending_post_id == post.pending_post_id)
            {
                *pending = post;
                return;
            }
        }
        self.last_fetched_at = self.last_fetched_at.max(post.create_at);
        match self
            .posts
            .iter()
            .rposition(|p| p.create_at <= post.create_at)
        {
            Some(idx) => self.posts.insert(idx + 1, post),
            None => self.posts.insert(0, post),
        }
    }

    /// Soft-deletes a post, matching the server's own semantics.
    pub fn remove(&mut self, post_id: &str) {
        self.posts.retain(|p| p.id != post_id);
    }
}

/// Everything the UI reads.
pub struct AppState {
    pub client: Client,
    pub ws: Option<WebSocket>,
    pub me: User,
    pub crt_enabled: bool,
    pub config: ClientConfig,

    pub teams: Vec<Team>,
    pub current_team: Option<String>,

    pub channels: HashMap<String, Channel>,
    /// Our own membership per channel — the hot, frequently rewritten half.
    pub memberships: HashMap<String, ChannelMember>,
    pub categories: OrderedSidebarCategories,
    pub current_channel: Option<String>,
    /// Per-channel stable message at roughly 30% of the viewport. Absence
    /// means the reader left that channel at its live bottom.
    pub scroll_anchors: HashMap<String, String>,

    pub users: HashMap<String, User>,
    pub statuses: HashMap<String, Presence>,
    pub feeds: HashMap<String, ChannelFeed>,
    /// Thread contents, keyed by **root post id**. A thread is identified by
    /// its root everywhere in the API, so replies are looked up through
    /// `Post::thread_root`, never through their own id.
    pub threads: HashMap<String, ChannelFeed>,
    /// The collapsed-reply-threads inbox for the current team.
    pub thread_inbox: Vec<UserThread>,
    /// Recent posts that name us, newest first.
    pub mentions: Vec<Post>,

    /// Calls discovery, when the plugin is installed and reachable.
    pub calls: Option<Discovery>,
    /// The server runs gomon, which then carries every call: a server with
    /// both is one migrating to it.
    pub gomon: bool,
    /// Channels with a call in progress, mapped to who is in it. User ids, in
    /// the order they joined — the sidebar draws the first few faces, and the
    /// length is the participant count.
    pub active_calls: HashMap<String, Vec<String>>,
    /// The call we are in, if any.
    pub call: Option<ActiveCall>,

    /// Unsent text, keyed by channel id. Mirrors what the server has, so the
    /// same half-written message is waiting on every device.
    pub drafts: HashMap<String, String>,
    /// When each person was last seen, from the status route. Only meaningful
    /// for people who are not online now.
    pub last_seen: HashMap<String, Millis>,
    /// The account's preferences, as the server holds them. Kept whole rather
    /// than picked apart: several features read one key each, and the list is
    /// small.
    pub preferences: Vec<Preference>,
    /// When the user map was last refreshed wholesale, so a resync can ask
    /// only for what changed since.
    pub users_fetched_at: Millis,
    /// Channels we have raised an incoming-call notification for, so it can
    /// be taken back down when the call ends or is answered elsewhere. The
    /// dismissal event names a *call*, not a channel, and we hold no mapping
    /// between the two — so this is the mapping.
    pub ringing: Vec<String>,
    /// Unread counts for teams other than the current one, so the switcher
    /// can say where something is waiting. Keyed by team id.
    pub team_unreads: HashMap<String, (i64, i64)>,
    /// The LLM bots this server offers, empty when the Agents plugin is not
    /// installed — which is the same as having nothing to offer, so it needs
    /// no separate flag.
    pub bots: Vec<crate::agents::Bot>,
    /// Files uploaded and waiting to be attached to the next message: the
    /// server's file id and the name to show on the chip.
    pub pending_files: Vec<(String, String)>,
    /// The last search's hits, newest first, and whether one is in flight.
    pub search_results: Vec<Post>,
    pub searching: bool,
    /// Posts you saved. Mattermost has no "saved" flag on a post — it is a
    /// preference in the `flagged_post` category, one row per post id.
    pub saved_posts: HashSet<String>,
    /// Unread reaction notices, from the reactions-notify plugin. Zero when
    /// the plugin is not installed, which is indistinguishable from "nothing
    /// new" and needs no special case.
    pub reaction_unread: i64,
    /// Unsent replies, keyed by thread root. Separate from `drafts` because
    /// the server keys them the same way: one per (channel, root) pair.
    pub thread_drafts: HashMap<String, String>,
    /// False once the server has told us drafts are turned off, after which
    /// they are kept in this map and nowhere else.
    pub drafts_synced: bool,

    /// Who is typing where: channel id → user id → when we heard about it.
    /// The server sends no "stopped typing", so entries are aged out instead.
    pub typing: HashMap<String, HashMap<String, Instant>>,
}

/// A joined call and the audio devices serving it.
pub struct ActiveCall {
    pub session: Arc<AnyCall>,
    pub channel_id: String,
    pub recording: bool,
    /// Media session id → user id. The media plane carries no user ids, so
    /// naming a track means going through the roster.
    pub roster: HashMap<String, String>,
    /// Dropped with the call, which stops the microphone and speakers.
    pub audio: crate::audio::AudioIo,
    /// Outbound video, if we are sending any. Dropping either stops it.
    pub screen: Option<crate::video::VideoSender>,
    pub camera: Option<crate::video::VideoSender>,
    pub muted: bool,
    /// Who the SFU last reported as speaking, newest first. Server-side voice
    /// activity, so it only ever names other people — we are never in here.
    pub speaking: Vec<String>,
    /// Who is sharing a screen right now, by user id.
    pub sharing: Vec<String>,
    /// Who has their microphone off. The SFU reports mute per session, and
    /// silence is the default, so absence from this set means unmuted.
    pub muted_users: std::collections::HashSet<String>,
    /// Raised hands, oldest first — the order is the queue.
    pub hands: Vec<String>,
    /// Whose call this is. The host can mute people, end the call and hand the
    /// role on; everyone else sees none of those controls.
    pub host_id: String,
    /// Media session ids by user, needed because host controls address a
    /// *session* rather than a person.
    pub sessions: std::collections::HashMap<String, String>,
}

pub type SharedState = Rc<RefCell<AppState>>;

impl AppState {
    pub fn new(client: Client, me: User, config: ClientConfig, crt_enabled: bool) -> Self {
        let mut users = HashMap::new();
        users.insert(me.id.clone(), me.clone());
        AppState {
            client,
            ws: None,
            me,
            crt_enabled,
            config,
            teams: Vec::new(),
            current_team: None,
            channels: HashMap::new(),
            memberships: HashMap::new(),
            categories: OrderedSidebarCategories::default(),
            current_channel: None,
            scroll_anchors: HashMap::new(),
            users,
            statuses: HashMap::new(),
            feeds: HashMap::new(),
            threads: HashMap::new(),
            thread_inbox: Vec::new(),
            mentions: Vec::new(),
            calls: None,
            gomon: false,
            active_calls: HashMap::new(),
            call: None,
            drafts: HashMap::new(),
            thread_drafts: HashMap::new(),
            drafts_synced: true,
            last_seen: HashMap::new(),
            preferences: Vec::new(),
            users_fetched_at: 0,
            ringing: Vec::new(),
            team_unreads: HashMap::new(),
            bots: Vec::new(),
            pending_files: Vec::new(),
            search_results: Vec::new(),
            searching: false,
            saved_posts: HashSet::new(),
            reaction_unread: 0,
            typing: HashMap::new(),
        }
    }

    /// Mattermost repeats `user_typing` about every five seconds while someone
    /// keeps typing, and never says they stopped, so this is the window an
    /// entry stays live for.
    pub const TYPING_TTL: Duration = Duration::from_secs(6);

    pub fn typing_started(&mut self, channel_id: String, user_id: String) {
        self.typing
            .entry(channel_id)
            .or_default()
            .insert(user_id, Instant::now());
    }

    /// Who is currently typing in a channel, oldest first, dropping anyone
    /// whose last keystroke has aged out.
    pub fn typing_in(&mut self, channel_id: &str) -> Vec<String> {
        let Some(people) = self.typing.get_mut(channel_id) else {
            return Vec::new();
        };
        people.retain(|_, at| at.elapsed() < Self::TYPING_TTL);
        let mut live: Vec<(&String, &Instant)> = people.iter().collect();
        live.sort_by_key(|(_, at)| **at);
        live.into_iter().map(|(id, _)| id.clone()).collect()
    }

    /// Finds a post anywhere we are holding one — channel feeds first, then
    /// threads, since a reply only lives in the latter under CRT.
    pub fn post(&self, post_id: &str) -> Option<Post> {
        self.feeds
            .values()
            .chain(self.threads.values())
            .find_map(|feed| feed.posts.iter().find(|p| p.id == post_id).cloned())
    }

    /// The current team's URL name, for building permalinks.
    pub fn current_team_name(&self) -> Option<String> {
        let id = self.current_team.as_ref()?;
        self.teams
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.name.clone())
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.get(id)
    }

    /// A channel's label, resolving DM/GM names to the people in them.
    ///
    /// DM channels carry `"<idA>__<idB>"` as their name and an empty display
    /// name, so the sidebar has to look the other person up itself.
    pub fn channel_title(&self, channel: &Channel) -> String {
        match channel.r#type {
            ChannelType::Direct => channel
                .dm_teammate_id(&self.me.id)
                .and_then(|id| self.users.get(id))
                .map(|u| u.display_name(self.teammate_name_display()))
                .unwrap_or_else(|| channel.display_name.clone()),
            _ => channel.display_name.clone(),
        }
    }

    pub fn teammate_name_display(&self) -> &str {
        self.config.get("TeammateNameDisplay").unwrap_or("username")
    }

    /// How a user should be named in this server's configured style.
    /// Whether any presence dot changed colour.
    pub fn apply_statuses(&mut self, statuses: Vec<Status>) -> bool {
        let mut changed = false;
        for status in statuses {
            // The last-activity stamp comes along with presence and is the
            // only thing that can answer "when were they last here".
            if status.last_activity_at > 0 {
                self.last_seen
                    .insert(status.user_id.clone(), status.last_activity_at);
            }
            let presence = status.presence();
            changed |= self.statuses.insert(status.user_id, presence) != Some(presence);
        }
        changed
    }

    /// Whose presence to ask the server about again: me (the account button),
    /// every DM partner, and everyone whose dot has been drawn at all
    /// (message authors, profiles).
    pub fn presence_watch_ids(&self) -> Vec<String> {
        let mut ids: HashSet<String> = self.statuses.keys().cloned().collect();
        ids.insert(self.me.id.clone());
        ids.extend(
            self.channels
                .values()
                .filter_map(|c| c.dm_teammate_id(&self.me.id))
                .map(str::to_string),
        );
        ids.remove("");
        ids.into_iter().collect()
    }

    pub fn presence(&self, user_id: &str) -> Presence {
        self.statuses.get(user_id).copied().unwrap_or_default()
    }

    pub fn display_name(&self, user: &User) -> String {
        user.display_name(self.teammate_name_display())
    }

    /// Unread thread count for the badge on the inbox button.
    pub fn unread_threads(&self) -> i64 {
        self.thread_inbox.iter().filter(|t| t.is_unread()).count() as i64
    }

    /// The number both inbox buttons show, so the sidebar and the chat header
    /// cannot disagree about it.
    pub fn inbox_badge_count(&self) -> i64 {
        self.mentions
            .len()
            .min(99)
            .max(self.unread_threads().max(0) as usize) as i64
            + self.reaction_unread
    }

    /// Applies a post to whichever feeds hold it: the channel, the thread, or
    /// both.
    ///
    /// Under collapsed reply threads a reply must **not** enter the channel
    /// feed — that is the whole point of the mode, and the webapp's reducers
    /// return early for exactly this case.
    pub fn apply_post(&mut self, post: Post) {
        let root = post.thread_root().to_string();

        if let Some(thread) = self.threads.get_mut(&root) {
            thread.upsert(post.clone());
        }

        if post.is_reply() {
            // Keep the root's reply counter live, since that is the only
            // affordance leading into the thread.
            //
            // Every post comes through here, and the same reply comes through
            // several times: the optimistic copy the composer draws, the REST
            // result of that send, the websocket echo of it, a redelivery
            // after a reconnect, an edit, a permalink or saved-post fetch. So
            // the counter cannot be a blind `+= 1` — it has to move only for a
            // reply we have not already counted, and the post itself says
            // which one it is:
            //
            // * a copy we wrote ourselves has `id == pending_post_id`, and the
            //   server's version of it carries that pending id under a real
            //   id, which is how the echo is recognised as already counted —
            //   the same key `ChannelFeed::upsert` retires the copy by;
            // * the root's `last_reply_at` is the newest reply anyone knows
            //   of, so a reply not newer than that is one we already hold.
            let mine = post.id == post.pending_post_id;
            let echo_of_mine =
                !mine && !post.pending_post_id.is_empty() && post.user_id == self.me.id;
            if let Some(feed) = self.feeds.get_mut(&post.channel_id) {
                if let Some(root_post) = feed.posts.iter_mut().find(|p| p.id == root) {
                    if mine || (!echo_of_mine && post.create_at > root_post.last_reply_at) {
                        root_post.reply_count += 1;
                    }
                    // An optimistic copy is stamped with our own clock. Letting
                    // it set the watermark would make every real reply that
                    // lands within the client's clock skew look like one we
                    // already have, so only server posts move it.
                    if !mine {
                        root_post.last_reply_at = root_post.last_reply_at.max(post.create_at);
                    }
                }
            }
            if self.crt_enabled {
                return;
            }
        }

        if let Some(feed) = self.feeds.get_mut(&post.channel_id) {
            feed.upsert(post);
        }
    }

    /// Drops a post from every feed and thread that holds it.
    ///
    /// Used to retire an optimistic copy: relying on `pending_post_id` coming
    /// back on the echo is not enough, because the REST response and the
    /// websocket event race, and only one of them is guaranteed to carry it.
    pub fn remove_post(&mut self, post_id: &str) {
        for feed in self.feeds.values_mut().chain(self.threads.values_mut()) {
            feed.remove(post_id);
        }
    }

    /// Adds or removes a reaction wherever the post is held.
    pub fn apply_reaction(&mut self, reaction: &Reaction, added: bool) {
        let feeds = self.feeds.values_mut().chain(self.threads.values_mut());
        for feed in feeds {
            if let Some(post) = feed.posts.iter_mut().find(|p| p.id == reaction.post_id) {
                let metadata = post.metadata.get_or_insert_with(Default::default);
                if added {
                    let already = metadata.reactions.iter().any(|r| {
                        r.user_id == reaction.user_id && r.emoji_name == reaction.emoji_name
                    });
                    if !already {
                        metadata.reactions.push(reaction.clone());
                    }
                } else {
                    metadata.reactions.retain(|r| {
                        !(r.user_id == reaction.user_id && r.emoji_name == reaction.emoji_name)
                    });
                }
                post.has_reactions = !metadata.reactions.is_empty();
            }
        }
    }

    /// True when we already hold this reaction from ourselves — used to decide
    /// whether clicking a chip adds or removes.
    pub fn has_my_reaction(&self, post_id: &str, emoji_name: &str) -> bool {
        self.feeds
            .values()
            .chain(self.threads.values())
            .filter_map(|feed| feed.posts.iter().find(|p| p.id == post_id))
            .any(|post| {
                post.reactions()
                    .iter()
                    .any(|r| r.user_id == self.me.id && r.emoji_name == emoji_name)
            })
    }

    /// Finds a post in any feed we hold.
    pub fn find_post(&self, post_id: &str) -> Option<&Post> {
        self.feeds
            .values()
            .chain(self.threads.values())
            .find_map(|feed| feed.posts.iter().find(|p| p.id == post_id))
    }

    pub fn unread(&self, channel_id: &str) -> UnreadState {
        match (
            self.channels.get(channel_id),
            self.memberships.get(channel_id),
        ) {
            (Some(c), Some(m)) => UnreadState::compute(c, m, self.crt_enabled),
            _ => UnreadState::default(),
        }
    }

    /// Author label for a post, honouring webhook/bot overrides.
    pub fn author_name(&self, post: &Post) -> String {
        if let Some(name) = post.override_username() {
            return name.to_string();
        }
        self.users
            .get(&post.user_id)
            .map(|u| u.display_name(self.teammate_name_display()))
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Channels of the current team, ordered the way the sidebar wants them,
    /// grouped by category.
    pub fn sidebar_groups(&self) -> Vec<(SidebarCategory, Vec<Channel>)> {
        let mut out = Vec::new();
        let order = &self.categories.order;
        let by_id: HashMap<&str, &SidebarCategory> = self
            .categories
            .categories
            .iter()
            .map(|c| (c.id.as_str(), c))
            .collect();

        // `order` is authoritative; fall back to declaration order if it is
        // missing (older servers, or a partial sync).
        let ordered: Vec<&SidebarCategory> = if order.is_empty() {
            self.categories.categories.iter().collect()
        } else {
            order
                .iter()
                .filter_map(|id| by_id.get(id.as_str()).copied())
                .collect()
        };

        for category in ordered {
            let mut channels: Vec<Channel> = category
                .channel_ids
                .iter()
                .filter_map(|id| self.channels.get(id))
                .filter(|c| !c.is_archived())
                .cloned()
                .collect();

            let members: Vec<ChannelMember> = channels
                .iter()
                .filter_map(|c| self.memberships.get(&c.id).cloned())
                .collect();

            mattermost_api::bootstrap::sort_channels(
                &mut channels,
                &members,
                &category.sorting,
                &category.channel_ids,
            );
            if !channels.is_empty() {
                out.push((category.clone(), channels));
            }
        }
        out
    }

    /// Total mentions across the current team — what a tray badge would show.
    pub fn total_mentions(&self) -> i64 {
        self.channels
            .keys()
            .map(|id| self.unread(id))
            .map(|u| u.mentions)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(id: &str, at: Millis) -> Post {
        Post {
            id: id.into(),
            create_at: at,
            ..Default::default()
        }
    }

    /// A state holding one channel with one root post that already has
    /// `replies` replies, the way the server reported it.
    fn state_with_root(replies: i64) -> AppState {
        let me = User {
            id: "me".into(),
            ..Default::default()
        };
        let mut st = AppState::new(
            Client::new("https://test.invalid").expect("static url"),
            me,
            ClientConfig::default(),
            true,
        );
        let mut root = post("root", 100);
        root.channel_id = "chan".into();
        root.reply_count = replies;
        root.last_reply_at = if replies > 0 { 100 } else { 0 };
        st.feeds
            .insert("chan".into(), ChannelFeed::from_posts(vec![root]));
        st
    }

    #[test]
    fn inbox_badge_counts_reactions_on_top_of_mentions() {
        let mut st = state_with_root(0);
        st.mentions = vec![post("m1", 1), post("m2", 2)];
        st.reaction_unread = 3;
        assert_eq!(st.inbox_badge_count(), 5);
    }

    fn reply(id: &str, at: Millis, user_id: &str) -> Post {
        Post {
            id: id.into(),
            create_at: at,
            channel_id: "chan".into(),
            root_id: "root".into(),
            user_id: user_id.into(),
            ..Default::default()
        }
    }

    fn counter(st: &AppState) -> i64 {
        st.feeds["chan"].posts[0].reply_count
    }

    #[test]
    fn our_own_reply_counts_once_across_the_optimistic_copy_and_its_echo() {
        let mut st = state_with_root(7);

        // What the composer draws immediately: id and pending id are the same
        // locally generated string.
        let mut optimistic = reply("pending-1", 150, "me");
        optimistic.pending_post_id = "pending-1".into();
        st.apply_post(optimistic);
        assert_eq!(counter(&st), 8, "the footer moves without the round trip");

        // The REST result and the websocket echo are the same post arriving
        // twice, both carrying the pending id back.
        let mut confirmed = reply("real-1", 160, "me");
        confirmed.pending_post_id = "pending-1".into();
        st.apply_post(confirmed.clone());
        st.apply_post(confirmed);
        assert_eq!(counter(&st), 8, "the echo is not a second reply");
    }

    #[test]
    fn someone_elses_reply_counts_once_however_often_it_is_delivered() {
        let mut st = state_with_root(7);

        let theirs = reply("r-1", 150, "lena");
        st.apply_post(theirs.clone());
        assert_eq!(counter(&st), 8);

        // A reconnect replays what we already have — same post, same id.
        st.apply_post(theirs.clone());
        // As does an edit of it, which arrives as a whole post.
        let mut edited = theirs;
        edited.message = "second thoughts".into();
        st.apply_post(edited);
        assert_eq!(counter(&st), 8, "redelivery is not a new reply");

        // A genuinely newer one still counts.
        st.apply_post(reply("r-2", 160, "mikk"));
        assert_eq!(counter(&st), 9);
    }

    #[test]
    fn a_reply_older_than_the_root_knows_about_is_history_not_news() {
        let mut st = state_with_root(7);
        st.apply_post(reply("r-old", 50, "sara"));
        assert_eq!(
            counter(&st),
            7,
            "a permalink or saved-post fetch must not inflate the count"
        );
    }

    #[test]
    fn polled_statuses_report_changes_and_cover_dm_partners() {
        let client = Client::new("http://x.test").unwrap();
        let me = User {
            id: "me".into(),
            ..Default::default()
        };
        let mut st = AppState::new(client, me, ClientConfig::default(), false);
        let dm = Channel {
            id: "dm".into(),
            r#type: ChannelType::Direct,
            name: "me__olga".into(),
            ..Default::default()
        };
        st.channels.insert(dm.id.clone(), dm);
        let status = |user: &str, s: &str| Status {
            user_id: user.into(),
            status: s.into(),
            ..Default::default()
        };

        // Never seen yet, but a DM row shows her dot and the account button
        // shows mine: ask about both.
        let mut ids = st.presence_watch_ids();
        ids.sort();
        assert_eq!(ids, ["me", "olga"]);

        assert!(st.apply_statuses(vec![status("olga", "online"), status("bob", "online")]));
        assert!(
            !st.apply_statuses(vec![status("olga", "online")]),
            "no change"
        );
        // The auto-away the websocket never tells other users about.
        assert!(st.apply_statuses(vec![status("olga", "away")]));
        assert_eq!(st.presence("olga"), Presence::Away);

        let mut ids = st.presence_watch_ids();
        ids.sort();
        assert_eq!(ids, ["bob", "me", "olga"]);
    }

    #[test]
    fn feed_keeps_posts_in_chronological_order() {
        let mut feed = ChannelFeed::default();
        feed.upsert(post("b", 200));
        feed.upsert(post("a", 100));
        feed.upsert(post("c", 300));
        let ids: Vec<&str> = feed.posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(feed.last_fetched_at, 300);
    }

    #[test]
    fn editing_a_post_replaces_it_in_place() {
        let mut feed = ChannelFeed::default();
        feed.upsert(post("a", 100));
        feed.upsert(post("b", 200));
        let mut edited = post("a", 100);
        edited.message = "edited".into();
        feed.upsert(edited);
        assert_eq!(feed.posts.len(), 2);
        assert_eq!(feed.posts[0].message, "edited");
    }

    #[test]
    fn the_server_echo_replaces_our_optimistic_copy() {
        let mut feed = ChannelFeed::default();
        let mut optimistic = post("pending-1", 100);
        optimistic.pending_post_id = "pending-1".into();
        feed.upsert(optimistic);

        let mut confirmed = post("real-id", 101);
        confirmed.pending_post_id = "pending-1".into();
        feed.upsert(confirmed);

        assert_eq!(feed.posts.len(), 1, "the pending post should be replaced");
        assert_eq!(feed.posts[0].id, "real-id");
    }

    #[test]
    fn from_list_sorts_a_thread_the_server_left_unordered() {
        // What `/posts/{id}/thread` really returns: the requested post first,
        // then replies in no particular order.
        let mut list = PostList {
            order: vec!["root".into(), "r2".into(), "r3".into(), "r1".into()],
            ..Default::default()
        };
        list.posts.insert("root".into(), post("root", 100));
        list.posts.insert("r1".into(), post("r1", 200));
        list.posts.insert("r2".into(), post("r2", 300));
        list.posts.insert("r3".into(), post("r3", 400));

        let feed = ChannelFeed::from_list(&list);
        let ids: Vec<&str> = feed.posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["root", "r1", "r2", "r3"]);
        assert_eq!(
            feed.last_fetched_at, 400,
            "the watermark is the newest post"
        );
    }

    #[test]
    fn from_list_reverses_the_servers_newest_first_order() {
        let mut list = PostList {
            order: vec!["c".into(), "b".into(), "a".into()],
            ..Default::default()
        };
        list.posts.insert("a".into(), post("a", 100));
        list.posts.insert("b".into(), post("b", 200));
        list.posts.insert("c".into(), post("c", 300));

        let feed = ChannelFeed::from_list(&list);
        let ids: Vec<&str> = feed.posts.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(feed.at_latest, "next_post_id was empty");
        assert!(feed.at_oldest, "prev_post_id was empty");
    }
}
