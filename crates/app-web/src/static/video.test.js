// Stage 14H.5: focused tests for the browser-side wire parsing and epoch/keyframe state
// machine in `video.js`. Deliberately zero dependencies - Node's own built-in test
// runner and assert module (Node 18+), no npm install, no framework, matching this
// crate's "framework-free" frontend (see `app.js`'s own top-of-file comment).
//
// Run with:
//   node --test crates/app-web/src/static/video.test.js
//
// This exercises the state machine and wire parsing only - it proves the *logic* is
// correct, not that a real Chrome browser displays real video (see the crate README's
// manual acceptance procedure for that; do not read a green run of this file as "browser
// video works").

const test = require("node:test");
const assert = require("node:assert/strict");
const {
  OP_MEDIA,
  OP_EPOCH_STARTED,
  parseMessage,
  LatestFrameSlot,
  DecoderStateMachine,
  STATE,
} = require("./video.js");

// ---- wire parsing --------------------------------------------------------------------

function mediaMessage({ randomAccess, epoch, timestampMs, payload }) {
  const buf = new ArrayBuffer(14 + payload.length);
  const view = new DataView(buf);
  view.setUint8(0, OP_MEDIA);
  view.setUint8(1, randomAccess ? 0x01 : 0x00);
  view.setUint32(2, epoch, false);
  view.setBigUint64(6, BigInt(timestampMs), false);
  new Uint8Array(buf, 14).set(payload);
  return buf;
}

function epochStartedMessage(epoch) {
  const buf = new ArrayBuffer(5);
  const view = new DataView(buf);
  view.setUint8(0, OP_EPOCH_STARTED);
  view.setUint32(1, epoch, false);
  return buf;
}

test("parseMessage decodes a Media message: epoch, key/delta, timestamp*1000, raw payload", () => {
  const payload = new Uint8Array([0x12, 0xab, 0xcd]);
  const parsed = parseMessage(mediaMessage({ randomAccess: true, epoch: 3, timestampMs: 5000, payload }));
  assert.equal(parsed.type, "media");
  assert.equal(parsed.epoch, 3);
  assert.equal(parsed.randomAccess, true);
  assert.equal(parsed.timestampMs, 5000);
  assert.deepEqual(Array.from(parsed.payload), [0x12, 0xab, 0xcd]);
});

test("parseMessage: a delta object has randomAccess false", () => {
  const parsed = parseMessage(
    mediaMessage({ randomAccess: false, epoch: 0, timestampMs: 0, payload: new Uint8Array() })
  );
  assert.equal(parsed.randomAccess, false);
});

test("parseMessage decodes EpochStarted", () => {
  const parsed = parseMessage(epochStartedMessage(42));
  assert.deepEqual(parsed, { type: "epoch_started", epoch: 42 });
});

test("parseMessage returns null for an unrecognized opcode", () => {
  const buf = new ArrayBuffer(1);
  new DataView(buf).setUint8(0, 0xff);
  assert.equal(parseMessage(buf), null);
});

// ---- LatestFrameSlot (item 9: decoded frames are closed, never queued unbounded) ------

function fakeFrame() {
  return { closed: false, close() { this.closed = true; } };
}

test("LatestFrameSlot: pushing a new frame while one is pending closes the old one", () => {
  const slot = new LatestFrameSlot();
  const a = fakeFrame();
  const b = fakeFrame();
  slot.push(a);
  slot.push(b);
  assert.equal(a.closed, true, "the dropped frame must be closed, not leaked");
  assert.equal(b.closed, false);
  assert.equal(slot.pending, b);
});

test("LatestFrameSlot: takeAndDraw draws then closes the frame, never leaving it pending", () => {
  const slot = new LatestFrameSlot();
  const f = fakeFrame();
  slot.push(f);
  let drawn = null;
  const took = slot.takeAndDraw((frame) => (drawn = frame));
  assert.equal(took, true);
  assert.equal(drawn, f, "draw must run before close, so it can still read the frame");
  assert.equal(f.closed, true);
  assert.equal(slot.pending, null);
});

test("LatestFrameSlot: clear() closes a pending frame without drawing it", () => {
  const slot = new LatestFrameSlot();
  const f = fakeFrame();
  slot.push(f);
  slot.clear();
  assert.equal(f.closed, true);
  assert.equal(slot.pending, null);
});

// ---- DecoderStateMachine ---------------------------------------------------------------

function harness() {
  const decodedChunks = [];
  const decoders = [];
  let closedCount = 0;
  const statuses = [];
  const machine = new DecoderStateMachine({
    createDecoder: () => {
      const d = {
        decode: (chunk) => decodedChunks.push(chunk),
        close: () => {
          closedCount++;
        },
      };
      decoders.push(d);
      return d;
    },
    makeChunk: (init) => init, // the chunk *is* the init object, for easy assertions
    onStatus: (s) => statuses.push(s),
  });
  return { machine, decodedChunks, decoders, statuses, closedCountRef: () => closedCount };
}

test("1. initial epoch defaults to 0", () => {
  const { machine } = harness();
  machine.start();
  assert.equal(machine.epoch, 0);
});

test("2. the initial keyframe is accepted without an EpochStarted message", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  assert.equal(machine.state, STATE.WAITING_INITIAL_KEYFRAME);
  const outcome = machine.handleMedia(0, true, 100, new Uint8Array([1]));
  assert.equal(outcome, "decoded");
  assert.equal(decodedChunks.length, 1);
  assert.equal(decodedChunks[0].type, "key");
  assert.equal(machine.state, STATE.DECODING);
});

test("3. deltas decode after the initial keyframe, in the same epoch", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  machine.handleMedia(0, true, 100, new Uint8Array([1]));
  const outcome = machine.handleMedia(0, false, 133, new Uint8Array([2]));
  assert.equal(outcome, "decoded");
  assert.equal(decodedChunks.length, 2);
  assert.equal(decodedChunks[1].type, "delta");
});

