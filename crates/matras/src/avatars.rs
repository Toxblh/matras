//! Profile picture cache.
//!
//! `gdk::Texture` is not `Send` and the download must not block the UI, so the
//! split is: fetch bytes on Tokio, decode into a texture on the GTK thread,
//! keep it in a plain `RefCell` map. Everything here therefore runs on the main
//! thread and needs no locking beyond that.
//!
//! The server always returns *an* image — it renders default initials for users
//! who never uploaded one — so there is no "user has no avatar" case to handle.
//! There is only "not fetched yet", which shows libadwaita's own initials until
//! the picture lands.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk::glib::object::ObjectExt;
use gtk::{gdk, glib};
use mattermost_api::Client;

use crate::resource_cache::ResourceCache;
use crate::runtime;

#[derive(Default)]
struct Inner {
    textures: HashMap<String, gdk::Texture>,
    /// The head of a video file — not a decoded image, so it sits beside
    /// `textures` rather than in it. See [`Avatars::video_head`].
    heads: HashMap<String, Rc<Vec<u8>>>,
    /// In flight, so redrawing a hundred messages does not start a hundred
    /// downloads of the same face.
    pending: HashSet<String>,
    /// Fetches that failed. Retried only via [`Avatars::forget`], so a broken
    /// avatar cannot turn into a request loop driven by every redraw.
    failed: HashSet<String>,
    /// Insertion order, so the oldest can be dropped when the cache is full.
    order: Vec<String>,
    /// What the textures add up to, kept as they go in and out rather than
    /// recomputed: the answer is wanted on every insert.
    held: usize,
    /// Live avatar widgets waiting for, or currently showing, a user's
    /// picture. Weak references keep virtualised rows recyclable: the cache
    /// must never become the owner of a row that GtkListView has unbound.
    avatar_widgets: HashMap<String, Vec<glib::WeakRef<adw::Avatar>>>,
}

/// What a decoded texture costs in memory: four bytes a pixel, whatever it
/// was compressed to on the wire.
fn texture_bytes(texture: &gdk::Texture) -> usize {
    use gdk::prelude::TextureExt;
    (texture.width().max(0) as usize) * (texture.height().max(0) as usize) * 4
}

impl Inner {
    /// Keeps the caches to their bounds. Called after every insert.
    fn trim(&mut self) {
        while self.heads.len() > MAX_HEADS {
            let Some(oldest) = self.heads.keys().next().cloned() else {
                break;
            };
            self.heads.remove(&oldest);
        }
        while self.held > MAX_TEXTURE_BYTES {
            let Some(oldest) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            if let Some(texture) = self.textures.remove(&oldest) {
                self.held = self.held.saturating_sub(texture_bytes(&texture));
                tracing::debug!(
                    held_mb = self.held / (1024 * 1024),
                    "picture cache full, dropped the oldest"
                );
            }
        }
    }
}

/// Fired on the GTK thread when a resource lands. The key lets consumers
/// invalidate one affected row instead of rebuilding every conversation.
type LoadedCallback = Rc<RefCell<Option<Box<dyn Fn(&str)>>>>;

/// Marks a cache key as a file thumbnail rather than a user's picture.
const FILE_PREFIX: &str = "file:";
/// Marks a cache key as a file's larger preview image, on the same cache but
/// never colliding with its own thumbnail entry.
const FILE_PREVIEW_PREFIX: &str = "preview:";
/// Marks a cache key as a custom emoji, looked up by name rather than by id.
const EMOJI_PREFIX: &str = "emoji:";
/// Marks a cache key as a video's head bytes (see [`Avatars::video_head`]).
const VIDEO_HEAD_PREFIX: &str = "video-head:";
/// Marks a cache key as a still taken from a video, which is made here rather
/// than fetched.
const POSTER_PREFIX: &str = "poster:";

/// How much of a video file to fetch for a poster attempt: enough to hold
/// `ftyp` + `moov` for a phone-shot clip's sample table, tiny next to the
/// clip itself. A file whose `moov` sits after `mdat` (no `-movflags
/// +faststart`) will not fit — `ui::media::head_playable` detects that
/// cheaply from what did come back, and the caller gives up rather than
/// asking for more.
const VIDEO_HEAD_BYTES: u64 = 1_500_000;

