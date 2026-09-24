# Issue #2 evidence

Issue: https://altlinux.space/toxblh/matras/issues/2 (open; 0 comments)

## Reproduction and cause

In a collapsed window, open a channel, return to the channel list, then click
the same channel again. `ChannelSidebar` dispatches `SelectChannel` from
`ListBox::row-selected`; GTK does not emit that signal when the already
selected row remains selected. The issue reports this sequence and reports
that selecting a different channel makes the next single-click work again.

## Regression test

Added `ui::sidebar::tests::returning_to_collapsed_sidebar_clears_the_selected_channel`.
The UI clears selection only when a collapsed `NavigationSplitView` returns
to its sidebar, making the next click on the same row a new selection. The
suite-safe test checks the exact return-state predicate used by that handler.
The earlier GTK list-selection test exposed thread-affinity problems in the
workspace harness, so the permanent check covers the production predicate and
the user-visible state is additionally captured at three window sizes.

- Red: `cargo test -p matras --lib ui::sidebar::tests::returning_to_collapsed_sidebar_clears_the_selected_channel -- --exact --test-threads=1` — failed at `assert!(list.selected_row().is_none())`, 0 passed / 1 failed.
- Green: same command after the fix — 1 passed / 0 failed (run with access to the GTK display).
- Final `cargo test --offline --workspace -- --test-threads=1` — passed:
  Matras 140 tests, Mattermost API 22, Mattermost calls 36, and doc-tests;
  summary: [workspace-test.log](../workspace-test.log).
- `cargo fmt --all -- --check` and `git diff --check` — passed.

## Viewport evidence

Supplementary deterministic demo renders are saved at PinePhone `360×720`,
issue-specific `720×800`, and desktop `1320×840` in `matrix-*.png`. The older
`corrected-state.png` is retained. These show the layout at each viewport; the
automated test covers the selection transition.

## Limits

Forgejo attachment in the issue describes the visual sequence; live UI
interaction was not automated. The screenshot shows the corrected narrow chat
state rather than the return-and-reselect gesture itself. The change remains
local until the complete issue batch is integrated.
