// Deliberately small, framework-free control surface for holochain-moq-app-web.
//
// The server's `/api/state` response is the single source of truth (see the crate's
// README/`view.rs`): this script renders from it on every poll/response rather than
// keeping a second client-side state machine. The only client-side state is UI-local
// (which panel is open, the current form values, whether a request is in flight) - never
// anything the server already tracks authoritatively. In particular, once the server
// reports `state === "sharing"` or `"watching"`, that *always* decides which panel is
// shown - a locally-clicked "Watch" while a share is live does not open a second panel
// next to it (see `setPanels`/`render`, and 14H.2's report).

const el = (id) => document.getElementById(id);

// Stage 17H: this file only *renders* what the server, the decoder and `ui.js` already decided. The
// helpers below build markup; none of them decides authorization, liveness or lifecycle.
const SVG_NS = "http://www.w3.org/2000/svg";
function icon(name) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG_NS, "use");
  use.setAttribute("href", `#i-${name}`);
  svg.appendChild(use);
  return svg;
}
/// The Public / Private chip: an icon AND a word, never a colour alone.
function accessChip(isPrivate, node) {
  const chip = node || document.createElement("span");
  chip.className = `chip ${isPrivate ? "chip-private" : "chip-public"}`;
  chip.replaceChildren(icon(isPrivate ? "lock" : "globe"), document.createTextNode(isPrivate ? "Private" : "Public"));
  return chip;
}

let pollTimer = null;
let panel = "menu"; // "menu" | "share" | "watch" - only consulted while state is "ready"
// Guards the auto-stop path in `refreshState` below: `setInterval` does not wait for a
// previous tick's promise to settle, so without this a `/stop` call slower than the 1s
// poll interval could overlap with a second, redundant one from the next tick.
let autoStopping = false;

// Stage 14H.5: the browser video WebSocket/WebCodecs consumer (see `video.js`, loaded
// before this script). It has no lifecycle of its own - `render()` below is the single
// place that starts/stops it, driven entirely by the existing, server-authoritative
// `state.state === "watching"` the rest of this file already reads from `/api/state`.
// No second application state machine is introduced for video (see `video.js`'s own
// top-of-file docs for the epoch/keyframe state machine that *is* video-specific).
const videoWatch = new HolochainMoqVideo.VideoWatch({
  canvas: el("watch-canvas"),
  wsUrl: `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/watch/video`,
  // The watch token proves this page started the watch, so this socket counts as attending it.
  wsProtocols: () => (watchCred ? ["moq-watch.v1", `moq-watch-token.${watchCred.token}`] : undefined),
  onStatus: renderVideoStatus,
  // An unexpectedly closed video socket is re-attached only while the server still reports a
  // running watch (never to a session that has ended).
  canReattach: () => lastWatchState !== null && !lastWatchFinished,
  // The server closed the socket normally: ask it what happened (an ended watch is then shown
  // as ended at once, not up to a poll interval later).
  onServerClosed: () => refreshState(),
});
let videoActive = false;

// Stage 17A: the browser-side media measurements (frames received/decoded/displayed and their
// timings), for a person (the collapsed "Media stats" panel) and for automation. Read-only.
window.HolochainMoqMedia = () => videoWatch.mediaStats();
setInterval(() => {
  const box = el("media-details");
  if (!videoActive || !box.open) return;
  const m = videoWatch.mediaStats();
  delete m.frame_times;
  el("media-stats-text").textContent = JSON.stringify(m, (k, v) => (typeof v === "number" ? Math.round(v * 100) / 100 : v), 1);
}, 1000);

