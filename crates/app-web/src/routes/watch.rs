//! `GET /api/sessions`, `POST /api/watch`, `POST /api/watch/stop`.

use super::{detached, detached_op, ensure_ready, ApiError, AppHandle};
use crate::view::{error_kind, sessions_view, watch_end_notice, AppStateView, SessionView};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::Json;
use holochain_moq_app::lifecycle::{OpId, OpKind};
use holochain_moq_node::{RetryPolicy, SessionDirectory};
use holochain_moq_protocol::SessionId;
use serde::Deserialize;

/// Bounded to roughly two seconds of live video at the existing default 30 fps /
/// keyframe-every-5 encoder settings (see `crates/app`'s `EncodedVideoQueue` docs for
/// why this must stay bounded rather than growing without limit): enough that a normal
/// `DRAIN_INTERVAL` tick (20ms, see `crate::video_ws`) never starves the browser, small
/// enough that a stalled/absent browser cannot make this process's memory grow.
const ENCODED_QUEUE_CAPACITY: usize = 64;

/// A pure query - `App::discover_sessions` requires no particular `state()` and changes
/// none, so this never touches the app's menu/lifecycle state (see its own docs). The
/// existing Stage 14D live/unexpired/endpoint-fresh discovery rule is applied entirely by
/// `holochain_moq_screenshare::joinable_sessions`, called through `App` unchanged; this
/// handler does not re-check or duplicate any of it.
pub async fn get_sessions<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    handle.liveness.touch();
    let app = handle.app.lock().await;
    let sessions = app
        .discover_sessions()
        .await
        .map_err(|e| ApiError::internal(format!("discovering live sessions: {e}")))?;
    Ok(Json(sessions_view(&sessions)))
}

#[derive(Debug, Deserialize)]
pub struct WatchRequest {
    pub session_id: String,
}

pub async fn post_watch<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    Json(req): Json<WatchRequest>,
) -> Result<Json<AppStateView>, ApiError> {
    // The operation begins here, BEFORE anything else can wait (parsing, the app lock), so a
    // Watch that never starts is still visible as `watch.requested` and where it stopped.
    let lifecycle = handle.lifecycle.clone();
    let op = lifecycle.begin(OpKind::Watch, "POST /api/watch");
    detached_op(
        lifecycle,
        op.clone(),
        "watch.failed",
        watch_inner(handle, req, op),
    )
    .await
}

async fn watch_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    req: WatchRequest,
    op: OpId,
) -> Result<Json<AppStateView>, ApiError> {
    let session_id: SessionId = req
        .session_id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid session id"))?;
    handle.lifecycle.set_session(&op, session_id);

    let waited = std::time::Instant::now();
    let mut app = handle.app.lock().await;
    handle.lifecycle.event(
        &op,
        "http.lock_acquired",
        Some(format!("waited {}ms", waited.elapsed().as_millis())),
    );

    if !app.choose_watch() {
        return Err(ApiError::conflict(
            "cannot start watching: the app is not at Ready (a share or watch is already active)",
        ));
    }
    if !app.enter_selecting() {
        // Cannot happen on the path just taken (`choose_watch` always leaves the app in
        // `Discovering`, which is exactly what `enter_selecting` requires) - handled
        // defensively rather than assumed, matching `App`'s own precondition checks.
        ensure_ready(&mut app).await;
        return Err(ApiError::internal("could not enter session selection"));
    }

    // Stage 14H.5: the decode-free watch (`App::start_watch_encoded`) - this process
    // never decodes the browser's video itself, so there is no `VideoSink`/`Progress`
    // pair to construct here any more (see the README, "Browser video" section, for why
    // the 14H.1 `placeholder_watch_sink` no longer applies). Existence/joinability/
    // expiry/admission are all checked inside `start_watch_encoded`'s own background
    // task (`watch_encoded` -> `find_joinable_session`), exactly as for `start_watch` -
    // not re-checked here. An unknown or no-longer-joinable `session_id` is therefore
    // *not* a synchronous error from this endpoint: it surfaces on a later
    // `GET /api/state` once the background attempt concludes (`watch.finished == true`
    // and, after `POST /api/watch/stop`, the real error text).
    app.adopt_op(op);
    match app
        .start_watch_encoded(session_id, RetryPolicy::default(), ENCODED_QUEUE_CAPACITY)
        .await
    {
        Ok(_queue) => {
            // Mint this watch's identity and its bearer token. The token is returned HERE,
            // once, and is never part of `GET /api/state`; `session_id` is public and is
            // never authority.
            let (watch_id, token) = handle.registry().issue(&session_id.to_string());
            if let Some(op) = app.watch_op() {
                handle
                    .lifecycle
                    .event(&op, "watch.granted", Some(format!("watch_id={watch_id}")));
            }
            Ok(Json(
                AppStateView::from_app(&app)
                    .with_watch_id(Some(&watch_id))
                    .with_watch_token(token),
            ))
        }
        Err(e) => {
            ensure_ready(&mut app).await;
            Err(ApiError::from(e))
        }
    }
}

