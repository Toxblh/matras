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

/// Tall enough to watch, short enough that a video does not push the
/// conversation off the screen — the same 180px an `.attachment-image` gets.
const MAX_HEIGHT: i32 = 180;

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
    head.starts_with(&EBML_MAGIC) || mp4_head_has_full_moov(head)
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
pub fn video_still(file: &FileInfo, head: &[u8]) -> Option<(gtk::gdk::Paintable, PathBuf)> {
    if std::env::var("MATRAS_NO_STILL").is_ok() || !head_playable(head) {
        return None;
    }

    let path = std::env::temp_dir().join(format!(
        "matras-poster-{}.{}",
        sanitised(&file.id),
        sanitised(&extension(file))
    ));
    std::fs::write(&path, head).ok()?;

    // `GtkMediaFile` starts prerolling as soon as the file is set — it does
    // not wait to be shown or played — so the first frame arrives (and
    // whatever paintable this becomes redraws) on its own; nothing here has
    // to wait for it.
    let media = gtk::MediaFile::for_filename(&path);
    media.set_loop(false);
    media.connect_error_notify(|media| {
        // A head that passed the box scan but still fails to decode — a
        // codec GStreamer does not have, say — is exactly the "quietly do
        // nothing" case: the play button underneath still works, because it
        // downloads and plays the whole file rather than this stub.
        if let Some(error) = media.error() {
            tracing::debug!(error = %error, "no poster for this video");
        }
    });
    Some((media.upcast(), path))
}

/// A player for one attached video or audio file.
///
/// Drop it to clean up the temporary file the bytes were written to; the
/// widget stops playing on its own when it leaves the window.
/// The still shown before a video is played. Matches the size an image
/// attachment is drawn at, so a channel of clips and photos reads evenly.
const POSTER_WIDTH: i32 = 420;

/// How long the decoder is given to produce the first frame before the still
/// is copied out of it. Generous: getting nothing means the row keeps its
/// play button, which is where it started.
const STILL_WAIT: std::time::Duration = std::time::Duration::from_millis(900);
const POSTER_HEIGHT: i32 = 260;

