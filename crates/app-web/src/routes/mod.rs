//! HTTP routes: thin translation only. Every handler in `state`/`share`/`watch` follows
//! the same shape - lock [`AppHandle`], call exactly one existing `App` method (plus, for
//! Share/Watch, construct the same existing video source/sink `holochain-moq-app`'s own
//! `main.rs` already constructs), map the result through `crate::view`, respond. No
//! discovery, expiry, admission, reconnect, session-ending or transport logic is
//! implemented here - see this crate's README, "Architectural rules", for the boundary
//! this module is required to hold.
//!
//! Generic over `D: SessionDirectory` so the exact same router runs against
//! `HolochainPeerDirectory` (the production binary, see `main.rs`) or
//! `InMemoryPeerDirectory` (this crate's own tests, no conductor needed) - the same
//! pattern `App<D>` itself already uses.

pub mod share;
pub mod state;
pub mod watch;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use holochain_moq_app::{App, AppState};
use holochain_moq_node::SessionDirectory;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The one piece of shared state every handler needs: `App` wrapped in an async mutex
/// (handlers hold it across `.await` points, e.g. `start_share`/`finish_share`) - a
/// web-layer-local synchronization concern, not a change to `App` itself (see 14H.0's
/// investigation report). `Clone` is cheap (an `Arc` clone) since `axum` hands a fresh
/// clone of the router's state to every handler.
pub struct AppHandle<D: SessionDirectory + Send + Sync + 'static> {
    pub app: Arc<Mutex<App<D>>>,
    /// Stage 15.3: whether anyone is still attending to this server (see [`Liveness`]).
    pub liveness: Arc<Liveness>,
    /// Stage 15.4: the app's lifecycle recorder, cloned out so a request can record events
    /// (including how long it waited for the app lock) before it holds the lock.
    pub lifecycle: Arc<holochain_moq_app::lifecycle::Lifecycle>,
    /// Who owns the active watch (its id, token hash, and owner-attendance clock); see
    /// [`crate::ownership`]. A std mutex, held only for short, non-awaiting sections.
    pub watches: Arc<std::sync::Mutex<crate::ownership::WatchRegistry>>,
}

impl<D: SessionDirectory + Send + Sync + 'static> Clone for AppHandle<D> {
    fn clone(&self) -> Self {
        Self {
            app: self.app.clone(),
            liveness: self.liveness.clone(),
            lifecycle: self.lifecycle.clone(),
            watches: self.watches.clone(),
        }
    }
}