test("4. EpochStarted resets decoder state (closes the old decoder, opens a fresh one, waits for a keyframe)", () => {
  const { machine, decoders, closedCountRef } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  assert.equal(machine.state, STATE.DECODING);

  machine.handleEpochStarted(1);
  assert.equal(machine.epoch, 1);
  assert.equal(machine.state, STATE.WAITING_KEYFRAME);
  assert.equal(closedCountRef(), 1, "the old decoder must be closed");
  assert.equal(decoders.length, 2, "a fresh decoder must be created");
});

test("5. deltas arriving before the new epoch's keyframe are discarded (never fed to the fresh decoder)", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  machine.handleEpochStarted(1);

  const outcome = machine.handleMedia(1, false, 10, new Uint8Array([9]));
  assert.equal(outcome, "discarded_no_keyframe_yet");
  assert.equal(decodedChunks.length, 1, "only the epoch-0 keyframe was ever decoded");

  const keyframeOutcome = machine.handleMedia(1, true, 20, new Uint8Array([10]));
  assert.equal(keyframeOutcome, "decoded");
  assert.equal(machine.state, STATE.DECODING);
});

test("6. old-epoch media arriving after a reconnect is discarded", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  machine.handleEpochStarted(1);

  // A stray epoch-0 object, delivered late (e.g. already in flight when the reconnect
  // happened) - must never reach the new decoder.
  const outcome = machine.handleMedia(0, true, 999, new Uint8Array([0xff]));
  assert.equal(outcome, "discarded_old_epoch");
  assert.equal(decodedChunks.length, 1);
});

test("7. timestamps are converted with timestamp_us = timestamp_ms * 1000", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  machine.handleMedia(0, true, 1234, new Uint8Array([1]));
  assert.equal(decodedChunks[0].timestamp, 1234000);
});

test("8. key/delta chunk types are correct", () => {
  const { machine, decodedChunks } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  machine.handleMedia(0, false, 33, new Uint8Array([2]));
  assert.equal(decodedChunks[0].type, "key");
  assert.equal(decodedChunks[1].type, "delta");
});

test("a decoder error recovers by waiting for the next keyframe of the SAME epoch, not a new one", () => {
  const { machine, decoders, closedCountRef } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  assert.equal(machine.state, STATE.DECODING);

  machine.handleDecoderError(new Error("boom"));
  assert.equal(machine.state, STATE.WAITING_KEYFRAME);
  assert.equal(machine.epoch, 0, "recovery does not invent a new epoch");
  assert.equal(closedCountRef(), 1);
  assert.equal(decoders.length, 2);
  assert.equal(machine.lastError, "boom");

  const deltaOutcome = machine.handleMedia(0, false, 10, new Uint8Array([2]));
  assert.equal(deltaOutcome, "discarded_no_keyframe_yet", "still waiting for a keyframe post-error");
  const keyframeOutcome = machine.handleMedia(0, true, 20, new Uint8Array([3]));
  assert.equal(keyframeOutcome, "decoded");
  assert.equal(machine.state, STATE.DECODING);
});

test("stop() closes the decoder and returns to idle", () => {
  const { machine, closedCountRef } = harness();
  machine.start();
  machine.handleMedia(0, true, 0, new Uint8Array([1]));
  machine.stop();
  assert.equal(machine.state, STATE.IDLE);
  assert.equal(closedCountRef(), 1);
});

// ---- 10. the WebSocket closes cleanly on Watch termination -----------------------------
//
// `VideoWatch` itself needs real `VideoDecoder`/`EncodedVideoChunk`/canvas APIs this
// Node environment does not have, so this test exercises only its WebSocket lifecycle by
// injecting fakes for everything else - proving `stop()` closes the socket it opened,
// without depending on any real browser API.

test("10. VideoWatch.stop() closes the WebSocket it opened in start()", () => {
  const { VideoWatch } = require("./video.js");
  let closeCalls = 0;
  const fakeSocket = {
    close: () => {
      closeCalls++;
    },
    set onmessage(_fn) {},
    set onerror(_fn) {},
  };
  const fakeDecoder = { decode: () => {}, close: () => {}, configure: () => {} };
  const watch = new VideoWatch({
    canvas: null,
    wsUrl: "ws://example.invalid/api/watch/video",
    createWebSocket: () => fakeSocket,
    createDecoder: () => fakeDecoder,
  });
  watch.start();
  assert.equal(closeCalls, 0);
  watch.stop();
  assert.equal(closeCalls, 1, "stop() must close the websocket exactly once");
  assert.equal(watch.ws, null, "the closed socket must not be reused");
});

// ---- Stage 15.1: browser recovery across a Rust-side reconnect ---------------------------
//
// These drive the real `VideoWatch` (wire parsing -> state machine -> frame slot) with a
// fake WebSocket and fake decoders injected, so a whole reconnect - media at epoch 0,
// `EpochStarted(1)`, stale/late callbacks, keyframe wait, resumed deltas - is exercised
// deterministically. They prove the browser-side logic only, not a real browser.

const { VideoWatch } = require("./video.js");

function recoveryHarness() {
  const sockets = [];
  const decoders = []; // every decoder ever created, in order, with its callbacks
  const decoded = []; // {decoder index, chunk}
  const watch = new VideoWatch({
    canvas: null,
    wsUrl: "ws://example.invalid/api/watch/video",
    createWebSocket: () => {
      const s = {
        closed: 0,
        close() {
          this.closed++;
        },
      };
      sockets.push(s);
      return s;
    },
    createDecoder: (onOutput, onError) => {
      const index = decoders.length;
      const d = {
        index,
        closed: false,
        onOutput,
        onError,
        configure() {},
        decode(chunk) {
          decoded.push({ decoder: index, chunk });
        },
        close() {
          this.closed = true;
        },
      };
      decoders.push(d);
      return d;
    },
    makeChunk: (init) => init,
  });
  const send = (buf) => sockets[sockets.length - 1].onmessage({ data: buf });
  return { watch, sockets, decoders, decoded, send };
}

