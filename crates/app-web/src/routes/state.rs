//! `GET /api/state`: the one endpoint the browser polls.

use super::AppHandle;
use crate::view::AppStateView;
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::Json;
use holochain_moq_node::SessionDirectory;

pub async fn get_state<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    headers: HeaderMap,
) -> Json<AppStateView> {
    handle.liveness.touch();
    // A poll that presents the active watch's token is the OWNER attending it; anyone else's
    // poll is allowed (read-only) but does not keep the watch alive (see `ownership`).
    if let Some(token) = crate::ownership::bearer(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    ) {
        handle.registry().owner_contact(token);
    }
    let mut app = handle.app.lock().await;
    // `App::refresh_watch` is what reflects the background watch's underlying
    // `ConnectionState` into `state()` (`Watching(Connecting)` -> `Watching(Live)` /
    // `Reconnecting`) - the interactive CLI shell calls it on every prompt refresh
    // (`crates/app/src/main.rs`), and this is this crate's own equivalent "each time the
    // caller asks what's going on" point. Without this, `watch.state` would report
    // `connecting` forever even once real media is flowing (discovered while manually
    // verifying Stage 14H.5's browser video path - a pre-existing gap since 14H.2/14H.3,
    // not something browser video introduced, but it means this endpoint's `state` field
    // has never actually reached `live` before now).
    app.refresh_watch();
    let diag = crate::diag::diag_view(&handle, &app);
    let watch_id = handle.registry().current_watch_id().map(str::to_owned);
    Json(
        AppStateView::from_app(&app)
            .with_watch_id(watch_id.as_deref())
            .with_diag(diag),
    )
}

/// `GET /api/diagnostics`: where each recent Share/Watch operation is and how it got there
/// (Stage 15.4). Loopback development aid; contains no credentials or media.
pub async fn get_diagnostics<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Json<crate::diag::Diagnostics> {
    let app = handle.app.lock().await;
    Json(crate::diag::diagnostics(&handle, &app))
}

/// `GET /api/media/timings`: the bounded per-object stage timestamps of the running share and/or
/// watch (Stage 17A measurement; loopback development aid, no media and no credentials).
pub async fn get_media_timings<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
) -> Json<crate::diag::MediaTimings> {
    let app = handle.app.lock().await;
    Json(crate::diag::media_timings(&app))
}
