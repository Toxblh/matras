# gomon calls — client protocol

What a native client (Matras GTK) must do to start, join and receive gomon calls.
gomon = LiveKit 1.13 SFU + backend `services/communication` (the "comms API") +
Mattermost plugin `ru.corp.comms`.

## Sources

| Side | Repo | Revision |
|---|---|---|
| **Mobile** (reference client) | `/var/home/toxblh/git/matras-mobile-gomon-fixes` | HEAD `47559d869`; call logic vendored from comms `4e47362` (`app/products/gomon/native/call_core/VERSION:1`) |
| **Server** (authority) | `/var/home/toxblh/git/vertushka/comms` | HEAD `b606aa4` |

When the two disagree, the server code wins; the mobile code shows what a working
client actually does.

Reference prefixes. **Mobile** refs are relative to the mobile repo; **server** refs
(marked *[srv]* in prose) are relative to `vertushka/comms`.

| Prefix | Side | Path |
|---|---|---|
| `G/` | mobile | `app/products/gomon/` |
| `N/` | mobile | `app/products/gomon/native/` |
| `C/` | mobile | `app/products/gomon/native/call_core/` |
| `P/` | server | `plugins/mattermost/server/` (the `ru.corp.comms` plugin, Go) |
| `S/` | server | `services/communication/internal/` (comms API, Go) |
| `S/main` | server | `services/communication/cmd/communication-api/main.go` |
| `W/` | server | `apps/meeting-web/src/` (the web call page) |
| `X/` | server | `spec/` and `contracts/` |

## 0. Architecture in one picture

```
Matras ──MM session──▶ Mattermost /plugins/ru.corp.comms/api/v1/*   (start / join / ring)
   │                         │ returns join_url = {WebURL}/call/<id>#code=<hex>
   │                         ▼
   └──no MM auth──▶ <join_url origin>/v1/*   (handoff/redeem → Bearer token → join, snapshot, chat…)
                             │ returns livekit_url + LiveKit token
                             ▼
                        LiveKit SFU (media; server-only data packets on topic "conf")
```

There are two separate auth domains:
- The plugin routes use the normal Mattermost session.
- The comms API uses its own bearer token. The client gets it by redeeming the
  one-time code from `join_url`.

The plugin talks to the comms API with an HMAC signature: headers `X-Comms-Timestamp` and
`X-Comms-Signature` over `ts.METHOD path.body` (`P/plugin.go:208-224` *[srv]*). That is
for `/v1/integrations/*` only, and a client never needs it.

## 1. Discovery

| Step | Detail | Ref |
|---|---|---|
| Plugin id | `ru.corp.comms` | `G/constants.ts:5` |
| Detect | `GET /api/v4/plugins/webapp`; gomon is on iff a manifest has `id == "ru.corp.comms"` | `G/actions.ts:48-55`, `G/utils.ts:48-50`, `app/client/rest/plugins.ts:13` |
| When | after every websocket (re)connect sync, and on WS `plugin_statuses_changed`, `plugin_enabled`, `plugin_disabled` | `app/actions/websocket/index.ts:107`, `app/actions/websocket/event.ts:216-221` |
| Per-channel state | `GET …/channels/{channel_id}/call` when a channel is shown; live call id cached per channel | `G/components/call_button.tsx:27-31`, `G/actions.ts:57-64` |
| All live calls (not used by mobile) | `GET …/active` → `{calls:[{channel_id, call_id, participant_ids, in_call}]}`; the user's own current call: `GET …/me/call` → `{call: view\|null, server_time}` | `P/f11.go:262-271` *[srv]* |
| Comms base URL | not configured on the client: the origin of `join_url`. *[srv]* `join_url` = plugin setting `WebURL` + `/call/{id}#code=…`, so the web page and `/v1` API share that origin. | `N/api.ts:9-10,44-53`, `P/plugin.go:420-423` *[srv]* |

Plugin settings (`P/plugin.go:36-40`, `mattermost-infra/env/plugin-settings.yaml:251-264`;
names only):
- `APIURL`: the comms API as the MM server reaches it.
- `WebURL`: the public call page.
- `SharedSecret`: at least 32 chars; it falls back to env `MM_PLUGIN_COMMS_SHARED_SECRET`
  (`P/plugin.go:61-81`).

Call states (`S/app/model.go`, `X/state-machines/sm1-session.yaml`):
- Live: `STARTING`, `ACTIVE`, `EMPTY_GRACE`.
- Also: `ENDING`, `ENDED`, `FAILED`.
- See `G/constants.ts:20` and `P/f11.go:132-134`.

## 2. Plugin REST API (Mattermost side)