impl<D: SessionDirectory + Send + Sync + 'static> AppHandle<D> {
    pub fn new(app: App<D>) -> Self {
        let lifecycle = app.lifecycle();
        Self {
            app: Arc::new(Mutex::new(app)),
            liveness: Arc::new(Liveness::new()),
            lifecycle,
            watches: Arc::default(),
        }
    }

    /// The watch registry. A poisoned lock only means another request panicked mid-update of
    /// plain bookkeeping; the data is still usable.
    pub(crate) fn registry(&self) -> std::sync::MutexGuard<'_, crate::ownership::WatchRegistry> {
        self.watches.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Stage 15.3: web-layer bookkeeping about the *browser*, never about the session (session
/// lifecycle stays entirely `App`'s). Two facts, both presentation-side:
///
/// * whether a page is still attending - it polls `/api/state`/`/api/sessions`, or holds the
///   video WebSocket open - so a Watch whose page was closed or crashed can be left instead
///   of running forever (see [`spawn_abandoned_watch_watchdog`]);
/// * which video WebSocket is *the* consumer: a newer connection (a refreshed page)
///   supersedes an older one, so the encoded queue never has two readers splitting frames.
pub struct Liveness {
    last_seen_ms: std::sync::atomic::AtomicI64,
    ws_open: std::sync::atomic::AtomicUsize,
    ws_generation: std::sync::atomic::AtomicU64,
}

impl Liveness {
    fn new() -> Self {
        let l = Self {
            last_seen_ms: Default::default(),
            ws_open: Default::default(),
            ws_generation: Default::default(),
        };
        l.touch();
        l
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// A page just talked to us.
    pub fn touch(&self) {
        self.last_seen_ms
            .store(Self::now_ms(), std::sync::atomic::Ordering::SeqCst);
    }

    pub fn since_seen(&self) -> std::time::Duration {
        let last = self.last_seen_ms.load(std::sync::atomic::Ordering::SeqCst);
        std::time::Duration::from_millis((Self::now_ms() - last).max(0) as u64)
    }

    pub fn video_sockets_open(&self) -> usize {
        self.ws_open.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Register a new video consumer: returns its generation and a guard that keeps it
    /// counted as open. Any older consumer sees its generation is no longer current.
    ///
    /// Only an AUTHENTICATED socket (valid token for the active watch) may ever reach this;
    /// registering is what supersedes the previous consumer, so it is the admission boundary
    /// (`video_ws::get_watch_video` + the re-check in `forward`).
    pub(crate) fn register_video_socket(self: &Arc<Self>) -> (u64, VideoSocketGuard) {
        use std::sync::atomic::Ordering::SeqCst;
        let generation = self.ws_generation.fetch_add(1, SeqCst) + 1;
        self.ws_open.fetch_add(1, SeqCst);
        (generation, VideoSocketGuard(self.clone()))
    }

    pub(crate) fn is_current_video_socket(&self, generation: u64) -> bool {
        self.ws_generation.load(std::sync::atomic::Ordering::SeqCst) == generation
    }
}

pub(crate) struct VideoSocketGuard(Arc<Liveness>);

impl Drop for VideoSocketGuard {
    fn drop(&mut self) {
        self.0
            .ws_open
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.touch();
    }
}

/// Leave a Watch that nobody is attending: no page has polled for `idle` **and** no video
/// WebSocket is open. Checked every `tick`. This only ever ends the *local viewer* (the same
/// `request_stop_watch`/`finish_watch` a Leave uses - never the publisher's session), and
/// never touches a Share: a page refresh or close must not end someone's screen share.
/// Without it, closing the tab left the Rust watch task, its QUIC connection and its queue
/// alive indefinitely, since the page's own polling is what normally notices a finished watch.
/// A hidden tab is safe: browsers may throttle its timers, but its open WebSocket still counts.
pub fn spawn_abandoned_watch_watchdog<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    idle: std::time::Duration,
    tick: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            // Attendance is the OWNER's: a state poll or video socket that presents the watch's
            // token. A page that merely keeps polling (a stale tab, a reloaded page that lost
            // its token, a restarted proxy's leftovers) must not keep an orphaned watch alive.
            // A watch with no grant (not started through `POST /api/watch`) keeps the older
            // any-page-contact rule.
            let owner_idle = handle.registry().owner_idle_for();
            let attended = match owner_idle {
                Some(since_owner) => since_owner < idle,
                None => handle.liveness.since_seen() < idle,
            };
            if handle.liveness.video_sockets_open() > 0 || attended {
                continue;
            }
            let mut app = handle.app.lock().await;
            if app.watch_session_id().is_some() {
                // Diagnosable: WHY this watch was judged abandoned, from the same facts the
                // decision used (Stage 15.4). The policy itself (`idle`) is unchanged.
                if let Some(op) = app.watch_op() {
                    let detail = format!(
                        "no owner contact for {}ms (limit {}ms), video sockets open: {}, watch state: {:?}",
                        owner_idle
                            .unwrap_or_else(|| handle.liveness.since_seen())
                            .as_millis(),
                        idle.as_millis(),
                        handle.liveness.video_sockets_open(),
                        app.watch_connection_state(),
                    );
                    handle.lifecycle.terminal(
                        &op,
                        "watch.abandoned",
                        Some("abandoned"),
                        Some(detail),
                    );
                    crate::diag::log_failure_snapshot(&handle, &app, &op, "abandoned");
                }
                tracing::warn!("no page is attending this watch; leaving it");
                app.request_stop_watch();
                let end = app.finish_watch().await;
                // The owner's token stays valid for repeats: a later stop replays this result.
                watch::end_current_watch(&handle, &app, end);
            }
        }
    })
}

