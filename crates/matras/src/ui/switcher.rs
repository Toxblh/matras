//! The quick switcher: Ctrl+K, type, Enter.
//!
//! Like [`super::dialogs::ChannelBrowser`] it only asks and answers — the
//! caller searches its own state on `on_search` and pours the matches back in
//! through [`Switcher::set_results`], so nothing here knows what a channel is.
//!
//! The whole point is that your hands stay on the keys, so the search entry
//! owns the focus for the window's whole life: Up and Down move the selection
//! in the list without ever giving it focus, which is why the key handling
//! lives on the entry rather than on the list where it would look at home.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// What was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Channel(String),
    User(String),
}

/// The window outlives this call; the handle is how results get in.
pub struct Switcher {
    list: gtk::ListBox,
    /// One per row, in row order — a row's index is its target's index.
    targets: Rc<RefCell<Vec<Target>>>,
}

impl Switcher {
    /// `on_search` fires as the user types (already the trimmed term).
    /// `on_pick` fires with the chosen target and the window closes itself.
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        on_search: impl Fn(String) + 'static,
        on_pick: impl Fn(Target) + 'static,
    ) -> Self {
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Jump to a channel or person")
            .hexpand(true)
            .build();
        let on_search = Rc::new(on_search);
        search.connect_search_changed({
            let on_search = on_search.clone();
            move |entry| on_search(entry.text().trim().to_string())
        });

        // A spinner until the first answer, then "No Matches" when there is
        // none (see `set_results`).
        let list = super::dialogs::loading_list();
        list.set_selection_mode(gtk::SelectionMode::Single);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(12)
            .child(&list)
            .build();

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::builder().title_widget(&search).build());
        view.set_content(Some(&scroller));

        let window = adw::Dialog::builder()
            .title("Jump To")
            .content_width(560)
            .content_height(420)
            .child(&view)
            .build();

        let targets: Rc<RefCell<Vec<Target>>> = Rc::default();

        // Weak, because the window owns the entry that owns the controller
        // below: a strong clone in here would outlive the closing window.
        let closing = window.downgrade();
        let pick: Rc<dyn Fn(&gtk::ListBoxRow)> = Rc::new({
            let targets = targets.clone();
            let closing = closing.clone();
            move |row| {
                let Some(target) = targets.borrow().get(row.index() as usize).cloned() else {
                    return;
                };
                if let Some(window) = closing.upgrade() {
                    window.close();
                }
                on_pick(target);
            }
        });
        // Clicking a row answers the same way Enter does.
        list.connect_row_activated({
            let pick = pick.clone();
            move |_, row| pick(row)
        });

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let list = list.clone();
            let scroller = scroller.clone();
            let targets = targets.clone();
            move |_, key, _, _| {
                use gtk::gdk::Key;
                match key {
                    Key::Up | Key::KP_Up => step(&list, &scroller, &targets.borrow(), -1),
                    Key::Down | Key::KP_Down => step(&list, &scroller, &targets.borrow(), 1),
                    Key::Return | Key::KP_Enter => {
                        if let Some(row) = list.selected_row() {
                            pick(&row);
                        }
                    }
                    Key::Escape => {
                        if let Some(window) = closing.upgrade() {
                            window.close();
                        }
                    }
                    // Everything else is typing, and typing belongs to the entry.
                    _ => return glib::Propagation::Proceed,
                }
                // Swallowed so the list never takes the focus off the entry.
                glib::Propagation::Stop
            }
        });
        search.add_controller(keys);

        window.present(Some(parent));
        search.grab_focus();
        // Ask once on open: the recent conversations are worth showing before
        // anyone types, and an empty window looks broken.
        on_search(String::new());

        Switcher { list, targets }
    }

    /// Results: (target, primary label, secondary label, icon name).
    pub fn set_results(&self, items: Vec<(Target, String, String, String)>) {
        super::dialogs::reset_list(&self.list, "system-search-symbolic", "No Matches");
        let mut targets = self.targets.borrow_mut();
        targets.clear();
        for (target, primary, secondary, icon) in items {
            // AdwActionRow reads both as Pango markup, and channel and user
            // names are free text — one ampersand would otherwise blank the row.
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&primary))
                .subtitle(glib::markup_escape_text(&secondary))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(&icon));
            self.list.append(&row);
            targets.push(target);
        }
        drop(targets);

        // Something is always selected, so Enter answers with the best match
        // straight after typing, without a press spent picking a start.
        self.list.select_row(self.list.row_at_index(0).as_ref());
    }
}

/// Moves the selection by `delta` and keeps it on screen. Selecting rather than
/// focusing is what leaves the entry able to carry on typing.
fn step(list: &gtk::ListBox, scroller: &gtk::ScrolledWindow, targets: &[Target], delta: i32) {
    let next = next_index(
        list.selected_row().map(|row| row.index()),
        delta,
        targets.len() as i32,
    );
    let Some(row) = list.row_at_index(next) else {
        return;
    };
    list.select_row(Some(&row));

    // The row cannot scroll itself into view without grabbing the focus, so
    // nudge the adjustment by however far the row hangs off either edge.
    let Some(bounds) = row.compute_bounds(list) else {
        return;
    };
    let adjustment = scroller.vadjustment();
    let top = bounds.y() as f64;
    let bottom = top + bounds.height() as f64;
    if top < adjustment.value() {
        adjustment.set_value(top);
    } else if bottom > adjustment.value() + adjustment.page_size() {
        adjustment.set_value(bottom - adjustment.page_size());
    }
}

/// Where Up or Down lands. Wraps at both ends: a screenful of results is
/// quicker to circle than to walk back through. With nothing selected — or
/// nothing to select — the first row is the only sensible answer.
fn next_index(selected: Option<i32>, delta: i32, len: i32) -> i32 {
    match selected {
        Some(current) if len > 0 => (current + delta).rem_euclid(len),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_wraps_at_both_ends() {
        assert_eq!(next_index(Some(0), 1, 3), 1);
        assert_eq!(next_index(Some(2), 1, 3), 0);
        assert_eq!(next_index(Some(0), -1, 3), 2);
        assert_eq!(next_index(Some(1), -1, 3), 0);
        // One result: both keys stay on it.
        assert_eq!(next_index(Some(0), 1, 1), 0);
    }

    #[test]
    fn nothing_to_move_from_lands_on_the_first_row() {
        assert_eq!(next_index(None, 1, 3), 0);
        assert_eq!(next_index(None, -1, 3), 0);
        // No rows at all: the caller finds no row 0 and gives up.
        assert_eq!(next_index(Some(0), 1, 0), 0);
    }
}
