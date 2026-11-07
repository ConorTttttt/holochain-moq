# holochain-moq-app-web: the local browser control surface (Stage 14H.1)

A local, loopback-only HTTP server that exposes the existing `holochain-moq-app`
application layer to a browser. It is a **presentation/control layer only**: every
Share/Watch operation is still owned and driven by `holochain_moq_app::App`, exactly as
it already is for the persistent CLI shell (`crates/app`). This crate adds no discovery,
expiry, admission, reconnect, session-ending, or transport logic of its own - see
"Architectural rules" below.

This is Stage 14H.1. Stage 14H.0's design report (see the project's own milestone
history) recommended this exact shape: a separate crate depending on `holochain-moq-app`
as a library, HTTP + polling (no WebSocket/SSE yet), a static vanilla-JS frontend, no
frontend framework.

## Relationship to `holochain-moq-app`

`crates/app` (the persistent CLI shell) is **unchanged** by this milestone and remains a
fully working, independent entry point. This crate is an *additional* way to drive the
same `App` API - it does not replace, wrap, or depend on the CLI binary in any way, and
the CLI does not know this crate exists. Both are thin callers of the same
`holochain_moq_app::App<D>`.

## Running it

```bash
cargo run -p holochain-moq-app-web -- --admin 127.0.0.1:<PORT>
```

against an already-running Holochain conductor with the `moq_directory` hApp installed
(see `crates/screenshare/README.md`/`crates/app/README.md` for how to stand one up - the
same conductors work for every entry point in this workspace). Startup mirrors
`crates/app`'s own CLI exactly: connect to the conductor, load the local agent identity,
reach `Ready` - then, additionally, start the local HTTP server.

Once running, open:

```text
http://127.0.0.1:4480/
```

in a browser.

### Configuration

| Flag | Default | Meaning |
|---|---|---|
| `--admin` | `127.0.0.1:4444` | the conductor's admin websocket (same as `holochain-moq-app`) |
| `--app-id` | `moq_directory` | the installed app id (same as `holochain-moq-app`) |
| `--bind` | `127.0.0.1` | address the local HTTP control surface listens on |
| `--port` | `4480` | port for the local HTTP control surface |

`4480` was chosen to avoid the ports this workspace already uses by convention (`4443`,
the default MoQ/QUIC share bind; `4444`, the default Holochain admin port).

## Local-only security model

**There is no authentication.** The default bind (`127.0.0.1`) means only processes on
the same machine can reach it - the same trust boundary the Holochain admin websocket
itself already relies on (`holochain-moq-app`/`holochain-moq-screenshare` both connect to
it the same way, with no credential beyond "can reach loopback").

The frontend is served from the **same origin** as the API (one HTTP server, one port),
so there is no cross-origin request to make and **no CORS configuration exists** - not
"CORS configured permissively," genuinely absent, since it is never needed.

**Binding `--bind` beyond `127.0.0.1` exposes an unauthenticated control interface.**
Anyone who can reach that address can start or stop sharing your screen and choose who is
allowed to watch a private session. The binary logs a warning at startup if `--bind` is
not loopback, but does not refuse to start - the decision is the operator's, made
explicitly (by typing a non-loopback address), never a default. Do not bind beyond
loopback without adding your own authentication/reverse-proxy layer in front of it; none
is provided here, and adding one is out of scope for this milestone.

## API