pub struct Player {
    pub widget: gtk::Widget,
    body: gtk::Box,
    poster: gtk::Widget,
    play: gtk::Button,
    spinner: gtk::Spinner,
    audio: bool,
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
    /// ownership of and deletes on drop. `None` when there is nothing to
    /// show yet (the head has not arrived) or ever (the container's index
    /// was not in the head, or this is audio).
    /// `on_still` is handed the first frame once the decoder has produced it,
    /// so it can be kept and the decoder let go; see the comment where it is
    /// called.
    pub fn new(
        file: &FileInfo,
        poster: Option<(gtk::gdk::Paintable, PathBuf)>,
        on_still: impl Fn(gtk::gdk::Texture) + 'static,
        on_load: impl Fn(String) + 'static,
    ) -> Player {
        let (poster_image, poster_temp) = match poster {
            Some((paintable, path)) => (Some(paintable), Some(path)),
            None => (None, None),
        };
        let play = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text("Play")
            .valign(gtk::Align::Center)
            .build();
        play.add_css_class("flat");
        play.add_css_class("circular");

        // Takes the play button's place for as long as the download runs.
        let spinner = gtk::Spinner::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();

        let label = gtk::Label::builder()
            .label(format!("{}  ·  {}", file.name, file.human_size()))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        controls.add_css_class("dim-label");
        controls.append(&play);
        controls.append(&spinner);
        controls.append(&label);

        // With a thumbnail the controls sit *over* the still, the way a video
        // reads everywhere else. Without one, a video still has to read as a
        // video rather than as a line of text: a tile with the play button in
        // the middle of it, the name underneath.
        let poster: gtk::Widget = match &poster_image {
            Some(paintable) => {
                // Height only, and the width capped by a clamp: a width
                // request is a floor, and in a narrow panel GTK cannot honour
                // it.
                let still = gtk::Picture::builder()
                    .paintable(paintable)
                    .content_fit(gtk::ContentFit::Contain)
                    .halign(gtk::Align::Start)
                    .height_request(POSTER_HEIGHT)
                    .build();
                still.add_css_class("attachment-image");

                // A `GtkMediaFile` is a whole GStreamer pipeline, and it stays
                // one for as long as anything holds it: a channel of clips was
                // a hundred and forty megabytes of decoders sitting on a
                // single frame each. So the frame is copied out — that is what
                // `current_image` is, a still of whatever the paintable shows
                // — and the pipeline dropped. The caller keeps the copy, so
                // the next redraw of this row starts nothing at all.
                if paintable.is::<gtk::MediaFile>() {
                    let still = still.clone();
                    let paintable = paintable.clone();
                    glib::timeout_add_local_once(STILL_WAIT, move || {
                        let frame = paintable.current_image();
                        // A blank paintable answers with a 1x1; that is a
                        // decoder that never got there, not a still.
                        if frame.intrinsic_width() > 1 && frame.intrinsic_height() > 1 {
                            still.set_paintable(Some(&frame));
                            if let Ok(texture) = frame.downcast::<gtk::gdk::Texture>() {
                                on_still(texture);
                            }
                        }
                    });
                }

                // Out of the row before into the overlay: a widget cannot be
                // given a second parent while it still has the first.
                controls.remove(&play);
                controls.remove(&spinner);
                controls.remove(&label);

                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                play.add_css_class("osd");
                play.add_css_class("circular");
                spinner.set_halign(gtk::Align::Center);
                spinner.set_valign(gtk::Align::Center);
                label.set_valign(gtk::Align::End);
                label.set_halign(gtk::Align::Start);
                label.add_css_class("osd");

                let sized = adw::Clamp::builder()
                    .maximum_size(POSTER_WIDTH)
                    .halign(gtk::Align::Start)
                    .child(&still)
                    .build();
                let overlay = poster_overlay(&sized);
                overlay.add_overlay(&play);
                overlay.add_overlay(&spinner);
                overlay.add_overlay(&label);
                overlay.upcast()
            }
            None if is_audio(file) => controls.upcast(),
            // No still to show, so the tile is a blank one. The play button
            // is the whole of it, sized like a video rather than like a row,
            // because that shape is what says "this is something to watch".
            None => {
                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                play.add_css_class("osd");
                spinner.set_halign(gtk::Align::Center);
                spinner.set_valign(gtk::Align::Center);

                // Out of the row before into the tile: a widget cannot be
                // given a second parent while it still has the first.
                controls.remove(&play);
                controls.remove(&spinner);
                controls.set_halign(gtk::Align::Start);

                let face = gtk::Overlay::builder()
                    .height_request(POSTER_HEIGHT)
                    .build();
                face.add_css_class("video-tile");
                face.add_overlay(&play);
                face.add_overlay(&spinner);

                let tile = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(4)
                    .halign(gtk::Align::Start)
                    .build();
                tile.append(
                    &adw::Clamp::builder()
                        .maximum_size(POSTER_WIDTH)
                        .child(&face)
                        .build(),
                );
                tile.append(&controls);
                tile.upcast()
            }
        };

        play.connect_clicked({
            let spinner = spinner.clone();
            let id = file.id.clone();
            move |play| {
                play.set_visible(false);
                spinner.set_visible(true);
                spinner.start();
                on_load(id.clone());
            }
        });

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        body.append(&poster);

        Player {
            widget: body.clone().upcast(),
            body,
            poster: poster.upcast(),
            play,
            spinner,
            audio: is_audio(file),
            id: file.id.clone(),
            title: file.name.clone(),
            extension: extension(file),
            temp: RefCell::new(None),
            poster_temp: RefCell::new(poster_temp),
        }
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

        let player: gtk::Widget = if self.audio {
            // Audio has no picture to show, so the controls are the widget.
            gtk::MediaControls::new(Some(&media)).upcast()
        } else {
            let video = gtk::Video::for_media_stream(Some(&media));
            // We start it ourselves below, on the click that asked for it.
            video.set_autoplay(false);
            no_offload(&video);
            video.set_height_request(MAX_HEIGHT);
            // A `GtkVideo` asks for the full size of whatever it is showing,
            // and a 1080p attachment would take the conversation with it. The
            // clamp is what actually holds the height down; the picture inside
            // scales to fit rather than being cropped.
            let clamp = adw::Clamp::builder()
                .maximum_size(MAX_HEIGHT)
                .halign(gtk::Align::Start)
                .child(&video)
                .build();
            clamp.set_orientation(gtk::Orientation::Vertical);

            // 180px in a column of messages is a postage stamp. The same
            // stream over the whole window is what the web calls the lightbox,
            // and it is the same object, so pausing there pauses here.
            let expand = gtk::Button::builder()
                .icon_name("view-fullscreen-symbolic")
                .tooltip_text("Fill the window")
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
                    let Some(window) = button.root().and_downcast::<adw::ApplicationWindow>()
                    else {
                        return;
                    };
                    super::lightbox::show_media(&window, &title, media.upcast_ref());
                }
            });

            let framed = gtk::Overlay::builder().child(&clamp).build();
            framed.add_overlay(&expand);
            framed.upcast()
        };

        self.body.remove(&self.poster);
        self.body.append(&player);
        media.play();
    }

    /// Puts the play button back and says why nothing happened.
    fn failed(&self, message: &str) {
        tracing::warn!(message, "could not play the attachment");
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

/// Keeps the controls positioned against the poster, even when the message
/// row gets wider than the poster's clamp.
fn poster_overlay(child: &impl IsA<gtk::Widget>) -> gtk::Overlay {
    gtk::Overlay::builder()
        .child(child)
        .halign(poster_overlay_alignment())
        .build()
}

fn poster_overlay_alignment() -> gtk::Align {
    gtk::Align::Start
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
    fn poster_overlay_is_aligned_to_its_clamped_child() {
        assert_eq!(poster_overlay_alignment(), gtk::Align::Start);
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