/// How much decoded picture to hold, in bytes. Counting entries was the wrong
/// unit by two orders of magnitude: an avatar is a few kilobytes and a posted
/// screenshot at draw size is four megabytes, so "a hundred and fifty of
/// them" meant anywhere between half a megabyte and six hundred. Evicting one
/// costs a refetch and nothing else — a picture still on screen is held by
/// the widget showing it.
///
/// ponytail: oldest-inserted rather than least-recently-used. An LRU needs
/// the read path to write, and the read path here is every redraw.
const MAX_TEXTURE_BYTES: usize = 96 * 1024 * 1024;

/// Video heads are a megabyte and a half each and are only read once, to make
/// a poster out of. A handful is plenty.
const MAX_HEADS: usize = 4;

/// The widest a picture is ever drawn: `message::scaled_size`'s cap, doubled
/// for a HiDPI screen. Decoding a 4000px photo to keep 500 of them is how a
/// conversation full of screenshots turns into gigabytes.
const MAX_DECODED: i32 = 1000;

/// Decodes to the size it will be drawn at rather than the size it arrived
/// in. `Pixbuf` scales during decode, so the full-size image is never held.
/// Faces and emoji are already small; only the file images are worth scaling.
fn decode(id: &str, bytes: Vec<u8>) -> Result<gdk::Texture, glib::Error> {
    let big = id.starts_with(FILE_PREVIEW_PREFIX) || id.starts_with(FILE_PREFIX);
    let data = glib::Bytes::from_owned(bytes);
    if !big {
        return gdk::Texture::from_bytes(&data);
    }
    let stream = gtk::gio::MemoryInputStream::from_bytes(&data);
    match gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        MAX_DECODED,
        MAX_DECODED,
        true,
        gtk::gio::Cancellable::NONE,
    ) {
        Ok(pixbuf) => Ok(gdk::Texture::for_pixbuf(&pixbuf)),
        // Pixbuf cannot read every format GdkTexture can, so a failure here
        // is a reason to try the plain path, not to give up on the picture.
        Err(_) => gdk::Texture::from_bytes(&data),
    }
}

#[derive(Clone)]
pub struct Avatars {
    inner: Rc<RefCell<Inner>>,
    client: Client,
    resources: ResourceCache,
    /// Notifies the view that it should redraw. Debouncing is the caller's
    /// business.
    on_loaded: LoadedCallback,
}

impl Avatars {
    pub fn new(client: Client) -> Self {
        let resources = ResourceCache::open(client.site_url());
        Avatars {
            inner: Rc::new(RefCell::new(Inner::default())),
            client,
            resources,
            on_loaded: Rc::new(RefCell::new(None)),
        }
    }

    pub(crate) fn resources(&self) -> ResourceCache {
        self.resources.clone()
    }

    pub fn connect_loaded(&self, f: impl Fn(&str) + 'static) {
        *self.on_loaded.borrow_mut() = Some(Box::new(f));
    }

    /// The texture for a user if we have it; otherwise starts a fetch and
    /// returns `None` so the caller can draw initials meanwhile.
    pub fn texture(&self, user_id: &str) -> Option<gdk::Texture> {
        self.cached(user_id)
    }

    /// A custom emoji's image, on the same cache as everything else here.
    /// Custom emoji are per-server uploads, so there is no local table to fall
    /// back to — either the picture arrives or the shortcode stands in.
    pub fn custom_emoji(&self, name: &str) -> Option<gdk::Texture> {
        self.cached(&format!("{EMOJI_PREFIX}{name}"))
    }

    /// The thumbnail for an attached image, on the same cache and the same
    /// "ask once, redraw when it lands" contract as a face. Keyed apart so a
    /// file id can never collide with a user id.
    pub fn file_thumbnail(&self, file_id: &str) -> Option<gdk::Texture> {
        self.cached(&format!("{FILE_PREFIX}{file_id}"))
    }

    /// An attached image's larger preview (server-capped at 1920px wide),
    /// for showing it inline without either a blurry thumbnail or a full
    /// download of the original.
    pub fn file_preview(&self, file_id: &str) -> Option<gdk::Texture> {
        self.cached(&format!("{FILE_PREVIEW_PREFIX}{file_id}"))
    }

    /// The first slice of a video file's bytes — not decoded into anything,
    /// just enough for the UI layer to judge whether a poster can be built
    /// from it. `None` while the fetch is in flight or has failed; a redraw
    /// triggered by [`connect_loaded`](Avatars::connect_loaded) is what
    /// picks it up once it lands.
    ///
    /// Cached like everything else here, so scrolling a video row out of view
    /// and back does not repeat the request.
    /// A video's still, if one has already been taken. Unlike everything else
    /// here it is never fetched — it is made locally from the head of the
    /// file and handed back with [`Avatars::remember_poster`] — so a miss
    /// starts nothing.
    pub fn poster(&self, file_id: &str) -> Option<gdk::Texture> {
        self.inner
            .borrow()
            .textures
            .get(&format!("{POSTER_PREFIX}{file_id}"))
            .cloned()
    }

