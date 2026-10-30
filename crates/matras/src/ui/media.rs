//! Playing an attached video or audio file where it was posted.
//!
//! A [`Player`] starts as a poster — a still, or failing that just the name,
//! the size and a play button — and only downloads the *whole* file when
//! someone presses play, because a channel scrolled past ten videos would
//! otherwise fetch ten videos. The still itself costs far less: the server
//! makes no thumbnail for video (it 400s `no_thumbnail`, confirmed against a
//! live server), so [`video_still`] builds one from just the head of the file
//! — see there for how much that is and which containers it works for.
//!
//! The file itself is behind the session token, so a URL handed to GStreamer
//! would come back 401; the bytes arrive the same way `open_image` gets an
//! image, through the client, and are handed back with [`Player::set_data`].

use std::cell::RefCell;
use std::path::PathBuf;

use gtk::glib;
use gtk::prelude::*;
use mattermost_api::models::FileInfo;

/// Containers and codecs a default GStreamer install usually cannot decode, so
/// offering a play button for them would only produce a black box.
///
/// Windows Media and RealMedia need `-ugly`/`-bad` demuxers plus `libav` for
/// the video; MPEG-1/2 program and transport streams need `mpeg2dec`; Flash
/// and DivX-flavoured AVI need `libav` decoders. All of those are packaged
/// separately from `gst-plugins-base`/`-good` on most distributions.
const UNDECODABLE_EXT: &[&str] = &[
    "asf", "wmv", "wma", "rm", "rmvb", "ram", "flv", "f4v", "mpg", "mpeg", "mpe", "m1v", "m2v",
    "vob", "ts", "m2ts", "mts", "avi", "divx", "mid", "midi",
];

/// The same list by mime type, for servers that fill it in.
const UNDECODABLE_MIME: &[&str] = &[
    "video/mpeg",
    "video/mp2t",
    "video/x-ms",
    "audio/x-ms",
    "video/x-flv",
    "video/x-msvideo",
    "video/vnd.rn-",
    "audio/vnd.rn-",
    "application/vnd.rn-",
    "audio/midi",
    "audio/x-midi",
];

/// What we play when the server leaves `mime_type` empty.
const VIDEO_EXT: &[&str] = &["mp4", "m4v", "mov", "webm", "mkv", "ogv", "3gp"];
const AUDIO_EXT: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "ogg", "oga", "opus", "weba", "aiff",
];