// Stage 15.6: can this browser show the video at all? Answered once, by feature detection
// (never the user-agent). `null` while the check runs. The server stays authoritative for
// everything about sessions; this only decides whether this page tries to play video.
let support = null;
HolochainMoqVideo.checkVideoSupport().then((result) => {
  support = result;
  refreshState();
});
let lastState = null; // the newest /api/state response (what the headline is derived from)
// How the last operation ended (`{error, kind, notice}`), kept - UI-local, display only - until
// the person starts something new. The server reports it in the *stop* response; a poll that was
// already in flight returns a plain `ready` right after, which would otherwise wipe "the
// publisher ended the session" (and make an ended session read as merely ready).
let outcome = null;
let lastWatchFinished = false;
let unreachable = false; // the last poll could not reach the app at all

const STALL_MS = 2000;
let lastVideoStatus = null;

/// The one status headline and the text over the video area, derived from facts other layers
/// already decided (see `describeStatus` in ui.js). Runs on every state response and every
/// decoder status change.
function renderHeadline() {
  const s = lastState;
  const h = HolochainMoqUi.describeStatus({
    unreachable,
    appState: s && s.state,
    watch: s && s.watch,
    share: s && s.share,
    lastError: outcome && outcome.error,
    lastErrorKind: outcome && outcome.kind,
    lastNotice: outcome && outcome.notice,
    support,
    video: lastVideoStatus,
    stalled: /^stalled/.test(el("video-status").textContent),
  });
  const box = el("status-headline");
  box.dataset.key = h.key;
  box.className = `headline tone-${h.tone}`;
  el("status-headline-label").textContent = h.label;
  el("status-headline-detail").textContent = h.detail;
  el("status-icon-use").setAttribute("href", `#i-${HolochainMoqUi.headlineIcon(h.key)}`);
  const stalled = /^stalled/.test(el("video-status").textContent);
  const frames = lastVideoStatus ? lastVideoStatus.frameCount : 0;
  const overlay = HolochainMoqUi.videoOverlay(h.key, frames, stalled);
  const wrap = el("video-wrap");
  wrap.dataset.key = h.key;
  // A last frame that is no longer live is dimmed AND labelled "Not live"; an ended/failed area
  // shows no picture at all - nothing frozen is ever presented as the live stream.
  const notLive = HolochainMoqUi.videoNotLive(h.key, frames, stalled);
  wrap.dataset.notlive = String(notLive);
  wrap.dataset.hidden = String(HolochainMoqUi.videoHidden(h.key));
  el("video-badge").classList.toggle("hidden", !notLive);
  el("video-overlay").textContent = overlay;
  el("video-overlay").classList.toggle("hidden", !overlay);
}

function renderVideoStatus(status) {
  lastVideoStatus = status;
  refreshVideoLabel();
  el("video-epoch").textContent = status.epoch;
  el("video-size").textContent =
    status.width && status.height ? `${status.width}x${status.height}` : "-";
  el("video-frames").textContent = status.frameCount;
  // A decoder that gave up is explained in words (the headline says it too); the raw browser
  // message stays available as the tooltip. A single recovered error is not shown at all.
  if (status.failure) {
    setVideoError(HolochainMoqUi.describePlaybackFailure(status.failure), status.failure.message);
  } else if (status.socket === "lost") {
    setVideoError("The video connection was lost. Leave and watch again to reconnect the video.", "");
  } else if (status.socket === "superseded") {
    setVideoError("This video is now shown in another tab or window.", "");
  } else {
    el("video-error").classList.add("hidden");
  }
  renderHeadline();
}

// The label also reflects real frame flow (see `describeVideoState` in video.js): a
// publisher that vanishes without a transport close is only noticed by the Rust side
// after the QUIC idle timeout, and `/api/state` says `live` until then. The note under the
// video is worded from the same facts (see `videoNote` in ui.js) - it decides nothing.
let lastWatchState = null;
let connectingSince = null; // UI-local: when the watch entered "connecting" (only used to word the note)
function refreshVideoLabel() {
  if (!lastVideoStatus) return;
  const label = HolochainMoqVideo.describeVideoState(
    lastVideoStatus.state,
    videoWatch.lastFrameAt,
    Date.now(),
    STALL_MS
  );
  el("video-status").textContent = label;
  renderHeadline();
  el("video-note").textContent = HolochainMoqUi.videoNote(
    lastWatchState,
    lastVideoStatus.state,
    lastVideoStatus.frameCount,
    label.startsWith("stalled"),
    connectingSince === null ? 0 : Date.now() - connectingSince
  );
}
setInterval(refreshVideoLabel, 500);