All endpoints are JSON. The browser polls `GET /api/state` at roughly 1-second intervals
only while a Share or Watch is active - see `src/static/app.js`. No WebSocket, no SSE
(deliberately deferred - see Stage 14H.0's report).

| Method & path | Behavior |
|---|---|
| `GET /` `/app.js` `/video.js` `/style.css` | the static frontend (compiled into the binary) |
| `GET /api/state` | the current `AppStateView` - readiness, identity, and (if active) the live Share or Watch view |
| `GET /api/sessions` | `App::discover_sessions()`'s result - every session that is joinable *right now* (Stage 14D unchanged) |
| `POST /api/share` | `{"source": "screen"\|"synthetic", "access": "public"\|"allowed", "allowed_agents": [...]}` - `access` defaults to public; an unknown value is a 400; `allowed_agents` are validated (see "Private sessions") |
| `POST /api/share/stop` | `request_stop()` then `finish_share()` - only responds once the existing Stage 14E drain has completed |
| `POST /api/watch` | `{"session_id": "..."}` - Stage 14H.5: starts the **decode-free** watch (`App::start_watch_encoded`); see "Browser video" below |
| `POST /api/watch/{watch_id}/stop` | needs `Authorization: Bearer <watch_token>`; then `request_stop_watch()` and `finish_watch()` - strictly local; never touches the publisher's session (Stage 14G.3 unchanged). Idempotent: repeating it (or a late one for an older watch) replays that watch's result and never touches a newer watch. See "Watch ownership". |
| `POST /api/watch/stop` | **removed** (answers `410 Gone`): it stopped whatever watch was active |
| `GET /api/watch/video` (WebSocket) | Stage 14H.5: the browser video carrier - see "Browser video" below |

Errors are `{"error": "<message>"}` with a normal 4xx/5xx status, using the same
`Display` text (`{:#}`) `holochain-moq-app`'s own CLI already prints for the identical
underlying error - never a Rust backtrace or panic payload.

## Architectural rules this crate follows

* **`App` stays authoritative.** Every handler is: extract the request, lock `App`, call
  exactly one existing `App` method (plus, for Share, constructing the same
  `ScreenSource`/`SyntheticVideoFrameSource` `crates/app`'s own CLI already constructs -
  see `src/config.rs`), map the result through `src/view.rs`, respond. No handler
  re-implements session discovery, expiry, admission/access control, reconnect, session
  ending, publisher cleanup, or transport lifecycle - all of that stays exactly where
  Stage 14D-14G already put it.
* **Serialization lives entirely in this crate.** `AppState`/`SharingState`/
  `WatchingState` in `holochain-moq-app` do **not** derive `Serialize`; `src/view.rs` maps
  `App`'s existing getters into this crate's own JSON view structs instead. `crates/app`
  remains independent of any web representation.
* **Long-running operations are not re-implemented here.** `App::start_share`/
  `start_watch` already spawn their own background task and return once it is launched
  (see `crates/app`'s own Stage 14G.4 ownership audit) - an HTTP handler calling either
  behaves exactly like the CLI's `run_share`/`run_watch` calling them, and returns in
  milliseconds. `POST /.../stop` blocks only for the existing, bounded Stage 14E drain -
  never task abortion.
* **Discovered API gap: no "access mode" on a live share.** `App` does not currently
  expose *which* `AccessConfig` a live share was started with (only `Sharer`'s
  constructors take it; nothing retains it for later reading). `ShareView` therefore does
  not report an access mode. This was left as a discovered gap rather than tracked
  server-side (which would risk becoming a second, possibly-stale copy of an
  access-relevant fact) - the browser's own submitted form value is the only place this
  is currently remembered, for display only, never for any authorization decision.

## Browser video (Stage 14H.5)

`POST /api/watch` starts a **decode-free** watch (`App::start_watch_encoded`, via the
new `holochain_moq_av_reconnect::EncodedFeed`/`EncodedVideoSink` seam in `crates/av-
reconnect` and `holochain_moq_screenshare::watch_encoded`): this process never decodes
the AV1 itself. It receives the same production `MediaObject`s the CLI's decoded path
does (same reconnect/epoch/admission machinery, same `MoQ`/QUIC transport, same AV1
bitstream, completely unmodified), queues them in a bounded, drop-oldest
`App::EncodedVideoQueue`, and the browser's own `VideoDecoder` (WebCodecs) does the
actual decoding, client-side.

```text
ScreenSource -> Av1Source -> MoqLivePublication -> QUIC/MoQ
        -> EncodedVideoHalf (EncodedFeed, no dav1d) -> EncodedVideoQueue (bounded,
           drop-oldest)
        -> GET /api/watch/video (WebSocket) -> static/video.js -> VideoDecoder -> canvas
```

The pre-14H.5 decoded path (`App::start_watch`, `ProgressSink<MemoryVideoSink>`) still
exists and is still exercised by this crate's own tests (it is what a non-browser,
CLI-style caller of this same HTTP surface would use), but `POST /api/watch` itself no
longer calls it - see `src/config.rs`'s `placeholder_watch_sink` (now `#[cfg(test)]`
only) for that history.

### Wire format

A small, explicit binary format - not the project's own 26-byte `MediaObject` wire
header sent blindly, not JSON/base64 - documented byte-for-byte in `src/wire.rs` and
mirrored in `src/static/video.js`:

* `Media` (opcode `0x01`): 1-byte opcode, 1-byte flags (bit 0 = keyframe/random-access),
  4-byte epoch (u32 BE), 8-byte `timestamp_ms` (u64 BE, `MediaObject::timestamp_ms`
  unchanged), then the raw AV1 access unit (`MediaObject::payload`, byte for byte).
* `EpochStarted` (opcode `0x02`): 1-byte opcode, 4-byte epoch (u32 BE) - sent once per
  reconnect, before any `Media` of the new epoch. **Never sent for the initial
  connection** - `EncodedVideoSink::epoch_started` (like the existing decoded
  `VideoSink::epoch_started`) only fires on a real reconnect; the initial epoch is
  always 0 by construction. `static/video.js` starts already in that state and accepts
  the first keyframe without waiting for an `EpochStarted` message.

### Backpressure

