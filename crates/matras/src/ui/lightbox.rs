//! The full-size view of an attached image, drawn over the window instead of
//! in a window of its own.
//!
//! What it replaces was a second `adw::Window`: it opened wherever the
//! compositor felt like putting it, needed a header bar to be closable at all,
//! and read as a different app. A lightbox is what people expect from a picture
//! on the web — it covers what you were looking at, and a click next to it puts
//! you back.
//!
//! # The CSS this needs in `style.css`
//!
//! ```css
//! /* ── lightbox ──────────────────────────────────────────────────────────
//!    The backdrop behind a full-size image. Deliberately not a themed colour:
//!    it covers the whole window and its one job is to drop everything behind
//!    the picture out of the way. */
//! .lightbox {
//!   background-color: alpha(black, 0.8);
//! }
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// Everything one open lightbox has to undo, in one struct so that dismissing
/// can `take()` the lot.
///
/// That `take` is doing two jobs. It makes dismiss idempotent — Escape, a click
/// on the backdrop and the close button can all arrive for the same gesture,
/// and the second one finds `None` and returns — and it drops the last strong
/// references at the same moment, which is what breaks the cycle from the
/// window, through the overlay and the buttons, back to this closure.
struct Open {
    parent: adw::ApplicationWindow,
    overlay: gtk::Overlay,
    dim: gtk::Box,
    /// True when `overlay` was built here and has to be taken back out. False
    /// when the window content already was an overlay, in which case nothing
    /// was moved and nothing may be moved back.
    wrapped: bool,
    /// The content that was displaced to make room for `overlay`.
    displaced: Option<gtk::Widget>,
    keys: gtk::EventControllerKey,
}