fn extension(file: &FileInfo) -> String {
    file.extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

/// True when this file is something we can play inline.
pub fn is_playable(file: &FileInfo) -> bool {
    let ext = extension(file);
    if UNDECODABLE_EXT.contains(&ext.as_str()) {
        return false;
    }

    // The mime type is the server's own answer, so it wins; the extension is
    // only consulted when there is nothing to win against.
    let mime = file.mime_type.trim().to_ascii_lowercase();
    if !mime.is_empty() {
        if UNDECODABLE_MIME.iter().any(|bad| mime.starts_with(bad)) {
            return false;
        }
        return mime.starts_with("video/") || mime.starts_with("audio/");
    }

    VIDEO_EXT.contains(&ext.as_str()) || AUDIO_EXT.contains(&ext.as_str())
}

/// True for the ones that have nothing to show: audio gets controls, no poster.
pub fn is_audio(file: &FileInfo) -> bool {
    let mime = file.mime_type.trim().to_ascii_lowercase();
    if !mime.is_empty() {
        return mime.starts_with("audio/");
    }
    AUDIO_EXT.contains(&extension(file).as_str())
}

/// Whether `head` — the first slice of a file, not necessarily all of it —
/// contains enough of the container's own index to build a poster from
/// without fetching any more of the file.
///
/// WebM/Matroska (EBML) is always worth trying: both mux their content as a
/// sequence of clusters that a demuxer can start reading from the front, so
/// there is no index that has to arrive first. An ISO base media file
/// (MP4/MOV/M4V) is the opposite: nothing can be decoded until `moov` (the
/// sample table) is in hand, and an encoder that skips `-movflags
/// +faststart` writes `mdat` — the (large) media itself — before it, putting
/// `moov` at the very end of the file. [`mp4_head_has_full_moov`] is the
/// cheap check for that; failing it means giving up quietly rather than
/// fetching the rest of a multi-hundred-MB file for a thumbnail.
fn head_playable(head: &[u8]) -> bool {
    // The four bytes every EBML document — WebM included — starts with.
    const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
    head.starts_with(&EBML_MAGIC) || head.starts_with(SPARSE_MAGIC) || mp4_head_has_full_moov(head)
}

/// Marks a head that carries the end of its file as well; see [`sparse_head`].
const SPARSE_MAGIC: &[u8; 8] = b"MTRSPRS1";

/// Where to fetch the index of an MP4 that was not written for streaming:
/// the end of the top-level box that runs past `head` — the media, `mdat` —
/// which is where `moov` starts. `None` when the head needs no help (it holds
/// `moov`, or is not an MP4) or the layout makes no sense.
///
/// Most clips from a phone are laid out this way, which is why so few had a
/// still. The browser plays them by asking for the end of the file; so does
/// this, and the end is usually tens of kilobytes.
pub(crate) fn mp4_tail_offset(head: &[u8]) -> Option<u64> {
    if head.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let mut pos = 0u64;
    let len = head.len() as u64;
    while pos + 8 <= len {
        let at = pos as usize;
        let size32 = u32::from_be_bytes(head[at..at + 4].try_into().ok()?) as u64;
        let kind = &head[at + 4..at + 8];
        let size = match size32 {
            1 => u64::from_be_bytes(head.get(at + 8..at + 16)?.try_into().ok()?),
            0 => return None, // runs to the end: nothing after it to fetch
            n => n,
        };
        if kind == b"moov" || size < 8 {
            return None;
        }
        if pos + size > len {
            return Some(pos + size);
        }
        pos += size;
    }
    None
}

/// Packs a head and the tail fetched from `offset` into one blob for the
/// cache. [`write_head`] unpacks it into a file of the full length with only
/// those two parts filled in — enough for a demuxer to read the index from the
/// end and the first frames from the start.
pub(crate) fn sparse_head(head: &[u8], offset: u64, tail: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(head.len() + tail.len() + 24);
    out.extend_from_slice(SPARSE_MAGIC);
    out.extend_from_slice(&(head.len() as u64).to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(head);
    out.extend_from_slice(tail);
    out
}

/// Writes what [`video_still`] decodes from: the head as it is, or a packed
/// head and tail spread out to where they belong in the file.
fn write_head(path: &std::path::Path, head: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let Some(rest) = head.strip_prefix(SPARSE_MAGIC) else {
        return std::fs::write(path, head);
    };
    let bad = || std::io::Error::other("truncated sparse head");
    let head_len = u64::from_be_bytes(rest.get(0..8).ok_or_else(bad)?.try_into().unwrap());
    let offset = u64::from_be_bytes(rest.get(8..16).ok_or_else(bad)?.try_into().unwrap());
    let body = &rest[16..];
    let (front, tail) = body.split_at((head_len as usize).min(body.len()));
    let mut file = std::fs::File::create(path)?;
    file.write_all(front)?;
    // Sparse: the middle is never written, so it costs no disk.
    file.set_len(offset + tail.len() as u64)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(tail)
}

/// Walks the top-level ISO-BMFF boxes in `head` (`[u32 size][4cc type][...]`,
/// or a `u64` size when the 32-bit one reads as `1`) looking for a `moov`
/// whose declared size ends inside what was fetched.
///
/// For a non-faststart file this runs out of `head` while still inside the
/// leading `mdat` — which, being the whole video, is almost always bigger
/// than `head` itself — and returns `false` well before reaching the real
/// `moov` at the end. That is the giving-up case, not a bug: the caller does
/// not fetch further to find out for certain.
fn mp4_head_has_full_moov(head: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos + 8 <= head.len() {
        let size32 = u32::from_be_bytes(head[pos..pos + 4].try_into().unwrap()) as u64;
        let kind = &head[pos + 4..pos + 8];

        let (header_len, size) = if size32 == 1 {
            // The real size is a 64-bit field right after the ordinary
            // header; a box needing one is rare enough that the head simply
            // not containing it is treated the same as not finding `moov`.
            if pos + 16 > head.len() {
                break;
            }
            (
                16u64,
                u64::from_be_bytes(head[pos + 8..pos + 16].try_into().unwrap()),
            )
        } else if size32 == 0 {
            // "Runs to the end of the file" — never true of a box we still
            // need to skip past to reach `moov` within a bounded head.
            break;
        } else {
            (8, size32)
        };

        if kind == b"moov" {
            return pos as u64 + size <= head.len() as u64;
        }
        if size < header_len {
            break; // malformed box: an infinite loop is worse than giving up
        }
        match pos.checked_add(size as usize) {
            Some(next) => pos = next,
            None => break,
        }
    }
    false
}

/// A still frame for a video attachment, decoded from just `head` (see
/// [`head_playable`] for which containers that works for). `None` when the
/// container's index was not in it — the caller keeps the plain play button
/// it already had rather than triggering a second, larger fetch.
///
/// The returned path is the temporary file GStreamer reads the still from;
/// the caller owns it from here and must delete it once the paintable is no
/// longer shown, the same as the full download in [`Player::set_data`] does
/// for itself.
pub fn video_still(file: &FileInfo, head: &[u8]) -> Option<PathBuf> {
    if std::env::var("MATRAS_NO_STILL").is_ok() || !head_playable(head) {
        return None;
    }
    // One file per player, not per clip: a row rebuilt while the old one
    // was still decoding would otherwise have its file deleted from under it
    // when the old player went.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "matras-poster-{}-{}-{n}.{}",
        sanitised(&file.id),
        std::process::id(),
        sanitised(&extension(file))
    ));
    write_head(&path, head).ok()?;
    Some(path)
}

