// Stage 14H.5: browser video over `/api/watch/video`.
//
// Split deliberately into two layers so the state-machine/wire-parsing logic (the part
// with real correctness rules worth getting wrong) can be unit-tested with plain Node
// (`node --test crates/app-web/src/static/video.test.js`, no framework, no browser, no
// build step - see that file), while the browser-only glue (WebSocket, VideoDecoder,
// canvas, requestAnimationFrame) stays a thin, untested-by-necessity wrapper around it:
//
//   parseMessage      - decodes one binary WebSocket message per the wire format
//                        `crates/app-web/src/wire.rs` documents (opcodes 0x01/0x02).
//                        Pure function; the single source of truth for the format on
//                        this side, mirrored from - not reinventing - the Rust one.
//   LatestFrameSlot   - "keep only the newest not-yet-drawn VideoFrame, closing the
//                        previous one" - the bounded, drop-oldest, real-time-oriented
//                        policy for decoded output the milestone asks for.
//   DecoderStateMachine - the epoch/keyframe state machine: WaitingForInitialKeyframe ->
//                        Decoding, EpochStarted -> WaitingForKeyframe -> Decoding, old-
//                        epoch and premature-delta discarding, decoder-error recovery.
//                        Takes an injected `createDecoder()` factory so it never touches
//                        `VideoDecoder`/`EncodedVideoChunk` directly - a test can inject
//                        a fake and observe exactly what it was told to decode.
//   VideoWatch        - the actual browser glue: opens the real WebSocket and the real
//                        VideoDecoder (via `DecoderStateMachine`), renders to a canvas,
//                        and exposes `start()`/`stop()` for `app.js` to call from the
//                        existing Watch lifecycle. Its recovery wiring - one socket for
//                        the whole watch, a decoder-generation guard so a closed
//                        decoder's late output/error is ignored, the frame slot cleared
//                        on every decoder reset - is unit-tested (video.test.js, the
//                        "15.1-" tests) by injecting fake socket/decoder factories.
//                        Only the raw browser APIs themselves (real VideoDecoder, canvas,
//                        requestAnimationFrame) are exercised by the manual Chrome runs.
//
// Critical epoch detail (see `crates/av-reconnect`: `EncodedVideoSink::epoch_started` is
// only called on a *reconnect* - never for the initial connection): the initial epoch is
// always 0 by construction, so `DecoderStateMachine` starts already in
// `waiting_initial_keyframe` at epoch 0 and accepts the very first keyframe without ever
// seeing an `EpochStarted` message. `EpochStarted` only ever arrives after that, on a
// real reconnect. Nothing here waits for one before decoding the first video.
(function (root, factory) {
  if (typeof module !== "undefined" && module.exports) {
    module.exports = factory();
  } else {
    root.HolochainMoqVideo = factory();
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  // Mirrors crates/app-web/src/wire.rs exactly - see its docs for the byte layout.
  const OP_MEDIA = 0x01;
  const OP_EPOCH_STARTED = 0x02;
  const FLAG_RANDOM_ACCESS = 0x01;
  const MEDIA_HEADER_LEN = 14;
  const EPOCH_STARTED_LEN = 5;
  // Frames of per-frame timing kept for the measurement join (bounded).
  const MEDIA_STATS_RING = 512;

  // Chrome 153's proven working codec string (see the crate README's "Browser video"
  // section and `prototypes/14h4-webcodecs/README.md`): `configure()` succeeds with NO
  // `description`, real AV1 access units decode. Not proven in Firefox - see the same
  // docs; Chromium/Chrome is this milestone's supported browser target.
  const DEFAULT_CODEC = "av01.0.31M.08";

  /// Decode one binary WebSocket message into `{type: "epoch_started", epoch}` or
  /// `{type: "media", epoch, randomAccess, timestampMs, payload}` (`payload` a
  /// `Uint8Array` view over the *same* buffer - not copied - the raw AV1 access unit,
  /// exactly as the server sent it). Returns `null` for an unrecognized opcode or a
  /// message too short to contain its own header (defensive only - the server never
  /// sends either; a malformed message is logged and dropped by the caller, not a fatal
  /// application error).
  function parseMessage(buffer) {
    const bytes = new Uint8Array(buffer);
    if (bytes.length < 1) return null;
    const view = new DataView(buffer);
    const opcode = bytes[0];
    if (opcode === OP_EPOCH_STARTED) {
      if (bytes.length < EPOCH_STARTED_LEN) return null;
      return { type: "epoch_started", epoch: view.getUint32(1, false) };
    }
    if (opcode === OP_MEDIA) {
      if (bytes.length < MEDIA_HEADER_LEN) return null;
      const flags = bytes[1];
      const epoch = view.getUint32(2, false);
      // `timestamp_ms` is a real u64 on the wire; capture-time milliseconds never
      // approach 2^53, so converting via the BigInt accessor and then to `Number` is
      // exact for every value this ever actually carries.
      const timestampMs = Number(view.getBigUint64(6, false));
      return {
        type: "media",
        epoch,
        randomAccess: (flags & FLAG_RANDOM_ACCESS) !== 0,
        timestampMs,
        payload: bytes.subarray(MEDIA_HEADER_LEN),
      };
    }
    return null;
  }

  /// Keeps at most one not-yet-drawn frame: pushing a new one while a previous one is
  /// still pending closes the previous one immediately (drop-oldest) rather than
  /// building a queue - the "prefer the newest available frame" / "never let decoded
  /// frames accumulate unbounded" policy the milestone requires. `frame` is any object
  /// with a `close()` method (a real `VideoFrame`, or a test double).
  class LatestFrameSlot {
    constructor() {
      this.pending = null;
      // Frames that were decoded but replaced by a newer one before they were ever drawn: the
      // browser's own drop-oldest, counted for the Stage 17A measurements.
      this.replaced = 0;
    }

    push(frame) {
      if (this.pending) {
        this.pending.close();
        this.replaced++;
      }
      this.pending = frame;
    }

    /// If a frame is pending, hand it to `draw(frame)` then close it and return `true`;
    /// otherwise a no-op returning `false`. `draw` is called before `close()` so it can
    /// still read the frame's pixels/dimensions.
    takeAndDraw(draw) {
      if (!this.pending) return false;
      const frame = this.pending;
      this.pending = null;
      draw(frame);
      frame.close();
      return true;
    }

    /// Discard whatever is pending without drawing it (used on `stop()` - the milestone
    /// requires releasing queued browser-side data, not silently leaking it).
    clear() {
      if (this.pending) {
        this.pending.close();
        this.pending = null;
      }
    }
  }

  const STATE = Object.freeze({
    WAITING_INITIAL_KEYFRAME: "waiting_initial_keyframe",
    WAITING_KEYFRAME: "waiting_keyframe",
    DECODING: "decoding",
    ERROR: "error",
    // Stage 15.6: playback cannot continue in this browser (see `DecoderStateMachine._fail`).
    FAILED: "failed",
    IDLE: "idle",
  });

  // A decoder that errors this many times in a row without ever producing a frame is not
  // going to recover by waiting for another keyframe (that is exactly how a browser that
  // cannot decode this stream behaves), so it is reported as failed instead of retried forever.
  const MAX_CONSECUTIVE_DECODER_ERRORS = 3;

  /// Feature (never user-agent) detection of everything the video path needs, before any
  /// playback is attempted. Resolves to `{ok: true}` or `{ok: false, reason}` where `reason` is
  /// one of `no_websocket`, `insecure_context`, `no_webcodecs`, `no_video_decoder`,
  /// `av1_unsupported`, `check_failed`. `env` is injectable for tests; by default it is the
  /// real browser globals. The server stays authoritative for everything about the session -
  /// this only answers "can this browser show the video at all".
  async function checkVideoSupport(env, codec = DEFAULT_CODEC) {
    const g =
      env ||
      (typeof globalThis !== "undefined" ? globalThis : {});
    if (typeof g.WebSocket !== "function") return { ok: false, reason: "no_websocket" };
    const hasDecoder = typeof g.VideoDecoder === "function";
    const hasChunk = typeof g.EncodedVideoChunk === "function";
    if (!hasDecoder && !hasChunk) {
      // WebCodecs is only exposed to secure contexts; say so when that is the likely cause.
      return { ok: false, reason: g.isSecureContext === false ? "insecure_context" : "no_webcodecs" };
    }
    if (!hasDecoder || !hasChunk) return { ok: false, reason: "no_video_decoder" };
    if (typeof g.VideoDecoder.isConfigSupported !== "function") {
      // An implementation too old to be asked: allowed through, and the runtime failure
      // path reports it if it cannot in fact decode.
      return { ok: true, checked: false };
    }
    try {
      const result = await g.VideoDecoder.isConfigSupported({ codec });
      if (!result || result.supported !== true) return { ok: false, reason: "av1_unsupported" };
    } catch (_e) {
      return { ok: false, reason: "check_failed" };
    }
    return { ok: true, checked: true };
  }

  /// `unsupported` (this decoder will never decode this stream) or `failed` (it tried and
  /// errored repeatedly): the two are worded differently for the person.
  function classifyDecoderError(error) {
    return error && error.name === "NotSupportedError" ? "decoder_unsupported" : "decoder_failed";
  }

  /// The label for the video row: the decoder state, except that a stream that is nominally
  /// `decoding` but has produced no frame for `stallMs` reads as stalled. Display only -
  /// it drives nothing. Needed because a publisher that dies without closing its QUIC
  /// connection is only noticed by the Rust subscriber at the transport idle timeout
  /// (measured ~14s), during which the server truthfully reports `live` yet no frames
  /// arrive. `lastFrameAt`/`now` are millisecond timestamps; `null` = no frame yet.
  function describeVideoState(state, lastFrameAt, now, stallMs) {
    if (state === STATE.DECODING && lastFrameAt !== null && now - lastFrameAt >= stallMs) {
      return `stalled (${Math.floor((now - lastFrameAt) / 1000)}s without frames)`;
    }
    return state;
  }

  /// The epoch/keyframe state machine, independent of any real WebCodecs/WebSocket API -
  /// see this file's top-of-file docs for why, and `video.test.js` for what it proves.
  class DecoderStateMachine {
    /// `createDecoder()` must return an object with `decode(chunk)` and `close()` - a
    /// real `VideoDecoder` (via `VideoWatch`) or a test double. Called once by `start()`
    /// and again on every reset (a new epoch, or recovering from a decoder error).
    /// `makeChunk(init)` must return whatever `decoder.decode()` expects (a real
    /// `EncodedVideoChunk`, or a test double recording `init`). `onStatus(snapshot)` is
    /// called after every state change with a plain-object snapshot for the UI.
    constructor({ createDecoder, makeChunk, onStatus, onReset }) {
      this.createDecoder = createDecoder;
      this.makeChunk = makeChunk;
      // Called whenever the decoder is discarded (new epoch, decoder error, stop) so the
      // owner can drop anything derived from the old decoder - see `VideoWatch`.
      this.onReset = onReset || function () {};
      this.onStatus = onStatus || function () {};
      this.decoder = null;
      this.state = STATE.IDLE;
      this.epoch = 0;
      this.frameCount = 0;
      this.lastError = null;
      // Set once, when playback can no longer continue: `{kind, message}` (see `_fail`).
      this.failure = null;
      this.consecutiveErrors = 0;
      this.maxConsecutiveErrors = MAX_CONSECUTIVE_DECODER_ERRORS;
    }

    /// Enter `waiting_initial_keyframe` at epoch 0 - see this file's top-of-file docs
    /// for why the initial epoch is never announced by an `EpochStarted` message.
    start() {
      this.epoch = 0;
      this.frameCount = 0;
      this.lastError = null;
      this.failure = null;
      this.consecutiveErrors = 0;
      this.state = STATE.WAITING_INITIAL_KEYFRAME;
      this._openDecoder();
      this._report();
    }

    stop() {
      this._closeDecoder();
      this.onReset();
      this.state = STATE.IDLE;
      this._report();
    }

    /// Open a fresh decoder. A browser that cannot even construct/configure one (missing API,
    /// unsupported codec) fails playback here, once and clearly, instead of throwing out of
    /// whatever called us. Returns whether a decoder is open.
    _openDecoder() {
      try {
        this.decoder = this.createDecoder();
        return true;
      } catch (e) {
        this.decoder = null;
        this._fail(classifyDecoderError(e) === "decoder_unsupported" ? "decoder_unsupported" : "decoder_unavailable", e);
        return false;
      }
    }

    /// Playback cannot continue: release everything and stay failed (nothing more is decoded,
    /// nothing retries) until `start()`/`stop()`.
    _fail(kind, error) {
      this._closeDecoder();
      this.onReset();
      this.lastError = String((error && error.message) || error);
      this.failure = { kind, message: this.lastError };
      this.state = STATE.FAILED;
      this._report();
    }

    /// The same plain object `onStatus` receives.
    snapshot() {
      return {
        state: this.state,
        epoch: this.epoch,
        frameCount: this.frameCount,
        lastError: this.lastError,
        failure: this.failure,
      };
    }

    _closeDecoder() {
      if (this.decoder) {
        try {
          this.decoder.close();
        } catch (_e) {
          // Already closed/errored - nothing further to release.
        }
        this.decoder = null;
      }
    }

    /// A real reconnect: discard/ignore anything from the old epoch from this point on
    /// (via the `epoch` check in `handleMedia`), reset the decoder, and wait for the new
    /// epoch's first keyframe. Never feeds a delta into the freshly reset decoder.
    handleEpochStarted(epoch) {
      if (this.state === STATE.FAILED || this.state === STATE.IDLE) return "ignored";
      this.epoch = epoch;
      this._closeDecoder();
      this.onReset();
      if (!this._openDecoder()) return "failed";
      this.state = STATE.WAITING_KEYFRAME;
      this._report();
      return "reset_for_new_epoch";
    }

    /// Returns a short outcome string (`"discarded_old_epoch"`,
    /// `"discarded_no_keyframe_yet"`, or `"decoded"`) so a caller/test can observe what
    /// happened without inspecting internal state.
    handleMedia(epoch, randomAccess, timestampMs, payload) {
      if (this.state === STATE.IDLE) return "ignored_idle";
      if (this.state === STATE.FAILED) return "ignored_failed";
      if (epoch !== this.epoch) return "discarded_old_epoch";
      const awaitingKeyframe =
        this.state === STATE.WAITING_INITIAL_KEYFRAME ||
        this.state === STATE.WAITING_KEYFRAME ||
        this.state === STATE.ERROR;
      if (awaitingKeyframe && !randomAccess) return "discarded_no_keyframe_yet";

      const chunk = this.makeChunk({
        type: randomAccess ? "key" : "delta",
        timestamp: timestampMs * 1000, // ms -> WebCodecs' microseconds
        data: payload,
      });
      try {
        this.decoder.decode(chunk);
      } catch (e) {
        // A synchronous refusal (a closed/errored decoder) is a decoder error like any other.
        this.handleDecoderError(e);
        return "decoder_error";
      }
      if (randomAccess) this.state = STATE.DECODING;
      this._report();
      return "decoded";
    }

    /// Called from the real decoder's `error` callback (`VideoWatch` wires this; never
    /// called from inside this class itself - `decode()` failures surface asynchronously
    /// in WebCodecs, not as a thrown exception). Recovers by resetting the decoder and
    /// waiting for the *same* epoch's next keyframe - the encoder already emits
    /// keyframes periodically regardless of subscriber behaviour (see
    /// `crates/codec-av1`'s `keyframe_interval`), so this self-heals without a new
    /// keyframe-request protocol.
    handleDecoderError(error) {
      if (this.state === STATE.FAILED || this.state === STATE.IDLE) return;
      this.consecutiveErrors++;
      const kind = classifyDecoderError(error);
      // A decoder that says it does not support this stream will never decode it, and one
      // that has failed repeatedly without a single frame is not recovering: stop, report.
      if (kind === "decoder_unsupported" || this.consecutiveErrors >= this.maxConsecutiveErrors) {
        this._fail(kind, error);
        return;
      }
      this.lastError = String((error && error.message) || error);
      this.state = STATE.ERROR;
      this._report();
      this._closeDecoder();
      this.onReset();
      if (!this._openDecoder()) return;
      this.state = STATE.WAITING_KEYFRAME;
      this._report();
    }

    /// Called from the real decoder's `output` callback once per decoded frame (after
    /// `VideoWatch` has handed it to the `LatestFrameSlot`/render loop) - only updates
    /// the count for status reporting, never touches the frame itself.
    onFrameDecoded() {
      this.frameCount++;
      this.consecutiveErrors = 0; // a decoded frame proves this decoder works
      this.lastError = null;
      this._report();
    }

    _report() {
      this.onStatus(this.snapshot());
    }
  }

  /// Stage 17A: what the browser received, decoded and displayed, and when. Measurement only:
  /// nothing here feeds back into playback. Times are fractional milliseconds on the wall clock
  /// (`performance.timeOrigin + performance.now()`), so a frame's stage times can be compared with
  /// the publisher's and the server's (same host) by joining on its media timestamp.
  class MediaStats {
    constructor(now) {
      this.now = now || (() => (typeof performance !== "undefined" ? performance.timeOrigin + performance.now() : Date.now()));
      this.reset();
    }

    reset() {
      this.startedAt = null;
      this.requestedAt = null;
      this.received = 0;
      this.receivedKey = 0;
      this.receivedBytes = 0;
      this.submitted = 0;
      this.decoded = 0;
      this.displayed = 0;
      this.decoderErrors = 0;
      this.discardedOldEpoch = 0;
      this.discardedNoKeyframe = 0;
      this.firstRecvAt = null;
      this.firstKeyRecvAt = null;
      this.firstDecodedAt = null;
      this.firstDisplayAt = null;
      this.lastDisplayAt = null;
      this.ring = [];
    }

    _entry(ts) {
      for (let i = this.ring.length - 1; i >= 0; i--) if (this.ring[i].ts === ts) return this.ring[i];
      return null;
    }

    onStart() {
      this.startedAt = this.now();
    }

    /// The person asked to watch (the click), for "time to first displayed frame from the click".
    markRequested() {
      this.requestedAt = this.now();
    }

    onMedia(parsed) {
      const t = this.now();
      this.received++;
      this.receivedBytes += parsed.payload.length;
      if (this.firstRecvAt === null) this.firstRecvAt = t;
      if (parsed.randomAccess) {
        this.receivedKey++;
        if (this.firstKeyRecvAt === null) this.firstKeyRecvAt = t;
      }
      if (this.ring.length >= MEDIA_STATS_RING) this.ring.shift();
      this.ring.push({ ts: parsed.timestampMs, key: parsed.randomAccess, bytes: parsed.payload.length, recv: t, decoded: null, displayed: null });
    }

    onOutcome(outcome) {
      if (outcome === "decoded") this.submitted++;
      else if (outcome === "discarded_old_epoch") this.discardedOldEpoch++;
      else if (outcome === "discarded_no_keyframe_yet") this.discardedNoKeyframe++;
    }

    onDecoded(frame) {
      const t = this.now();
      this.decoded++;
      if (this.firstDecodedAt === null) this.firstDecodedAt = t;
      if (frame && typeof frame.timestamp === "number") {
        const e = this._entry(Math.round(frame.timestamp / 1000));
        if (e && e.decoded === null) e.decoded = t;
      }
    }

    onDisplayed(frame) {
      const t = this.now();
      this.displayed++;
      if (this.firstDisplayAt === null) this.firstDisplayAt = t;
      this.lastDisplayAt = t;
      if (frame && typeof frame.timestamp === "number") {
        const e = this._entry(Math.round(frame.timestamp / 1000));
        if (e && e.displayed === null) e.displayed = t;
      }
    }

    onDecoderError() {
      this.decoderErrors++;
    }

    /// The counters, derived rates and first-frame timings; `replaced` is the frame slot's count
    /// of frames dropped before display.
    snapshot(replaced) {
      const span = (a, b) => (a !== null && b !== null && b > a ? (b - a) / 1000 : 0);
      const rate = (n, first, last) => (n > 1 && span(first, last) > 0 ? (n - 1) / span(first, last) : 0);
      const since = (a, base) => (a !== null && base !== null ? a - base : null);
      const lastRecv = this.ring.length ? this.ring[this.ring.length - 1].recv : null;
      const stages = { recv_to_decoded_ms: [], decoded_to_displayed_ms: [], recv_to_displayed_ms: [] };
      for (const e of this.ring) {
        if (e.decoded !== null) stages.recv_to_decoded_ms.push(e.decoded - e.recv);
        if (e.decoded !== null && e.displayed !== null) stages.decoded_to_displayed_ms.push(e.displayed - e.decoded);
        if (e.displayed !== null) stages.recv_to_displayed_ms.push(e.displayed - e.recv);
      }
      const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : null);
      return {
        frames_received: this.received,
        keyframes_received: this.receivedKey,
        bytes_received: this.receivedBytes,
        frames_submitted_to_decoder: this.submitted,
        frames_decoded: this.decoded,
        frames_displayed: this.displayed,
        dropped_before_display: replaced || 0,
        decoder_errors: this.decoderErrors,
        discarded_old_epoch: this.discardedOldEpoch,
        discarded_waiting_for_keyframe: this.discardedNoKeyframe,
        receive_fps: rate(this.received, this.firstRecvAt, lastRecv),
        display_fps: rate(this.displayed, this.firstDisplayAt, this.lastDisplayAt),
        time_to_first_keyframe_ms: since(this.firstKeyRecvAt, this.startedAt),
        time_to_first_decoded_ms: since(this.firstDecodedAt, this.startedAt),
        time_to_first_display_ms: since(this.firstDisplayAt, this.startedAt),
        time_to_first_display_from_request_ms: since(this.firstDisplayAt, this.requestedAt),
        avg_recv_to_decoded_ms: avg(stages.recv_to_decoded_ms),
        avg_decoded_to_displayed_ms: avg(stages.decoded_to_displayed_ms),
        avg_recv_to_displayed_ms: avg(stages.recv_to_displayed_ms),
      };
    }

    /// The bounded per-frame record (oldest first), joinable with the publisher's and the
    /// server's on `ts` (the frame's media timestamp, ms).
    frames() {
      return this.ring.map((e) => Object.assign({}, e));
    }
  }

  /// The real browser glue: a WebSocket to `/api/watch/video`, a `DecoderStateMachine`
  /// driving a real `VideoDecoder`, and a `LatestFrameSlot` + `requestAnimationFrame`
  /// render loop onto `canvas`. `app.js` owns exactly two calls into this: `start()`
  /// when Watch begins, `stop()` when it ends (by any means: user Stop, the publisher
  /// ending the session, expiry, an unrecoverable reconnect failure) - see its own call
  /// sites for why those are the only two hooks the existing `/api/state`-driven
  /// lifecycle needs.
  class VideoWatch {
    constructor({
      canvas,
      onStatus,
      codec = DEFAULT_CODEC,
      wsUrl,
      // Sub-protocols offered on the socket (the watch token travels there; see app-web's
      // `ownership`). A function so it is read at each (re)attach.
      wsProtocols,
      createWebSocket,
      createDecoder,
      makeChunk,
      // Stage 15.6: whether an unexpectedly closed socket may be re-attached (the caller knows
      // whether the server still reports a watch); the delays between attempts; and the timer
      // functions (injectable so tests need no real waiting).
      canReattach,
      now,
      // Called when the server closed the video socket normally (the watch is over, or this page
      // was told to leave it), so the owner can re-read the server's state at once.
      onServerClosed,
      reattachDelaysMs = [1000, 2000, 4000],
      setTimer,
      clearTimer,
    }) {
      this.canReattach = canReattach || (() => true);
      this.onServerClosed = onServerClosed || function () {};
      this.reattachDelaysMs = reattachDelaysMs;
      this.setTimer = setTimer || ((fn, ms) => setTimeout(fn, ms));
      this.clearTimer = clearTimer || ((h) => clearTimeout(h));
      this.reattachTimer = null;
      this.reattachAttempts = 0;
      this.media = new MediaStats(now);
      this.active = false;
      // `open` | `reattaching` | `closed` (the server finished with it) | `superseded` (another page
      // took over the video) | `lost`
      this.socketState = "open";
      this.canvas = canvas || null;
      this.ctx = this.canvas ? this.canvas.getContext("2d") : null;
      this.codec = codec;
      this.wsUrl = wsUrl;
      this.wsProtocols = wsProtocols || (() => undefined);
      this.createWebSocket =
        createWebSocket ||
        function (url, protocols) {
          const ws = protocols ? new WebSocket(url, protocols) : new WebSocket(url);
          ws.binaryType = "arraybuffer";
          return ws;
        };
      this.createDecoderImpl =
        createDecoder ||
        ((onOutput, onError) =>
          new VideoDecoder({
            output: onOutput,
            error: onError,
          }));
      this.ws = null;
      this.frameSlot = new LatestFrameSlot();
      this.rafHandle = null;
      this.width = 0;
      this.height = 0;
      this.lastStatus = null;
      this.onStatusCallback = onStatus || null;
      // Bumped every time a decoder is created or the watch stops. A callback from a
      // decoder of an older generation (a late `output`/`error` delivered after that
      // decoder was closed for a new epoch or an error) is stale: its frame is closed
      // unseen and its error ignored, so an old-epoch frame can never be drawn - and an
      // old decoder's error can never reset the new one.
      this.decoderGeneration = 0;
      // Arrival time (ms) of the newest frame from the *current* decoder; reset with it.
      this.lastFrameAt = null;

      const codecString = this.codec;
      this.machine = new DecoderStateMachine({
        createDecoder: () => {
          const generation = ++this.decoderGeneration;
          const decoder = this.createDecoderImpl(
            (frame) => {
              if (generation !== this.decoderGeneration) {
                frame.close();
                return;
              }
              this._onFrame(frame);
            },
            (err) => {
              if (generation !== this.decoderGeneration) return;
              this.media.onDecoderError();
              this.machine.handleDecoderError(err);
            }
          );
          // Stage 17C: without `optimizeForLatency` Chrome holds ~4 frames before emitting the
          // first output (measured: ~400 ms at 100 ms cadence, ~1600 ms at 400 ms, on a probe
          // of this repository's real AV1 sample). It is only a hint, so a decoder that rejects
          // it is configured the old way instead - it never becomes a playback requirement.
          try {
            decoder.configure({ codec: codecString, optimizeForLatency: true });
          } catch (_e) {
            decoder.configure({ codec: codecString });
          }
          return decoder;
        },
        makeChunk: makeChunk || ((init) => new EncodedVideoChunk(init)),
        onStatus: (status) => this._onStatus(status),
        // Anything already decoded but not yet drawn belongs to the discarded decoder.
        // The canvas itself is deliberately NOT cleared: the last drawn frame stays
        // visible while the stream recovers.
        onReset: () => {
          this.decoderGeneration++;
          this.lastFrameAt = null;
          this.frameSlot.clear();
        },
      });
    }

    /// Blank the canvas. Called on start and stop only - never on a reconnect (epoch reset),
    /// where the last frame deliberately stays up (Stage 15.1) - so one Watch's final frame
    /// can never appear at the beginning of the next.
    _clearCanvas() {
      if (this.ctx && this.canvas) {
        this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);
      }
    }

    start() {
      this.stop();
      this._clearCanvas();
      this.active = true;
      this.socketState = "open";
      this.reattachAttempts = 0;
      this.frameSlot.replaced = 0;
      const requested = this.media.requestedAt;
      this.media.reset();
      this.media.requestedAt = requested; // a click just before start() belongs to this watch
      this.media.onStart();
      this.machine.start();
      if (this.machine.state === STATE.FAILED) return; // this browser cannot even open a decoder
      this._attachSocket();
      this._startRenderLoop();
    }

    /// One WebSocket for the whole watch. The Rust subscriber's reconnects (an epoch change) are
    /// invisible to it; only if the *socket itself* closes unexpectedly is it re-attached (see
    /// `_onSocketClosed`), against the server-side watch that is still there.
    _attachSocket() {
      const ws = this.createWebSocket(this.wsUrl, this.wsProtocols());
      this.ws = ws;
      ws.onmessage = (ev) => {
        if (ws !== this.ws) return; // a socket we have already replaced or closed
        this.reattachAttempts = 0; // it is delivering: the attempts start over
        if (this.socketState !== "open") {
          this.socketState = "open";
          this._onStatus(this.machine.snapshot());
        }
        this._onMessage(ev.data);
      };
      ws.onerror = () => {
        // Nothing to do here: an error is always followed by a close, which decides.
      };
      ws.onclose = (ev) => this._onSocketClosed(ws, ev);
    }

    _closeSocket() {
      const ws = this.ws;
      this.ws = null;
      if (ws) {
        ws.onmessage = null;
        ws.onclose = null;
        try {
          ws.close();
        } catch (_e) {
          // Already closed/closing.
        }
      }
    }

    /// The socket closed and we did not do it. Server close codes (`crates/app-web/src/video_ws.rs`):
    /// 4009 = a newer page took over the video (do not fight it), 4004 = there is no watch, and a
    /// normal close (1000/1005) = the server finished with this socket (the watch ended): nothing
    /// to re-attach to, so just tell the owner to re-read the server's state. Any other close - a
    /// network hiccup or a server restart (1006) - is retried a bounded number of times, and only
    /// while the caller says the watch is still there.
    _onSocketClosed(ws, ev) {
      if (ws !== this.ws || !this.active) return;
      this.ws = null;
      const code = ev && ev.code;
      if (code === 4009) return this._setSocketState("superseded");
      if (code === 4004) return this._setSocketState("lost");
      if (code === 1000 || code === 1005) {
        this._setSocketState("closed");
        return this.onServerClosed();
      }
      if (!this.canReattach() || this.reattachAttempts >= this.reattachDelaysMs.length) {
        return this._setSocketState("lost");
      }
      const delay = this.reattachDelaysMs[this.reattachAttempts++];
      this._setSocketState("reattaching");
      this.reattachTimer = this.setTimer(() => {
        this.reattachTimer = null;
        if (!this.active || this.machine.state === STATE.FAILED) return;
        if (!this.canReattach()) return this._setSocketState("lost");
        // The new socket joins the stream mid-way: whatever the decoder holds is discarded and
        // it waits for the next keyframe of the SAME epoch (no frame is drawn out of order).
        this.machine.handleEpochStarted(this.machine.epoch);
        if (this.machine.state !== STATE.FAILED) this._attachSocket();
      }, delay);
    }

    _setSocketState(state) {
      this.socketState = state;
      this._onStatus(this.machine.snapshot());
    }

    stop() {
      this.active = false;
      if (this.reattachTimer !== null) {
        this.clearTimer(this.reattachTimer);
        this.reattachTimer = null;
      }
      this._closeSocket();
      this._stopRenderLoop();
      this.frameSlot.clear();
      this.machine.stop();
      this._clearCanvas();
      this.width = 0;
      this.height = 0;
      this.socketState = "open";
    }

    _stopRenderLoop() {
      if (this.rafHandle !== null && typeof cancelAnimationFrame === "function") {
        cancelAnimationFrame(this.rafHandle);
      }
      this.rafHandle = null;
    }

    _onMessage(data) {
      const parsed = parseMessage(data);
      if (!parsed) {
        console.warn("video websocket: dropping an unrecognized/malformed message");
        return;
      }
      if (parsed.type === "epoch_started") {
        this.machine.handleEpochStarted(parsed.epoch);
        return;
      }
      this.media.onMedia(parsed);
      this.media.onOutcome(
        this.machine.handleMedia(
          parsed.epoch,
          parsed.randomAccess,
          parsed.timestampMs,
          parsed.payload
        )
      );
    }

    /// The browser-side media counters and timings (Stage 17A measurement; see `MediaStats`).
    mediaStats() {
      return Object.assign(this.media.snapshot(this.frameSlot.replaced), {
        frame_times: this.media.frames(),
      });
    }

    _onFrame(frame) {
      this.media.onDecoded(frame);
      this.lastFrameAt = Date.now();
      this.frameSlot.push(frame);
      this.machine.onFrameDecoded();
    }

    _startRenderLoop() {
      if (typeof requestAnimationFrame !== "function") return; // no-op outside a browser
      const draw = () => {
        this.frameSlot.takeAndDraw((frame) => {
          const w = frame.displayWidth || frame.codedWidth;
          const h = frame.displayHeight || frame.codedHeight;
          if (this.canvas && (this.canvas.width !== w || this.canvas.height !== h)) {
            this.canvas.width = w;
            this.canvas.height = h;
          }
          this.width = w;
          this.height = h;
          if (this.ctx) this.ctx.drawImage(frame, 0, 0, w, h);
          this.media.onDisplayed(frame);
        });
        this.rafHandle = requestAnimationFrame(draw);
      };
      this.rafHandle = requestAnimationFrame(draw);
    }

    _onStatus(status) {
      if (status.state === STATE.FAILED && this.active && this.ws) {
        // Playback failed for good: stop pulling video this browser cannot show, and leave no
        // picture behind. The watch itself (server side) is untouched; the person can Leave.
        this._closeSocket();
        this._stopRenderLoop();
        this.frameSlot.clear();
        this._clearCanvas();
        this.width = 0;
        this.height = 0;
      }
      this.lastStatus = Object.assign({}, status, {
        width: this.width,
        height: this.height,
        socket: this.socketState,
      });
      if (this.onStatusCallback) this.onStatusCallback(this.lastStatus);
    }
  }

  return {
    OP_MEDIA,
    OP_EPOCH_STARTED,
    FLAG_RANDOM_ACCESS,
    MEDIA_HEADER_LEN,
    EPOCH_STARTED_LEN,
    DEFAULT_CODEC,
    STATE,
    parseMessage,
    LatestFrameSlot,
    DecoderStateMachine,
    VideoWatch,
    describeVideoState,
    MediaStats,
    checkVideoSupport,
    classifyDecoderError,
    MAX_CONSECUTIVE_DECODER_ERRORS,
  };
});