/// The removed unscoped stop. Answers clearly rather than 404 so an old client learns why.
pub async fn post_watch_stop_removed() -> ApiError {
    ApiError::gone(
        "POST /api/watch/stop was removed: use POST /api/watch/{watch_id}/stop with \
         `Authorization: Bearer <watch_token>` (both returned by POST /api/watch)",
    )
}

/// Record how the active watch ended so a repeated or late stop for it replays the same
/// answer, and return that answer. Used by every path that ends a watch (stop, watchdog).
pub(crate) fn end_current_watch<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    app: &holochain_moq_app::App<D>,
    finished: anyhow::Result<holochain_moq_screenshare::PlayEnd>,
) -> AppStateView {
    let view = match finished {
        Ok(end) => AppStateView::from_app(app).with_notice(watch_end_notice(end)),
        Err(e) => AppStateView::from_app(app)
            .with_last_error(format!("{e:#}"))
            .with_error_kind(error_kind(&e)),
    };
    handle
        .registry()
        .end_current(serde_json::to_value(&view).unwrap_or(serde_json::Value::Null));
    view
}

/// `POST /api/watch/{watch_id}/stop` with `Authorization: Bearer <watch_token>`.
///
/// * missing/malformed credentials: 401; wrong token for that watch: 403; unknown watch: 404;
/// * the active watch with its token: stop it (strictly local - never touches the publisher);
/// * a watch that already ended, with its token: 200, replaying the ending result - so a
///   repeated stop (pagehide racing Leave, a retry) is safe, and a LATE stop can never
///   reach a newer watch because it is matched to its own watch id and token only.
pub async fn post_watch_stop<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    Path(watch_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::ownership::{bearer, StopCheck};
    let Some(token) = bearer(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    ) else {
        return Err(ApiError::unauthorized(
            "stopping a watch needs `Authorization: Bearer <watch_token>`",
        ));
    };
    let token = token.to_string();
    // Cheap rejections and replays never wait for the app lock.
    match handle.registry().check_stop(&watch_id, &token) {
        StopCheck::Unknown => {
            return Err(ApiError::not_found(
                "no such watch (it may have ended long ago)",
            ))
        }
        StopCheck::BadToken => {
            tracing::warn!(%watch_id, "rejected a stop with the wrong token");
            return Err(ApiError::forbidden("that token does not own this watch"));
        }
        StopCheck::AlreadyEnded(view) => return Ok(Json(view)),
        StopCheck::Current => {}
    }
    detached(watch_stop_inner(handle, watch_id, token)).await
}

async fn watch_stop_inner<D: SessionDirectory + Send + Sync + 'static>(
    handle: AppHandle<D>,
    watch_id: String,
    token: String,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::ownership::StopCheck;
    let mut app = handle.app.lock().await;
    // Re-checked under the app lock: a concurrent stop or the watchdog may have ended this
    // watch (or a newer one begun) while this request waited.
    match handle.registry().check_stop(&watch_id, &token) {
        StopCheck::Current => {}
        StopCheck::AlreadyEnded(view) => return Ok(Json(view)),
        StopCheck::BadToken => {
            return Err(ApiError::forbidden("that token does not own this watch"))
        }
        StopCheck::Unknown => return Err(ApiError::not_found("no such watch")),
    }
    // Defence in depth: the grant records which session this watch was started for; the app's
    // active watch must still be exactly that one, or nothing is stopped.
    let granted = handle.registry().current_session_id().map(str::to_owned);
    if let Some(active) = app.watch_session_id() {
        if granted.as_deref() != Some(active.to_string().as_str()) {
            return Err(ApiError::conflict(
                "the active watch is not the one this token was issued for",
            ));
        }
    }
    let op = app.watch_op();
    let started = std::time::Instant::now();
    if let Some(op) = &op {
        handle.lifecycle.event(op, "http.stop_received", None);
    }

    if !app.request_stop_watch() {
        // This owner's watch is gone from the app already (nothing left to stop): settle the
        // grant so repeats are idempotent, and report the app as it is.
        let view = AppStateView::from_app(&app);
        handle
            .registry()
            .end_current(serde_json::to_value(&view).unwrap_or(serde_json::Value::Null));
        return Ok(Json(serde_json::to_value(view).unwrap_or_default()));
    }

    // Blocks only until the background task reports (immediately, if it had already
    // finished on its own) - never touches the publisher (`request_stop_watch` is
    // strictly the local subscriber's own cancellation, unchanged from 14G.3).
    let finished = app.finish_watch().await;
    if let Some(op) = &op {
        handle.lifecycle.event(
            op,
            "http.stop_completed",
            Some(format!("{}ms", started.elapsed().as_millis())),
        );
    }
    let view = end_current_watch(&handle, &app, finished);
    Ok(Json(serde_json::to_value(view).unwrap_or_default()))
}