/// Shows an image over the whole window, dimmed, like a lightbox.
/// `parent` is the application window; the overlay is added to it and removes
/// itself when dismissed.
///
/// Dismissed by Escape, by the close button, or by a click on the dimmed area —
/// but not by a click on the picture, which is where you click to look at it.
/// Ctrl+C, and the copy button, put the image on the clipboard.
pub fn show(parent: &adw::ApplicationWindow, title: &str, texture: &gtk::gdk::Texture) {
    // The picture is handed the whole area and centres itself inside it, so an
    // image smaller than the window is drawn at its own size and a bigger one
    // is scaled down to fit. Never scaled up: `ScaleDown` only ever shrinks.
    let picture = gtk::Picture::builder()
        .paintable(texture)
        .content_fit(gtk::ContentFit::ScaleDown)
        .can_shrink(true)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .tooltip_text(title)
        .build();

    let copy = osd_button("edit-copy-symbolic", "Copy Image");
    let save = osd_button("document-save-symbolic", "Save Image As…");
    let close = osd_button("window-close-symbolic", "Close");

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_end(12)
        .build();
    buttons.append(&copy);
    buttons.append(&save);
    buttons.append(&close);

    let (dismiss, keys) = mount(parent, &picture, &buttons, None);

    // The clipboard belongs to the display, not to the window, so holding one
    // here keeps no widget alive.
    let to_clipboard: Rc<dyn Fn()> = Rc::new({
        let clipboard = parent.clipboard();
        let texture = texture.clone();
        move || clipboard.set_texture(&texture)
    });

    close.connect_clicked({
        let dismiss = dismiss.clone();
        move |_| dismiss()
    });
    // Somewhere to land the focus, so Escape is not the only way out for a
    // keyboard, and so typing does not go on filling the entry underneath.
    close.grab_focus();

    copy.connect_clicked({
        let to_clipboard = to_clipboard.clone();
        move |button| {
            to_clipboard();
            // The only feedback a clipboard ever gives is that nothing
            // happened, so the button says so itself for a moment.
            button.set_icon_name("object-select-symbolic");
            glib::timeout_add_local_once(std::time::Duration::from_secs(2), {
                let button = button.clone();
                move || button.set_icon_name("edit-copy-symbolic")
            });
        }
    });

    save.connect_clicked({
        let texture = texture.clone();
        let name = png_name(title);
        move |button| {
            let dialog = gtk::FileDialog::builder()
                .title("Save Image")
                .initial_name(&name)
                .modal(true)
                .build();
            let window = button.root().and_downcast::<adw::ApplicationWindow>();
            let toast_window = window.clone();
            let texture = texture.clone();
            dialog.save(
                window.as_ref(),
                gtk::gio::Cancellable::NONE,
                move |result| {
                    let Ok(file) = result else {
                        return; // Cancelled, which is not an error.
                    };
                    let Some(path) = file.path() else { return };
                    let title = match texture.save_to_png(&path) {
                        Ok(()) => super::message::saved_toast(&path),
                        Err(e) => {
                            tracing::warn!(error = %e, ?path, "could not write the image");
                            "Could not save file".to_string()
                        }
                    };
                    if let Some(window) = &toast_window {
                        super::message::window_toast(window, &title);
                    }
                },
            );
        }
    });

    // Escape closes, Ctrl+C copies; everything else carries on to whatever has
    // the focus.
    keys.connect_key_pressed(move |_, key, _, modifier| match key {
        gtk::gdk::Key::Escape => {
            dismiss();
            glib::Propagation::Stop
        }
        gtk::gdk::Key::c | gtk::gdk::Key::C
            if modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK) =>
        {
            to_clipboard();
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
}

/// The same, for something being played rather than looked at: the video fills
/// the window, the backdrop and Escape put it back, and closing stops it.
///
/// The stream is shared with the row it was opened from rather than copied —
/// two `GtkVideo`s onto one `GtkMediaStream` show the same frame, so pausing
/// in one place pauses in both and the position never has to be handed over.
pub fn show_media(parent: &adw::ApplicationWindow, title: &str, media: &gtk::MediaStream) {
    let video = gtk::Video::builder()
        // Same reason as the row's own player; see `media::no_offload`.
        .graphics_offload(gtk::GraphicsOffloadEnabled::Disabled)
        .media_stream(media)
        .autoplay(true)
        .tooltip_text(title)
        .hexpand(true)
        .vexpand(true)
        .build();

    let close = osd_button("window-close-symbolic", "Close");
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_end(12)
        .build();
    buttons.append(&close);

    let (dismiss, keys) = mount(
        parent,
        &video,
        &buttons,
        Some(Box::new({
            let media = media.clone();
            // Sound going on behind a lightbox nobody can see is worse than
            // losing the position, and the position is kept anyway.
            move || media.pause()
        })),
    );

    close.connect_clicked({
        let dismiss = dismiss.clone();
        move |_| dismiss()
    });
    close.grab_focus();

    keys.connect_key_pressed(move |_, key, _, _| match key {
        gtk::gdk::Key::Escape => {
            dismiss();
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
}

fn osd_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    button.add_css_class("osd");
    button.add_css_class("circular");
    button
}

/// What to call the file in the save dialog. `save_to_png` writes a PNG
/// whatever the attachment was called upstream, so the suggested name says PNG
/// and does not promise a JPEG it is not about to write.
fn png_name(title: &str) -> String {
    let stem = title.rsplit_once('.').map_or(title, |(stem, _)| stem);
    let stem = stem.trim();
    if stem.is_empty() {
        "image.png".to_string()
    } else {
        format!("{stem}.png")
    }
}

/// Puts `content` over everything in the window, with `buttons` in the corner,
/// and gives back the one way out and the key controller to hang shortcuts on.
///
/// `on_close` runs first when it is dismissed, for whatever the content needs
/// stopping — a video keeps playing otherwise, behind a lightbox that is no
/// longer there.
fn mount(
    parent: &adw::ApplicationWindow,
    content: &impl IsA<gtk::Widget>,
    buttons: &gtk::Box,
    on_close: Option<Box<dyn Fn()>>,
) -> (Rc<dyn Fn()>, gtk::EventControllerKey) {
    // The content and the buttons over it, inset so there is always a strip of
    // backdrop to click on even when the content fills the window.
    let frame = gtk::Overlay::builder()
        .hexpand(true)
        .vexpand(true)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .child(content)
        .build();
    frame.add_overlay(buttons);

    let dim = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    dim.add_css_class("lightbox");
    dim.append(&frame);

    // ── Getting this inside the window, which is the part that can break ──
    //
    // An `adw::ApplicationWindow` holds exactly one content widget. Today it is
    // an `adw::ToastOverlay`, but nothing here checks for that or depends on
    // it: whatever `content()` returns is taken as opaque.
    //
    // If it already is a `gtk::Overlay`, nothing is moved at all — the lightbox
    // is added to it as an overlay child and later removed again, and the
    // window content never changes. (This is also what makes a second lightbox
    // on top of a first one harmless.)
    //
    // Otherwise the content is displaced into a fresh `gtk::Overlay`. The order
    // matters: `set_content(None)` first, because a widget cannot be given a
    // second parent and `set_child` on a still-parented widget is a GTK error.
    // Then the overlay becomes the content, and the lightbox goes in as an
    // overlay child — same allocation as the main child, drawn on top of it,
    // and, because the box fills that allocation, swallowing the clicks that
    // would otherwise reach the conversation behind it.
    //
    // Dismiss reverses exactly this, and only if it did happen; see `Open`.
    let content = parent.content();
    let existing = content
        .as_ref()
        .and_then(|w| w.downcast_ref::<gtk::Overlay>())
        .cloned();
    let (overlay, wrapped) = match existing {
        Some(overlay) => (overlay, false),
        None => {
            let overlay = gtk::Overlay::new();
            parent.set_content(gtk::Widget::NONE);
            overlay.set_child(content.as_ref());
            parent.set_content(Some(&overlay));
            (overlay, true)
        }
    };
    overlay.add_overlay(&dim);

    let keys = gtk::EventControllerKey::new();
    // Capture, so Escape and Ctrl+C are ours before anything with the focus —
    // the message entry behind the lightbox has its own opinion about both.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    parent.add_controller(keys.clone());

    let open = Rc::new(RefCell::new(Some(Open {
        parent: parent.clone(),
        overlay,
        dim: dim.clone(),
        wrapped,
        displaced: content,
        keys: keys.clone(),
    })));

    let dismiss: Rc<dyn Fn()> = Rc::new({
        let open = open.clone();
        move || {
            let Some(open) = open.borrow_mut().take() else {
                return; // Already dismissed by one of the other three ways.
            };
            if let Some(on_close) = &on_close {
                on_close();
            }
            open.parent.remove_controller(&open.keys);
            open.overlay.remove_overlay(&open.dim);
            // Only put the content back if the overlay is still where it was
            // left. If something else has taken over the window in the
            // meantime — a sign-out dropping back to the login page, say —
            // restoring here would throw that away.
            if open.wrapped
                && open.parent.content().as_ref() == Some(open.overlay.upcast_ref::<gtk::Widget>())
            {
                open.overlay.set_child(gtk::Widget::NONE);
                open.parent.set_content(open.displaced.as_ref());
            }
        }
    });

    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.connect_released({
        let dismiss = dismiss.clone();
        let dim = dim.clone();
        let frame = frame.clone();
        move |_, _, x, y| {
            // `pick` gives the deepest widget under the pointer, so a click on
            // the content or on a button comes back as that widget and is left
            // alone. Only the backdrop itself — the dimmed box, or the inset
            // area around it — closes.
            let hit = dim.pick(x, y, gtk::PickFlags::DEFAULT);
            if hit.is_none_or(|w| w == dim || w == frame) {
                dismiss();
            }
        }
    });
    dim.add_controller(click);

    (dismiss, keys)
}

#[cfg(test)]
mod tests {
    use super::png_name;

    #[test]
    fn suggested_name_is_always_a_png() {
        assert_eq!(png_name("holiday.jpeg"), "holiday.png");
        assert_eq!(png_name("screenshot.png"), "screenshot.png");
        assert_eq!(png_name("no extension"), "no extension.png");
        assert_eq!(png_name(""), "image.png");
        assert_eq!(png_name(".hidden"), "image.png");
    }
}