`App::EncodedVideoQueue` is bounded (64 objects, chosen for roughly two seconds of live
video at this project's default 30fps/keyframe-every-5 encoder settings) and
drop-oldest: a slow or absent browser can never make this process's memory grow without
bound, and can never stall the publisher or the underlying `ReconnectingSubscriber` -
the queue is the only thing a slow consumer can affect. `GET /api/watch/video`'s
forwarding loop (`src/video_ws.rs`) drains it every 20ms. Browser-side, `static/video.js`
keeps at most one not-yet-drawn decoded `VideoFrame` (`LatestFrameSlot`): a new decoded
frame arriving before the previous one was drawn closes the previous one immediately
(drop-oldest, prefer-newest) rather than building a frame queue - real-time-oriented, not
frame-complete-oriented.

### Browser decoder state machine

`static/video.js`'s `DecoderStateMachine` (unit-tested in `static/video.test.js`, run
with `node --test crates/app-web/src/static/video.test.js` - zero npm dependencies,
Node's own built-in test runner):

```text
waiting_initial_keyframe (epoch 0, no EpochStarted needed)
        │  first real keyframe
        ▼
      decoding  ───────────────┐
        │  EpochStarted(N)     │ VideoDecoder error
        ▼                      ▼
  waiting_keyframe  <──────────┘
        │  first keyframe of the new epoch
        ▼
      decoding
```

Deltas arriving while `waiting_initial_keyframe`/`waiting_keyframe`/after an error are
discarded, never fed to a freshly (re)configured decoder. Media tagged with an epoch
other than the current one (a stray old-epoch object still in flight when a reconnect
happens) is discarded. A real `VideoDecoder` error recovers by closing/reconfiguring the
decoder and waiting for the *next keyframe of the same epoch* - not a new keyframe-
request protocol: the encoder already emits keyframes periodically
(`Av1Config::keyframe_interval`) regardless of subscriber behaviour, so this self-heals.

### Verified browser results

**Chrome 153/Linux: real production video confirmed.** Manually verified via a real
Chrome browser (headed and automated via `puppeteer-core`/CDP) against this crate's
actual production HTTP/WebSocket surface, backed by two real, separate Holochain
sandbox conductors (`hc sandbox generate ... network mem` - see below) and a real
`screenshare` CLI publisher: `VideoDecoder` configured with `"av01.0.31M.08"` and no
`description` (the same proven-working string from `prototypes/14h4-webcodecs/`);
real, continuously-increasing decoded frame counts; the canvas showed real,
non-blank decoded pixel data (1280×720, matching the configured capture size); ending
the publisher's session (a graceful `SIGINT`, the same as Ctrl-C) drove the browser
through the existing auto-stop lifecycle back to `ready`, with the video WebSocket
opening and closing exactly once (proving no leaked background task). See "Manual
acceptance run" below for the exact procedure and what was and was not covered.

**Firefox is not a supported target for this milestone.** See
`prototypes/14h4-webcodecs/README.md` for the full, verified Chrome-success/
Firefox-failure results (Firefox 156 fails on the first real keyframe across every
codec string tried, including diagnostic substituted levels - the failure is **not**
established as caused by the encoder's `seq_level_idx = 31`, and the exact root cause
remains unresolved). Chromium/Chrome is this milestone's supported production browser
target; Firefox support is an explicit, documented limitation, not a goal.

### Manual acceptance run

Performed once, in an environment where `hc sandbox generate`'s default (internet
bootstrap/relay) network configuration was not tested this round, but **`network mem`
worked**: two independent `holochain` conductor processes, two real agent identities,
real cross-conductor DHT discovery (`GET /api/sessions` on the viewer's conductor
listed the session announced from the publisher's conductor) - this is the real
`HolochainPeerDirectory` path, not `InMemoryPeerDirectory`.

```bash
# Two real conductors, network mem (works without external bootstrap connectivity):
hc sandbox generate dna/workdir/moq_directory.happ -a moq_directory -n 2 network mem
hc sandbox run --all -p <PORT_A> -p <PORT_B>

# Publisher (real screenshare CLI, real AV1/QUIC):
./target/release/screenshare --admin 127.0.0.1:<ADMIN_A> share --source synthetic

# Viewer (this crate, production binary):
./target/release/holochain-moq-app-web --admin 127.0.0.1:<ADMIN_B> --app-id moq_directory
# open http://127.0.0.1:4480/ in Chrome, click Watch, select the session
```

Confirmed: real decoded video appeared on the canvas and kept updating; dimensions
(1280×720) were correctly reported; ending the publisher session (Ctrl-C) drove the
browser to `ready` with the video WebSocket closing cleanly.

**Reconnect was not covered by that 14H.5 run** - see "Reconnect and recovery
(Stage 15.1)" below, which covers it (and corrects this section's earlier claim that the
`screenshare` CLI had no way to produce a same-session publisher restart: it does, see
below).

### Reconnect and recovery (Stage 15.1)

Three separate kinds of proof, deliberately not conflated:

1. **Browser state machine (deterministic, no browser).** `static/video.test.js`, the
   `15.1-` tests: driving the real `VideoWatch` with fake socket/decoder factories
   through epoch 0 media -> `EpochStarted(1)` -> premature delta -> keyframe -> deltas;
   decoder-error recovery; stale-frame discarding; exactly one socket across repeated
   reconnects; teardown after a reconnect; the stall label.
2. **Rust reconnect + browser-facing wire (real QUIC, no browser).** `src/tests.rs`
   `watch_video_websocket_survives_a_publisher_outage_and_ends_cleanly`: a real publisher
   is killed without ending its session, stays down 1.5s, and returns at the same address;
   over the real `/api/watch/video` WebSocket it checks exactly one `EpochStarted(1)`,
   strictly before any epoch-1 media, with no epoch-0 media after it, then a keyframe;
   `/api/state` goes `reconnecting` -> `live`; then the session ends and the WebSocket
   closes and the watch reports finished. Underneath it are the unchanged
   `crates/av-reconnect` / `crates/screenshare` reconnect tests.
3. **Live, real Chrome, two real conductors, real publisher processes.** See below.

**How to produce a same-session publisher reconnect with the existing CLI** (this
corrects an earlier claim): a session stays `Live` in the directory until it is ended, and
a viewer's reconnect re-discovers the publisher's *endpoint*, not the session's process.
So `kill -9` the `screenshare ... share` process (no clean end), then start another
`screenshare ... share` with the same `--admin` (same default bind `127.0.0.1:4443`): it
re-announces the endpoint under the same agent and the viewer reconnects to it. The
replacement publisher also creates its own new session; that is harmless.

**What the live run showed** (Chrome 153; `kill -9` publisher, 1.5s later a replacement):

* The watch stayed on the Watch page; the canvas kept the last drawn frame; the video
  status went `decoding` -> `stalled (Ns without frames)` -> `decoding` at epoch 1 with
  frames resuming at 1280x720; exactly one WebSocket for the whole watch, closed once
  when the session ended; ending the replacement's session took the page to `ready`.
* **Loss detection is slow: ~14s.** A peer that dies without closing its QUIC connection
  is only noticed at the transport idle timeout (measured 13.8s). For that whole time
  `/api/state` truthfully reports `live` - the transport does not yet know - while no
  frames arrive. The Rust reconnect machinery is unchanged; this is a transport timing
  characteristic, and shortening it (keep-alive / idle-timeout tuning) is a transport
  change out of this stage's scope. So the video status label also reflects actual frame
  flow: `decoding` becomes `stalled (Ns without frames)` after 2s with no decoded frame.
  It is display-only (`describeVideoState` in `video.js`), drives nothing, and is not a
  second state machine.