test("15.1-1. epoch 0 media then EpochStarted(1): decoder resets, epoch is 1, old-epoch media ignored", () => {
  const { watch, decoders, decoded, send } = recoveryHarness();
  watch.start();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));
  send(mediaMessage({ randomAccess: false, epoch: 0, timestampMs: 33, payload: new Uint8Array([2]) }));
  assert.equal(decoded.length, 2);

  send(epochStartedMessage(1));
  assert.equal(watch.machine.epoch, 1);
  assert.equal(decoders.length, 2, "a fresh decoder");
  assert.equal(decoders[0].closed, true, "the old decoder is closed");

  // A late epoch-0 object, even a keyframe, must not reach the new decoder.
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 66, payload: new Uint8Array([3]) }));
  assert.equal(decoded.length, 2, "old-epoch media ignored");
});

test("15.1-2. after EpochStarted a delta is discarded with no decode attempt; the keyframe is accepted; later deltas decode", () => {
  const { watch, decoded, send } = recoveryHarness();
  watch.start();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));
  send(epochStartedMessage(1));

  send(mediaMessage({ randomAccess: false, epoch: 1, timestampMs: 100, payload: new Uint8Array([9]) }));
  assert.equal(decoded.length, 1, "no decode attempt for the premature delta");
  assert.equal(watch.machine.state, STATE.WAITING_KEYFRAME);

  send(mediaMessage({ randomAccess: true, epoch: 1, timestampMs: 133, payload: new Uint8Array([10]) }));
  send(mediaMessage({ randomAccess: false, epoch: 1, timestampMs: 166, payload: new Uint8Array([11]) }));
  assert.equal(decoded.length, 3);
  assert.equal(decoded[1].decoder, 1, "decoded by the NEW decoder");
  assert.equal(decoded[1].chunk.type, "key");
  assert.equal(decoded[2].chunk.type, "delta");
  assert.equal(watch.machine.state, STATE.DECODING);
});

test("15.1-3. a WebCodecs decode error re-enters keyframe waiting, opens no new socket, and resumes on the next keyframe", () => {
  const { watch, sockets, decoders, decoded, send } = recoveryHarness();
  watch.start();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));

  decoders[0].onError(new Error("decode failed"));
  assert.equal(watch.machine.state, STATE.WAITING_KEYFRAME);
  assert.equal(watch.machine.epoch, 0, "no invented epoch");
  assert.equal(sockets.length, 1, "no new transport connection");
  assert.equal(sockets[0].closed, 0, "the existing socket is left alone");

  send(mediaMessage({ randomAccess: false, epoch: 0, timestampMs: 33, payload: new Uint8Array([2]) }));
  assert.equal(decoded.length, 1, "delta discarded while waiting");
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 66, payload: new Uint8Array([3]) }));
  assert.equal(decoded.length, 2);
  assert.equal(watch.machine.state, STATE.DECODING);
});

test("15.1-4. stale frames are never rendered: pending old frame cleared on reset, late output from the closed decoder is closed unseen", () => {
  const { watch, decoders, send } = recoveryHarness();
  watch.start();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));

  // An old-epoch frame decoded but not yet drawn when the reconnect arrives.
  const pendingOld = fakeFrame();
  decoders[0].onOutput(pendingOld);
  assert.equal(watch.frameSlot.pending, pendingOld);

  send(epochStartedMessage(1));
  assert.equal(pendingOld.closed, true, "the undrawn old frame is closed");
  assert.equal(watch.frameSlot.pending, null, "nothing old left to draw");

  // The closed decoder delivers one more output late (already in flight).
  const late = fakeFrame();
  decoders[0].onOutput(late);
  assert.equal(late.closed, true, "late old-decoder output is closed, not queued");
  assert.equal(watch.frameSlot.pending, null, "and never becomes drawable");

  // The NEW decoder's frames do render.
  const fresh = fakeFrame();
  decoders[1].onOutput(fresh);
  assert.equal(watch.frameSlot.pending, fresh);
  assert.equal(fresh.closed, false);
});

test("15.1-4b. a late error from a closed decoder cannot reset the new decoder", () => {
  const { watch, decoders, send } = recoveryHarness();
  watch.start();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));
  send(epochStartedMessage(1));
  send(mediaMessage({ randomAccess: true, epoch: 1, timestampMs: 100, payload: new Uint8Array([2]) }));
  assert.equal(watch.machine.state, STATE.DECODING);

  decoders[0].onError(new Error("late error from the epoch-0 decoder"));
  assert.equal(watch.machine.state, STATE.DECODING, "the healthy new decoder is untouched");
  assert.equal(decoders.length, 2, "no spurious extra decoder");
});

test("15.1-5. repeated reconnects create no duplicate WebSockets or consumers", () => {
  const { watch, sockets, decoders, send } = recoveryHarness();
  watch.start();
  for (let epoch = 1; epoch <= 5; epoch++) {
    send(epochStartedMessage(epoch));
    send(mediaMessage({ randomAccess: true, epoch, timestampMs: epoch * 100, payload: new Uint8Array([epoch]) }));
  }
  assert.equal(sockets.length, 1, "exactly one WebSocket for the whole watch");
  assert.equal(sockets[0].closed, 0);
  assert.equal(decoders.filter((d) => !d.closed).length, 1, "exactly one live decoder");
  assert.equal(watch.machine.epoch, 5);
});

