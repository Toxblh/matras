//! The call dock: a strip pinned to the bottom of the sidebar for as long as
//! you are in a call.
//!
//! A call outlives the channel you started it in — you can read somewhere else
//! while it runs — so its controls cannot live in the conversation header, the
//! way they used to. They live here, where they are reachable from every
//! channel, and the dock doubles as the way back to the call's own channel.
//!
//! It is the bottom bar of the sidebar's [`adw::ToolbarView`], which is the
//! idiomatic slot for this: space is reserved rather than overlaid, the
//! separator and background come free, and it survives navigation because it
//! sits outside the content stack.

use std::cell::Cell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use adw::prelude::*;

use crate::avatars::Avatars;
use crate::state::SharedState;

const HAND_ICON_NAME: &str = "hand-openyay-symbolic";

pub(crate) fn dock_belongs_in_chat(collapsed: bool, showing_content: bool) -> bool {
    collapsed && showing_content
}

/// What the host can do to somebody else in the call.
#[derive(Debug, Clone, Copy)]
pub enum HostAction {
    MuteOthers,
    EndCall,
    Mute,
    StopSharing,
    LowerHand,
    Remove,
    MakeHost,
}

pub struct CallDock {
    pub widget: gtk::Box,
    roster: gtk::Box,
    /// The participant list as one value, so voice activity does not rebuild it.
    roster_drawn: Cell<Option<u64>>,
    on_host: Rc<dyn Fn(String, HostAction)>,
    caption: gtk::Label,
    mute_others: gtk::Button,
    end_call: gtk::Button,
    avatar: adw::Avatar,
    title: gtk::Label,
    subtitle: gtk::Label,
    mute: gtk::Button,
    screen: gtk::Button,
    camera: gtk::Button,
    record: gtk::Button,
    hand: gtk::Button,
}