* Each reconnect attempt then took ~10s (dial timeout); with the default 3 attempts, a
  publisher that never returns produces `Failed` at ~47s after the kill. Observed in
  Chrome: `live` -> `stalled` -> `reconnecting (attempt 1)` -> `(attempt 2)` ->
  `(attempt 3)` -> `ready` with the real error text, video `idle`, WebSocket closed once.
* The 1s `/api/state` poll can miss a *short* `reconnecting` state (a fast recovery
  finishes inside one poll interval); in the successful live run above the page never
  displayed `reconnecting`, only `stalled` then `decoding` at epoch 1. It is displayed
  whenever it lasts longer than a poll, as the failure run shows.
* `RetryPolicy::default()` was left as is: an outage of 12s recovered in the real-QUIC
  test, because each attempt itself blocks for the dial timeout.

**Two defects found and fixed by this stage's testing:**

* `EncodedVideoQueue` drop-oldest could evict an `EpochStarted` marker under overflow,
  after which a browser would discard every later object as a stale epoch, forever. The
  queue now evicts the oldest *media item* instead (`encoded_video_queue_never_evicts_an_epoch_marker`).
* `VideoWatch` did not ignore late `output`/`error` callbacks from a decoder already
  closed for a new epoch, and did not clear an undrawn decoded frame on reset - a stale
  old-epoch frame could be drawn after the new epoch began, and an old decoder's error
  could reset the new one. A decoder-generation guard and a frame-slot clear on every
  reset fix both.

**Still not proven:** a browser observing a reconnect on the *Holochain-backed* path with
a transport outage long enough for the 1s poll to display `reconnecting` *and* then
recover (only the failing variant of that was seen live); private-session browser
reconnect (still unsupported, see below); Firefox.

### Browser decode latency (Stage 17C)

`video.js` configures its `VideoDecoder` with `optimizeForLatency: true` (one place: the decoder
factory in `VideoWatch`). Without it Chrome holds about four frames before emitting output. It is
only a hint: if a browser's `configure()` rejects it the decoder is configured the old way, and the
capability check (`checkVideoSupport`) still asks about the codec alone, so nothing about which
browsers can play changed. No Rust, encoder, transport, wire-format or session code changed.

Measured in **Chrome 153 / Linux** only (headless, via `puppeteer-core`):

* Probe (the real AV1 sample in `prototypes/14h4-webcodecs/sample`, 3 runs per row): median
  receive-to-output 401 ms -> 0.4 ms at 100 ms frame cadence and 1601 ms -> 0.5 ms at 400 ms;
  outputs before any `flush()` 6/10 -> 10/10; 0 decoder errors either way.
* Real application, two conductors, live synthetic 1280x720 (4 runs each): `recv_to_decoded`
  722-803 ms -> 2.1-2.3 ms; first displayed frame 71-682 ms (very variable) -> 58-69 ms;
  decoded-to-displayed unchanged (~10-12 ms); 0 decoder errors.
* Real application, prerecorded real screencast to EOF (1 run each, 15 fps, 1280x678):
  `recv_to_decoded` 264.5 ms -> 1.2 ms. At end of stream the old configuration left the last 4
  received frames inside the decoder (362 of 366 decoded); the new one decoded all 366.
* Cost: with frames no longer held back, 1-4 of ~100 decoded frames were replaced in the
  latest-frame display slot before being drawn (`dropped_before_display`; 0 before). This is the
  existing display-slot behaviour, not new dropping code.
* Reconnect (live, `kill -9` publisher, replacement 1.5 s later; new configuration only):
  stalled, then epoch 1 decoding at 1280x720 at ~15-18 s, a second decoder configured the same
  way, 0 decoder errors, one WebSocket for the whole watch; publisher Stop ended the watch
  normally.

**Not measured:** other browsers; a live *screen* source (capture failed on this Wayland session:
`ximagesrc` could not link); capture-to-display latency (the browser has no capture timestamp);
private-session flows with the flag; the baseline under reconnect. The "before" runs use the same
build with the flag removed at `configure()` by a CDP hook, not an older build.

### Private sessions (Stage 15.2)

Browser Watch now works for private (allowed-agents) sessions, reusing the existing Stage
14F machinery unchanged. **Authorization is publisher-side and cryptographic; nothing in
this crate or the browser decides it.**

```text
Browser -> POST /api/watch {session_id}          (no identity anywhere in the request)
  -> App::start_watch_encoded                    (adds this process's own admission signer)
  -> screenshare::watch_encoded                  (signs only if the session record is private)
  -> ReconnectingSubscriber -> QUIC/MoQ connect
       viewer:    signs SHA-256(connection id) bound to the session id, sends the proof on
                  the reverse control track       (HolochainPeerDirectory::sign_admission_proof
                                                  -> coordinator zome -> real key in lair)
       publisher: live::admit verifies signature + allowlist (or publisher itself)
                  |-- ok:     publish_namespace -> media -> EncodedVideoQueue -> WebSocket
                  `-- refused: close the session with ADMISSION_REFUSED_CODE; media is never
                              published to it (this is BEFORE publish_namespace)
