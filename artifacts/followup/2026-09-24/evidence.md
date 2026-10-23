# UI follow-up — 2026-09-24

Screenshots below are real GTK window renders from `MATRAS_DEMO=1`, with
deterministic sample Mattermost content and the exact application window size in
each filename. They do not use a live server, microphone, camera, or screen
capture permission. The inbox and thread shots are taken after opening the
actual sidebar button signal or thread action, not from the pre-action chat.

## Inbox and thread navigation

- Inbox opened by emitting the sidebar Inbox button's normal click signal while
  no channel is selected: [360×720](inbox-from-cold-sidebar-360x720.png),
  [720×800](inbox-from-cold-sidebar-720x800.png),
  [1320×840](inbox-from-cold-sidebar-1320x840.png).
- Open thread, showing only the window close control and keeping its reply field
  and send button in bounds: [360×720](thread-360x720.png),
  [720×800](thread-720x800.png), [1320×840](thread-1320x840.png).
- Account status dot beside the channel list and account header:
  [360×720](sidebar-presence-360x720.png),
  [720×800](sidebar-presence-720x800.png),
  [1320×840](sidebar-presence-1320x840.png).

## Composer and call controls

- Empty message composer starts as one line: [360×720](chat-composer-360x720.png),
  [720×800](chat-composer-720x800.png),
  [1320×840](chat-composer-1320x840.png).
- Microphone, screen share, and camera in their active states use accent
  foregrounds on every viewport: [360×720](call-controls-active-360x720.png),
  [720×800](call-controls-active-720x800.png),
  [1320×840](call-controls-active-1320x840.png).

## Regression tests

`cargo test --offline -p matras --lib` covers inbox navigation from the
collapsed channel list, thread panel layout policy, thread close behavior,
header presence badge sizing, call active control styling, and composer line
height. The actual inbox and thread screenshots use deterministic demo actions
that emit the GTK button click and open-thread callbacks after the window has
been presented.