impl CallDock {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        on_open: impl Fn() + 'static,
        on_mute: impl Fn() + 'static,
        on_screen: impl Fn() + 'static,
        on_camera: impl Fn() + 'static,
        on_record: impl Fn() + 'static,
        on_hand: impl Fn() + 'static,
        on_leave: impl Fn() + 'static,
        on_host: impl Fn(String, HostAction) + 'static,
        on_host_all: impl Fn(HostAction) + 'static,
        on_react: impl Fn(String, String) + 'static,
    ) -> Self {
        let avatar = adw::Avatar::builder().size(28).build();

        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("heading");
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        subtitle.add_css_class("caption");
        subtitle.add_css_class("dim-label");

        let labels = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        labels.append(&title);
        labels.append(&subtitle);

        let summary_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        summary_box.append(&avatar);
        summary_box.append(&labels);
        summary_box.append(&gtk::Image::from_icon_name("go-next-symbolic"));

        // The whole summary is the way back to the call's channel — a chevron
        // alone would be a very small target for the most likely click here.
        let summary = gtk::Button::builder()
            .child(&summary_box)
            .tooltip_text("Go to the call")
            .hexpand(true)
            .build();
        summary.add_css_class("flat");
        summary.add_css_class("call-dock-summary");
        summary.connect_clicked(move |_| on_open());

        // Who is actually in there, with their hands and microphones. A count
        // answers "is it worth joining"; the list answers "who is that".
        let roster = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .width_request(240)
            .build();
        let people = gtk::MenuButton::builder()
            .icon_name("system-users-symbolic")
            .tooltip_text("Who is in the call")
            .popover(
                &gtk::Popover::builder()
                    .child(
                        &gtk::ScrolledWindow::builder()
                            .hscrollbar_policy(gtk::PolicyType::Never)
                            .propagate_natural_height(true)
                            .max_content_height(280)
                            .child(&roster)
                            .build(),
                    )
                    .build(),
            )
            .valign(gtk::Align::Center)
            .build();
        people.add_css_class("flat");
        people.add_css_class("circular");

        let summary_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .build();
        summary_row.append(&summary);
        summary_row.append(&people);

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .build();

        let mute = icon_button("microphone-disabled-symbolic", "Unmute", on_mute);
        let screen = icon_button("video-display-symbolic", "Share your screen", on_screen);
        let camera = icon_button("camera-web-symbolic", "Turn the camera on", on_camera);
        let record = icon_button("media-record-symbolic", "Record the call", on_record);
        let hand = icon_button(HAND_ICON_NAME, "Raise your hand", on_hand);

        // Without the video feature there is nothing behind these two; a
        // control that can never work is not drawn rather than drawn disabled.
        #[cfg(not(feature = "video"))]
        for hidden in [&screen, &camera] {
            hidden.set_visible(false);
        }

        // A quick reaction, which is how people agree without interrupting.
        let reactions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .build();
        let on_react = Rc::new(on_react);
        for (name, glyph) in [
            ("+1", "👍"),
            ("clap", "👏"),
            ("joy", "😂"),
            ("open_mouth", "😮"),
        ] {
            let button = gtk::Button::builder()
                .label(glyph)
                .tooltip_text(format!(":{name}:"))
                .build();
            button.add_css_class("flat");
            button.add_css_class("circular");
            button.connect_clicked({
                let on_react = on_react.clone();
                move |_| on_react(name.to_string(), glyph.to_string())
            });
            reactions.append(&button);
        }
        let react = gtk::MenuButton::builder()
            .icon_name("face-smile-symbolic")
            .tooltip_text("React")
            .popover(&gtk::Popover::builder().child(&reactions).build())
            .build();
        react.add_css_class("flat");
        react.add_css_class("circular");
        controls.append(&react);
        for button in [&mute, &screen, &camera, &hand, &record] {
            controls.append(button);
        }

        // Host-only, and destructive for everyone else in the call, so they
        // sit apart from the controls that only affect you.
        let on_host_all = Rc::new(on_host_all);
        let mute_others = icon_button("microphone-disabled-symbolic", "Mute everyone else", {
            let on_host = on_host_all.clone();
            move || on_host(HostAction::MuteOthers)
        });
        mute_others.set_visible(false);
        let end_call = icon_button("window-close-symbolic", "End the call for everyone", {
            let on_host = on_host_all.clone();
            move || on_host(HostAction::EndCall)
        });
        end_call.add_css_class("destructive-action");
        end_call.set_visible(false);
        controls.append(&mute_others);
        controls.append(&end_call);

        let leave = icon_button("call-stop-symbolic", "Leave the call", on_leave);
        leave.add_css_class("destructive-action");
        leave.set_halign(gtk::Align::End);
        leave.set_hexpand(true);
        controls.append(&leave);

        // One line, replaced as it goes: a subtitle, not a transcript. It
        // takes no space when the server is not captioning.
        let caption = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .build();
        caption.add_css_class("caption");
        caption.add_css_class("dim-label");

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        widget.add_css_class("call-dock");
        widget.append(&summary_row);
        widget.append(&caption);
        widget.append(&controls);

        CallDock {
            widget,
            roster,
            roster_drawn: Cell::new(None),
            on_host: Rc::new(on_host),
            mute_others,
            end_call,
            caption,
            avatar,
            title,
            subtitle,
            mute,
            screen,
            camera,
            record,
            hand,
        }
    }

    /// Populates the dock with deterministic controls for issue screenshots.
    pub fn show_demo_preview(&self, hand_raised: bool) {
        self.title.set_text("Sara Okafor is talking");
        self.subtitle.set_text("Development · 3 in the call");
        let (icon, tooltip, flat) = hand_button_appearance(hand_raised);
        set_state(&self.hand, hand_raised, (icon, tooltip), "suggested-action");
        if flat {
            self.hand.add_css_class("flat");
        } else {
            self.hand.remove_css_class("flat");
        }
        self.widget.set_visible(true);
    }

    /// Shows one line of live transcription, or clears it.
    pub fn set_caption(&self, who: &str, text: &str) {
        let show = !text.trim().is_empty();
        self.caption.set_visible(show);
        if show {
            // A reaction arrives with no words around it, so it reads better
            // as "Anna 👏" than as "Anna: 👏".
            let separator = if who.is_empty() { "" } else { " " };
            self.caption.set_text(&format!("{who}{separator}{text}"));
        }
    }

    /// Fills the dock in from the call we are in. Returns whether there is one
    /// — the caller reveals or hides the bar on that.
    pub fn refresh(&self, state: &SharedState, avatars: &Avatars) -> bool {
        let st = state.borrow();
        let Some(call) = st.call.as_ref() else {
            return false;
        };

        // Server-side voice activity only ever names other people, so a quiet
        // call falls back to the channel — never to "you are talking".
        let speaker = call.speaking.first();
        match speaker.and_then(|id| st.users.get(id)) {
            Some(user) => {
                let name = user.display_name(st.teammate_name_display());
                avatars.apply(&self.avatar, &user.id, &name);
                self.title.set_text(&format!("{name} is talking"));
            }
            None => {
                self.avatar.set_show_initials(false);
                self.avatar.set_custom_image(None::<&gtk::gdk::Paintable>);
                self.avatar.set_icon_name(Some("audio-headphones-symbolic"));
                self.title.set_text("In a call");
            }
        }

        let channel = st
            .channel(&call.channel_id)
            .map(|c| st.channel_title(c))
            .unwrap_or_else(|| "a channel".to_string());
        let people = st.active_calls.get(&call.channel_id).map_or(1, Vec::len);
        let sharing = if call.sharing.is_empty() {
            String::new()
        } else {
            " · sharing".to_string()
        };
        self.subtitle
            .set_text(&format!("{channel} · {people} in the call{sharing}"));

        self.refresh_roster(&st, avatars, call);

        let i_am_host = call.host_id == st.me.id;
        self.mute_others.set_visible(i_am_host);
        self.end_call.set_visible(i_am_host);

        set_state(
            &self.mute,
            !call.muted,
            if call.muted {
                ("microphone-disabled-symbolic", "Unmute")
            } else {
                ("audio-input-microphone-symbolic", "Mute")
            },
            "suggested-action",
        );
        set_state(
            &self.screen,
            call.screen.is_some(),
            (
                "video-display-symbolic",
                if call.screen.is_some() {
                    "Stop sharing your screen"
                } else {
                    "Share your screen"
                },
            ),
            "suggested-action",
        );
        set_state(
            &self.camera,
            call.camera.is_some(),
            (
                "camera-web-symbolic",
                if call.camera.is_some() {
                    "Turn the camera off"
                } else {
                    "Turn the camera on"
                },
            ),
            "suggested-action",
        );
        // Your own hand: the button reflects whether it is up, since you
        // cannot see yourself in the roster otherwise.
        let my_hand = call.hands.iter().any(|id| id == &st.me.id);
        let (hand_icon, hand_tooltip, hand_flat) = hand_button_appearance(my_hand);
        set_state(
            &self.hand,
            my_hand,
            (hand_icon, hand_tooltip),
            "suggested-action",
        );
        if hand_flat {
            self.hand.add_css_class("flat");
        } else {
            self.hand.remove_css_class("flat");
        }
        set_state(
            &self.record,
            call.recording,
            (
                "media-record-symbolic",
                if call.recording {
                    "Stop recording"
                } else {
                    "Record the call"
                },
            ),
            "destructive-action",
        );
        true
    }
}