```

* **Identity.** `main.rs` builds the signer from the app's own conductor connection, exactly
  as `screenshare`'s CLI `watch` does. The signer can only sign as that conductor's agent.
  There is no agent parameter on any route: a browser cannot choose or claim an identity
  (regression-tested with a viewer that sends another agent's key in the body, a query
  parameter and a header).
* **The WebSocket is not an authority.** `/api/watch/video` only drains
  `EncodedVideoQueue`, which only an already-admitted Rust watch can fill. For a refused
  viewer the queue receives nothing (`queue.len() == 0 && dropped() == 0`, asserted) and the
  socket delivers zero binary frames (also confirmed in real Chrome via CDP).
* **Refusal is now definitive.** Before 15.2 a refusal reached the viewer as a generic
  `connection error: closed`, identical to a crashed publisher, so the reconnect layer
  retried it. The publisher now ends a *refused* session with a dedicated close code
  (after failing closed; the 14F invariant is untouched) and the viewer reports
  `AdmissionRefused`, deliberately not `TransportLost`. The existing reconnect logic already
  treats any non-`TransportLost` error as terminal, so no reconnect change was needed:
  `WatchError::Unauthorized` -> `WatchingState::Unauthorized`, one proof attempt, no retry.
  `SessionExpired` (14D) and ended (14E) are untouched and tested not to be reclassified.
* **Share side.** The Share form's Public / Allowed agents choice maps to the existing
  `AccessConfig`. Keys are validated (`AccessConfig::allowed`: well-formed agent key, no
  duplicates, blank entries ignored; the offending value is named). An unrecognized `access`
  value is now a 400 - it used to silently become *public*. The live share shows its access
  mode and allowlist, read from the durable session record (this closes the earlier
  "no access mode on a live share" gap). The session list marks private sessions; a private
  session is still *listed* to everyone - discovery is not authorization.
* `POST /api/watch/{watch_id}/stop` returns `last_error_kind` (`"unauthorized"` / `"expired"`), a typed
  classification, so the UI shows a specific message without parsing prose.

**Limitations (found, not hidden):**

* **A proof is bound to the session id, and a restarted publisher is a new session.**
  `Sharer` (and so the CLI and app-web Share) mints a fresh session id per start. A viewer of
  the old session that reconnects to the restarted publisher is therefore *definitively
  refused* (`Unauthorized`), not silently rejoined; it must pick the new session. A
  same-session private reconnect (the transport drops, the same publisher comes back) is
  proven at the Rust level, but no existing tool produces one live, so it was not exercised
  in Chrome.
* **Transient `live` before a refusal.** The transport connects before admission completes,
  so a refused viewer's state can read `live` for a few milliseconds (the refusal arrives
  ~6ms after joining) before becoming `Unauthorized`. No media flows in that window; with a
  1s poll it is rarely visible. It is an honesty gap in the state label, not a media leak.
* **Allowlist changes** are not exposed through `App`/app-web (`Sharer::set_allowed` exists
  and is covered by 14F, but the running share is owned by a background task). Not added,
  per scope: future work. The 14F rule (existing connections are not cut off; later
  connections use the new list) is unchanged.
* An unauthorized peer can still open a transient transport session before it is refused
  (already documented in `crates/realtime/src/admission.rs`); it cannot obtain media.

## Session management and UX (Stage 15.3)

No new architecture: the same `App` lifecycle, one polling loop, no framework. This stage
makes the existing lifecycle legible and robust under repeated use.

**Lifecycle, as the UI now presents it** (`/api/state` stays the only authority; nothing below
is a second state machine):

```text
Share:  Ready -> [title, source, Public/Private(+allowed)] -> Live (title, access, allowed,
        "joinable for") -> End session for everyone (asks first, 17H) -> drains, session Ended -> Ready
Watch:  Ready -> Watch list (self-refreshing; title, Public/Private, publisher, source,
        time left) -> Connecting -> Live <-> Reconnecting -> Leave | ended | error -> Ready
