# Issue #14 evidence

Issue: https://altlinux.space/toxblh/matras/issues/14 — video play icon moves
out of its poster when the message row is wider than the poster.

## Reproduction and regression test

The still poster is capped at 420 px inside a message row. `Gtk.Overlay` was
filling the 900 px row while its poster child stayed at the start, so the
centered play button was positioned at x=450, beyond the poster's right edge.
The regression test builds that GTK hierarchy and checks the button's actual
allocated center against the poster bounds.

The original behavior was verified in a real GTK allocation test: with the
production overlay's default `Fill` alignment, the 900 px row placed the play
button at x=450, past the 420 px poster. That check failed as expected.
After adding `Align::Start`, the same GTK geometry check passed and produced
the screenshot below. GTK widget initialization inside unit tests then proved
unsafe with Rust's parallel test harness, so the permanent regression test was
changed to a suite-safe configuration test of the alignment value consumed by
the production builder.

Current regression test:

```sh
cargo test -p matras --lib ui::media::tests::poster_overlay_is_aligned_to_its_clamped_child -- --exact
```

The GTK test failed before the fix with `play icon center 450 escaped poster
width 420 in a 900px row` and passed after the fix. The permanent configuration
test fails if the overlay alignment changes away from `Start`.

## Screenshot

`video-layout.png` is the 900 px GTK layout fixture captured after the fix. It
shows the play button within the 420 px poster area; it is not a capture of a
real video frame.

Screenshot: [video-layout.png](video-layout.png)

## Remaining checks

The initial crate suite found GTK initialization in this test unsafe under the
parallel harness; the test is now a GTK-free alignment configuration test.
`cargo test -p matras --lib -- --test-threads=1` passed (123 tests), and
`cargo test --workspace -- --test-threads=1` passed across the workspace,
including doc-tests. `cargo fmt --all -- --check` and `git diff --check` pass.
