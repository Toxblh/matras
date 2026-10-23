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

/// The Keyboard Shortcuts window. Items name their action, so GTK shows the
/// accelerator actually registered for it and the list cannot drift from it;
/// the composer's keys are handled by a key controller, not an action, so
/// those two spell their keys out.
const HELP_OVERLAY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="help_overlay">
    <property name="modal">True</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">shortcuts</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Preferences</property><property name="action-name">win.preferences</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Keyboard Shortcuts</property><property name="action-name">win.show-help-overlay</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close Window</property><property name="action-name">win.close</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Quit</property><property name="action-name">app.quit</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Navigation</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Jump to a Channel or Person</property><property name="action-name">win.quick-switch</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Search Messages</property><property name="action-name">win.search</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Mentions and Threads</property><property name="action-name">win.inbox</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Next Unread Channel</property><property name="action-name">win.next-unread</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Previous Unread Channel</property><property name="action-name">win.previous-unread</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close Panel or Go Back</property><property name="action-name">win.close-panel</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Channels</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">New Channel</property><property name="action-name">win.new-channel</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Composer</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Send Message</property><property name="accelerator">Return</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">New Line</property><property name="accelerator">&lt;Shift&gt;Return</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>"#;

/// Gives `window` its Keyboard Shortcuts window. GTK adds the
/// `win.show-help-overlay` action; its Ctrl+? is ours to bind, because GTK
/// only does that for an overlay loaded from the app's resources.
pub(super) fn install_help_overlay(app: &adw::Application, window: &adw::ApplicationWindow) {
    let builder = gtk::Builder::from_string(HELP_OVERLAY);
    let overlay: gtk::ShortcutsWindow = builder
        .object("help_overlay")
        .expect("the shortcuts window is in its own UI string");
    window.set_help_overlay(Some(&overlay));
    app.set_accels_for_action("win.show-help-overlay", &["<Control>question"]);
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