function setVideoError(text, raw) {
  el("video-error-text").textContent = text;
  el("video-error").title = raw || "";
  el("video-error").classList.remove("hidden");
}

/// `retry`: the app itself could not be reached, so the one useful thing is to ask again.
function showError(message, raw, retry = false) {
  const banner = el("error-banner");
  el("error-banner-text").textContent = message;
  banner.title = raw || "";
  el("error-actions").classList.toggle("hidden", !retry);
  banner.classList.remove("hidden");
}

function clearError() {
  el("error-banner").classList.add("hidden");
  el("error-banner-text").textContent = "";
  el("error-actions").classList.add("hidden");
}

// What `POST /api/watch` returned: this watch's id and its secret bearer token. Held in memory
// only. Presenting it on state polls keeps the watch attended; it is required to stop the watch.
let watchCred = null;

async function api(method, path, body) {
  const headers = body ? { "content-type": "application/json" } : {};
  if (watchCred && (path === "/api/state" || path.startsWith("/api/watch/"))) {
    headers.authorization = `Bearer ${watchCred.token}`;
  }
  const res = await fetch(path, {
    method,
    headers,
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(data.error || `${method} ${path} failed (${res.status})`);
    err.status = res.status;
    throw err;
  }
  return data;
}

/// The message for a failed request: a rejected one keeps the server's own wording (it is for
/// the person); an unreachable app or a server fault is described, not dumped as a raw error.
/// The raw text stays in the banner's tooltip.
function errText(e) {
  return HolochainMoqUi.describeRequestFailure(e && e.status, String((e && e.message) || e));
}

/// Disable `button` (and, if given, every element in `alsoDisable`) for the duration of
/// `fn`, so a rapid double-click cannot fire the same request twice while the first is
/// still in flight - the actual operation is already made safe server-side (a single
/// `Arc<Mutex<App>>` serializes every request), but a second, redundant request would
/// otherwise surface a confusing "cannot start/stop: already ..." error right after a
/// first request that *succeeded*. Always re-enables in `finally`, even on error.
async function withBusy(button, fn, alsoDisable = []) {
  const targets = [button, ...alsoDisable];
  for (const t of targets) t.disabled = true;
  try {
    await fn();
  } finally {
    for (const t of targets) t.disabled = false;
  }
}

function setPanels(state) {
  // Once the server reports an operation is actually active, that decides the visible
  // panel - a locally-clicked navigation choice never opens a second panel alongside it.
  const effectivePanel =
    state.state === "sharing" ? "share" : state.state === "watching" ? "watch" : panel;
  el("home-panel").classList.toggle("hidden", effectivePanel !== "menu");
  el("share-panel").classList.toggle("hidden", effectivePanel !== "share");
  el("watch-panel").classList.toggle("hidden", effectivePanel !== "watch");
  // The tab for the view on screen is marked (and underlined by the CSS), for assistive technology too.
  for (const [id, name] of [["btn-share", "share"], ["btn-watch", "watch"]]) {
    if (effectivePanel === name) el(id).setAttribute("aria-current", "page");
    else el(id).removeAttribute("aria-current");
  }
  // The top-level nav buttons cannot start the *other* flow while one is active - the
  // server already refuses this (409), but the button should not invite it.
  el("btn-share").disabled = state.state === "watching";
  // Watching needs a browser that can decode the video: without it, starting a watch would
  // only occupy a session nobody can see, so the button says why instead.
  const canPlay = support !== null && support.ok === true;
  el("btn-watch").disabled = state.state === "sharing" || !canPlay;
  el("btn-watch").title =
    support === null
      ? "Checking browser support..."
      : canPlay
        ? ""
        : HolochainMoqUi.describeSupport(support.reason);
  // The home cards mirror the tabs they stand for (same enabled state, same reason).
  el("home-share").disabled = el("btn-share").disabled;
  el("home-watch").disabled = el("btn-watch").disabled;
  el("home-watch").title = el("btn-watch").title;
  const banner = el("support-banner");
  banner.classList.toggle("hidden", support === null || support.ok === true);
  if (support && !support.ok) el("support-banner-text").textContent = HolochainMoqUi.describeSupport(support.reason);
  manageSessionsTimer(effectivePanel === "watch" && state.state === "ready");
}

function render(state) {
  unreachable = false;
  lastState = state;
  if (state.last_error || state.last_notice) {
    outcome = { error: state.last_error, kind: state.last_error_kind, notice: state.last_notice };
  } else if (state.state !== "ready") {
    outcome = null; // a new operation is running: the previous outcome is history
  }
  lastWatchFinished = !!(state.watch && state.watch.finished);
  el("app-state").textContent = state.state;
  // The server reports the agent only while it is `ready`; the person is the same one throughout, so
  // the last value seen stays on screen (display only - nothing decides anything from it).
  if (state.agent) lastAgent = state.agent;
  el("app-agent").textContent = lastAgent || "-";
  el("app-agent").title = lastAgent || "";

  // The server is authoritative here too: a `last_error` present in *this* response is
  // shown, and its absence clears whatever was shown before - an error from a past,
  // already-superseded operation must not linger forever once things are working again.
  if (outcome && outcome.error) {
    // `last_error_kind` is the server's typed classification of an existing Rust error
    // (unauthorized / expired / ended / not_found / publisher_unavailable / connection_failed);
    // each gets its own wording. Nothing here decides or infers authorization or lifecycle.
    showError(HolochainMoqUi.describeError(outcome.kind, outcome.error));
  } else {
    clearError();
  }
  // A watch that ended *without* an error: the publisher ending the session is worth
  // saying; the viewer's own Leave is not.
  const notice = HolochainMoqUi.describeNotice(outcome && outcome.notice);
  el("notice-banner-text").textContent = notice || "";
  el("notice-banner").classList.toggle("hidden", !notice);

  setPanels(state);

  // Share panel.
  const sharing = state.state === "sharing" && state.share;
  if (!sharing && endConfirming) setEndConfirm(false, false);
  el("share-form").classList.toggle("hidden", sharing);
  el("share-status").classList.toggle("hidden", !sharing);
  if (sharing) {
    el("share-title-shown").textContent = state.share.title || "Untitled session";
    el("share-session").textContent = state.share.session_id
      ? `${HolochainMoqUi.shortId(state.share.session_id)}... (${state.share.session_id})`
      : "-";
    const ss = HolochainMoqUi.describeShareState(state.share.state);
    const chipEl = el("share-live-state");
    chipEl.className = `state-chip tone-${ss.tone}`;
    chipEl.replaceChildren(icon(ss.icon), document.createTextNode(ss.label));
    el("share-lifetime").textContent = state.share.expires_at
      ? HolochainMoqUi.formatRemaining(state.share.expires_at - Date.now())
      : "-";
    el("share-source").textContent = HolochainMoqUi.describeShareSource(state.share.source);
    // Read back from the durable session record the publisher announced (not remembered
    // client-side), so what is shown is what viewers actually see.
    el("share-access").textContent =
      state.share.access === "allowed" ? "Private (allowed agents only)" : state.share.access === "public" ? "Public" : "-";
    const isPrivate = state.share.access === "allowed";
    el("share-allowed-row").classList.toggle("hidden", !isPrivate);
    el("share-allowed").textContent =
      isPrivate && state.share.allowed_agents && state.share.allowed_agents.length
        ? state.share.allowed_agents.join(", ")
        : "only you";
    el("share-objects").textContent = state.share.objects_published ?? "-";
    el("share-viewers").textContent = state.share.viewers ?? "-";
  }

  // Watch panel.
  const watching = state.state === "watching" && state.watch;
  el("watch-select").classList.toggle("hidden", watching);
  el("watch-status").classList.toggle("hidden", !watching);
  if (watching) {
    lastWatchState = state.watch.state;
    if (state.watch.state === "connecting") {
      if (connectingSince === null) connectingSince = Date.now();
    } else {
      connectingSince = null;
    }
    // Title/publisher/access come from the session list the server gave us at selection
    // time (display only). After a page refresh that is gone, so look it up again.
    const info = selectedSession || lastSessions.find((x) => x.session_id === state.watch.session_id);
    if (info) {
      const d = HolochainMoqUi.describeSession(info, Date.now());
      el("watch-title").textContent = d.title;
      el("watch-publisher").textContent = d.publisher;
      accessChip(d.isPrivate, el("watch-access"));
    } else {
      el("watch-title").textContent = `Session ${HolochainMoqUi.shortId(state.watch.session_id)}...`;
      el("watch-publisher").textContent = "-";
      el("watch-access").className = "chip chip-public hidden";
      el("watch-access").textContent = "-";
      refreshSessions(true);
    }
    const ws = HolochainMoqUi.describeWatchState(state.watch.state, state.watch.reconnect_attempt);
    el("watch-live-state").textContent = ws.label;
    el("watch-live-state").dataset.state = state.watch.state; // the server's own token, for tooling
    el("watch-live-state").className = `state-chip tone-${ws.tone}`;
    refreshVideoLabel();
  }
  // The browser video WebSocket/decoder starts once, on entering `watching` (any of its
  // sub-states - `connecting`/`live`/`reconnecting` all keep it running), and stops the
  // moment the server no longer reports `watching`, however that happened (a manual
  // Stop, the publisher ending the session, expiry, an unrecoverable reconnect failure -
  // `refreshState`'s existing auto-stop path above already reduces every one of those to
  // exactly this same `state.state` transition, so nothing extra is needed here).
  // ...but only in a browser that can show it (`support`); otherwise no socket is opened.
  if (watching && !videoActive && support && support.ok) {
    videoActive = true;
    videoWatch.start();
  } else if (!watching && videoActive) {
    videoActive = false;
    videoWatch.stop();
  }
  if (!watching && document.fullscreenElement) {
    document.exitFullscreen().catch(() => {}); // nothing left to show full screen
  }
  if (!watching) {
    // Nothing of a finished Watch may linger into the next one: not its numbers, its note,
    // or the session it was showing (its last frame is cleared by `videoWatch.stop()`).
    selectedSession = null;
    lastWatchState = null;
    connectingSince = null;
    lastVideoStatus = null;
    for (const id of ["video-status", "video-epoch", "video-size", "video-frames"]) {
      el(id).textContent = "-";
    }
    el("video-note").textContent = "";
    el("video-error").classList.add("hidden");
  }

  // Focus follows a real change of what is on screen, so a keyboard or screen-reader user is never
  // left on a control that has just disappeared: the new view's heading, or the page itself once a
  // session has ended.
  if (prevAppState !== null && state.state !== prevAppState) {
    if (state.state === "watching") el("watch-title").focus();
    else if (state.state === "sharing") el("share-title-shown").focus();
    else if (state.state === "ready" && (prevAppState === "watching" || prevAppState === "sharing")) el("main").focus();
  }
  prevAppState = state.state;

  renderHeadline();
  schedulePolling(state);
}
let prevAppState = null;
let lastAgent = null;

function schedulePolling(state) {
  const active = state.state === "sharing" || state.state === "watching";
  if (active && !pollTimer) {
    pollTimer = setInterval(refreshState, 1000);
  } else if (!active && pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function refreshState() {
  try {
    const state = await api("GET", "/api/state");
    render(state);
    // Mirrors `crates/app`'s own CLI loop (`share_finished()`/`watch_finished()` ->
    // `finish_share()`/`finish_watch()`): a background operation can conclude on its own
    // (the publisher ends the session, it expires, an unrecoverable reconnect failure)
    // without anyone pressing Stop. `finished` is already true in that case, but the app
    // only actually leaves `Sharing`/`Watching` once `finish_share`/`finish_watch` is
    // called - without this, the browser would keep showing a stale "connecting"/"live"
    // status forever. Reuses the exact same `/stop` endpoints and lifecycle a manual
    // Stop already uses; nothing new is implemented here.
    const needsAutoStop =
      (state.state === "sharing" && state.share && state.share.finished) ||
      (state.state === "watching" && state.watch && state.watch.finished);
    if (needsAutoStop && !autoStopping) {
      autoStopping = true;
      try {
        if (state.state === "sharing") {
          await stopShare();
        } else {
          await stopWatch();
        }
      } finally {
        autoStopping = false;
      }
    }
  } catch (e) {
    if (e && e.status === undefined) unreachable = true; // a network failure, not a rejection
    showError(errText(e), String((e && e.message) || e), unreachable);
    renderHeadline();
  }
}

// The joinable sessions as the server last reported them, and the one being watched (both
// display-only: the server, not this list, decides what is joinable).
let lastSessions = [];
let selectedSession = null;
let sessionsTimer = null;
let sessionsTickTimer = null;

function renderSessions() {
  const list = el("session-list");
  list.innerHTML = "";
  if (lastSessions.length === 0) {
    // An empty list is an ordinary state, so it explains itself and says what to do next.
    const li = document.createElement("li");
    li.className = "session-empty empty";
    const title = document.createElement("strong");
    title.textContent = "No live sessions right now";
    const text = document.createElement("p");
    text.textContent = "Sessions appear here while someone is sharing. This list refreshes on its own.";
    li.append(icon("watch"), title, text);
    list.appendChild(li);
    return;
  }
  for (const s of lastSessions) {
    const d = HolochainMoqUi.describeSession(s, Date.now());
    const li = document.createElement("li");
    li.className = "session";
    li.dataset.sessionId = s.session_id;
    const main = document.createElement("div");
    main.className = "session-main";
    const title = document.createElement("span");
    title.className = "session-title";
    title.textContent = d.title;
    const meta = document.createElement("div");
    meta.className = "session-meta";
    // Discovery is not authorization: a private session is listed to everyone, and only the
    // publisher's admission check decides who actually receives video. The row says so.
    const publisher = document.createElement("span");
    publisher.textContent = `${d.source} \u00b7 shared by ${d.publisher}`;
    publisher.title = s.publisher || "";
    const expires = document.createElement("span");
    expires.className = "session-expires";
    expires.dataset.expiresAt = String(s.expires_at);
    expires.textContent = d.expired ? "expired" : `listed for ${d.expires}`;
    expires.title = "How long this session stays listed. A publisher that has gone offline can stay listed until then.";
    const idline = document.createElement("span");
    idline.className = "mono";
    idline.textContent = `id ${d.shortId}...`;
    idline.title = s.session_id; // the full id stays available for copying/debugging
    meta.append(accessChip(d.isPrivate), publisher, expires, idline);
    main.append(title, meta);
    if (d.isPrivate) {
      const note = document.createElement("span");
      note.className = "session-note";
      note.textContent = d.accessNote;
      main.appendChild(note);
    }
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn";
    btn.append(icon("watch"), document.createTextNode("Watch"));
    btn.setAttribute("aria-label", d.actionLabel);
    btn.onclick = () => withBusy(btn, () => startWatch(s));
    li.classList.toggle("is-expired", d.expired);
    if (d.expired) btn.disabled = true;
    li.append(main, btn);
    list.appendChild(li);
  }
}

// Remaining time is counted down locally between refreshes (display only). A row whose time has run
// out stops offering Watch - it never keeps inviting a join to something that may be gone. The
// server still decides what is really joinable, and the next refresh removes the row.
function tickSessionTimes() {
  for (const li of document.querySelectorAll("#session-list li.session")) {
    const s = lastSessions.find((x) => x.session_id === li.dataset.sessionId);
    if (!s) continue;
    const d = HolochainMoqUi.describeSession(s, Date.now());
    li.querySelector(".session-expires").textContent = d.expired ? "expired" : `listed for ${d.expires}`;
    li.classList.toggle("is-expired", d.expired);
    const btn = li.querySelector("button");
    if (btn && d.expired && !btn.disabled) btn.disabled = true;
    if (btn) btn.setAttribute("aria-label", d.actionLabel);
  }
}

/// Fetch the joinable list. `quiet` (a background refresh) never raises an error banner and
/// keeps showing the previous list if the fetch fails.
async function refreshSessions(quiet = false) {
  try {
    lastSessions = await api("GET", "/api/sessions");
    renderSessions();
    el("sessions-updated").textContent = `(updated ${new Date().toLocaleTimeString()})`;
  } catch (e) {
    if (!quiet) showError(errText(e), String((e && e.message) || e));
    else el("sessions-updated").textContent = "(could not refresh)";
  }
}

async function loadSessions() {
  await refreshSessions(false);
}

// While the Watch list is what's on screen (Ready, Watch panel open) it refreshes itself, so
// a session that ended or expired disappears and a new one appears without a click.
function manageSessionsTimer(visible) {
  if (visible && !sessionsTimer) {
    sessionsTimer = setInterval(() => refreshSessions(true), 3000);
    sessionsTickTimer = setInterval(tickSessionTimes, 1000);
  } else if (!visible && sessionsTimer) {
    clearInterval(sessionsTimer);
    clearInterval(sessionsTickTimer);
    sessionsTimer = sessionsTickTimer = null;
  }
}

async function startShare() {
  clearError();
  const source = document.querySelector('input[name="source"]:checked').value;
  const access = document.querySelector('input[name="access"]:checked').value;
  const allowedRaw = el("allowed-agents").value;
  const allowed_agents = allowedRaw
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const title = el("share-title").value.trim();
  try {
    const state = await api("POST", "/api/share", { source, access, allowed_agents, title });
    render(state);
  } catch (e) {
    showError(errText(e), String((e && e.message) || e));
  }
}

async function stopShare() {
  clearError();
  try {
    const state = await api("POST", "/api/share/stop");
    render(state);
  } catch (e) {
    showError(errText(e), String((e && e.message) || e));
  }
}

async function startWatch(session) {
  videoWatch.media.markRequested(); // the click: "time to first displayed frame" starts here
  clearError();
  selectedSession = session; // for display only: title/publisher/access of what is being watched
  try {
    const state = await api("POST", "/api/watch", { session_id: session.session_id });
    watchCred = state.watch_token && state.watch ? { id: state.watch.watch_id, token: state.watch_token } : null;
    render(state);
  } catch (e) {
    selectedSession = null;
    showError(errText(e), String((e && e.message) || e));
  }
}

async function stopWatch() {
  clearError();
  try {
    if (!watchCred) {
      // This page did not start the running watch (e.g. it was reloaded): it cannot stop it.
      // The server leaves a watch nobody owns-and-attends after its idle limit.
      throw new Error("this page does not own the running watch; it will end on its own");
    }
    const cred = watchCred;
    const state = await api("POST", `/api/watch/${encodeURIComponent(cred.id)}/stop`);
    watchCred = null;
    render(state);
  } catch (e) {
    showError(errText(e), String((e && e.message) || e));
  }
}

el("btn-share").onclick = () => {
  panel = "share";
  outcome = null;
  clearError();
  refreshState();
};
el("btn-watch").onclick = () => {
  panel = "watch";
  outcome = null;
  clearError();
  loadSessions();
  refreshState();
};
el("home-share").onclick = () => el("btn-share").click();
el("home-watch").onclick = () => el("btn-watch").click();
el("btn-retry").onclick = () => {
  clearError();
  refreshState();
};
el("btn-start-share").onclick = () => withBusy(el("btn-start-share"), startShare);

// Ending a share affects everyone watching, so it takes two deliberate steps: the button opens an
// inline confirmation whose safe choice ("Keep sharing") has the focus, and Escape backs out. The
// confirmation is UI-local; what actually ends is still the one existing `/api/share/stop` call.
let endConfirming = false;
function setEndConfirm(on, moveFocus = true) {
  endConfirming = on;
  el("end-confirm").classList.toggle("hidden", !on);
  el("btn-stop-share").setAttribute("aria-expanded", String(on));
  if (!moveFocus) return;
  (on ? el("btn-cancel-end") : el("btn-stop-share")).focus();
}
el("btn-stop-share").setAttribute("aria-expanded", "false");
el("btn-stop-share").onclick = () => setEndConfirm(true);
el("btn-cancel-end").onclick = () => setEndConfirm(false);
el("btn-confirm-end").onclick = () =>
  withBusy(
    el("btn-confirm-end"),
    async () => {
      await stopShare();
      setEndConfirm(false, false);
    },
    [el("btn-cancel-end")]
  );
el("end-confirm").addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") {
    ev.stopPropagation();
    setEndConfirm(false);
  }
});