test("15.1-6. stop() after a reconnect closes the socket once and releases decoder and frames", () => {
  const { watch, sockets, decoders, send } = recoveryHarness();
  watch.start();
  send(epochStartedMessage(1));
  send(mediaMessage({ randomAccess: true, epoch: 1, timestampMs: 0, payload: new Uint8Array([1]) }));
  const pending = fakeFrame();
  decoders[1].onOutput(pending);

  watch.stop();
  assert.equal(sockets[0].closed, 1, "socket closed exactly once");
  assert.equal(decoders.filter((d) => !d.closed).length, 0, "no live decoder remains");
  assert.equal(pending.closed, true, "queued frame released");
  assert.equal(watch.machine.state, STATE.IDLE);

  // Output arriving after stop is closed, never queued.
  const late = fakeFrame();
  decoders[1].onOutput(late);
  assert.equal(late.closed, true);
  assert.equal(watch.frameSlot.pending, null);
});

// ---- Stage 15.1: stall indication ------------------------------------------------------
//
// A publisher that dies without a QUIC close (crash, SIGKILL, network loss) is only
// noticed by the Rust subscriber at the transport idle timeout - measured at ~14s - and
// during that time `/api/state` truthfully reports `live` while no frames arrive. The
// video status label therefore also reflects actual frame flow.

const { describeVideoState } = require("./video.js");

test("15.1-7. a decoding stream with no recent frame is labelled stalled; recent frames or non-decoding states are not", () => {
  const now = 100_000;
  assert.equal(describeVideoState("decoding", now - 500, now, 2000), "decoding");
  assert.equal(describeVideoState("decoding", now - 2500, now, 2000), "stalled (2s without frames)");
  assert.equal(describeVideoState("decoding", now - 13_900, now, 2000), "stalled (13s without frames)");
  // Never decoded anything yet / not in a decoding state: report the real state, not a stall.
  assert.equal(describeVideoState("decoding", null, now, 2000), "decoding");
  assert.equal(describeVideoState("waiting_keyframe", now - 60_000, now, 2000), "waiting_keyframe");
  assert.equal(describeVideoState("idle", now - 60_000, now, 2000), "idle");
});

test("15.1-8. VideoWatch records the arrival time of the newest decoded frame, ignoring stale-decoder output", () => {
  const { watch, decoders, send } = recoveryHarness();
  watch.start();
  assert.equal(watch.lastFrameAt, null);
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: new Uint8Array([1]) }));
  decoders[0].onOutput(fakeFrame());
  const first = watch.lastFrameAt;
  assert.equal(typeof first, "number");

  send(epochStartedMessage(1));
  assert.equal(watch.lastFrameAt, null, "a reset starts a fresh stall clock; no stale timestamp lingers");
  decoders[0].onOutput(fakeFrame()); // late output from the closed decoder
  assert.equal(watch.lastFrameAt, null, "stale output must not look like live flow");
});

// ---- Stage 15.3: no stale frame across Watch sessions ----------------------------------

test("15.3-1. stop() and start() clear the canvas so a previous session's last frame never shows; a reconnect does not", () => {
  const clears = [];
  const canvas = {
    width: 1280,
    height: 720,
    getContext: () => ({ clearRect: (...a) => clears.push(a), drawImage() {} }),
  };
  const sockets = [];
  const watch = new VideoWatch({
    canvas,
    wsUrl: "ws://example.invalid/x",
    createWebSocket: () => {
      const s = { close() {} };
      sockets.push(s);
      return s;
    },
    createDecoder: () => ({ configure() {}, decode() {}, close() {} }),
    makeChunk: (i) => i,
  });

  watch.start();
  const afterStart = clears.length;
  assert.ok(afterStart >= 1, "start() clears any leftover picture");
  assert.deepEqual(clears[0], [0, 0, 1280, 720]);

  // A Rust-side reconnect keeps the last frame visible while the stream recovers (15.1).
  sockets[0].onmessage({ data: epochStartedMessage(1) });
  assert.equal(clears.length, afterStart, "an epoch reset must NOT blank the picture");

  watch.stop();
  assert.ok(clears.length > afterStart, "stop() clears the canvas");

  // The next Watch starts from a blank canvas, not the last session's final frame.
  const before = clears.length;
  watch.start();
  assert.ok(clears.length > before);
});

// ---- Stage 15.6: capability detection, decoder failure, socket re-attach -----------------------

const { checkVideoSupport, STATE: S15_6, MAX_CONSECUTIVE_DECODER_ERRORS } = require("./video.js");

const fakeEnv = (over = {}) => ({
  WebSocket: function () {},
  VideoDecoder: Object.assign(function () {}, { isConfigSupported: async () => ({ supported: true }) }),
  EncodedVideoChunk: function () {},
  isSecureContext: true,
  ...over,
});

test("15.6-1. capability detection tells every missing piece apart, by feature, never by user agent", async () => {
  assert.deepEqual(await checkVideoSupport(fakeEnv()), { ok: true, checked: true });
  assert.deepEqual(await checkVideoSupport(fakeEnv({ WebSocket: undefined })), { ok: false, reason: "no_websocket" });
  assert.deepEqual(
    await checkVideoSupport(fakeEnv({ VideoDecoder: undefined, EncodedVideoChunk: undefined })),
    { ok: false, reason: "no_webcodecs" }
  );
  assert.deepEqual(
    await checkVideoSupport(fakeEnv({ VideoDecoder: undefined, EncodedVideoChunk: undefined, isSecureContext: false })),
    { ok: false, reason: "insecure_context" }
  );
  assert.deepEqual(await checkVideoSupport(fakeEnv({ VideoDecoder: undefined })), { ok: false, reason: "no_video_decoder" });
  assert.deepEqual(await checkVideoSupport(fakeEnv({ EncodedVideoChunk: undefined })), { ok: false, reason: "no_video_decoder" });
});