```

* **Leave vs End session.** *Leave* (viewer) is `request_stop_watch` + `finish_watch`: it ends
  only that viewer and never the publisher's session or another viewer (tested: the publisher
  stays `Live`, its session record stays `Live`, and a second viewer keeps receiving).
  *End session for everyone* (publisher; called *Stop sharing* before Stage 17H) ends the session
  (14E drain first) for everyone, who then get
  "The publisher ended the session." Distinguished server-side: `POST /api/watch/{watch_id}/stop`
  returns `last_notice` = `session_ended` | `left`.
* **Error kinds** (`last_error_kind`, each one an *existing* `WatchError` variant, none
  invented): `unauthorized`, `expired`, `ended`, `not_found`, `publisher_unavailable`,
  `connection_failed`. Each has its own wording (`static/ui.js`); anything untyped keeps the
  server's own text rather than becoming a generic message. Security-sensitive wording carries
  no cryptographic or transport detail (tested). "No sessions" is an explicit empty state;
  malformed requests and invalid allowlist entries are 400s naming the problem.
* **Title.** The Share form has an optional title stored in the session's existing `title`
  field (trimmed, blank = none, 200-byte limit from the protocol, control characters
  rejected - validated before any state changes).
* **Discovery stays fresh.** While the Watch list is showing it refreshes every 3s and counts
  time-left down locally, so ended/expired sessions disappear and new ones appear without a
  click. The server's `joinable_sessions` still decides what is offered (14D/14E untouched).
* **Removed** the legacy `Pictures / Epoch / Resolution` Watch rows: they came from the decoded
  path unused since 14H.5 and always read `0 / 0 / 0x0` beside the real `Video size`.
* **No stale video.** Leaving clears the canvas (a reconnect deliberately does not - 15.1), and
  the video rows, note and selected-session info are reset.

**Robustness fixes found by this stage's testing** (each with a regression test):

1. **A disconnecting client could wedge the app.** hyper drops a handler when its client goes
   away; a handler cancelled between two `App` transitions left the server stuck (found live:
   `Sharing(ChoosingSource)` with nothing able to stop it, after Chrome was killed mid-request).
   All four state-changing handlers now run their body in a detached task (`routes::detached`).
   Live-verified on the release binary (abort a `/stop` mid-request: it still completes and
   releases UDP 4443). The Share-stop test fails without the fix; the Leave variant does not
   discriminate on its own (it passes unfixed in-process) and is kept as coverage only.
2. **A video WebSocket left open after Leave was never closed server-side** (`watch_finished()`
   is false forever once `finish_watch` clears the watch). It now also ends when its queue is
   no longer the app's active one.
3. **An abandoned page left a Watch running forever.** A watchdog leaves a *Watch* (never a
   Share) when no page has polled for 30s and no video socket is open. A hidden tab is safe:
   its open socket counts.
4. **One video consumer.** A newer video socket (a refreshed page) supersedes the older one, so
   the queue never has two readers.

**Limitations / future work:**

* A one-off stall was seen once in the live repeated-cycle run (cycle 2's watch never
  connected: no dial appeared in Bob's log, no error shown); it did **not** reproduce in 13
  further cycles on the final build, its cause is unknown, and it is not claimed fixed.
* Listing several sessions from one publisher uses the directory's endpoint list as before;
  a publisher restarted on a *different* port in the in-memory directory leaves its dead
  endpoint listed first (a test-directory artifact; a real deployment reuses the fixed port).
* The share lifetime ("joinable for") is the session lease, not an elapsed timer. Viewer
  counts remain `LiveStats::session_count()`; nothing new was added.
* Chrome throttles timers in background tabs, so a hidden tab's UI can lag; its video
  socket stays open and keeps it from being treated as abandoned.

## Interface (Stage 17H)

A presentation-only redesign of the browser page. The backend, the JSON API, the video WebSocket, the
decode pipeline and every lifecycle rule are unchanged; `/api/state` is still the only authority and
`app.js` still renders from it without a client-side state machine. Plain HTML, CSS and JavaScript in
`src/static/`: no framework, no new dependency, no icon library (one inline SVG sprite).

**Layout.** A top bar (name, the Share and Watch tabs, who you are), one status strip that is the
page's live region, then the current view:

* *Home* (neither tab chosen): two plain choices.
* *Share*: source (Screen first; the moving **test pattern** is present but visibly second), optional
  title, who can watch (Public, or Allowed agents only, with the agent-key guidance). While live it
  shows title, state, source, access, time left, viewers and objects, and **End session for
  everyone**, which is deliberately unlike the viewer's Leave and asks first (an inline confirmation
  whose safe choice, *Keep sharing*, has the focus; Escape backs out; what ends is still the one
  existing `POST /api/share/stop`).
* *Watch*: the list of live sessions (title, Public/Private with an icon, source, publisher, how long it
  stays **listed**), or, once watching, the video. On windows 1100 px and wider the video takes the
  left column and a slim column beside it holds the title, publisher, access, state, **Leave** (with a
  note that it stops only your viewing) and the playback details; below that the parts stack and the
  video is sized so its controls stay in view.

**States.** The headline (`ui.js` `describeStatus`, unchanged) always has an icon, words and a tone;
state is never colour alone. `Starting`, `Ready`, `Sharing`, `Discovering sessions`, `Connecting`,
`Live`, `Reconnecting (attempt n)`, `Ended`, `Expired`, `Access denied`, `Browser unsupported`,
`Playback failed` and `Error` each read differently, and the reason for an outcome (an ended session,
a refusal, an unreachable app) stays until the next action. An unreachable app offers **Try again**;
after any outcome the session list is the way back.

**The video stage** keeps the fixed 16:9 box and the existing decoder. **Fit** (default, whole picture)
and **Fill** (crops to cover) are CSS only and remembered in `localStorage` when it is available;
**Fullscreen** uses the browser Fullscreen API on the whole stage, so the overlay stays visible. A last
frame that is no longer live (reconnecting, or live with no frames arriving) is dimmed and labelled
**Not live - showing the last frame**; an ended, failed or unsupported area shows no picture at all, so
nothing frozen can be mistaken for the stream. While a watch is still connecting, the note says it is
taking a while and that Leave is available (no time limit is promised).

**Accessibility.** Landmarks (`header`, `nav`, `main`), a skip link, one `h1`, labelled controls,
buttons named for what they act on ("Watch *title*"), decorative icons hidden, a polite live region for
status and alerts for errors, a visible 3 px keyboard focus ring, focus moved to the new view when a
session starts and to the page when it ends (never left on a control that disappeared), `aria-current`
on the current tab, `aria-pressed` on Fit/Fill/Fullscreen, reduced motion respected (spinners stop),
forced-colors rules present (not visually verified in a high-contrast mode), touch targets of at least 44 px on small screens. Colour pairs are checked against
WCAG AA by a test that reads the stylesheet's own tokens (text 4.5:1, control boundaries and focus 3:1).

**Deliberately not here (needs backend or protocol changes, or would be invented):**

* **No video-file source in the page.** `POST /api/share` accepts `screen` and `synthetic` only; the
  file source exists in the `screenshare` CLI, not in this API.
* **Ended, expired and unavailable sessions cannot be listed.** `/api/sessions` returns live sessions
  only, so those states appear only as the *outcome* of a watch. The list also cannot tell whether a
  publisher is still reachable: a publisher that is killed without a goodbye stays listed until its
  session lease runs out, and watching it ends, after a wait, with a plain error and a return to the
  list. The list therefore says **listed for** (the lease), not "joinable".
* Viewer counts, if shown, are the existing `viewers` value of the share view; nothing was added.

**Tests for this stage** (plain Node, no browser, no dependency):

```bash
node --test crates/app-web/src/static/        # ui.test.js, style.test.js, markup.test.js, video.test.js
```

* `ui.test.js` - the wording helpers (icons, share/watch state words, expired rows, the not-live and
  hidden-picture rules, the connecting hint).
* `style.test.js` - WCAG AA for every colour pair in light and dark, and the accessibility layers.
* `markup.test.js` - unique ids, **every id `app.js` looks up exists in the page**, every label,
  `aria-*` reference and skip link resolves, every button is named, no inline style or handler,
  Leave and End are worded apart, the stage has its controls, and `app.js` touches neither WebCodecs
  nor a socket.
* `tests.rs` `the_served_ui_keeps_its_accessibility_and_state_structure` pins the same structure in the
  served page and stylesheet.

**Browser acceptance** is a real Chrome against the real viewer and a real publisher, not part of
`cargo test` (it needs two conductors): `crates/screenshare/scripts/browser/` has `stack.sh` (the
two-conductor recipe above), `ui-acceptance.js` (about 90 checks over landing, empty state, a live
watch, Fit/Fill/Fullscreen, reconnect, publisher end, Leave, a private session refused, a listed
session whose publisher is gone, sharing and the two-step end, five viewport widths, dark, reduced
motion, an unsupported browser, keyboard order and focus, plus axe-core scans of each state) and
`smoke.js` (the Stage 17F/17G smoke test). Needs `npm i puppeteer-core axe-core` somewhere on
`NODE_PATH`.

## Reliability and observability (Stage 15.4)

Goal: if a Share/Watch ever stalls again, the last confirmed boundary, how long it has been
there, and what the app was doing are recorded - without changing behaviour.

**Operation correlation.** Every Share and Watch invocation gets an `OpId` (`W0007-a3f1`:
kind, per-process counter, per-run salt). Server-generated, derived from nothing secret, never
on the network protocol, never persisted. A session id stays the session's identity; the op id
only traces one invocation. `crates/app/src/lifecycle.rs` holds a bounded recorder (32
operations, 64 events each) and emits `tracing` events (target `lifecycle`) carrying the op; the
Watch/Share tasks run under a `tracing` span with `op=`, so logs from `screenshare`, `node` and
`av-reconnect` correlate with no plumbing. It *describes* the existing state machine; it drives
nothing and is not a second one.

**Events** (`Lifecycle::event`, names are static):

```text
HTTP      <kind>.requested (before the app lock is awaited) -> http.task_spawned ->
          http.lock_acquired "waited Nms" -> http.task_completed | http.stop_received/_completed