impl CallDock {
    /// Redraws the participant list: face, name, and whatever is true of them
    /// right now — talking, hand up, muted.
    fn refresh_roster(
        &self,
        st: &crate::state::AppState,
        avatars: &Avatars,
        call: &crate::state::ActiveCall,
    ) {
        // Voice activity arrives continuously while somebody talks, and every
        // frame of it reached this list. Rebuilding takes the row out from
        // under the pointer and costs a widget tree per participant, for a
        // list that usually did not change at all.
        let signature = {
            let mut hash = DefaultHasher::new();
            call.channel_id.hash(&mut hash);
            call.host_id.hash(&mut hash);
            st.me.id.hash(&mut hash);
            if let Some(people) = st.active_calls.get(&call.channel_id) {
                for user_id in people {
                    user_id.hash(&mut hash);
                    st.users
                        .get(user_id)
                        .map(|user| user.display_name(st.teammate_name_display()))
                        .hash(&mut hash);
                }
            }
            call.hands.hash(&mut hash);
            call.speaking.first().hash(&mut hash);
            for user_id in &call.muted_users {
                user_id.hash(&mut hash);
            }
            hash.finish()
        };
        if self.roster_drawn.get() == Some(signature) {
            return;
        }
        self.roster_drawn.set(Some(signature));

        while let Some(child) = self.roster.first_child() {
            self.roster.remove(&child);
        }

        let people = st
            .active_calls
            .get(&call.channel_id)
            .cloned()
            .unwrap_or_default();
        for user_id in &people {
            let name = st
                .users
                .get(user_id)
                .map(|u| u.display_name(st.teammate_name_display()))
                .unwrap_or_else(|| "Someone".to_string());

            let avatar = adw::Avatar::builder().size(24).build();
            avatars.apply(&avatar, user_id, &name);

            let label = gtk::Label::builder()
                .label(&name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();

            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            row.append(&avatar);
            row.append(&label);

            // A raised hand is a request and outranks the rest.
            if let Some(place) = call.hands.iter().position(|id| id == user_id) {
                let hand = gtk::Label::new(Some(&format!("✋{}", place + 1)));
                hand.set_tooltip_text(Some("Wants to speak"));
                row.append(&hand);
            }
            if call.speaking.first() == Some(user_id) {
                let speaking = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
                speaking.add_css_class("success");
                speaking.set_tooltip_text(Some("Talking"));
                row.append(&speaking);
            } else if call.muted_users.contains(user_id) {
                let muted = gtk::Image::from_icon_name("microphone-disabled-symbolic");
                muted.add_css_class("dim-label");
                muted.set_tooltip_text(Some("Muted"));
                row.append(&muted);
            }
            // Host controls, on the people they apply to. Shown only to the
            // host, and never against the host's own row: muting yourself is
            // the button already in the dock.
            let i_am_host = call.host_id == st.me.id;
            if i_am_host && user_id != &st.me.id {
                if let Some(session_id) = call.sessions.get(user_id) {
                    // Only the controls that apply: offering "stop sharing"
                    // to someone who is not sharing is a button that does
                    // nothing.
                    let mut controls = vec![(
                        "microphone-disabled-symbolic",
                        "Mute them",
                        HostAction::Mute,
                    )];
                    if call.sharing.contains(user_id) {
                        controls.push((
                            "video-display-symbolic",
                            "Stop their screen share",
                            HostAction::StopSharing,
                        ));
                    }
                    if call.hands.iter().any(|id| id == user_id) {
                        controls.push((HAND_ICON_NAME, "Lower their hand", HostAction::LowerHand));
                    }
                    controls.push((
                        "emblem-default-symbolic",
                        "Make them the host",
                        HostAction::MakeHost,
                    ));
                    controls.push((
                        "list-remove-symbolic",
                        "Remove from call",
                        HostAction::Remove,
                    ));

                    for (icon, tooltip, action) in controls {
                        let button = gtk::Button::builder()
                            .tooltip_text(tooltip)
                            .valign(gtk::Align::Center)
                            .build();
                        apply_named_icon(&button, icon);
                        button.add_css_class("flat");
                        button.add_css_class("circular");
                        button.connect_clicked({
                            let on_host = self.on_host.clone();
                            // Most host routes address a session; making
                            // someone host addresses the person, since every
                            // session of theirs gains it at once.
                            let target = match action {
                                HostAction::MakeHost => user_id.clone(),
                                _ => session_id.clone(),
                            };
                            move |_| on_host(target.clone(), action)
                        });
                        row.append(&button);
                    }
                }
            }

            if call.sharing.contains(user_id) {
                let sharing = gtk::Image::from_icon_name("video-display-symbolic");
                sharing.add_css_class("accent");
                sharing.set_tooltip_text(Some("Sharing a screen"));
                row.append(&sharing);
            }

            self.roster.append(&row);
        }
    }
}

fn icon_button(icon: &str, tooltip: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let button = gtk::Button::builder().tooltip_text(tooltip).build();
    apply_named_icon(&button, icon);
    button.add_css_class("flat");
    button.add_css_class("circular");
    button.connect_clicked(move |_| on_click());
    button
}

fn set_state(button: &gtk::Button, on: bool, look: (&str, &str), class: &str) {
    apply_named_icon(button, look.0);
    button.set_tooltip_text(Some(look.1));
    if on {
        button.add_css_class(class);
    } else {
        button.remove_css_class(class);
    }
}

fn apply_named_icon(button: &gtk::Button, icon: &str) {
    if icon == HAND_ICON_NAME
        && gtk::gdk::Display::default()
            .is_some_and(|display| !gtk::IconTheme::for_display(&display).has_icon(HAND_ICON_NAME))
    {
        button.set_icon_name("");
        button.set_child(Some(&gtk::Label::new(Some("✋"))));
    } else {
        button.set_icon_name(icon);
    }
}

fn hand_button_appearance(raised: bool) -> (&'static str, &'static str, bool) {
    (
        HAND_ICON_NAME,
        if raised {
            "Lower your hand"
        } else {
            "Raise your hand"
        },
        !raised,
    )
}

#[cfg(test)]
mod hand_button_tests {
    use super::hand_button_appearance;

    #[test]
    fn raised_hand_uses_the_hand_icon_and_a_visible_active_button() {
        assert_eq!(
            hand_button_appearance(true),
            ("hand-openyay-symbolic", "Lower your hand", false)
        );
    }

    #[test]
    fn lowered_hand_keeps_the_flat_raise_action() {
        assert_eq!(
            hand_button_appearance(false),
            ("hand-openyay-symbolic", "Raise your hand", true)
        );
    }
}

#[cfg(test)]
mod dock_location_tests {
    use super::dock_belongs_in_chat;

    #[test]
    fn mobile_dock_follows_the_visible_page() {
        assert!(!dock_belongs_in_chat(true, false));
        assert!(dock_belongs_in_chat(true, true));
        assert!(!dock_belongs_in_chat(false, true));
    }
}