test("15.6-2. AV1 support is asked of the decoder: unsupported and unverifiable are distinct, an old API is allowed through", async () => {
  const withSupport = (fn) =>
    fakeEnv({ VideoDecoder: Object.assign(function () {}, { isConfigSupported: fn }) });
  assert.deepEqual(await checkVideoSupport(withSupport(async () => ({ supported: false }))), { ok: false, reason: "av1_unsupported" });
  assert.deepEqual(await checkVideoSupport(withSupport(async () => ({}))), { ok: false, reason: "av1_unsupported" });
  assert.deepEqual(await checkVideoSupport(withSupport(async () => { throw new TypeError("bad"); })), { ok: false, reason: "check_failed" });
  assert.deepEqual(await checkVideoSupport(fakeEnv({ VideoDecoder: function () {} })), { ok: true, checked: false });
  let asked = null;
  await checkVideoSupport(withSupport(async (cfg) => { asked = cfg; return { supported: true }; }));
  assert.equal(asked.codec, "av01.0.31M.08", "it asks about the codec the watch really configures");
});

function failingHarness({ createThrows, decodeThrows } = {}) {
  const sockets = [];
  const decoders = [];
  const decoded = [];
  const statuses = [];
  let clears = 0;
  const canvas = {
    width: 1280,
    height: 720,
    getContext: () => ({ clearRect: () => clears++, drawImage() {} }),
  };
  const timers = [];
  const watch = new VideoWatch({
    canvas,
    wsUrl: "ws://example.invalid/x",
    onStatus: (s) => statuses.push(s),
    createWebSocket: () => {
      const s = { closed: 0, close() { this.closed++; } };
      sockets.push(s);
      return s;
    },
    createDecoder: (onOutput, onError) => {
      if (createThrows) throw createThrows;
      const d = {
        index: decoders.length,
        closed: false,
        onOutput,
        onError,
        configure() {},
        decode(chunk) {
          if (decodeThrows) throw decodeThrows;
          decoded.push(chunk);
        },
        close() { this.closed = true; },
      };
      decoders.push(d);
      return d;
    },
    makeChunk: (i) => i,
    setTimer: (fn, ms) => { timers.push({ fn, ms, cleared: false }); return timers.length - 1; },
    clearTimer: (h) => { timers[h].cleared = true; },
    canReattach: () => watch.allowReattach !== false,
  });
  const send = (buf) => sockets[sockets.length - 1].onmessage({ data: buf });
  return { watch, sockets, decoders, decoded, statuses, timers, send, clears: () => clears };
}

const key = (epoch = 0, t = 0) => mediaMessage({ epoch, randomAccess: true, timestampMs: t, payload: [1, 2, 3] });
const delta = (epoch = 0, t = 1) => mediaMessage({ epoch, randomAccess: false, timestampMs: t, payload: [4, 5] });

test("15.6-3. a decoder that cannot be constructed fails playback once, clearly - nothing throws, no socket opens", () => {
  const h = failingHarness({ createThrows: new ReferenceError("VideoDecoder is not defined") });
  h.watch.start(); // must not throw
  const last = h.statuses[h.statuses.length - 1];
  assert.equal(last.state, S15_6.FAILED);
  assert.equal(last.failure.kind, "decoder_unavailable");
  assert.match(last.failure.message, /VideoDecoder is not defined/);
  assert.equal(h.sockets.length, 0, "no video socket is opened for a browser that cannot decode");
  h.watch.stop(); // and stopping a failed watch is fine
});

test("15.6-4. a NotSupportedError is fatal at once and worded as unsupported; other errors retry", () => {
  const h = failingHarness();
  h.watch.start();
  h.send(key());
  const err = new Error("codec not supported");
  err.name = "NotSupportedError";
  h.decoders[0].onError(err);
  const last = h.statuses[h.statuses.length - 1];
  assert.equal(last.state, S15_6.FAILED);
  assert.equal(last.failure.kind, "decoder_unsupported");
  assert.equal(h.decoders.length, 1, "no replacement decoder is created after a fatal failure");
  assert.equal(h.decoders[0].closed, true, "the failed decoder is released");
});

test("15.6-5. repeated decode errors with no frame in between end in a fatal failure - not an endless retry", () => {
  const h = failingHarness();
  h.watch.start();
  for (let i = 0; i < MAX_CONSECUTIVE_DECODER_ERRORS; i++) {
    h.send(key(0, i));
    h.decoders[h.decoders.length - 1].onError(Object.assign(new Error("EncodingError"), { name: "EncodingError" }));
  }
  const last = h.statuses[h.statuses.length - 1];
  assert.equal(last.state, S15_6.FAILED);
  assert.equal(last.failure.kind, "decoder_failed");
  const decodersAtFailure = h.decoders.length;
  assert.equal(h.sockets[0].onmessage, null, "the socket is detached: no later media can reach the decoder");
  assert.equal(h.sockets[0].closed, 1);
  assert.equal(h.decoders.length, decodersAtFailure, "nothing creates another decoder");
  assert.equal(h.decoded.length, MAX_CONSECUTIVE_DECODER_ERRORS, "and nothing more is decoded");
  assert.ok(h.decoders.every((d) => d.closed), "every decoder is released");
});

test("15.6-6. a decoded frame proves the decoder works: earlier errors no longer count towards failure", () => {
  const h = failingHarness();
  h.watch.start();
  const boom = () => Object.assign(new Error("transient"), { name: "EncodingError" });
  for (let round = 0; round < 3; round++) {
    h.send(key(0, round));
    h.decoders[h.decoders.length - 1].onError(boom());
    h.send(key(0, 100 + round));
    h.decoders[h.decoders.length - 1].onOutput({ close() {}, displayWidth: 1280, displayHeight: 720 });
  }
  const last = h.statuses[h.statuses.length - 1];
  assert.notEqual(last.state, S15_6.FAILED, "errors separated by real frames never accumulate");
  assert.equal(last.lastError, null, "a recovered error is not shown any more");
});