    /// Keeps a still that was just taken, so the next redraw of that row
    /// draws a picture instead of starting a decoder over again.
    pub fn remember_poster(&self, file_id: &str, texture: gdk::Texture) {
        let key = format!("{POSTER_PREFIX}{file_id}");
        let mut inner = self.inner.borrow_mut();
        inner.held += texture_bytes(&texture);
        inner.order.push(key.clone());
        if let Some(old) = inner.textures.insert(key, texture) {
            inner.held = inner.held.saturating_sub(texture_bytes(&old));
        }
        inner.trim();
    }

    /// Records that a video's head cannot make a still — its index sits after
    /// the media, or the codec is missing — and lets the bytes go.
    ///
    /// Without this the head stayed wanted for ever: the small head cache
    /// evicted it for another clip's, the row asked again, the answer came
    /// back from disk at once and rebuilt the row, which evicted the next
    /// one. A channel with more clips than the cache holds rebuilt its video
    /// rows several times a second, each time with a fresh decoder — the
    /// blinking first video.
    pub fn no_still(&self, file_id: &str) {
        let key = format!("{VIDEO_HEAD_PREFIX}{file_id}");
        let mut inner = self.inner.borrow_mut();
        inner.heads.remove(&key);
        inner.failed.insert(key);
    }

    pub fn video_head(&self, file_id: &str) -> Option<Rc<Vec<u8>>> {
        // The prefix belongs in the lookup as well as the fetch. Without it
        // the hit never happened: every redraw asked again, every answer
        // redrew, and the two chased each other at the speed of the network
        // while the feed was rebuilt from scratch each time round.
        let key = format!("{VIDEO_HEAD_PREFIX}{file_id}");
        if let Some(bytes) = self.inner.borrow().heads.get(&key) {
            return Some(bytes.clone());
        }
        self.request(&key);
        None
    }

    fn cached(&self, key: &str) -> Option<gdk::Texture> {
        if let Some(texture) = self.inner.borrow().textures.get(key) {
            return Some(texture.clone());
        }
        self.request(key);
        None
    }

    /// Fills an avatar widget, falling back to initials until the picture
    /// arrives.
    pub fn apply(&self, avatar: &adw::Avatar, user_id: &str, display_name: &str) {
        avatar.set_text(Some(display_name));
        avatar.set_show_initials(true);
        match self.texture(user_id) {
            Some(texture) => avatar.set_custom_image(Some(&texture)),
            None => {
                avatar.set_custom_image(gdk::Paintable::NONE);
                if !user_id.is_empty() {
                    let mut inner = self.inner.borrow_mut();
                    let widgets = inner.avatar_widgets.entry(user_id.to_string()).or_default();
                    // A row may have been recycled since the last request.
                    // Clear its dead weak reference before adding the new one.
                    widgets.retain(|weak| weak.upgrade().is_some());
                    widgets.push(avatar.downgrade());
                }
            }
        }
    }

