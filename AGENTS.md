# Issue fixes and visual verification

This is the GTK Matras client. Treat each issue's steps and viewport as an
acceptance test, not as background context.

## Before changing code

- Read the issue and identify the exact starting state, action, resulting state,
  and target window size. For navigation bugs, include the path from a fresh
  launch with no channel selected.
- Reproduce the action itself. A passing helper test that computes whether a
  button should be visible does not prove that the button's click signal changes
  the visible page or opens the intended panel.
- Add a regression test for the reported state transition before changing its
  implementation. Prefer a GTK interaction test or a deterministic demo action
  that emits the same widget signal as a click. Keep pure policy tests as
  additional coverage, not as substitutes for interaction coverage.

## Before closing an issue

- Run the focused regression tests and the relevant workspace test suite.
- Capture and inspect the real GTK window in every state named by the issue.
  Screenshots must show the changed control and the resulting state. A screenshot
  path existing on disk is not visual verification; check that it is not the
  pre-action state, a hidden panel, a stale package, or an off-screen column.
- For responsive UI, check at least 360x720 (PinePhone), 720x800 (intermediate),
  and 1320x840 (desktop). When a panel opens, capture that panel after opening
  it at each width; a screenshot of the underlying chat does not count.
- Exercise controls in both inactive and active states. For calls, inspect mute,
  camera, and screen-share after toggling them on, including icon contrast and
  button bounds.
- Describe screenshot provenance and exact viewport in the evidence. Say when a
  screenshot uses deterministic demo data instead of a live Mattermost server.
- Verify the installed Flatpak by its commit before launch. To check updates,
  update from the published signed remote, compare the commit before and after,
  then launch that installation. A native debug build or the old installed
  package does not verify Flatpak delivery.
- For package-format changes, verify the serving Forgejo version and its live
  registry endpoints first. A successful upload and a package entry in the UI
  do not prove Flatpak can install or update from it; add the remote, install
  the app, and check a later update through the same published route.
- Close an issue only after its concrete acceptance actions pass. If the test or
  screenshot did not exercise the reported action, keep the issue open and fix
  the verification gap before claiming success.