test("15.6-7. decode() throwing synchronously is treated as a decoder error", () => {
  const h = failingHarness({ decodeThrows: new Error("InvalidStateError: closed") });
  h.watch.start();
  for (let i = 0; i < MAX_CONSECUTIVE_DECODER_ERRORS; i++) h.send(key(0, i));
  assert.equal(h.statuses[h.statuses.length - 1].state, S15_6.FAILED);
});

test("15.6-8. fatal failure closes the socket, clears the picture and leaves no live decoder or pending frame", () => {
  const h = failingHarness();
  h.watch.start();
  h.send(key());
  const clearsBefore = h.clears();
  const pending = { closed: false, close() { this.closed = true; }, displayWidth: 1, displayHeight: 1 };
  h.decoders[0].onOutput(pending); // a decoded frame waiting to be drawn
  const err = Object.assign(new Error("nope"), { name: "NotSupportedError" });
  h.decoders[0].onError(err);
  assert.equal(h.sockets[0].closed, 1, "the video socket is closed");
  assert.equal(pending.closed, true, "the undrawn frame is released");
  assert.ok(h.clears() > clearsBefore, "the canvas is blanked - no stale picture");
  assert.ok(h.decoders.every((d) => d.closed));
  h.watch.stop();
  assert.equal(h.sockets[0].closed, 1, "closed once, not twice");
});

test("15.6-9. across many epoch resets there is never more than one open decoder, and a late output from an old one is dropped", () => {
  const h = failingHarness();
  h.watch.start();
  h.send(key(0, 0));
  const old = h.decoders[0];
  for (let epoch = 1; epoch <= 10; epoch++) {
    h.send(epochStartedMessage(epoch));
    assert.equal(h.decoders.filter((d) => !d.closed).length, 1, `epoch ${epoch}: exactly one live decoder`);
  }
  assert.equal(h.sockets.length, 1, "and one socket");
  const late = { closed: false, close() { this.closed = true; }, displayWidth: 1, displayHeight: 1 };
  old.onOutput(late);
  assert.equal(late.closed, true, "an obsolete generation's frame is discarded, never drawn");
  h.send(key(3, 5)); // old epoch media after the reset
  assert.equal(h.decoded.length, 1, "old-epoch media is not decoded (only the first keyframe was)");
});

test("15.6-10. stop() (Leave / session end) leaves no decoder, socket, timer or frame; late callbacks are inert", () => {
  const h = failingHarness();
  h.watch.start();
  h.send(key());
  const d = h.decoders[0];
  h.sockets[0].onclose({ code: 1006 }); // schedules a re-attach
  assert.equal(h.timers.length, 1);
  h.watch.stop();
  assert.equal(h.timers[0].cleared, true, "the pending re-attach is cancelled");
  assert.ok(d.closed);
  const late = { closed: false, close() { this.closed = true; } };
  d.onOutput(late);
  assert.equal(late.closed, true);
  h.timers[0].fn(); // even if the timer fired anyway
  assert.equal(h.sockets.length, 1, "no socket appears after stop");
  assert.equal(h.watch.lastStatus.state, S15_6.IDLE);
});

test("15.6-11. an unexpectedly closed socket is re-attached once per attempt, waits for a keyframe, and never duplicates", () => {
  const h = failingHarness();
  h.watch.start();
  h.send(key());
  h.sockets[0].onclose({ code: 1006 });
  assert.equal(h.watch.lastStatus.socket, "reattaching");
  assert.equal(h.timers[0].ms, 1000);
  h.timers[0].fn();
  assert.equal(h.sockets.length, 2, "a new socket replaces the closed one");
  assert.equal(h.decoders.filter((d) => !d.closed).length, 1, "one live decoder");
  h.send(delta(0, 2)); // mid-stream delta on the new socket: not fed to a fresh decoder
  assert.equal(h.decoded.length, 1);
  h.send(key(0, 9));
  assert.equal(h.decoded.length, 2, "decoding resumes at the next keyframe of the same epoch");
  assert.equal(h.watch.lastStatus.socket, "open");
  // a late close from the replaced socket is ignored
  h.sockets[0].onclose && h.sockets[0].onclose({ code: 1006 });
  assert.equal(h.timers.length, 1);
});

test("15.6-12. socket close codes: 4009 (another page took over) and 4004 (no watch) are not retried", () => {
  for (const [code, state] of [[4009, "superseded"], [4004, "lost"]]) {
    const h = failingHarness();
    h.watch.start();
    h.sockets[0].onclose({ code });
    assert.equal(h.watch.lastStatus.socket, state);
    assert.equal(h.timers.length, 0, `code ${code} schedules no retry`);
  }
});

test("15.6-13. no re-attach once the watch is over, and only a bounded number of attempts", () => {
  const over = failingHarness();
  over.watch.allowReattach = false; // the server says the watch is finished
  over.watch.start();
  over.sockets[0].onclose({ code: 1006 });
  assert.equal(over.timers.length, 0);
  assert.equal(over.watch.lastStatus.socket, "lost");

  const h = failingHarness();
  h.watch.start();
  for (let attempt = 0; attempt < 3; attempt++) {
    h.sockets[h.sockets.length - 1].onclose({ code: 1006 });
    h.timers[h.timers.length - 1].fn();
  }
  h.sockets[h.sockets.length - 1].onclose({ code: 1006 });
  assert.equal(h.timers.length, 3, "three attempts, then it stops");
  assert.equal(h.watch.lastStatus.socket, "lost");
  assert.deepEqual(h.timers.map((t) => t.ms), [1000, 2000, 4000]);
});