/// Stage 15.5: end whatever the app is doing, cleanly, before the process exits (SIGINT/SIGTERM).
/// A live Share is stopped through the same bounded 14E path as `POST /api/share/stop`, so its
/// viewers see the session *end* (not a lost transport that they would retry against); an active
/// Watch is left through the same path as `POST /api/watch/stop`. Bounded by `limit` overall: a
/// handler holding the app lock, or a drain that never completes, must not block exit forever.
/// Returns whether everything ended within the bound.
pub async fn shutdown_app<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    limit: std::time::Duration,
) -> bool {
    tokio::time::timeout(limit, async {
        let mut app = handle.app.lock().await;
        if app.request_stop() {
            tracing::info!("shutdown: ending the active share");
            if let Err(e) = app.finish_share().await {
                tracing::warn!("shutdown: the share did not end cleanly: {e:#}");
            }
        }
        if app.request_stop_watch() {
            tracing::info!("shutdown: leaving the active watch");
            let _ = app.finish_watch().await;
        }
    })
    .await
    .is_ok()
}

/// Run a state-changing handler's whole body to completion **even if the request that started
/// it is dropped**. hyper drops a handler's future when its client goes away (a closed tab, a
/// refresh, a crashed browser); a handler cancelled between two `App` transitions - after
/// `choose_share()` but before `start_share` finished, or after `finish_share()` took the
/// active share but before it resolved - left the app wedged in a state nothing could leave
/// (found by the Stage 15.3 manual run). Spawning the body means the transition always runs to
/// its end; the request future merely waits for the result, and if it is dropped the result is
/// discarded and the app is still consistent.
pub(crate) async fn detached<T, F>(fut: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, ApiError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(result) => result,
        Err(e) => Err(ApiError::internal(format!("the request task failed: {e}"))),
    }
}

/// [`detached`], recording the task's lifecycle on `op` (Stage 15.4): `http.task_spawned`, and -
/// from inside the spawned task, so it is recorded even if the request that started it is
/// dropped - `http.task_completed` with the elapsed time. A rejected request becomes the
/// operation's terminal event, classified from its status (`invalid_input` for 4xx-validation,
/// `request_rejected` for a conflict); a task that panicked or was cancelled is an
/// `internal_task_failure`. `failed` names the terminal event for the operation's kind.
pub(crate) async fn detached_op<T, F>(
    lifecycle: Arc<holochain_moq_app::lifecycle::Lifecycle>,
    op: holochain_moq_app::lifecycle::OpId,
    failed: &'static str,
    fut: F,
) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, ApiError>> + Send + 'static,
{
    lifecycle.event(&op, "http.task_spawned", None);
    let started = std::time::Instant::now();
    let (lc, op_task) = (lifecycle.clone(), op.clone());
    let task = tokio::spawn(async move {
        let result = fut.await;
        let elapsed = started.elapsed().as_millis();
        match &result {
            Ok(_) => lc.event(
                &op_task,
                "http.task_completed",
                Some(format!("{elapsed}ms")),
            ),
            Err(e) => {
                lc.event(
                    &op_task,
                    "http.task_completed",
                    Some(format!("{elapsed}ms, rejected {}", e.status.as_u16())),
                );
                let kind = if e.status == StatusCode::CONFLICT {
                    "request_rejected"
                } else if e.status == StatusCode::INTERNAL_SERVER_ERROR {
                    "internal_error"
                } else {
                    "invalid_input"
                };
                lc.terminal(&op_task, failed, Some(kind), Some(e.message.clone()));
            }
        }
        result
    });
    match task.await {
        Ok(result) => result,
        Err(e) => {
            lifecycle.terminal(&op, failed, Some("internal_task_failure"), None);
            Err(ApiError::internal(format!("the request task failed: {e}")))
        }
    }
}

