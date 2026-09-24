# Issue #1: Неадаптивный интерфейс

Issue: https://altlinux.space/toxblh/matras/issues/1 (open)

## Reproduction and fix

The attached screenshots show the chat composer and message content extending
beyond a narrow window. Regression checks first failed because the composer
scroller had an unconstrained minimum width and code blocks propagated their
unwrapped natural width. Their red/green output is in [test-log.txt](test-log.txt).

The composer now has a one-pixel minimum content width, code lines scroll inside
their own block, and message hover controls overlay the body without widening
the row. The Inbox panel's three oversized tabs were also replaced with a
compact, equal-width tab bar. In the composer, attachment, priority, and
send-later actions live under a four-square menu left of the text field; the
send button stays visible at the right. The narrow row regression test failed
with a 385px minimum before the overlay fix, then passed; see
[responsive-red.log](responsive-red.log) and
[responsive-green.log](responsive-green.log).

## Screenshot evidence

Each image is a deterministic demo state, saved separately at every required
viewport. `corrected-state-360x720.png` shows the compact composer and full
header/feed; `menu-state-360x720.png` is the latest screenshot of the four-square
layout. The 360px run after the Inbox tab fix had no overlay-width warning.

- PinePhone 360×720: [corrected state](corrected-state-360x720.png), [compact composer](menu-state-360x720.png)
- Issue attachment application bounds, 720×800: [corrected state](corrected-state-720x800.png)
- Desktop, 1320×840: [corrected state](corrected-state-1320x840.png)
- Earlier 400×800 render: [corrected-state.png](corrected-state.png)

The code-block scrolling behavior is proved by its regression test rather than
claimed as visible in these screenshots.

## Checks

Before-fix assertion failures and focused green runs are recorded in
[test-log.txt](test-log.txt); the minimum-width regression is recorded in the
responsive logs. The final combined command
`cargo test --offline --workspace -- --test-threads=1` passed (Matras 140, API
22, Calls 36, plus doc-tests); summary: [workspace-test.log](../workspace-test.log).
`cargo fmt --all -- --check`, `git diff --check`, and `cargo build --offline -p
matras` passed.
