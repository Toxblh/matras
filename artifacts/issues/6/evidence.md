# Issue #6 evidence

Issue: <https://altlinux.space/toxblh/matras/issues/6>

The Inbox header no longer contains its own close button. The window close
control dismisses an open Inbox first. Closing a different panel remains
eligible for the existing background-mode window behavior.

Regression sequence:

- Red: `cargo test -p matras --lib ui::rhs::inbox_close_tests -- --nocapture`.
  The two Inbox assertions failed against the original policy; see [red.log](red.log).
- Green: same command; all 3 close-policy tests pass; see [green.log](green.log).
- `cargo build -p matras` passed after the current UI changes.

Viewport screenshots use `MATRAS_DEMO_OPEN_INBOX=1` and show the Inbox with its
window close control. The issue attachment is a 784x180 crop; the issue-specific
capture below uses 784x720 so the full app is visible.

- 360x720: [state-360x720.png](state-360x720.png)
- Issue-specific 784x720: [state-784x720.png](state-784x720.png)
- 1320x840: [state-1320x840.png](state-1320x840.png)

The 360x720 screenshot was inspected: there is one close control in the title
bar and no duplicate X beside the Inbox heading. The final combined workspace
suite passed; see [workspace-test.log](../workspace-test.log). The issue remains
open until the changes land on the default branch.