    /// Drops a cached picture so the next request refetches — used when a
    /// user's `last_picture_update` moves.
    pub fn forget(&self, user_id: &str) {
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(texture) = inner.textures.remove(user_id) {
                inner.held = inner.held.saturating_sub(texture_bytes(&texture));
            }
            inner.failed.remove(user_id);
            inner.order.retain(|key| key != user_id);
        }
        let resources = self.resources.clone();
        let key = user_id.to_string();
        runtime::runtime().spawn(async move { resources.remove(key).await });
        // Existing widgets still show the old paintable. Fetch immediately;
        // the weak widget list below will replace it in place when it lands.
        self.request(user_id);
    }

    fn request(&self, user_id: &str) {
        // A post from a webhook or an integration carries no author id, and
        // the server answers a request for one with "invalid user_id" — a
        // round trip to be told what is already known here.
        if user_id.is_empty() || user_id.ends_with(':') {
            return;
        }
        {
            let mut inner = self.inner.borrow_mut();
            if inner.pending.contains(user_id) || inner.failed.contains(user_id) {
                return;
            }
            inner.pending.insert(user_id.to_string());
        }

        let client = self.client.clone();
        let resources = self.resources.clone();
        let fetch_id = user_id.to_string();
        let done_id = user_id.to_string();
        let this = self.clone();

        runtime::spawn(
            async move {
                let ttl = if fetch_id.starts_with(EMOJI_PREFIX)
                    || (!fetch_id.contains(':') && !fetch_id.is_empty())
                {
                    // Names and profile pictures are mutable. A websocket
                    // update invalidates them immediately while the app is
                    // open; this bounds staleness across restarts.
                    std::time::Duration::from_secs(24 * 60 * 60)
                } else {
                    // File ids identify immutable uploads.
                    std::time::Duration::from_secs(30 * 24 * 60 * 60)
                };
                let cache_key = fetch_id.clone();
                resources
                    .get_or_fetch(cache_key, ttl, || async move {
                        if let Some(file_id) = fetch_id.strip_prefix(FILE_PREFIX) {
                            return client.file_thumbnail_bytes(file_id).await;
                        }
                        if let Some(file_id) = fetch_id.strip_prefix(FILE_PREVIEW_PREFIX) {
                            return client.file_preview_bytes(file_id).await;
                        }
                        if let Some(file_id) = fetch_id.strip_prefix(VIDEO_HEAD_PREFIX) {
                            return client.download_file_range(file_id, VIDEO_HEAD_BYTES).await;
                        }
                        if let Some(name) = fetch_id.strip_prefix(EMOJI_PREFIX) {
                            let emoji = client.emoji_by_name(name).await?;
                            return client.emoji_image_bytes(&emoji.id).await;
                        }
                        client.user_image_bytes(&fetch_id).await
                    })
                    .await
            },
            move |result| {
                let mut loaded = false;
                let mut avatar_widgets = Vec::new();
                let mut loaded_texture = None;
                {
                    let mut inner = this.inner.borrow_mut();
                    inner.pending.remove(&done_id);
                    match result {
                        // Raw bytes to keep, not an image to decode — the
                        // caller inspects and plays these, this cache just
                        // spares it a second fetch on the next redraw.
                        Ok(bytes) if done_id.starts_with(VIDEO_HEAD_PREFIX) => {
                            inner.heads.insert(done_id.clone(), Rc::new(bytes));
                            inner.trim();
                            loaded = true;
                        }
                        Ok(bytes) => {
                            match decode(&done_id, bytes) {
                                Ok(texture) => {
                                    inner.held += texture_bytes(&texture);
                                    inner.order.push(done_id.clone());
                                    if let Some(old) =
                                        inner.textures.insert(done_id.clone(), texture.clone())
                                    {
                                        inner.held = inner.held.saturating_sub(texture_bytes(&old));
                                    }
                                    loaded_texture = Some(texture);
                                    if !done_id.contains(':') {
                                        avatar_widgets = inner
                                            .avatar_widgets
                                            .get(&done_id)
                                            .cloned()
                                            .unwrap_or_default();
                                    }
                                    inner.trim();
                                    loaded = true;
                                }
                                // A shortcode that is neither Unicode nor uploaded to
                                // this server is an ordinary thing for someone to
                                // type, not a failure worth the word "failed".
                                Err(e) if done_id.starts_with(EMOJI_PREFIX) => {
                                    tracing::debug!(
                                        emoji = done_id.trim_start_matches(EMOJI_PREFIX),
                                        "no such custom emoji on this server"
                                    );
                                    let _ = e;
                                    inner.failed.insert(done_id.clone());
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "undecodable avatar image");
                                    inner.failed.insert(done_id.clone());
                                }
                            }
                        }
                        // A shortcode that is neither Unicode nor uploaded to
                        // this server is an ordinary thing for someone to
                        // type, not a failure worth the word "failed".
                        Err(e) if done_id.starts_with(EMOJI_PREFIX) => {
                            tracing::debug!(
                                emoji = done_id.trim_start_matches(EMOJI_PREFIX),
                                "no such custom emoji on this server"
                            );
                            let _ = e;
                            inner.failed.insert(done_id.clone());
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "avatar fetch failed");
                            inner.failed.insert(done_id.clone());
                        }
                    }
                }
                // The borrow is released before the callback: it will almost
                // certainly redraw, and redrawing reads this same map.
                if loaded {
                    if let Some(texture) = loaded_texture.as_ref() {
                        for weak in avatar_widgets {
                            if let Some(avatar) = weak.upgrade() {
                                avatar.set_custom_image(Some(texture));
                            }
                        }
                    }
                    if let Some(callback) = this.on_loaded.borrow().as_ref() {
                        callback(&done_id);
                    }
                }
            },
        );
    }
}