test("15.6-14. a normal server close (the watch ended) is not retried: the owner is told to re-read the server's state", () => {
  for (const code of [1000, 1005]) {
    let told = 0;
    const sockets = [];
    const timers = [];
    const w = new VideoWatch({
      canvas: null,
      wsUrl: "ws://example.invalid/x",
      createWebSocket: () => { const s = { close() {} }; sockets.push(s); return s; },
      createDecoder: () => ({ configure() {}, decode() {}, close() {} }),
      makeChunk: (i) => i,
      setTimer: (fn, ms) => timers.push({ fn, ms }),
      onServerClosed: () => told++,
    });
    w.start();
    sockets[0].onclose({ code });
    assert.equal(told, 1, `code ${code}`);
    assert.equal(timers.length, 0, `code ${code}: no re-attach to a finished watch`);
    assert.equal(w.lastStatus.socket, "closed");
    assert.equal(sockets.length, 1);
  }
});

// ---- Stage 17A: browser-side media measurement -------------------------------------------------

const { MediaStats } = require("./video.js");

test("17A-1. MediaStats counts received, decoded and displayed frames and joins their stage times on the media timestamp", () => {
  let t = 1000;
  const m = new MediaStats(() => t);
  m.onStart();
  const frame = (ts) => ({ timestamp: ts * 1000 });
  for (let i = 0; i < 4; i++) {
    t = 1100 + i * 100;
    m.onMedia({ timestampMs: i * 66, randomAccess: i === 0, payload: new Uint8Array(100 + i) });
    m.onOutcome("decoded");
    t += 20; m.onDecoded(frame(i * 66));
    t += 5; m.onDisplayed(frame(i * 66));
  }
  const s = m.snapshot(2);
  assert.equal(s.frames_received, 4);
  assert.equal(s.keyframes_received, 1);
  assert.equal(s.bytes_received, 100 + 101 + 102 + 103);
  assert.equal(s.frames_submitted_to_decoder, 4);
  assert.equal(s.frames_decoded, 4);
  assert.equal(s.frames_displayed, 4);
  assert.equal(s.dropped_before_display, 2);
  assert.equal(s.avg_recv_to_decoded_ms, 20);
  assert.equal(s.avg_decoded_to_displayed_ms, 5);
  assert.equal(s.avg_recv_to_displayed_ms, 25);
  assert.equal(s.time_to_first_keyframe_ms, 100, "from start() to the first keyframe arriving");
  assert.equal(s.time_to_first_display_ms, 125);
  assert.ok(Math.abs(s.receive_fps - 10) < 1e-9, "3 intervals over 300 ms");
  assert.equal(m.frames().length, 4);
  assert.deepEqual(m.frames().map((f) => f.ts), [0, 66, 132, 198]);
});

test("17A-2. time to first displayed frame is measured from the request (the click) as well as from start()", () => {
  let t = 5000;
  const m = new MediaStats(() => t);
  m.markRequested();
  t = 5300; m.onStart();
  t = 5400; m.onMedia({ timestampMs: 0, randomAccess: true, payload: new Uint8Array(10) });
  t = 5450; m.onDecoded({ timestamp: 0 });
  t = 5460; m.onDisplayed({ timestamp: 0 });
  const s = m.snapshot(0);
  assert.equal(s.time_to_first_display_ms, 160);
  assert.equal(s.time_to_first_display_from_request_ms, 460);
  assert.equal(new MediaStats(() => 0).snapshot(0).time_to_first_display_ms, null, "unknown until it happens");
});

test("17A-3. discarded and errored frames are counted and the ring is bounded", () => {
  const m = new MediaStats(() => 1);
  m.onOutcome("discarded_old_epoch");
  m.onOutcome("discarded_no_keyframe_yet");
  m.onOutcome("discarded_no_keyframe_yet");
  m.onDecoderError();
  const s = m.snapshot(0);
  assert.deepEqual([s.discarded_old_epoch, s.discarded_waiting_for_keyframe, s.decoder_errors], [1, 2, 1]);
  for (let i = 0; i < 700; i++) m.onMedia({ timestampMs: i, randomAccess: false, payload: new Uint8Array(1) });
  assert.equal(m.frames().length, 512, "bounded");
  assert.equal(m.frames()[511].ts, 699, "the newest are kept");
  assert.equal(m.snapshot(0).frames_received, 700, "totals are not ring-limited");
});

test("17A-4. VideoWatch feeds the counters from the real message/decoder/draw path without changing playback", () => {
  const { VideoWatch } = require("./video.js");
  let clock = 0;
  const sockets = [];
  const decoders = [];
  const watch = new VideoWatch({
    canvas: null,
    wsUrl: "ws://example.invalid/x",
    now: () => clock,
    createWebSocket: () => { const s = { close() {} }; sockets.push(s); return s; },
    createDecoder: (onOutput, onError) => { const d = { onOutput, onError, configure() {}, decode() {}, close() {} }; decoders.push(d); return d; },
    makeChunk: (i) => i,
  });
  watch.start();
  const send = (b) => sockets[0].onmessage({ data: b });
  clock = 10; send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: [1, 2, 3] }));
  clock = 20; send(mediaMessage({ randomAccess: false, epoch: 0, timestampMs: 66, payload: [4, 5] }));
  send(mediaMessage({ randomAccess: false, epoch: 7, timestampMs: 99, payload: [6] })); // an old epoch
  clock = 30; decoders[0].onOutput({ close() {}, timestamp: 0, displayWidth: 1, displayHeight: 1 });
  clock = 40; decoders[0].onError(new Error("boom"));
  const s = watch.mediaStats();
  assert.equal(s.frames_received, 3);
  assert.equal(s.frames_submitted_to_decoder, 2);
  assert.equal(s.discarded_old_epoch, 1);
  assert.equal(s.frames_decoded, 1);
  assert.equal(s.decoder_errors, 1);
  assert.equal(s.frame_times[0].decoded, 30, "joined on the media timestamp");
  assert.equal(s.frame_times[0].recv, 10);
  // Stats are per watch: a new start() begins from zero.
  watch.start();
  assert.equal(watch.mediaStats().frames_received, 0);
});

