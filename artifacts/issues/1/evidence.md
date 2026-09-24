# Issue #1: Неадаптивный интерфейс

Issue: https://altlinux.space/toxblh/matras/issues/1 (open; no comments)

## Reproduction

Inspected both issue attachments. At the reported narrow width the composer
extends beyond the visible chat pane. The second attachment shows message
content widening and clipping the rest of the interface. The cause was
confirmed in the widget configuration: the composer scroller left its minimum
content width unconstrained (`-1`), and code block scrollers propagated the
natural width of their unwrapped labels.

## Regression tests

Both assertions failed on the unfixed widget properties before implementation:

- Code scroller: `assertion failed: !scroller.propagates_natural_width()`.
- Composer: `left: -1`, expected `right: 1` for its minimum content width.

After the fix both focused tests passed. Exact commands and output excerpts are
in [test-log.txt](test-log.txt).

## Fix

- Set the composer scroller minimum content width to one pixel so the horizontal
  composer can take the pane's available width.
- Stop code block scrollers from propagating long unwrapped lines as the chat
  pane's natural width; those lines remain horizontally scrollable within the
  code block.

## Visual evidence

[corrected-state.png](corrected-state.png) is a deterministic Matras demo at
400×800. I inspected it: the composer, text entry and send button are all inside
the window, and message text wraps within the chat pane. This screenshot shows
the corrected narrow layout; the code-line scrolling behavior is covered by the
regression test rather than claimed as visible in the screenshot.

## Checks

- Focused code scroller regression: passed.
- Focused composer regression: passed.
- `cargo test -p matras --lib -- --test-threads=1`: 123 passed.
- `cargo test --workspace -- --test-threads=1`: passed; Matras 123 tests,
  Mattermost API 22 tests, Mattermost calls 36 tests, plus doc-tests.
- `cargo check -p matras --lib`: passed.
- `rustfmt --check --edition 2021 crates/matras/src/ui/chat.rs
  crates/matras/src/ui/message.rs`: passed.

Workspace-wide `cargo fmt --all -- --check` and `git diff --check` both pass
with all parallel issue changes integrated.