/// A JSON `{"error": "..."}` body with a normal HTTP status - never a backtrace or panic
/// payload, always the same `Display` text (`{:#}`) `holochain-moq-app`'s own CLI already
/// prints for the same underlying error.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn gone(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::GONE,
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // The operation was rejected (a bind/announce failure, an admission/expiry
        // refusal reported by the existing `WatchError`/`Sharer` machinery, etc.) - a
        // client-correctable 4xx, not a server fault. `{e:#}` is exactly the text the CLI
        // already prints; nothing here inspects the error's type or duplicates what it
        // means.
        Self::bad_request(format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

/// Return the app to `Ready` if a failed operation left it anywhere else - the exact same
/// safety net `holochain-moq-app`'s own `main.rs` already applies after `run_share`/
/// `run_watch`, factored out here since multiple handlers need it. `App`'s own methods
/// already do this themselves in every case they can (see `start_share`/`start_watch`'s
/// docs); this only covers the same residual "rejected before anything started" cases the
/// CLI's outer loop already has to handle (e.g. an unrecognized source, or a capture
/// device failing to open).
pub async fn ensure_ready<D: SessionDirectory + Send + Sync + 'static>(app: &mut App<D>) {
    if !matches!(app.state(), AppState::Ready { .. }) {
        let _ = app.return_to_menu().await;
    }
}

/// The frontend is three static files, compiled into the binary (`include_str!`) rather
/// than read from disk at runtime - no path-resolution dependency on the working
/// directory the binary happens to be launched from, and no new dependency (e.g.
/// `tower-http`'s `ServeDir`) for three files. Served same-origin as the API, so no CORS
/// configuration exists to get wrong (see the README, "Local-only security model").
async fn index() -> impl IntoResponse {
    ([("content-type", "text/html; charset=utf-8")], INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    ([("content-type", "text/javascript; charset=utf-8")], APP_JS)
}

async fn video_js() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        VIDEO_JS,
    )
}

async fn ui_js() -> impl IntoResponse {
    ([("content-type", "text/javascript; charset=utf-8")], UI_JS)
}

async fn style_css() -> impl IntoResponse {
    ([("content-type", "text/css; charset=utf-8")], STYLE_CSS)
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const VIDEO_JS: &str = include_str!("../static/video.js");
const UI_JS: &str = include_str!("../static/ui.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

/// Largest request body any route accepts. The biggest legitimate one is a Share request with
/// a full allowlist (see `holochain_moq_app::MAX_ALLOWED_AGENTS`), a few tens of KiB at most.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// The router with the loopback [`HostPolicy`] (the default posture, and the one every test uses).
pub fn router<D: SessionDirectory + Send + Sync + 'static>(handle: AppHandle<D>) -> Router {
    router_with_policy(handle, crate::guard::HostPolicy::Loopback)
}

pub fn router_with_policy<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    policy: crate::guard::HostPolicy,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/video.js", get(video_js))
        .route("/ui.js", get(ui_js))
        .route("/style.css", get(style_css))
        .route("/api/state", get(state::get_state::<D>))
        .route("/api/sessions", get(watch::get_sessions::<D>))
        .route("/api/share", post(share::post_share::<D>))
        .route("/api/share/stop", post(share::post_share_stop::<D>))
        .route("/api/watch", post(watch::post_watch::<D>))
        // Scoped to ONE watch and authorised by that watch's bearer token (see `ownership`).
        .route(
            "/api/watch/{watch_id}/stop",
            post(watch::post_watch_stop::<D>),
        )
        // The old unscoped stop ended whatever watch was active; it is gone.
        .route("/api/watch/stop", post(watch::post_watch_stop_removed))
        .route("/api/diagnostics", get(state::get_diagnostics::<D>))
        .route("/api/media/timings", get(state::get_media_timings::<D>))
        .route(
            "/api/watch/video",
            get(crate::video_ws::get_watch_video::<D>),
        )
        .with_state(handle)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            policy,
            crate::guard::guard,
        ))
}
