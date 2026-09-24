# Issue #5: GTK Inspector shortcut conflict

Issue: <https://altlinux.space/toxblh/matras/issues/5>

## Reproduction and change

Matras registered `<Control><Shift>i` for `win.inbox`, consuming GTK's usual
Inspector shortcut. The inbox now uses `<Control><Shift>m`; the Inspector key
is no longer claimed by a Matras window accelerator.

## Regression test

The test `ui::shortcuts::tests::inbox_shortcut_does_not_steal_the_gtk_inspector`
was run before and after the change:

- Before: failed as expected because the inbox accelerator was
  `<Control><Shift>i`; see [red-test.txt](red-test.txt).
- After: passed (`1 passed, 0 failed`).
  See [green-test.txt](green-test.txt).

Command:

```sh
cargo test -p matras --lib ui::shortcuts::tests::inbox_shortcut_does_not_steal_the_gtk_inspector -- --exact --nocapture
```

## Screenshot

`corrected-state.png` is a local Matras demo screenshot from the built app. It
shows the app running after the shortcut change; it does not depict the GTK
Inspector or prove that the host GTK runtime exposes its Inspector. The
automated test proves Matras no longer binds the conflicting key. Runtime
Inspector availability still depends on the GTK build/runtime configuration.
Supplementary app renders at PinePhone `360×720`, issue-size `720×800`, and
desktop `1320×840` are saved as `matrix-*.png`; they show layout only and do
not claim to demonstrate the shortcut at runtime.

## Status

The focused red/green test is complete. The final
`cargo test --offline --workspace -- --test-threads=1` passed (140 Matras, 22
API, 36 Calls, and doc-tests; see [workspace-test.log](../workspace-test.log));
`cargo fmt --all -- --check` and `git diff --check` passed. The
app has not been published, so the issue must remain open until the change
reaches the default branch.