Base: `{mm_server}/plugins/ru.corp.comms/api/v1` (**no** `/api/v4`)
(`G/client/rest.ts:27-29`, `app/client/rest/base.ts:206-207`).

Auth: the ordinary MM session, `Authorization: Bearer <MM token>` plus
`X-Requested-With: XMLHttpRequest` (`app/managers/network_manager.ts:57`).
*[srv]* MM turns the session into the `Mattermost-User-Id` header. Without it, every
route except `/join/` and `/login` returns plain-text `401 unauthorized`
(`P/plugin.go:455,469-472`).

| Purpose | Request | Body | Response | Ref |
|---|---|---|---|---|
| Start (or reuse) a channel/DM call | `POST /calls` | `{"channel_id", "video": bool}` | `{"call_id", "reused", "join_url"}` | `G/client/rest.ts:31-33`; *[srv]* `P/plugin.go:479-492,326-417` |
| Join an existing call | `POST /join/{call_id}` | none | `{"join_url"}` | `G/client/rest.ts:35-37`; *[srv]* `P/plugin.go:533-540,544-570` |
| Channel's current call | `GET /channels/{channel_id}/call` | — | `{"call": view\|null}` | `G/client/rest.ts:39-41`; *[srv]* `P/f11.go:250-261` |
| Invitation action | `POST /invitations/{invitation_id}/{ack\|accept\|decline}` | none | `{"state"}` (+ `"join_url"` on accept) | `G/client/rest.ts:43-45`; *[srv]* `P/plugin.go:494-532` |
| Ring action by channel | `POST /channels/{channel_id}/{accept\|decline}` | none | as above; 404 `{"error":"нет входящего вызова"}` if no ring is remembered | `G/client/rest.ts:47-49`; *[srv]* `P/plugin.go:456-466` |