/// The largest a video is drawn in the conversation, the same box an image
/// gets, so a channel of clips and photos reads evenly.
const FRAME_MAX_WIDTH: i32 = 480;
const FRAME_MAX_HEIGHT: i32 = 320;

/// The frame for a video of `width`×`height`, inside the maximum box with its
/// shape kept. Unknown dimensions get 16:9, the shape most clips are.
pub(crate) fn frame_size(width: i32, height: i32) -> (i32, i32) {
    let (w, h) = if width > 1 && height > 1 {
        (width as f64, height as f64)
    } else {
        (16.0, 9.0)
    };
    let scale = (FRAME_MAX_WIDTH as f64 / w).min(FRAME_MAX_HEIGHT as f64 / h);
    ((w * scale).round() as i32, (h * scale).round() as i32)
}

/// `1:05`, `12:40`, `1:02:03` — a clip's length the way players write it.
pub(crate) fn format_duration(micros: i64) -> String {
    let total = (micros / 1_000_000).max(0);
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// What a video row starts with.
pub enum Poster {
    /// Nothing yet, or nothing ever.
    None,
    /// A still kept from before.
    Still(gtk::gdk::Texture),
    /// The head of the file on disk, to take a still from.
    Head(PathBuf),
}

/// What a still knows about its clip, kept with the still so a redraw has it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StillInfo {
    /// Microseconds; 0 when the decoder did not say.
    pub duration: i64,
    /// The picture as it is meant to be seen — after rotation — or 0×0 when
    /// unknown.
    pub width: i32,
    pub height: i32,
    /// Clockwise degrees the decoded frames must be turned: 0, 90, 180, 270.
    pub rotation: u16,
}

