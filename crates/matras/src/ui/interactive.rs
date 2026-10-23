//! The form an integration asks for: Mattermost interactive dialogs.
//!
//! Like [`super::dialogs`], nothing here talks to the server or to
//! [`crate::state`]. [`present`] renders a [`Dialog`] and hands the filled-in
//! values to a callback; the caller POSTs them to
//! `/api/v4/actions/dialogs/submit`.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use mattermost_api::models::dialog::{Dialog, DialogElement};
use serde_json::Value;

/// Reads one field's current answer, in the JSON shape the integration expects.
type Getter = Box<dyn Fn() -> Value>;
/// Whether one field is currently fit to submit.
type Check = Box<dyn Fn() -> bool>;

/// Shows a dialog form. `on_submit` gets the values keyed by element name;
/// `on_cancel` fires when it is dismissed.
pub fn present(
    parent: &impl IsA<gtk::Window>,
    dialog: &Dialog,
    on_submit: impl Fn(HashMap<String, Value>) + 'static,
    on_cancel: impl Fn() + 'static,
) {
    let submit = gtk::Button::builder()
        .label(if dialog.submit_label.is_empty() {
            "Submit"
        } else {
            &dialog.submit_label
        })
        .build();
    submit.add_css_class("suggested-action");

    let mut getters: Vec<(String, Getter)> = Vec::new();
    // Filled while the fields are built, read only from signal handlers, which
    // cannot fire before this function returns.
    let checks: Rc<std::cell::RefCell<Vec<Check>>> = Rc::default();
    let revalidate: Rc<dyn Fn()> = {
        let checks = checks.clone();
        let submit = submit.clone();
        Rc::new(move || submit.set_sensitive(checks.borrow().iter().all(|ok| ok())))
    };

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();

    if !dialog.introduction_text.is_empty() {
        // Markdown on the wire; shown as-is, since a paragraph of intro text is
        // not worth a second markdown renderer.
        let intro = gtk::Label::builder()
            .label(&dialog.introduction_text)
            .wrap(true)
            .xalign(0.0)
            .build();
        content.append(&intro);
    }

    // Rows stack into one boxed list; a text area or a radio group needs its
    // own group (it is not a row, and it carries its own title), so it ends
    // the current list and the next row starts a fresh one.
    let mut group: Option<adw::PreferencesGroup> = None;

    for element in &dialog.elements {
        let name = element.name.clone();
        match element.element_type.as_str() {
            "textarea" => {
                group = None;
                let (widget, getter, check) = text_area(element, &revalidate);
                content.append(&widget);
                getters.push((name, getter));
                checks.borrow_mut().push(check);
            }
            "radio" if !element.options.is_empty() => {
                group = None;
                let (widget, getter) = radio_group(element);
                content.append(&widget);
                getters.push((name, getter));
            }
            kind => {
                // AdwEntryRow has neither a subtitle nor a placeholder, so a
                // text field with a hint gets a group of its own and the hint
                // goes in the group description, where it stays visible.
                let hint = match kind {
                    "select" if !element.options.is_empty() => String::new(),
                    "bool" => String::new(),
                    _ => entry_hint(element),
                };
                if !hint.is_empty() {
                    group = None;
                }
                let group = group.get_or_insert_with(|| {
                    let group = adw::PreferencesGroup::new();
                    if !hint.is_empty() {
                        group.set_description(Some(&escaped(&hint)));
                    }
                    content.append(&group);
                    group
                });
                let (row, getter, check) = match kind {
                    "select" if !element.options.is_empty() => select_row(element),
                    "bool" => switch_row(element),
                    // Everything else — `text`, a `select` whose options live on
                    // the server, and any type this client does not know yet —
                    // is a text field. An unknown type degrading to text is
                    // better than a form with a hole in it.
                    _ => entry_row(element, &revalidate),
                };
                group.add(&row);
                getters.push((name, getter));
                if let Some(check) = check {
                    checks.borrow_mut().push(check);
                }
            }
        }
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&adw::Clamp::builder().child(&content).build())
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&submit);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));

    let window = adw::Window::builder()
        .title(&dialog.title)
        .default_width(460)
        .default_height(560)
        .modal(true)
        .content(&view)
        .build();
    window.set_transient_for(Some(parent));

    // Submitting and dismissing both close the window, so the callbacks hang
    // off the one close handler and this says which of the two it was.
    let submitted = Rc::new(Cell::new(false));
    window.connect_close_request({
        let submitted = submitted.clone();
        move |_| {
            if !submitted.get() {
                on_cancel();
            }
            glib::Propagation::Proceed
        }
    });
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    submit.connect_clicked({
        let window = window.clone();
        move |_| {
            submitted.set(true);
            window.close();
            on_submit(getters.iter().map(|(n, get)| (n.clone(), get())).collect());
        }
    });

    revalidate();
    window.present();
}

/// A single-line field. Also where a `select` backed by `users` or `channels`
/// lands: those carry no options in the payload, the client is expected to
/// search the server for them, and this file has no client to search with.
fn entry_row(
    element: &DialogElement,
    revalidate: &Rc<dyn Fn()>,
) -> (gtk::Widget, Getter, Option<Check>) {
    let title = escaped(&element.display_name);
    // A password is worth hiding even though the payload treats it as text.
    let row: gtk::Widget = if element.subtype == "password" {
        let row = adw::PasswordEntryRow::builder().title(&title).build();
        row.set_text(&element.default);
        row.upcast()
    } else {
        let row = adw::EntryRow::builder().title(&title).build();
        row.set_text(&element.default);
        row.upcast()
    };
    let editable = row.clone().downcast::<gtk::Editable>().expect("entry row");

    let length = {
        let editable = editable.clone();
        move || editable.text().chars().count()
    };
    let check = length_check(element, length.clone());
    editable.connect_changed({
        let revalidate = revalidate.clone();
        move |_| revalidate()
    });

    let getter: Getter = Box::new(move || Value::String(editable.text().to_string()));
    (row, getter, Some(check))
}