*[srv]* Call view (`P/f11.go:137-179`): `call_id`, `channel_id`, `state` (live only;
otherwise `call` is `null`), `card_post_id`, `participants`, `participant_names`,
`participant_ids` (comma-separated MM user ids), `started_at` (ms, card create time),
`in_call` (bool), `deep_link`, `channel_name` (the other person's name for DMs),
`channel_type`. Membership is required; otherwise 403 `{"error":"нет доступа к каналу"}`.

*[srv]* **Error format of plugin routes: `{"error": "<human Russian text>"}`** with
status 400 (start), 403 (join, invitation session), 409 (invitation command failed),
404, or 502. No machine code is included (`P/plugin.go:461,485,490,504,515,537`). It is
not MM's `{id,message}` format and not the comms API's `{error:{code}}` format.

### What `POST /calls` does *[srv]* (`P/plugin.go:324-417`)

| Channel type | `kind` | Who is rung |
|---|---|---|
| DM (`D`) | `dm` | the other member |
| Group DM (`G`) | `group` | every other member (first 50; bots/deactivated skipped) |
| Public/private | `channel` | **nobody**; members join from the card |

Details:
- The caller must be a channel member.
- The comms call gets context key `mm:<channel_id>` and `title` = the channel display
  name (`S/httpapi/server.go:460-489`).
- One live call per channel. Another call already live in the channel works like this:
  - If the caller is involved in it, the server returns that call (`reused: true`)
    (`S/app/commands.go:229-240`).
  - If they are not involved (comms `409 conflict`), the plugin joins them to the running
    call through the card, also with `reused: true` (`P/plugin.go:379-390`).
- Double-click protection: the idempotency key is `mm-{channel}-{user}-{video}-{5 s
  bucket}`. A replay also reports `reused: true` (`P/plugin.go:367-378,394-396`).
- The first start creates the card post (`ensureCard`, `P/plugin.go:427-450`).
- **`video`** is stored on the call and exposed as `video` in the comms snapshot
  (`S/app/commands.go:168-171`, `S/app/model.go:235`). No server logic depends on it.
  Its only effect is the web client's default camera: camera on iff `call.video` and not
  a "quiet join" (`W/pages/CallPage.tsx:61,98`). It has no effect on ringing.

### Other plugin routes the mobile app does not use *[srv]*

| Route | Effect | Ref |
|---|---|---|
| `GET /plugins/ru.corp.comms/join/{call_id}[?autojoin=1]` | browser deep link; 302 to a fresh `join_url` (adds `?autojoin=1`, which skips the web pre-join screen) | `P/f11.go:181-218` |
| `GET /plugins/ru.corp.comms/login?return=/path` | 302 to `{WebURL}{return}#code=<handoff>`, a call-less sign-in to the comms web app | `P/f11.go:219-247` |
| slash command `/call [audio\|video]` | same as `POST /calls`, ephemeral reply | `P/plugin.go:584-599` |

## 3. Joining (comms API)

### 3.1 `join_url` and handoff

- Format: `{WebURL}/call/<call_id>#code=<hex>` (`N/api.ts:44-53`, `P/plugin.go:420-423`).
- *[srv]* The code lives **2 minutes** and is redeemable **once**
  (`S/app/mattermost.go:123-129,132-138`). A spent or expired code gives
  **410 `gone`**: "link expired or already used; start again from Mattermost". To get a
  new one, call `POST …/join/{call_id}` again.
- Mobile keeps the redeemed token, so a re-created call component does not redeem twice
  (`G/store.ts:75-79`, `N/use_call.ts:316-325`).

### 3.2 Comms API conventions *[srv]*

| Item | Value | Ref |
|---|---|---|
| Base | `join_url` origin, paths `/v1/…` | `N/api.ts:58-65` |
| Auth | `Authorization: Bearer <comms token>`; `?access_token=` is accepted **only** on `/v1/events` | `S/httpapi/server.go:87-107` |
| Body | JSON, **max 64 KiB** (bad JSON → 400 `validation`) | `S/httpapi/server.go:198-204` |
| Idempotency | every command (`s.command`/`joinLike`) **requires** `idempotency_key` of **8..128 chars**, scoped per (user, key). The same key with a different body → **422 `idempotency_mismatch`**. A replay returns the stored response with `"replayed": true`. 5xx results are not stored, so the same key can be retried. | `S/app/app.go:321-330,360-378,391` |
| Success envelope | commands add `command_id`, `operation_id`, `status:"accepted"`, `replayed:false` to the body | `S/app/app.go:404-409` |
| Errors | `{"error": {"code", "message", "details"?}}`; unknown errors → 500 `internal` | `S/app/app.go:425-431,464-470` |
| Headers sent | `Cache-Control: no-store` on `/v1/`, `X-Content-Type-Options: nosniff`, CORS (`COMMS_CORS_ORIGINS`) | `S/httpapi/server.go:120-140` |

Exceptions: `POST /v1/handoff/redeem` (no auth, no key) and
`POST /v1/calls/{id}/reactions` (the mobile app sends no key; `N/api.ts:93`).

Human error texts per code are in `C/errors.ts:4-26`.

### 3.3 Sequence

| # | Request | Body | Response | Ref |
|---|---|---|---|---|
| 1 | `POST /v1/handoff/redeem` (no auth) | `{"code"}` (≥16 chars) | `{"token", "principal", "call_id"}` | `N/api.ts:81-86`; *[srv]* `S/httpapi/server.go:509-522` |
| 2 | `POST /v1/calls/{call_id}/join` | `{"idempotency_key", "device_id"}` (`device_id` ≤ 64 chars) | `{"connection_id", "livekit_url", "token", "expires_at"}` + envelope | `N/api.ts:88`; *[srv]* `S/app/commands.go:367-429` |
| 3 | LiveKit `room.connect(livekit_url, token)` | — | — | `N/use_call.ts:367` |
| 4 | Event stream + snapshot + chat; enable mic/camera | — | — | `N/use_call.ts:392-397,134-144` |

#### Comms token *[srv]*

- It is an employee token, valid **12 h** (`COMMS_SESSION_TTL`) (`S/app/principals.go:14-28`,
  `S/main:112`).
- It is **not scoped to the call**: it works for any API the user may use.
- No refresh endpoint exists. When it expires (401), get a new handoff from the plugin.
- Suspension is checked on every request (`S/app/principals.go:38-55`).

#### Join *[srv]*

- Join needs an *involved* role. For channel calls, `POST …/join/{call_id}` on the plugin
  grants channel access (`/v1/integrations/mattermost/grant`) before issuing the code
  (`P/plugin.go:544-570`, `S/app/mattermost.go:100-121`).
- A join on a non-live call → `invalid_transition`; not permitted → `forbidden`.
  Both commit a `DISCONNECTED` connection with `details:{connection_id, reason}`
  (`S/app/commands.go:385-405`).

#### LiveKit token contents *[srv]* (`S/app/commands.go:408-440`, `S/lk/lk.go:30-62`)

| Claim | Value |
|---|---|
| room | **`call_id`** |
| identity (`sub`) | **`connection_id`** (confirms the mobile inference) |
| name | display name |
| metadata | JSON `{"principal_id", "kind": "employee"\|"guest"\|"room", "display_name", "call_id", "avatar_url"?}` |
| grants | `roomJoin`, `canPublish`, `canSubscribe`; **`canPublishData: false`** |
| TTL | **600 s** (`COMMS_LIVEKIT_TOKEN_TTL`, `S/main:111`); only matters for the first connect, because the SFU refreshes tokens for a live session |

#### Replayed join *[srv]*

A replayed join (same key) **re-mints** a token for the same connection while it is still
`CONNECTING`, and returns `livekit_url` again (`S/httpapi/server.go:627-651`,
`S/app/commands.go:749-761`). After a real drop, use a **new** key: that makes a new
connection.

#### `device_id` and mobile behaviour

- `device_id` is also how invitations know the device (`ack`/`accept`) and how
  `/v1/events` marks the device seen (`S/httpapi/server.go:815`, `S/app/queries.go:153-158`).
- Mobile uses `matras-<8 chars>` per app run (`N/use_call.ts:24-25`).
- Mobile reads only `participant.identity` and `name` (`N/stage.tsx:124-128`) and ignores
  metadata. A desktop client can take `avatar_url` and `kind` from metadata.

### 3.4 Snapshot, state and events

| Request | Response | Ref |
|---|---|---|
| `GET /v1/calls/{call_id}` | `Call` snapshot | `N/api.ts:19-24,91`; *[srv]* `S/app/model.go:231-300` |
| `GET /v1/state` | `{"cursor", "me", "calls":[Call…], "incoming":[invitation…], "history":[invitation…]}` | `N/api.ts:165-167`; *[srv]* `S/app/queries.go:27-91` |
| `GET /v1/events?after={cursor}&wait={0..25}&device_id=` | `{"events":[Event…], "cursor"}` | `N/api.ts:158-190`; *[srv]* `S/httpapi/server.go:808-850` |

Event stream rules *[srv]*:
- `wait` is capped at **25 s**.
- A batch holds at most 500 events, filtered to the caller's audience
  (`S/app/queries.go:116-150`).
- A cursor beyond the head → **410 `gone`**: re-read `/v1/state`.
- `Accept: text/event-stream` switches to **SSE** (`S/httpapi/server.go:816-819,882-`).
- `/v1/state` lists invitations still ringing (`incoming`), so a client that starts late
  can still show a pending ring.

**Event envelope** *[srv]* (`S/app/queries.go:93-109`, `X/../contracts/events.schema.json`):
`event_id`, `seq`, `schema_version` (1), `type`, `aggregate_type`, `aggregate_id`,
`aggregate_version`, `org_id`, `call_id?`, `occurred_at`, `correlation_id`,
`causation_id`, `operation_id`, `data`.
- A state transition's `data` is `{from, to, transition, reason, entity?}`
  (`S/app/app.go:203-217`).
- Call-relevant `type`s: `call.created|state_changed|updated`,
  `invitation.created|state_changed`, `connection.state_changed`,
  `admission.*`, `conf.hand_changed|spotlight_changed|moderation`,
  `chat.message_posted|message_deleted|policy_updated`, `recording.*`, `sip_leg.*`.

The mobile app (like the web) **ignores the payloads**. Any non-empty batch triggers a
re-read of the snapshot and chat (80 ms coalesce), with a 15 s fallback poll
(`N/use_call.ts:121-132,395,432,437`). This is the simplest correct approach, because
snapshots are versioned.

`Call` snapshot fields:
- From mobile (`N/api.ts:16-24`): `call_id`, `state`, `title`, `host_id`, `cohost_ids`,
  `my_role` (`host|cohost|participant|invitee|guest|none`), `connections[]`,
  `invitations[]`, `code`, `meet_url`, `conf`, `locked`, `lobby[]`, `pin`, `created_at`.
- Also from the server *[srv]* (`S/app/model.go:231-290`): `version`, `video`,
  `grace_deadline`, `ended_reason`, `context:{kind: adhoc|dm|group|channel, key:"mm:<channel_id>"}`.
- Extension parts: `conf`, `recording`, `recording_options` (moderators only), `broadcast`,
  `sip` (`C/rec.ts:5-13`, `C/sip.ts:9-18`, `S/app/rec_recording.go:588-605`).
- Invitation objects (`S/app/model.go:165-172`): `invitation_id`, `call_id`, `invitee_id`,
  `invitee_name`, `inviter_id`, `inviter_name`, `state`, `accepted_device_id`, `version`,
  `created_at`, `deadline`, `reason`.
- Connection objects (`S/app/model.go:174-179`): `connection_id`, `call_id`,
  `principal_id`, `kind`, `display_name`, `state`, `device_id`, `version`, `reason`.

Server verdicts from the snapshot (`N/use_call.ts:111-119`):
- My connection in `REMOVED`, `SESSION_ENDED` or `AUTH_REVOKED` → exit with that reason.
- Call not live and not `ENDING` → `SESSION_ENDED`.

`conf` (`C/conf.ts:4-7`): `hands[]`, `spotlight[]`, `my_hand{raised, position}`,
`can{raise_hand, react, lower_other_hand, mute_other, mute_all, request_unmute, spotlight}`.

### 3.5 Data channel

*[srv]* Participants get `canPublishData: false` (`S/lk/lk.go:37-41`). The server sends
control packets with `SendData` on topic **`conf`** (`S/app/conf.go:34,140-145`). The
mobile app accepts a packet only if it has no sender identity and `topic == "conf"`
(`C/conf.ts:36-50`).

| `type` | Fields | Client action |
|---|---|---|
| `reaction` | `reaction`, `connection_id`, `name`, `at` | flying emoji on that tile |
| `muted` | `by`, `all?` | toast (the server already muted the track) |
| `unmute_request` | `by`, `request_id` | prompt; yes → enable mic |

Reaction keys (`C/conf.ts:17-25`): `thumbs_up`, `clap`, `heart`, `laugh`, `wow`, `party`.

| Action | Request | Body | Ref |
|---|---|---|---|
| React | `POST /v1/calls/{id}/reactions` | `{"reaction", "connection_id"}` | `N/api.ts:93`; *[srv]* `S/httpapi/conf.go:42`, **5 per 5 s per person** (`S/app/conf.go:30-31,239`) |
| Raise/lower hand | `POST /v1/calls/{id}/hand` | `{"idempotency_key", "raised"}` | `N/api.ts:92`; *[srv]* `S/httpapi/conf.go:15` |
| Spotlight (mobile unused) | `POST /v1/calls/{id}/spotlight` | `{"idempotency_key", "connection_id", "source", "on"?}` | *[srv]* `S/httpapi/conf.go:89-100` |

### 3.6 Meeting chat

| Request | Body / query | Response | Ref |
|---|---|---|---|
| `GET /v1/calls/{id}/chat?after_rev={rev}` | — | `{messages[], rev, can_post, moderator, files?, guest_history?}` | `C/chat.ts:15,65` |
| `POST /v1/calls/{id}/chat/messages` | `{idempotency_key, text, file_id?, mentions?[]}` | `{message}` | `C/chat.ts:122-128` |
| `POST /v1/chat/messages/{msg_id}/delete` | `{idempotency_key}` | `{message}` | `C/chat.ts:130-133` |
| `POST /v1/calls/{id}/chat/files?name=` | raw bytes, `application/octet-stream` | `{file}` | `C/chat.ts:136`, `N/api.ts:121-152` |
| `GET /v1/chat/files/{file_id}/link[?inline=1]` | — | `{url, expires_at, name}` | `C/chat.ts:138-140` |

Limits and projection:
- *[srv]* Text is at most **4000** chars (`COMMS_CHAT_MAX_LEN`, `S/app/chat.go:37`).
- File links live 300 s by default (`S/app/chat.go:55,64`).
- *[srv]* The meeting chat is mirrored to the call card's MM thread, and replies in that
  thread go back to the meeting chat (shown to employees only) (`P/chat.go:3-12`). So a
  desktop client can show the chat simply as the card's MM thread.

## 4. Signalling active calls

### 4.1 Websocket events *[srv]*

The plugin publishes events named `incoming`, `incoming_closed`, `call_state` and `my_call`.
MM prefixes them as `custom_ru.corp.comms_<name>`.

| Event | Broadcast to | Payload (exact) | Ref |
|---|---|---|---|
| `custom_ru.corp.comms_incoming` | invitee user | `{invitation_id, call_id, inviter_name, state}` with `state` ∈ `DISPATCHING`, `RINGING` | `P/plugin.go:620-638` |
| `custom_ru.corp.comms_incoming_closed` | invitee user | same fields; `state` = any other invitation state (`CREATED`, `ACCEPTED`, `DECLINED`, `EXPIRED`, `FAILED`, `BUSY`, `CANCELLED`) | `P/plugin.go:630-634` |
| `custom_ru.corp.comms_call_state` | **every member of the channel** | `{call_id, channel_id, state, participants}` | `P/f11.go:55-56` |
| `custom_ru.corp.comms_my_call` | **each user whose presence in that call changed** (joined or left) | same payload | `P/f11.go:57-58` |

Notes:
- **`incoming` has no `channel_id`.** The mobile type marks it optional, and in practice
  it is always empty (`G/actions.ts:183,208`). To map a ring to a channel, use `call_id`
  with the card / `call_state` events, or the comms snapshot `context.key` (`mm:<channel_id>`).
- `call_state` and `my_call` are emitted together whenever the plugin refreshes a card,
  that is on any non-chat call event (`P/plugin.go:744-765,770-790`).
  - `call_state` → the channel's live indicator.
  - `my_call` → "am I in a call" (e.g. a return-to-call bar). Read `GET …/me/call` for details.
- A brand-new invitation (`CREATED`) produces `incoming_closed` before `incoming`;
  harmless, but don't treat `incoming_closed` for an unknown id as an error.

Mobile handling (`G/actions.ts:189-224`): `incoming` → ring; `incoming_closed` → stop;
`call_state`/`my_call` → update `channel_id → call_id`.

### 4.2 Call card post *[srv]* (`P/plugin.go:427-450,770-846,866-898`)

Post `type == "custom_comms_call"`, created by the caller. The message is
"Звонок начат", later "Идёт звонок" or "Звонок завершён".

| Prop | Meaning |
|---|---|
| `call_id` | call id |
| `state` | call state |
| `version` | call version; the card never moves backwards |
| `pin` | call PIN (phone/meet code) |
| `participants` | number of `CONNECTED` connections |
| `participant_names` | `", "`-joined display names |
| `participant_ids` | `","`-joined **MM user ids** of connected people |
| `missed` | `", "`-joined `"<invitee_name>:<STATE>"`, STATE ∈ `EXPIRED`, `DECLINED`, `FAILED`, `BUSY` |
| `recording_url` | `{SiteURL}/plugins/ru.corp.comms/login?return=/recordings/{id}`, set once a recording is `READY`/`PARTIAL`; a bot reply with the same link is posted in the card thread |

Mobile reads `call_id`, `state`, `participants`, `participant_names`, `missed` (name part
only) and `recording_url` (`G/components/call_post.tsx:57-63`).

### 4.3 Push notification *[srv]* (`P/plugin.go:640-679`)

- The push is sent **only** when an invitation enters `DISPATCHING`.
- Fields: `Version` v2, `type` message, **`sub_type: "comms_call"`**, `server_id`,
  `channel_id`, `channel_name` (the inviter's name for DMs), `post_id` (the card),
  `sender_id` (card author), `sender_name` (inviter), message "Входящий звонок", `team_id`.
  It carries no invitation id.
- The plugin remembers the invitation under `ring:<user>:<channel>` for **180 s**. That is
  how `/channels/{id}/accept|decline` resolves it.
- VoIP transport is used only with server env `MM_PLUGIN_COMMS_VOIP_PUSH=1`.
- **No cancel/clear push exists.** A ring that was answered elsewhere or expired is
  closed only by the WS `incoming_closed`.

Mobile handling: foreground → ignored, because the WS rings
(`app/init/push_notifications.ts:142-145`); Android CallStyle and iOS CallKit as
described in the mobile files. A desktop client with a live WS can ignore the push.

## 5. Incoming ring

| Aspect | Behaviour | Ref |
|---|---|---|
| Trigger | WS `incoming`, new `invitation_id` | `G/actions.ts:191-201` |
| **Ack** | while `state == DISPATCHING`, **some device must `ack` within 10 s** (`COMMS_DELIVERY_TIMEOUT`), otherwise the invitation becomes `FAILED` ("could not deliver") and stops ringing. `ack` moves it to `RINGING`. | `G/actions.ts:202-204`; *[srv]* `X/state-machines/sm2-invitation.yaml:15-21,58-67`, `S/main:118` |
| Ring timeout | server-side **30 s** from `created_at` (`COMMS_RING_TIMEOUT`) → `EXPIRED`; the invitation object has `deadline`. Mobile closes its screen after 30 s too. | *[srv]* `S/main:117`, `S/app/model.go:170`, `sm2-invitation.yaml:51-57`; `app/constants/calls.ts:28` |
| DND | an invitee whose comms status is `dnd` goes straight to `BUSY` | *[srv]* `S/app/worker.go:149-171` |
| Accept | `POST …/invitations/{id}/accept` → `{state, join_url}`; first accept wins (a second → conflict) and other devices are dismissed | `G/actions.ts:130-139`; *[srv]* `sm2-invitation.yaml:22-29` |
| Decline | `POST …/invitations/{id}/decline` | `G/actions.ts:141-147` |
| By channel | `POST …/channels/{channel_id}/{accept\|decline}` (push path) | `G/actions.ts:149-181` |
| Cancel | the inviter, a moderator, or the call ending → `CANCELLED` | *[srv]* `sm2-invitation.yaml:30-36` |
| Stop ringing | WS `incoming_closed` for that `invitation_id` | `G/actions.ts:214-215` |

*[srv]* The plugin always sends `device_id: "mattermost-web"` on invitation commands, so
every MM client looks like the same device to comms (`P/plugin.go:514`). Plugin
idempotency keys are `mm-inv-{id}-{action}`, so a repeat is a replay.

In-call invites:
- `GET /v1/users?q=` → `POST /v1/calls/{id}/invitations {invitees:[principal ids]}`
  (`N/api.ts:94-95`).
- *[srv]* At most **100 invitations per minute** per inviter, otherwise 429
  `rate_limited` (`S/app/commands.go:81-85,764-765`).
- At most 50 invitees per call create (`S/app/commands.go:141`).

## 6. In-call actions

| Action | How | Ref |
|---|---|---|
| Leave | `POST /v1/connections/{connection_id}/leave {idempotency_key}`, then `room.disconnect()` | `N/api.ts:89`, `N/use_call.ts:557-563` |
| End for all | `POST /v1/calls/{id}/end`; `invalid_transition` = already ending | `N/api.ts:90`, `N/use_call.ts:565-577` |
| Own mic / camera / screen share | LiveKit only, **no REST** | `N/use_call.ts:459-491`, `N/native_call.tsx:296-309` |
| Mute other / all | `POST …/mute {connection_id}` / `mute-all` | `N/api.ts:103-104` |
| Ask to unmute | `POST …/request-unmute {connection_id}`; *[srv]* 1 per 4 s per target | `N/api.ts:105`; `S/app/conf.go:32,368` |
| Lower hand(s) | `POST …/hands/lower {connection_id}` or `{all:true}` | `N/api.ts:106` |
| Lock / co-host / remove | `POST …/lock {locked}`, `…/cohosts {user_id}`, `…/remove {connection_id}` | `N/api.ts:100-102` |
| Lobby | `POST /v1/admissions/{id}/{admit\|deny}` | `N/api.ts:107` |
| Guest link | `POST …/guest-links {ttl_seconds}`; revoke `POST /v1/guest-links/{id}/revoke` | `N/api.ts:108-109` |
| Recording | `POST …/recording/start {retention?, mode?, summary?}`, `…/recording/stop`, `GET /v1/recordings/{id}` | `N/api.ts:112-114` |
| SIP | `POST …/sip/dial`, `/v1/sip/legs/{id}/{cancel\|hangup\|redial}`, `GET /v1/sip/directory?q=` | `N/api.ts:117-119` |
| DTMF | LiveKit `publishDtmf` | `N/phone.tsx:72` |
| Status | `POST /v1/me/status {status: available\|dnd}` (affects ringing) | *[srv]* `S/httpapi/server.go:55`, `S/app/commands.go:742-743` |
| Telemetry | `POST /v1/diagnostics` (optional) | `N/api.ts:96`, `C/telemetry.ts` |

*[srv]* Recording start (`S/app/rec_recording.go:250-275`):
- `retention` defaults to the org policy; it must be one of `unlimited|365d|90d|30d` and
  no longer than the policy (else 400 with `details.allowed`).
- `mode` defaults to `video`; the only values are `video|audio`.
- `summary` is allowed only if summaries are available.
- A recording already running → 409 `conflict`.

Snapshot `recording_options` (moderators only, `S/app/rec_recording.go:600-604`):
- `default` = org retention policy.
- `allowed` = retention classes ≤ policy (`:156-164`), which is **retention, not media**.
- `modes` = always `["video","audio"]` (the media modes).
- `enabled`, `summary_available`.

### End / disconnect handling

| Signal | Meaning | Ref |
|---|---|---|
| LiveKit `ROOM_DELETED` | call ended | `N/use_call.ts:295-296` |
| `PARTICIPANT_REMOVED` | removed; re-read the snapshot. *[srv]* This is also sent when the user leaves the MM channel (`UserHasLeftChannel` → `AUTH_REVOKED`). | `N/use_call.ts:297-302`; `P/plugin.go:849-861`, `S/app/mattermost.go:148-180` |
| `DUPLICATE_IDENTITY` | same connection elsewhere | `N/use_call.ts:303-304` |
| Other | LiveKit reconnect, then a fresh `/join` (new key) every 3 s for 90 s. *[srv]* The server keeps a dropped connection rejoinable for `COMMS_REJOIN` = 45 s; an empty call ends after `COMMS_GRACE` = 60 s. | `N/use_call.ts:156-229`; `S/main:120,124` |

## 7. LiveKit usage

| Feature | Mobile | Web (`W/`, *[srv]* repo) |
|---|---|---|
| Room options | `{adaptiveStream: true, dynacast: true}` (`N/use_call.ts:80`) | same plus capture defaults (`W/components/InCall.tsx:203`) |
| Codec / simulcast | SDK defaults | SDK defaults (none set anywhere in `W/`) |
| Mic processing | defaults | `echoCancellation`, `autoGainControl` on; `noiseSuppression` and `voiceIsolation` off only in "open mic" (speakerphone) mode (`W/components/InCall.tsx:35-37`) |
| Camera capture | `facingMode` | `resizeMode: "none"` on touch devices, else defaults (`W/components/InCall.tsx:33`) |
| Screen share | `setScreenShareEnabled(on)` | `setScreenShareEnabled(on, {audio: true, selfBrowserSurface: "exclude"})`, so **system/tab audio is shared**, with default resolution and codec (`W/components/InCall.tsx:343`) |
| Data | receive only, topic `conf` | same |
| Participant metadata | ignored | JSON (see §3.3) |
| Quiet join | **not implemented** | joining with mic and camera **off** if ≥3 other people are connected, or in a `channel`-kind call with ≥1 other (`W/pages/CallPage.tsx:35-38,61`) |

Mobile SDK: `livekit-client` 2.22.3 + `@livekit/react-native` 3.0.0
(`docs/gomon-native-calls.md` phase-0 table).

## 8. Fallback: the web call page

Mobile can load `join_url` with `?embed=rn[&cam=0]` in a WebView (`G/utils.ts:13-20`).
*[srv]* `W/extensions/embed/embed.ts:5-13`:
- `embed=mm&host=<origin>` → the page talks to `window.parent` with postMessage.
- `embed=rn` → page-to-host messages go to `window.ReactNativeWebView.postMessage`.
- Messages: `{type:"comms:ready"}`, `{type:"comms:state", mic, cam, people}`,
  `{type:"comms:ended", reason}`.
- Host commands: `window.dispatchEvent(new CustomEvent("comms:command",{detail:"mic"|"leave"}))`.
- Embedded or `?autojoin=1` skips the pre-join screen (`W/pages/CallPage.tsx:53-62`).

For WebKitGTK, inject `window.ReactNativeWebView = {postMessage: m =>
webkit.messageHandlers.<name>.postMessage(m)}` before the page loads. Otherwise, open
`join_url` (or `/plugins/ru.corp.comms/join/{id}?autojoin=1`) in the system browser.

## Minimal desktop implementation checklist

1. On WS connect and plugin events: `GET /api/v4/plugins/webapp`, look for `ru.corp.comms`.
2. Seed live calls with `GET …/active`; on channel open, `GET …/channels/{id}/call`.
   Keep `channel_id → call_id` from `call_state`; use `my_call` (or `GET …/me/call`)
   for "I am in a call".
3. Start: `POST …/calls {channel_id, video}`. Join: `POST …/join/{call_id}`.
4. Use `join_url` within 2 min: redeem → `/v1/calls/{id}/join` (fresh 8..128-char key)
   → LiveKit with the returned `livekit_url`/`token`.
5. Ring on `incoming`. **Ack within 10 s** while `DISPATCHING`. Stop on
   `incoming_closed` or after 30 s. Accept/decline via `…/invitations/{id}/…`.
6. Render `custom_comms_call` posts from props (§4.2). The thread is the meeting chat.
7. Track call/connection state: `/v1/events` long poll (or SSE) plus re-reading
   `GET /v1/calls/{id}`. Leave via `/v1/connections/{connection_id}/leave`.
8. Optional parity with web: quiet join, screen-share audio, metadata avatars.

## Open questions (remaining)

All ten earlier questions are answered above from server code:
- `/v1/events` envelope: §3.4.
- `call_state` vs `my_call`: §4.1.
- Who rings: §2.
- `video`: §2.
- Error formats: §2, §3.2.
- `missed`: §4.2.
- Ring cancel: §4.3 and §5.
- Token lifetime: §3.3.
- Identity: §3.3.
- Web screen share: §7.
- `RecOptions`: §6.

Still open:
1. **Per-type `entity` shapes in events.** The envelope and the transition `data` are known.
   The full `entity` for each `type` would need a pass over every `Emit`/`Move` call site;
   clients that re-read snapshots do not need it.
2. **`ack` from a desktop vs the mobile app.** All MM clients ack and accept as the
   same `device_id` "mattermost-web" through the plugin. The server code does not show
   whether two clients acking the same invitation causes any visible difference; replays
   suggest it does not.
3. **LiveKit token refresh during long calls** depends on the LiveKit server (1.13
   refreshes tokens for connected participants). This was not verified against the
   deployment config.
4. **`WebURL` vs API origin.** The mobile app assumes the comms `/v1` API is served on
   the `WebURL` origin (a reverse proxy); this holds in production by observation, but
   it is deployment config, not code.
5. **`/active` cost.** `GET …/active` scans the plugin KV (up to 20×200 keys) on every
   call (`P/f11.go:106-130`). It is fine at startup, but don't poll it.