/// The video track's size and rotation from an MP4's `tkhd` boxes.
///
/// A phone films sideways and says so in the track's matrix instead of
/// turning the pixels; a browser honours that, GStreamer's GTK sink does not,
/// so without this a portrait clip lies on its side.
pub(crate) fn mp4_geometry(head: &[u8]) -> Option<StillInfo> {
    let mut from = 0;
    while let Some(found) = find(&head[from..], b"tkhd") {
        let at = from + found;
        from = at + 4;
        // [size][tkhd][version+flags]…; version 1 has 64-bit times.
        let version = *head.get(at + 4)?;
        let matrix_at = at + 8 + if version == 1 { 32 } else { 20 } + 16;
        let int = |o: usize| -> Option<i32> {
            Some(i32::from_be_bytes(head.get(o..o + 4)?.try_into().ok()?))
        };
        let (width, height) = (int(matrix_at + 36)? >> 16, int(matrix_at + 40)? >> 16);
        if width <= 0 || height <= 0 {
            continue; // an audio track
        }
        // a b u / c d v / x y w, in 16.16; only the turn matters here.
        let (a, b, c) = (int(matrix_at)?, int(matrix_at + 4)?, int(matrix_at + 12)?);
        let rotation = match (a.signum(), b.signum(), c.signum()) {
            (0, 1, -1) => 90,
            (-1, 0, 0) => 180,
            (0, -1, 1) => 270,
            _ => 0,
        };
        let (width, height) = if rotation % 180 == 90 {
            (height, width)
        } else {
            (width, height)
        };
        return Some(StillInfo {
            width,
            height,
            rotation,
            ..Default::default()
        });
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A player for one attached video or audio file.
///
/// A video is one frame, sized to the clip's own shape: the still, a play
/// button in the middle and the length in the corner, the way every
/// messenger draws one. Clicking anywhere on it downloads the clip and plays
/// it *in that same frame*, so nothing moves.
///
/// Drop it to clean up the temporary file the bytes were written to; the
/// widget stops playing on its own when it leaves the window.
pub struct Player {
    pub widget: gtk::Widget,
    body: gtk::Box,
    /// The video's frame; `None` for audio.
    frame: Option<gtk::Overlay>,
    /// What the frame shows before playing.
    face: gtk::Widget,
    play: gtk::Button,
    spinner: gtk::Spinner,
    badge: gtk::Label,
    id: String,
    title: String,
    extension: String,
    temp: RefCell<Option<PathBuf>>,
    /// The head-only file backing the still, if there is one — separate from
    /// `temp` (the *full* download) because the two can be alive together:
    /// the still is still on screen for the moment right after Play is
    /// pressed but before the real bytes land.
    poster_temp: RefCell<Option<PathBuf>>,
}

impl Drop for Player {
    fn drop(&mut self) {
        for slot in [&self.temp, &self.poster_temp] {
            if let Some(path) = slot.borrow_mut().take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

impl Player {
    /// `on_load` is called with the file id when the user asks to play;
    /// answer it with [`Player::set_data`].
    ///
    /// `poster` is what [`video_still`] built from the head of the file —
    /// a paintable and the temp file backing it, which this `Player` takes
    /// ownership of and deletes on drop — or a still kept from before, with
    /// what was learnt about the clip then. `None` when there is nothing to
    /// show yet (the head has not arrived) or ever.
    /// `on_still` is handed the first frame once the decoder has produced it,
    /// so it can be kept and the decoder let go; see the comment where it is
    /// called. `None` means the decoder never got there.
    pub fn new(
        file: &FileInfo,
        poster: Poster,
        known: StillInfo,
        on_still: impl Fn(Option<(gtk::gdk::Texture, StillInfo)>) + 'static,
        on_load: impl Fn(String) + 'static,
    ) -> Player {
        let audio = is_audio(file);
        let poster_temp = match &poster {
            Poster::Head(path) => Some(path.clone()),
            _ => None,
        };
        let play = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text("Play")
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        play.add_css_class("circular");

        // Takes the play button's place for as long as the download runs.
        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .visible(false)
            .build();

        let size = file.human_size().to_string();
        let label_size = size.clone();
        let badge = gtk::Label::builder()
            .label(if known.duration > 0 {
                format!("{}  ·  {size}", format_duration(known.duration))
            } else {
                size.clone()
            })
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .build();
        badge.add_css_class("video-badge");
        badge.add_css_class("numeric");

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();

        if audio {
            // Audio has nothing to look at: a row with the button and the name.
            play.add_css_class("flat");
            let label = gtk::Label::builder()
                .label(format!("{}  ·  {size}", file.name))
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .build();
            let row = gtk::Box::builder().spacing(6).build();
            row.add_css_class("dim-label");
            row.append(&play);
            row.append(&spinner);
            row.append(&label);
            body.append(&row);
            let face = row.upcast::<gtk::Widget>();
            let player = Player {
                widget: body.clone().upcast(),
                body,
                frame: None,
                face,
                play,
                spinner,
                badge,
                id: file.id.clone(),
                title: file.name.clone(),
                extension: extension(file),
                temp: RefCell::new(None),
                poster_temp: RefCell::new(poster_temp),
            };
            player.connect_play(on_load);
            return player;
        }

        // A kept still is already the right way up and the right shape;
        // before one exists the container's own word is taken for it.
        let shape = match &poster {
            Poster::Still(texture) => (texture.width(), texture.height()),
            _ => (known.width, known.height),
        };
        let (width, height) = frame_size(shape.0, shape.1);

        let still = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .build();
        match poster {
            Poster::Still(texture) => still.set_paintable(Some(&texture)),
            Poster::Head(path) => {
                // Decoded off the GTK thread, at twice the frame's size for a
                // HiDPI screen; the dark tile shows until it lands.
                let size = (known.width > 0).then(|| (width as u32 * 2, height as u32 * 2));
                let still = still.clone();
                let badge = badge.clone();
                let label = label_size.clone();
                crate::runtime::spawn(
                    async move {
                        tokio::task::spawn_blocking(move || crate::video::still(&path, size))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                    },
                    move |result| match result {
                        Ok(frame) => {
                            let texture: gtk::gdk::Texture = gtk::gdk::MemoryTexture::new(
                                frame.width as i32,
                                frame.height as i32,
                                gtk::gdk::MemoryFormat::R8g8b8a8,
                                &glib::Bytes::from_owned(frame.pixels),
                                frame.stride,
                            )
                            .upcast();
                            still.set_paintable(Some(&texture));
                            if frame.duration > 0 {
                                badge.set_label(&format!(
                                    "{}  ·  {label}",
                                    format_duration(frame.duration)
                                ));
                            }
                            on_still(Some((
                                texture,
                                StillInfo {
                                    duration: frame.duration,
                                    ..known
                                },
                            )));
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "no still for this video");
                            on_still(None);
                        }
                    },
                );
            }
            Poster::None => {}
        }
        let face: gtk::Widget = still.upcast();

        // The frame's size comes from an empty paintable of the clip's shape,
        // never from what is shown in it: a decoder reports no size at all
        // until its first frame, and a frame sized by it collapsed to a sliver.
        // The still, the spinner and the playing video lie over it.
        let sizer = gtk::Picture::for_paintable(&gtk::gdk::Paintable::new_empty(width, height));
        sizer.add_css_class("video-tile");
        let frame = gtk::Overlay::builder()
            .child(&sizer)
            .height_request(height)
            .overflow(gtk::Overflow::Hidden)
            .tooltip_text(&file.name)
            .build();
        frame.add_css_class("video-frame");
        frame.add_overlay(&face);
        play.add_css_class("video-play");
        frame.add_overlay(&play);
        frame.add_overlay(&spinner);
        frame.add_overlay(&badge);
        // A narrow thread panel can still have it: the clamp lets the frame
        // shrink below its width rather than overflow the row.
        let clamp = adw::Clamp::builder()
            .maximum_size(width)
            .tightening_threshold(width)
            .halign(gtk::Align::Start)
            .child(&frame)
            .build();
        body.append(&clamp);

        // The whole picture is the button, as it is everywhere else.
        let click = gtk::GestureClick::new();
        click.connect_released({
            let play = play.clone();
            move |gesture, _, _, _| {
                if play.is_visible() {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    play.emit_clicked();
                }
            }
        });
        frame.add_controller(click);

        let player = Player {
            widget: body.clone().upcast(),
            body,
            frame: Some(frame),
            face,
            play,
            spinner,
            badge,
            id: file.id.clone(),
            title: file.name.clone(),
            extension: extension(file),
            temp: RefCell::new(None),
            poster_temp: RefCell::new(poster_temp),
        };
        player.connect_play(on_load);
        player
    }

    fn connect_play(&self, on_load: impl Fn(String) + 'static) {
        self.play.connect_clicked({
            let spinner = self.spinner.clone();
            let id = self.id.clone();
            move |play| {
                play.set_visible(false);
                spinner.set_visible(true);
                spinner.start();
                on_load(id.clone());
            }
        });
    }

    /// Hands over the downloaded bytes; the player starts from them.
    pub fn set_data(&self, bytes: Vec<u8>) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        if self.temp.borrow().is_some() {
            return;
        }

        // A temporary file rather than a `gio::MemoryInputStream`, because
        // GTK's GStreamer backend only learned to read a `MediaFile` built
        // from a stream recently: before that it answered one with "Input
        // Streams are currently not supported. Please pass a File based
        // MediaFile." It works here on GTK 4.22 and does nothing at all on the
        // 4.18 in the GNOME 48 flatpak runtime; `for_filename` works on both.
        let path = std::env::temp_dir().join(format!(
            "matras-{}.{}",
            sanitised(&self.id),
            sanitised(&self.extension)
        ));
        if let Err(e) = std::fs::write(&path, &bytes) {
            self.failed(&format!("could not write {}: {e}", path.display()));
            return;
        }
        *self.temp.borrow_mut() = Some(path.clone());

        let media = gtk::MediaFile::for_filename(&path);
        media.set_loop(false);
        media.connect_error_notify({
            let body = self.body.clone();
            move |media| {
                let Some(error) = media.error() else { return };
                tracing::warn!(error = %error, "could not play the attachment");
                body.append(&note(&error.to_string()));
            }
        });

        let Some(frame) = self.frame.as_ref() else {
            // Audio: the controls take the row's place.
            self.body.remove(&self.face);
            self.body
                .append(&gtk::MediaControls::new(Some(&media)).upcast::<gtk::Widget>());
            media.play();
            return;
        };

        let video = gtk::Video::for_media_stream(Some(&media));
        // We start it ourselves below, on the click that asked for it.
        video.set_autoplay(false);
        no_offload(&video);

        // The same stream over the whole window is what the web calls the
        // lightbox, and it is the same object, so pausing there pauses here.
        let expand = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text("Fill Window")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_end(6)
            .build();
        expand.add_css_class("osd");
        expand.add_css_class("circular");
        expand.connect_clicked({
            let media = media.clone();
            let title = self.title.clone();
            move |button| {
                let Some(window) = button.root().and_downcast::<adw::ApplicationWindow>() else {
                    return;
                };
                super::lightbox::show_media(&window, &title, media.upcast_ref());
            }
        });

        // Into the same frame, over the still, so the clip plays exactly where
        // its still was and at its size.
        frame.remove_overlay(&self.face);
        frame.add_overlay(&video);
        self.play.set_visible(false);
        self.badge.set_visible(false);
        frame.add_overlay(&expand);
        media.play();
    }

    /// Puts the play button back and says why nothing happened.
    pub fn failed(&self, message: &str) {
        tracing::warn!(message, "could not play the attachment");
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.play.set_visible(true);
        self.body.append(&note(message));
    }
}

/// Keeps the video frames inside this process instead of handing them to the
/// compositor.
///
/// GTK's graphics offload passes the decoder's dmabuf straight through to a
/// Wayland subsurface, so the buffer is imported by the compositor — and when
/// that import goes wrong on a driver, what dies is the whole session rather
/// than this window. That is what happened here on an Intel Arrow Lake i915;
/// the cause is unproven, but with offload off the same fault can only take
/// down the app.
///
/// ponytail: turned off everywhere rather than only where it misbehaves. The
/// cost is a copy per frame for a video in a chat window. Revisit when the
/// driver is known good.
fn no_offload(video: &gtk::Video) {
    video.set_graphics_offload(gtk::GraphicsOffloadEnabled::Disabled);
}

fn note(message: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(message)
        .xalign(0.0)
        .wrap(true)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label
}

/// Both halves of the temporary file's name come from the server, and both end
/// up in a path.
fn sanitised(text: &str) -> String {
    let clean: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    if clean.is_empty() {
        "bin".to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_keep_the_clip_shape_inside_the_box() {
        assert_eq!(frame_size(1920, 1080), (480, 270));
        // Portrait phone video: height-bound.
        assert_eq!(frame_size(1080, 1920), (180, 320));
        assert_eq!(frame_size(0, 0), (480, 270));
    }

    /// A `tkhd` as a phone writes it: 1920×1080 frames to be turned 90°.
    #[test]
    fn a_sideways_phone_clip_is_portrait() {
        let mut tkhd = vec![0u8; 4];
        tkhd.extend(b"tkhd");
        tkhd.extend([0u8; 4 + 20 + 16]); // version 0, times, layer/volume
        for v in [0i32, 65536, 0, -65536, 0, 0, 0, 0, 1 << 30] {
            tkhd.extend(v.to_be_bytes());
        }
        tkhd.extend((1920i32 << 16).to_be_bytes());
        tkhd.extend((1080i32 << 16).to_be_bytes());
        let info = mp4_geometry(&tkhd).unwrap();
        assert_eq!((info.width, info.height, info.rotation), (1080, 1920, 90));
    }

    #[test]
    fn durations_read_like_a_player_writes_them() {
        assert_eq!(format_duration(65_000_000), "1:05");
        assert_eq!(format_duration(3_723_000_000), "1:02:03");
        assert_eq!(format_duration(0), "0:00");
    }

    fn file(mime: &str, extension: &str) -> FileInfo {
        FileInfo {
            mime_type: mime.to_string(),
            extension: extension.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_mime_type_decides_when_there_is_one() {
        assert!(is_playable(&file("video/mp4", "mp4")));
        assert!(is_playable(&file("audio/mpeg", "mp3")));
        // An extension we would not have guessed from, and one we would have
        // guessed wrong from.
        assert!(is_playable(&file("video/webm", "")));
        assert!(!is_playable(&file("application/pdf", "pdf")));
        assert!(!is_playable(&file("image/png", "png")));
    }

    #[test]
    fn the_extension_answers_when_the_mime_type_is_empty() {
        assert!(is_playable(&file("", "mp4")));
        assert!(is_playable(&file("", ".MP3")));
        assert!(is_playable(&file("", "opus")));
        assert!(!is_playable(&file("", "pdf")));
    }

    #[test]
    fn nothing_to_go_on_is_not_playable() {
        assert!(!is_playable(&file("", "")));
    }

    #[test]
    fn formats_gstreamer_will_not_have_are_excluded() {
        // Excluded by mime type and by extension alike, because the server
        // may give us either.
        assert!(!is_playable(&file("video/x-ms-wmv", "wmv")));
        assert!(!is_playable(&file("video/mp4", "wmv")));
        assert!(!is_playable(&file("", "avi")));
        assert!(!is_playable(&file("video/mpeg", "")));
    }

    #[test]
    fn audio_is_told_apart_from_video() {
        assert!(is_audio(&file("audio/flac", "flac")));
        assert!(!is_audio(&file("video/mp4", "mp4")));
        assert!(is_audio(&file("", "m4a")));
        assert!(!is_audio(&file("", "mkv")));
    }

    /// A single ISO-BMFF box: 4-byte size (including this header) + 4-byte
    /// type + `payload`.
    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// `ffmpeg -movflags +faststart` on a real clip lays out `ftyp`, `moov`,
    /// `free`, `mdat`, in that order (verified against `/tmp/mmtest.mp4`) —
    /// `moov` is small and near the front, so a bounded head contains it whole.
    #[test]
    fn a_faststart_head_has_a_full_moov() {
        let mut head = mp4_box(b"ftyp", &[0; 24]);
        head.extend(mp4_box(b"moov", &[0; 200]));
        // The rest of the file (`mdat`) need not even be present.
        assert!(head_playable(&head));
    }

    /// Without `+faststart`, `ffmpeg` instead writes `ftyp`, `free`, `mdat`,
    /// `moov` — `mdat` is the whole video, so a head bounded well short of
    /// the file's end never reaches the trailing `moov`.
    #[test]
    fn a_streaming_unfriendly_head_gives_up() {
        let mut head = mp4_box(b"ftyp", &[0; 24]);
        head.extend(mp4_box(b"free", &[]));
        // Declares a size far bigger than what's actually in `head` — the
        // stand-in for "the rest of a multi-hundred-MB clip".
        head.extend(50u32.to_be_bytes());
        head.extend(b"mdat");
        // No `moov` ever follows within this slice.
        assert!(!head_playable(&head));
    }

    /// `moov`'s header is in the head, but its declared size runs past the
    /// end of what was actually fetched — an incomplete atom is as useless
    /// as no atom, and must not be read out of bounds either.
    #[test]
    fn a_moov_box_cut_off_mid_atom_gives_up() {
        let mut head = mp4_box(b"ftyp", &[0; 24]);
        head.extend(200u32.to_be_bytes());
        head.extend(b"moov");
        head.extend([0; 50]); // far short of the 200 the header promises
        assert!(!head_playable(&head));
    }

    #[test]
    fn a_trailing_index_is_found_after_the_media() {
        let mut head = mp4_box(b"ftyp", &[0; 24]);
        let mdat_at = head.len() as u64;
        head.extend(5_000_000u32.to_be_bytes());
        head.extend(b"mdat");
        head.extend([0; 100]);
        assert_eq!(mp4_tail_offset(&head), Some(mdat_at + 5_000_000));
        // Written for streaming: nothing to fetch.
        let mut fast = mp4_box(b"ftyp", &[0; 24]);
        fast.extend(mp4_box(b"moov", &[0; 50]));
        assert_eq!(mp4_tail_offset(&fast), None);
        assert_eq!(mp4_tail_offset(b"\x1a\x45\xdf\xa3 webm"), None);
    }

    #[test]
    fn a_sparse_head_unpacks_to_the_full_length() {
        let blob = sparse_head(b"HEAD", 100, b"TAIL");
        assert!(head_playable(&blob));
        let path = std::env::temp_dir().join(format!("matras-sparse-test-{}", std::process::id()));
        write_head(&path, &blob).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len(), 104);
        assert_eq!(&bytes[..4], b"HEAD");
        assert_eq!(&bytes[100..], b"TAIL");
    }

    /// WebM/Matroska never needs the box scan at all — only the magic bytes.
    #[test]
    fn a_webm_head_is_always_playable() {
        let mut head = vec![0x1A, 0x45, 0xDF, 0xA3];
        head.extend([0; 16]); // arbitrary EBML content
        assert!(head_playable(&head));
    }

    #[test]
    fn empty_or_garbage_is_not_playable() {
        assert!(!head_playable(&[]));
        assert!(!head_playable(b"not a media file at all"));
    }
}
