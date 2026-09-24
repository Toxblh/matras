# Issue #4 evidence

Issue: https://altlinux.space/toxblh/matras/issues/4 — “Внутренние уведомления всегда непрочитаны”. The report says opening a chat mention notification does not mark it read.

## Reproduction and fix

The Mentions inbox row opens a message through `jump_to_post`. That selected its channel through `select_channel`, which returned immediately when the channel was already selected. The return skipped Mattermost's `view_channel` request and the local unread-counter reset. The inbox target commonly lives in the channel already on screen, so this is a real user path.

Added `opening_a_mention_in_the_current_channel_still_syncs_read_state`. It asserts that a post jump back into the selected channel still requires the read sync. The test failed before the fix and passed afterward; summarized command output is in `red-test.txt` and `green-test.txt`.

Post jumps now take a dedicated same-channel path through `select_channel_inner`. That path calls the same `mark_channel_viewed` routine as a channel change, which sends `view_channel` and clears local unread mention counters. Ordinary re-selection does not take this path.

## Checks

- Regression test before fix: failed on the assertion; see `red-test.txt`.
- Regression test after fix: passed; see `green-test.txt`.
- `cargo test -p matras --lib`: passed, 123 tests.
- `cargo fmt --check`: passed.
- `git diff --check`: passed.
- `cargo test --workspace`: the default parallel run stalled in a GTK sidebar test; `cargo test --workspace -- --test-threads=1` then passed across the workspace, including doc tests.
- Final combined workspace run also passes all 140 Matras, 22 API, and 36 calls tests; see [workspace-test.log](../workspace-test.log).

## Screenshot

`corrected-state.png` is a 1320×840 deterministic demo render launched with `MATRAS_DEMO_OPEN_MENTION=1`. The demo begins with Development already selected, then follows the same mention post jump path and shows the target post in the conversation. This is supplementary UI evidence; the screenshot cannot show the server-side read-state update. The inbox badge counts recent mention results, so it remains visible after opening a result.

The same deterministic mention state was captured at PinePhone `360×720`,
issue-specific `720×800`, and desktop `1320×840` in `matrix-*.png` and inspected
at each size.

Issue closed automatically by PR #16, merged into `master` as `2166a9c`.
