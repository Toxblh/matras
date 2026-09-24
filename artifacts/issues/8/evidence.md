# Issue #8 evidence

Issue: <https://altlinux.space/toxblh/matras/issues/8>

The composer now has a 40px content-height limit, a rounded entry surface, and
a clearer send icon. On narrow windows the attach, priority, and schedule
actions are grouped behind a compact menu so the editor and send button fit.
The editor keeps a 1px minimum content width to avoid imposing its text
buffer's natural width on the window.

Regression sequence:

- Red: `cargo test -p matras --lib ui::chat::tests::composer_is_one_line_high_and_uses_a_clear_send_affordance -- --exact --nocapture`.
  It failed with `left: 160`, `right: 40` on the original composer; see
  [red.log](red.log).
- Green: same command passed; see [green.log](green.log).
- `cargo build -p matras` passed.

Screenshots use deterministic seeded chat content:

- 360x720: [state-360x720.png](state-360x720.png)
- Issue-specific 919x720: [state-919x720.png](state-919x720.png)
- 1320x840: [state-1320x840.png](state-1320x840.png)

The 360x720 screenshot was inspected: the compact action menu, rounded
composer, and send control are visible within the window. The issue attachment
is a 919x133 composer crop; 919x720 captures the full app at the same width.
The final combined workspace suite passed; see
[workspace-test.log](../workspace-test.log). The issue remains open until the
changes land on the default branch.