/// What a text field shows above itself: its help text and its placeholder.
fn entry_hint(element: &DialogElement) -> String {
    [&element.help_text, &element.placeholder]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A multi-line field. `GtkTextView` is not a row, so it gets its own group
/// with the label and help text on the group itself.
fn text_area(element: &DialogElement, revalidate: &Rc<dyn Fn()>) -> (gtk::Widget, Getter, Check) {
    let view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(6)
        .right_margin(6)
        .height_request(96)
        .build();
    view.buffer().set_text(&element.default);

    let frame = gtk::Frame::new(None);
    frame.set_child(Some(&view));

    let group = adw::PreferencesGroup::builder()
        .title(escaped(&element.display_name))
        .description(escaped(&element.help_text))
        .build();
    group.add(&frame);

    let buffer = view.buffer();
    let text = {
        let buffer = buffer.clone();
        move || {
            let (start, end) = buffer.bounds();
            buffer.text(&start, &end, false).to_string()
        }
    };
    let check = length_check(element, {
        let text = text.clone();
        move || text().chars().count()
    });
    buffer.connect_changed({
        let revalidate = revalidate.clone();
        move |_| revalidate()
    });

    let getter: Getter = Box::new(move || Value::String(text()));
    (group.upcast(), getter, check)
}

/// A `select` whose options came in the payload. A combo always has something
/// selected, so there is nothing to validate.
fn select_row(element: &DialogElement) -> (gtk::Widget, Getter, Option<Check>) {
    let labels: Vec<&str> = element.options.iter().map(|o| o.text.as_str()).collect();
    let row = adw::ComboRow::builder()
        .title(escaped(&element.display_name))
        .subtitle(escaped(&element.help_text))
        .model(&gtk::StringList::new(&labels))
        .build();
    let selected = element
        .options
        .iter()
        .position(|o| o.value == element.default)
        .unwrap_or(0);
    row.set_selected(selected as u32);

    // ponytail: a `multiselect` select submits only the highlighted option;
    // AdwComboRow picks one. Swap in a check-list row if an integration that
    // uses it turns up.
    let values: Vec<String> = element.options.iter().map(|o| o.value.clone()).collect();
    let getter: Getter = Box::new({
        let row = row.clone();
        move || {
            let value = values.get(row.selected() as usize).cloned();
            Value::String(value.unwrap_or_default())
        }
    });
    (row.upcast(), getter, None)
}

/// A `bool`. The default arrives as the *string* `"true"`, and the answer goes
/// back as a real JSON boolean — the server's `map[string]any` keeps the type.
fn switch_row(element: &DialogElement) -> (gtk::Widget, Getter, Option<Check>) {
    let row = adw::SwitchRow::builder()
        .title(escaped(&element.display_name))
        .subtitle(escaped(&element.help_text))
        .active(element.default == "true")
        .build();
    let getter: Getter = Box::new({
        let row = row.clone();
        move || Value::Bool(row.is_active())
    });
    (row.upcast(), getter, None)
}

/// Radio buttons, one per option, sharing a group. One is always active, so
/// there is nothing to validate either.
fn radio_group(element: &DialogElement) -> (gtk::Widget, Getter) {
    let group = adw::PreferencesGroup::builder()
        .title(escaped(&element.display_name))
        .description(escaped(&element.help_text))
        .build();
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();

    let mut buttons: Vec<(gtk::CheckButton, String)> = Vec::new();
    for option in &element.options {
        let button = gtk::CheckButton::with_label(&option.text);
        if let Some((first, _)) = buttons.first() {
            button.set_group(Some(first));
        }
        list.append(&button);
        buttons.push((button, option.value.clone()));
    }
    // Nothing checked would submit an empty string for a field the integration
    // expects one of its own values in, so fall back to the first option.
    let active = buttons
        .iter()
        .position(|(_, value)| *value == element.default)
        .unwrap_or(0);
    if let Some((button, _)) = buttons.get(active) {
        button.set_active(true);
    }
    group.add(&list);

    let getter: Getter = Box::new(move || {
        let chosen = buttons
            .iter()
            .find(|(button, _)| button.is_active())
            .map(|(_, value)| value.clone());
        Value::String(chosen.unwrap_or_default())
    });
    (group.upcast(), getter)
}

/// Required-ness and the length bounds, checked on every keystroke rather than
/// on submit: a button that is grey until the form is right needs no error
/// message.
fn length_check(element: &DialogElement, length: impl Fn() -> usize + 'static) -> Check {
    let optional = element.optional;
    let min = element.min_length.max(0) as usize;
    let max = element.max_length.max(0) as usize;
    Box::new(move || {
        let len = length();
        if len == 0 {
            return optional;
        }
        (min == 0 || len >= min) && (max == 0 || len <= max)
    })
}

/// Titles and subtitles are Pango markup, and the text in them is the
/// integration's, not ours.
fn escaped(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}
