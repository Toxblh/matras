# Issue #9 evidence

Issue: https://altlinux.space/toxblh/matras/issues/9 (open; 0 comments)

## Behavior

The mentions/threads inbox had a single button in the conversation header. On
narrow windows, the channel list and the conversation are separate navigation
pages, so the button disappeared while browsing the channel list. In desktop
layout it also belonged in the sidebar rather than in a particular chat.

The inbox entry now lives in the sidebar on desktop and while a collapsed
window shows its channel list. It moves to the conversation header when a
collapsed window is showing a chat. Both placements dispatch the same inbox
action and show the same unread count. Changing between the chat and sidebar
updates visibility with the navigation state. Returning to the mobile sidebar
also closes an open inbox panel, so it does not remain open when the chat is
opened again.

## Regression and visual evidence

- Red: `cargo test --offline -p matras --lib ui::sidebar::tests::inbox_button_moves_between_desktop_sidebar_and_mobile_content -- --exact` — failed because the mobile-chat state left the button in the sidebar.
- Green: same command after the placement fix — 1 passed / 0 failed.
- Red: `cargo test --offline -p matras --lib ui::sidebar::tests::returning_to_mobile_sidebar_closes_an_open_inbox_panel -- --exact` — failed because the return transition did not request inbox dismissal.
- Green: same command after wiring navigation Back to `CloseRightPanel` — 1 passed / 0 failed.
- `mobile-chat-360x720.png`: collapsed conversation header with its inbox button.
- `mobile-sidebar-400x800.png`: collapsed channel list with its inbox button, covering availability outside the chat.
- `issue-specific-720x800.png`: full app capture at the issue-specific window size.
- `desktop-1320x840.png`: desktop sidebar placement.

Each PNG is a separate deterministic demo rendering. The state matrix test
checks the desktop, collapsed-chat, and collapsed-sidebar visibility rules.
The final combined workspace suite passed; see
[workspace-test.log](../workspace-test.log).
