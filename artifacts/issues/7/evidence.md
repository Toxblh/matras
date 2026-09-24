# Issue #7 evidence

Issue: <https://altlinux.space/toxblh/matras/issues/7>

Startup no longer selects the cached last channel. Escape returns from an open
chat to the no-channel page after closing any open side panel. The demo flag
`MATRAS_DEMO_EMPTY_CHAT=1` clears the seed channel before constructing the UI,
so the screenshots capture the actual placeholder rather than the feed.

Regression sequence:

- Red: startup test returned `Some("last-opened")` instead of `None`; Escape
  test returned `None` instead of `Home`. See [red.log](red.log).
- Green: both exact tests passed; see [green.log](green.log).
- `cargo build -p matras` passed.

Screenshots use deterministic demo state and show the “No channel selected”
placeholder:

- 360x720: [state-360x720.png](state-360x720.png)
- Issue-specific 1024x720: [state-1024x720.png](state-1024x720.png)
- 1320x840: [state-1320x840.png](state-1320x840.png)

The 360x720 screenshot was inspected and contains the placeholder with no
selected feed. The issue attachment is a Fractal reference image rather than a
Matras viewport, so 1024x720 is the chosen issue-specific full-window capture.
The final combined workspace suite passed; see
[workspace-test.log](../workspace-test.log). The issue was closed automatically by PR #16, merged as `2166a9c`.

Issue closed automatically by PR #16, merged into `master` as `2166a9c`.
