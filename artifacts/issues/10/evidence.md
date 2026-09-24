# Issue #10 evidence

Issue: https://altlinux.space/toxblh/matras/issues/10 (open; 0 comments)

## Behavior

Ctrl+Q already invokes the application-wide `app.quit` action. Added a
window-scoped `win.close` action on every Matras window and bound Ctrl+W to
it. It calls `GtkWindow::close`, so session windows continue through the
existing close-request handler: save the snapshot, then hide when background
mode is enabled; otherwise close the window normally. Ctrl+Q still quits the
application and releases the background hold.

## Regression test

- Red: `cargo test --offline -p matras --lib ui::shortcuts::tests::close_window_shortcut_is_ctrl_w_and_is_distinct_from_inbox -- --exact --test-threads=1` — without the close action installed, the expected window action was missing.
- Green: same command after the fix — 1 passed / 0 failed. The initial GTK
  integration test checked Ctrl+W registration, activated `win.close`, and
  observed `close-request` (see the red/green logs). Its widget initialization
  was later replaced in the permanent suite by a GTK-free check of the action
  name and accelerator, so the full suite is safe under the GTK thread model.

This is a keyboard action change; no screenshot is claimed as evidence of
keyboard behavior. Full-app
demo renders are retained only as supplementary layout checks at
`supplement-360x720.png`, `supplement-720x800.png`, and
`supplement-1320x840.png`.
The final combined workspace suite passed; see
[workspace-test.log](../workspace-test.log).
