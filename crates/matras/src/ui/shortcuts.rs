use adw::prelude::*;

/// The inbox shortcut must leave GTK's inspector accelerator available.
pub(super) const INBOX_ACCELERATOR: &[&str] = &["<Control><Shift>m"];
pub(super) const CLOSE_WINDOW_ACCELERATOR: &[&str] = &["<Control>w"];
const CLOSE_WINDOW_ACTION: &str = "close";
const CLOSE_WINDOW_ACTION_PATH: &str = "win.close";

pub(super) fn startup_channel(
    current: Option<String>,
    candidate: Option<String>,
) -> Option<String> {
    let _ = candidate;
    current
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EscapeAction {
    ClosePanel,
    Home,
    ShowChannels,
    None,
}

pub(super) fn escape_action(
    panel_open: bool,
    channel_open: bool,
    collapsed: bool,
    showing_content: bool,
) -> EscapeAction {
    if panel_open {
        EscapeAction::ClosePanel
    } else if channel_open {
        EscapeAction::Home
    } else if collapsed && showing_content {
        EscapeAction::ShowChannels
    } else {
        EscapeAction::None
    }
}

pub(super) fn install_window_close_shortcut(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
) {
    app.set_accels_for_action(CLOSE_WINDOW_ACTION_PATH, CLOSE_WINDOW_ACCELERATOR);
    let action = gtk::gio::SimpleAction::new(CLOSE_WINDOW_ACTION, None);
    action.connect_activate({
        let window = window.downgrade();
        move |_, _| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        }
    });
    window.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::{
        escape_action, startup_channel, EscapeAction, CLOSE_WINDOW_ACCELERATOR,
        CLOSE_WINDOW_ACTION, CLOSE_WINDOW_ACTION_PATH, INBOX_ACCELERATOR,
    };

    #[test]
    fn inbox_shortcut_does_not_steal_the_gtk_inspector() {
        assert_ne!(INBOX_ACCELERATOR, &["<Control><Shift>i"]);
    }

    #[test]
    fn startup_stays_on_the_empty_page_instead_of_selecting_a_saved_channel() {
        assert_eq!(startup_channel(None, Some("last-opened".into())), None);
    }

    #[test]
    fn escape_returns_from_a_channel_to_the_empty_page() {
        assert_eq!(escape_action(false, true, false, true), EscapeAction::Home);
    }

    #[test]
    fn close_window_shortcut_is_ctrl_w_and_is_distinct_from_inbox() {
        assert_eq!(CLOSE_WINDOW_ACCELERATOR, &["<Control>w"]);
        assert_ne!(CLOSE_WINDOW_ACCELERATOR, INBOX_ACCELERATOR);
        assert_eq!(CLOSE_WINDOW_ACTION, "close");
        assert_eq!(CLOSE_WINDOW_ACTION_PATH, "win.close");
    }
}