Share     share.accepted -> share.setup_started -> share.session_created -> share.live ->
          share.stop_requested -> share.ending -> share.ended | share.failed
Watch     watch.accepted -> watch.task_spawned -> watch.discovery_started ->
          watch.session_lookup/_found -> watch.endpoints_lookup/_found -> watch.session_selected ->
          watch.connect_started -> watch.admission_check/_ok -> watch.connected -> watch.live
          (first media queued) -> watch.reconnecting -> watch.reconnected / watch.epoch_started ->
          watch.leave_requested -> watch.ended | watch.expired | watch.unauthorized |
          watch.failed | watch.abandoned
Video WS  ws.requested -> ws.accepted -> ws.active -> ws.superseded -> ws.first_frame ->
          ws.epoch_start_sent -> ws.closed (reason: client_closed | client_disconnected |
          send_failed | superseded | watch_ended | queue_inactive)
```

`watch.session_lookup ... watch.endpoints_found` and `watch.admission_check/_ok` are the
directory reads that run *before* the first dial (five of them per Watch: two in setup, two more
in the per-attempt admission check, one more inside the connect). They are unbounded awaits, so a
slow or hung read there looks exactly like "no dial, no error"; each is now bracketed. The finer
steps of one connection attempt (admission check, endpoint discovery + dial) are logged at
`debug` by `holochain_moq_node` under the same span. No retry policy or timeout was changed.

**Phase and phase age.** The phase is the last event; its age is measured on a monotonic clock
(`Instant`, injectable for tests), never on protocol timestamps. Observability only.

**Diagnostics.** `GET /api/state` carries a compact `diag {op, phase, phase_age_ms,
video_socket}`. `GET /api/diagnostics` (loopback development aid) returns the app state, page
liveness and the recent operations with their event timelines. Failures log one bounded WARN line
(`snapshot=op=... phase=... phase_age_ms=... recent=[...]`) plus the app state, connection state
and liveness. The recorder only ever receives static event names and short, caller-chosen details:
no keys, signatures, certificates, media, request bodies or raw error text (a test signs a real
admission proof and asserts none of its bytes, in any encoding, appear in the diagnostics).

**Failure classification** (`classify_watch_error`, one existing typed error each): `unauthorized`,
`expired`, `ended`, `not_found`, `publisher_unavailable`, `discovery_failed`, `reconnect_exhausted`,
`connection_failed`; plus `request_rejected` (a 409), `invalid_input` (a 400), `abandoned` (the
watchdog) and `internal_task_failure`. A watch that ends because the session had already ended is
an ordinary outcome, not a failure.

**Watchdog.** Unchanged (30s idle, 5s tick; an open video socket prevents abandonment). Its
decision is now recorded: `no page contact for 32381ms (limit 30000ms), video sockets open: 0,
watch state: Some(Connected)`.

### Fixed (15.4.1): a public viewer's session was recycled every 30 seconds

Found by the 15.4 traces: a healthy public Watch showed `watch.reconnecting -> watch.reconnected`
at 30.0s, 60.0s, ... and the publisher logged `PublishNamespace response timed out` exactly 30s
after each accept. Cause: the publisher announces its namespace to the viewer (`PUBLISH_NAMESPACE`)
and `moq-transport` gives it 30s to be answered (`REQUEST_OK`). The private viewer path always
answered (it accepts the announcement after presenting its proof); the public path never did.
Fix, in `crates/realtime/src/subscribe.rs` only: the public viewer now accepts the announcement
concurrently with `subscribe` and holds the accepted handle for the subscription's life. The 30s
timeout is unchanged (an unanswered announcement still times out), and admission is untouched.
Regressions: `crates/realtime/tests/namespace_ack.rs` (real QUIC, 35s, public + private + refused
viewer) and this crate's `a_healthy_public_watch_holds_one_connection_past_30_seconds` /
`a_healthy_private_watch_holds_one_connection_past_30_seconds` (35s each, 0 `watch.reconnecting`).
The 15.3 unexplained stall is not claimed fixed.

### Soak evidence (Stage 15.5, batch 3)

`cargo test -p holochain-moq-app-web soak:: -- --test-threads=1 --nocapture` prints one `SOAK` line
per scenario: 30 public watch/leave cycles (settled task count flat), 10 publisher-Stop cycles on
the production route (viewer sees `session_ended` each time), 4 real reconnect outages on one
Watch (epochs exactly 0..=4, one socket, one watch operation), permanent publisher loss (terminal
`reconnect_exhausted` after 3 connect attempts, ~30s by the existing policy, no retry loop after),
12 private rounds (refused viewer gets zero media; authorized viewer undisturbed), and browser
socket churn (Watch survives every close; unread queue stays at its 64 bound).

Observed, deliberately not changed: (1) an agent's older endpoint advertisements never expire (no
TTL), so after several shares on *different* addresses a viewer pays ~10s of connect timeout per
dead one - the product always shares on 127.0.0.1:4443 (same address, replaced), so this does not
occur today, and fixing it means changing Stage 13 freshness; (2) UDP 4443 is released ~90ms after
Stop returns when a viewer had been attached (QUIC endpoint draining inside the library).

## Testing

`src/tests.rs` drives the `axum::Router` in-process via `tower::ServiceExt::oneshot` (no
real TCP listener, no browser) against `App<InMemoryPeerDirectory>` - the same
conductor-free pattern `crates/app`'s own test suite already uses. Run:

```bash
cargo test -p holochain-moq-app-web
```

One test (`post_share_via_http_reaches_live_and_stop_drains_it`) exercises
`POST /api/share` itself end-to-end and so binds the same fixed address
`crates/app`'s own `main.rs` uses for Share (`127.0.0.1:4443`) - matching what a real
deployment actually does. Every other test reaches a `Sharing`/`Watching` precondition by
calling `App::start_share`/`start_watch` directly with an ephemeral port, to avoid
needing that fixed address.

Stage 14H.5 adds two tests that, unlike every other test in this suite, run a real
`axum::serve` over a real TCP listener and a real WebSocket client
(`tokio-tungstenite`) - a `tower::oneshot` request/response cannot drive a protocol
upgrade: `watch_video_websocket_delivers_a_real_epoch_and_keyframe` (the whole chain,
real AV1 bytes, real wire framing) and
`watch_video_websocket_completes_the_close_handshake_when_the_client_closes_first` (a
regression test for a real bug the manual Chrome run above caught - see
`src/video_ws.rs`'s `forward` function docs).

The browser-side wire parsing and epoch/keyframe state machine (`src/static/video.js`)
have their own zero-dependency test file, run with plain Node (18+; no npm install, no
framework):

```bash
node --test crates/app-web/src/static/video.test.js
```

Stage 17H adds `ui.test.js` cases, `style.test.js` and `markup.test.js` beside it; run them all with
`node --test crates/app-web/src/static/`.

This proves the *logic* only - it does not touch a real browser. It does not, on its
own, demonstrate that browser video works; see "Manual acceptance run" above for that.


## Watch ownership

The control surface is loopback-only, but loopback is not "one person": a second tab, a stale
page, or a restarted dev proxy can all reach it. So a Watch has an **owner**, and only the
owner can stop it or keep it alive.

* `POST /api/watch` (on success) returns the watch's `watch.watch_id` (a UUID; not secret; also
  in `GET /api/state`) and, **once**, a top-level `watch_token` (256 random bits, hex). `session_id`
  is public and is never authority. Only a SHA-256 of the token is kept; it is redacted in
  `Debug` and never logged. `GET /api/state` never contains it.
* `POST /api/watch/{watch_id}/stop` + `Authorization: Bearer <watch_token>`:

  | situation | answer |
  |---|---|
  | no / malformed credentials | `401` |
  | unknown watch id | `404` |
  | known watch, wrong token (incl. a `session_id`, or another watch's token) | `403`, nothing stopped |
  | the active watch, right token | `200`, watch stopped (same body as before: `last_notice` / `last_error_kind`) |
  | a watch that already ended, right token | `200`, the *same* body its ending returned (idempotent) - a late or repeated stop can never reach a newer watch |

  The last 16 ended watches are remembered for the replay; the grant also records the watch's
  `session_id` and a stop refuses (`409`) if the app's active watch is a different session.
* **Attendance belongs to the owner.** The abandonment watchdog (30 s) now counts only the
  owner's contact: `GET /api/state` presenting `Authorization: Bearer <watch_token>`, or a video
  socket presenting the token as a WebSocket sub-protocol (`Sec-WebSocket-Protocol:
  moq-watch.v1, moq-watch-token.<token>`; browsers cannot set `Authorization` on a socket and a
  URL token would be logged). An anonymous page that merely keeps polling - a stale tab, a
  reloaded page that lost its token, an old proxy's leftovers - no longer keeps an orphaned watch
  alive. Anonymous video sockets are still served; they just do not count as attending. A watch
  not started over HTTP (in-process use) has no owner and keeps the earlier any-contact rule.
* Ownership lives in this process, not in a proxy: a dev proxy that restarts forgets nothing, and
  the page that started the watch can still stop it with the token it holds. If the owner itself
  disappears (browser killed, token lost), the watchdog ends the watch after its idle limit.
* Clients keep the token in memory only and should send a best-effort
  `fetch(..., { keepalive: true })` stop on `pagehide`; the watchdog remains the fallback.