// ---- Stage 17C: optimizeForLatency -------------------------------------------------------------
//
// Deterministic, no browser: a fake decoder records what it is configured with. The real-Chrome
// effect of the flag (latency, outputs per input) is measured separately by the standalone probe
// and the real application run described in the crate README.

function latencyHarness(configure) {
  const sockets = [];
  const decoders = [];
  const statuses = [];
  const watch = new VideoWatch({
    canvas: null,
    wsUrl: "ws://example.invalid/x",
    onStatus: (s) => statuses.push(s),
    createWebSocket: () => { const s = { close() {} }; sockets.push(s); return s; },
    createDecoder: (onOutput, onError) => {
      const d = { onOutput, onError, configs: [], decoded: 0, closed: false,
        configure(c) { this.configs.push(c); if (configure) configure(c); },
        decode() { this.decoded++; }, close() { this.closed = true; } };
      decoders.push(d);
      return d;
    },
    makeChunk: (i) => i,
  });
  watch.start();
  return { watch, decoders, statuses, send: (b) => sockets[0].onmessage({ data: b }) };
}

test("17C-1. the decoder is configured with optimizeForLatency and the unchanged codec", () => {
  const { decoders } = latencyHarness();
  assert.equal(decoders.length, 1);
  assert.deepEqual(decoders[0].configs, [{ codec: "av01.0.31M.08", optimizeForLatency: true }]);
});

test("17C-2. every decoder opened after a reset (new epoch, decoder error) is configured the same way", () => {
  const { decoders, send, watch } = latencyHarness();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: [1] }));
  send(epochStartedMessage(1)); // a Rust-side reconnect: the decoder is replaced
  assert.equal(decoders.length, 2);
  decoders[1].onError(new Error("boom")); // a decoder error: replaced again
  assert.equal(decoders.length, 3);
  for (const d of decoders) {
    assert.deepEqual(d.configs, [{ codec: "av01.0.31M.08", optimizeForLatency: true }]);
  }
  assert.equal(decoders[0].closed, true, "the old decoder is still closed on reset");
  assert.notEqual(watch.machine.state, "failed");
});

test("17C-3. output, epoch handling and stale callbacks behave as before", () => {
  const { decoders, send, watch } = latencyHarness();
  send(mediaMessage({ randomAccess: true, epoch: 0, timestampMs: 0, payload: [1] }));
  send(mediaMessage({ randomAccess: false, epoch: 0, timestampMs: 66, payload: [2] }));
  assert.equal(decoders[0].decoded, 2);
  decoders[0].onOutput({ close() {}, timestamp: 0, displayWidth: 1, displayHeight: 1 });
  assert.equal(watch.mediaStats().frames_decoded, 1);
  send(epochStartedMessage(1));
  send(mediaMessage({ randomAccess: false, epoch: 1, timestampMs: 99, payload: [3] }));
  assert.equal(decoders[1].decoded, 0, "a delta is not fed to the new decoder before a keyframe");
  send(mediaMessage({ randomAccess: true, epoch: 1, timestampMs: 133, payload: [4] }));
  assert.equal(decoders[1].decoded, 1);
  // A frame from the replaced decoder is closed unseen, never drawn.
  const before = watch.mediaStats().frames_decoded;
  let staleClosed = 0;
  decoders[0].onOutput({ close() { staleClosed++; }, timestamp: 66, displayWidth: 1, displayHeight: 1 });
  assert.equal(staleClosed, 1);
  assert.equal(watch.mediaStats().frames_decoded, before);
});

test("17C-4. a decoder that rejects the hint is configured the old way; it is not a playback requirement", () => {
  const { decoders, watch } = latencyHarness((c) => {
    if ("optimizeForLatency" in c) throw new TypeError("unknown option");
  });
  assert.equal(decoders.length, 1);
  assert.deepEqual(decoders[0].configs, [
    { codec: "av01.0.31M.08", optimizeForLatency: true },
    { codec: "av01.0.31M.08" },
  ]);
  assert.notEqual(watch.machine.state, "failed");
});

test("17C-5. a decoder that cannot be configured at all still fails playback exactly as before", () => {
  const { watch, statuses } = latencyHarness(() => {
    const e = new Error("no av1"); e.name = "NotSupportedError"; throw e;
  });
  assert.equal(watch.machine.state, "failed");
  assert.equal(statuses[statuses.length - 1].failure.kind, "decoder_unsupported");
});

test("17C-6. the capability check still asks about the codec alone", async () => {
  let asked;
  const env = fakeEnv({ VideoDecoder: Object.assign(function () {}, {
    isConfigSupported: async (c) => { asked = c; return { supported: true }; } }) });
  assert.deepEqual(await checkVideoSupport(env), { ok: true, checked: true });
  assert.deepEqual(asked, { codec: "av01.0.31M.08" });
});

test("the watch token is offered as a socket sub-protocol and re-read on every attach", () => {
  const seen = [];
  let token = "t1";
  const w = new VideoWatch({
    wsUrl: "ws://x/api/watch/video",
    wsProtocols: () => ["moq-watch.v1", `moq-watch-token.${token}`],
    createWebSocket: (url, protocols) => { seen.push(protocols); return { close() {} }; },
    createDecoder: () => ({ configure() {}, decode() {}, close() {}, state: "configured" }),
    makeChunk: (x) => x,
  });
  w._attachSocket();
  token = "t2";
  w._attachSocket();
  assert.deepEqual(seen, [["moq-watch.v1", "moq-watch-token.t1"], ["moq-watch.v1", "moq-watch-token.t2"]]);
});