// Video stage controls. Fit (the whole picture, letterboxed) is the default; Fill crops to cover.
// Both are CSS only, and the choice is a per-viewer convenience kept in localStorage when it works.
const FIT_KEY = "holochain-moq.video-fit";
function setFit(mode, remember = true) {
  const fit = HolochainMoqUi.normalizeFit(mode);
  el("video-wrap").dataset.fit = fit;
  el("btn-fit").setAttribute("aria-pressed", String(fit === "fit"));
  el("btn-fill").setAttribute("aria-pressed", String(fit === "fill"));
  if (!remember) return;
  try {
    localStorage.setItem(FIT_KEY, fit);
  } catch (_) {
    /* storage may be blocked (private windows): the choice then simply lasts for this page */
  }
}
let savedFit = null;
try {
  savedFit = localStorage.getItem(FIT_KEY);
} catch (_) {
  /* unavailable: the default applies */
}
setFit(savedFit, false);
el("btn-fit").onclick = () => setFit("fit");
el("btn-fill").onclick = () => setFit("fill");

// Fullscreen applies to the whole stage (picture, overlay and controls), so an overlay such as
// "Reconnecting..." is still visible in fullscreen. Hidden where the browser offers no Fullscreen API.
function syncFullscreen() {
  const on = document.fullscreenElement === el("video-stage");
  el("btn-fullscreen").setAttribute("aria-pressed", String(on));
  el("fullscreen-label").textContent = on ? "Exit fullscreen" : "Fullscreen";
}
if (!document.fullscreenEnabled) el("btn-fullscreen").classList.add("hidden");
el("btn-fullscreen").onclick = async () => {
  try {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await el("video-stage").requestFullscreen();
  } catch (_) {
    /* the browser refused (no user gesture, policy): nothing to report, the picture is unchanged */
  }
};
document.addEventListener("fullscreenchange", syncFullscreen);
el("btn-refresh-sessions").onclick = () => withBusy(el("btn-refresh-sessions"), loadSessions);
el("btn-stop-watch").onclick = () => withBusy(el("btn-stop-watch"), stopWatch);
document.querySelectorAll('input[name="access"]').forEach((radio) => {
  radio.addEventListener("change", () => {
    el("allowed-agents").disabled = radio.value !== "allowed" || !radio.checked;
  });
});

// A page coming back (the tab shown again, or restored by back/forward) re-reads the server's
// state straight away instead of waiting for the next poll; a restored page also drops its
// stale video socket so the watch re-attaches cleanly.
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") refreshState();
});
window.addEventListener("pageshow", (ev) => {
  if (ev.persisted) {
    videoWatch.stop();
    videoActive = false;
    refreshState();
  }
});

refreshState();
