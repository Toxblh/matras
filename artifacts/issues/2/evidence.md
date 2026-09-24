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
It creates a GTK `ListBox`, selects a row, then calls the same clear-selection
helper used by the navigation notification handler. Before the fix the row
remained selected and the assertion failed; after the fix it is unselected.
The UI only clears selection when a collapsed `NavigationSplitView` returns
to its sidebar, making the next click on the same row a new selection.

- Red: `cargo test -p matras --lib ui::sidebar::tests::returning_to_collapsed_sidebar_clears_the_selected_channel -- --exact --test-threads=1` — failed at `assert!(list.selected_row().is_none())`, 0 passed / 1 failed.
- Green: same command after the fix — 1 passed / 0 failed (run with access to the GTK display).
- `cargo test -p matras --lib -- --test-threads=1` — passed, 123 tests.
- `cargo test --workspace -- --test-threads=1` — passed across the workspace,
  including doc-tests.
- `cargo fmt --all -- --check` and `git diff --check` — passed.
- Screenshot: `corrected-state.png` is a deterministic 400×800 demo rendering of the narrow chat layout. It confirms the UI renders at the narrow size; it does not by itself demonstrate the return-and-reselect interaction.

## Limits

Forgejo attachment in the issue describes the visual sequence; live UI
interaction was not automated. The screenshot shows the corrected narrow chat
state rather than the return-and-reselect gesture itself. The fix is local and
the issue remains open until it reaches the default branch.
