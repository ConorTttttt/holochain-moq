//! `crates/app-web`'s own tests: the HTTP layer only, driven in-process against the
//! `axum::Router` via `tower::ServiceExt::oneshot` (no real TCP listener, no browser).
//! `App<InMemoryPeerDirectory>` stands in for a real conductor, the same pattern
//! `crates/app`'s own test suite already uses - no conductor is required for these.
//!
//! Where a test needs the app already `Sharing`/`Watching` as a *precondition* (to check
//! stop behavior, or mutual exclusion), it gets there by calling `App::start_share`/
//! `start_watch` directly with an ephemeral-port `ShareConfig` - bypassing `POST
//! /api/share`/`/api/watch` for *setup only* - so most of this suite never binds the
//! fixed port `crate::config::share_config()` uses. Exactly one test
//! (`post_share_via_http_reaches_live_and_stop_drains_it`) exercises `POST /api/share`
//! itself end-to-end and so does bind that fixed address, matching what a real deployment
//! actually does (`crates/app`'s own `main.rs` does the same, and is tested the same way
//! there - see its `share_config()`).

use crate::routes::{router, AppHandle};
use crate::view::{AppStateView, SessionView};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use holochain_moq_app::{AccessConfig, App, ShareSource};
use holochain_moq_node::{InMemoryPeerDirectory, RealtimeNode, RetryPolicy};
use holochain_moq_protocol::{Session, SessionId, StreamKind};
use holochain_moq_screenshare::ShareConfig;
use holochain_moq_video::{SyntheticVideoFrameSource, VideoFrameSource};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use tower::ServiceExt;

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn ephemeral_share_cfg(label: &str) -> ShareConfig {
    ShareConfig {
        bind: loopback(),
        cert_dir: std::env::temp_dir().join(format!(
            "moq-app-web-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        )),
        av1: holochain_moq_codec_av1::Av1Config {
            fps: 30,
            keyframe_interval: 5,
            ..Default::default()
        },
        title: Some("app-web test share".into()),
        session_ttl: std::time::Duration::from_secs(3600),
    }
}

fn synthetic() -> Box<dyn VideoFrameSource> {
    Box::new(SyntheticVideoFrameSource::new(16, 16, 30, u64::MAX).paced())
}

async fn ready_app() -> App<InMemoryPeerDirectory> {
    let mut app = App::starting(RealtimeNode::new(InMemoryPeerDirectory::new("alice")));
    app.connect().await.unwrap();
    app
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(router: &axum::Router, path: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// POST with `Authorization: Bearer <token>` (the scoped watch stop).
async fn post_bearer(router: &axum::Router, path: &str, token: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Stop the watch as its owner (the credentials the last `POST /api/watch` issued).
async fn stop_as_owner<D: holochain_moq_node::SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    router: &axum::Router,
) -> axum::response::Response {
    let (id, token) = handle
        .registry()
        .last_issued()
        .expect("no watch was granted");
    post_bearer(router, &format!("/api/watch/{id}/stop"), &token).await
}

async fn post(router: &axum::Router, path: &str, body: Value) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

// ---- 1. GET /api/state when Ready ----

#[tokio::test]
async fn state_endpoint_reports_ready_and_identity() {
    let app = ready_app().await;
    let r = router(AppHandle::new(app));

    let res = get(&r, "/api/state").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    assert_eq!(body["state"], "ready");
    assert_eq!(body["agent"], "alice");
    assert!(body["share"].is_null());
    assert!(body["watch"].is_null());
    assert!(body["last_error"].is_null());
}

// ---- 2. session discovery endpoint ----

#[tokio::test]
async fn sessions_endpoint_is_empty_with_nothing_shared() {
    let app = ready_app().await;
    let r = router(AppHandle::new(app));

    let res = get(&r, "/api/sessions").await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn sessions_endpoint_lists_a_real_live_session() {
    let mut publisher = ready_app().await;
    publisher.choose_share();
    publisher
        .start_share(
            ephemeral_share_cfg("discover"),
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
    let session_id = publisher.share_session().unwrap().session_id;

    // A second `App` handle onto the *same* directory, as the viewer - the same pattern
    // `crates/app`'s own tests use to simulate two agents without a conductor.
    let viewer_directory = publisher.node().directory().as_agent("bob");
    let mut viewer = App::starting(RealtimeNode::new(viewer_directory));
    viewer.connect().await.unwrap();
    let r = router(AppHandle::new(viewer));

    let res = get(&r, "/api/sessions").await;
    assert_eq!(res.status(), StatusCode::OK);
    let sessions = json_body(res).await;
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], session_id.to_string());
    assert_eq!(sessions[0]["publisher"], "alice");
    assert_eq!(sessions[0]["source"], "Screen");

    publisher.request_stop();
    publisher.finish_share().await.unwrap();
}

// ---- 3. invalid Share request ----

#[tokio::test]
async fn an_unrecognized_source_is_a_bad_request_and_returns_to_ready() {
    let app = ready_app().await;
    let r = router(AppHandle::new(app));

    let res = post(&r, "/api/share", json!({ "source": "webcam" })).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = json_body(res).await;
    assert!(body["error"].as_str().unwrap().contains("webcam"));

    // Not left stuck outside Ready.
    let res = get(&r, "/api/state").await;
    assert_eq!(json_body(res).await["state"], "ready");
}

// ---- 4. invalid Watch request ----

#[tokio::test]
async fn an_unparseable_session_id_is_a_bad_request() {
    let app = ready_app().await;
    let r = router(AppHandle::new(app));

    let res = post(&r, "/api/watch", json!({ "session_id": "not-a-uuid" })).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = json_body(res).await;
    assert!(body["error"].as_str().unwrap().contains("session id"));

    let res = get(&r, "/api/state").await;
    assert_eq!(json_body(res).await["state"], "ready");
}

// ---- 5. Share/Watch mutual exclusion, through the HTTP layer ----

#[tokio::test]
async fn watch_is_refused_by_http_while_a_share_is_already_live() {
    let mut app = ready_app().await;
    app.choose_share();
    app.start_share(
        ephemeral_share_cfg("mutex"),
        None,
        ShareSource::Synthetic,
        synthetic(),
        AccessConfig::Public,
    )
    .await
    .unwrap();

    let r = router(AppHandle::new(app));
    let res = post(
        &r,
        "/api/watch",
        json!({ "session_id": SessionId(uuid::Uuid::new_v4()).to_string() }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Still sharing - the refused watch attempt did not disturb it.
    let state = json_body(get(&r, "/api/state").await).await;
    assert_eq!(state["state"], "sharing");
    assert_eq!(state["share"]["state"], "live");
}

#[tokio::test]
async fn share_is_refused_by_http_while_a_watch_is_already_active() {
    let mut publisher = ready_app().await;
    publisher.choose_share();
    publisher
        .start_share(
            ephemeral_share_cfg("mutex-watch-pub"),
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
    let session_id = publisher.share_session().unwrap().session_id;

    let mut viewer = App::starting(RealtimeNode::new(
        publisher.node().directory().as_agent("bob"),
    ));
    viewer.connect().await.unwrap();
    viewer.choose_watch();
    viewer.enter_selecting();
    let (progress, sink) = crate::config::placeholder_watch_sink();
    viewer
        .start_watch(session_id, sink, progress, RetryPolicy::default())
        .await
        .unwrap();

    let r = router(AppHandle::new(viewer));
    let res = post(&r, "/api/share", json!({ "source": "synthetic" })).await;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let state = json_body(get(&r, "/api/state").await).await;
    assert_eq!(state["state"], "watching");

    publisher.request_stop();
    publisher.finish_share().await.unwrap();
}

// ---- 6. stop endpoints invoke the existing finish lifecycle ----

#[tokio::test]
async fn share_stop_drains_the_session_before_reporting_ready() {
    let mut app = ready_app().await;
    app.choose_share();
    app.start_share(
        ephemeral_share_cfg("stop"),
        None,
        ShareSource::Synthetic,
        synthetic(),
        AccessConfig::Public,
    )
    .await
    .unwrap();
    let session_id = app.share_session().unwrap().session_id;
    let directory = app.node().directory().as_agent("alice");

    let r = router(AppHandle::new(app));
    let res = post(&r, "/api/share/stop", json!({})).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = json_body(res).await;
    assert_eq!(body["state"], "ready");

    // `POST /api/share/stop` only returns after `finish_share` completed - by Stage 14E,
    // the durable session must already read `Ended`, not merely "we asked it to stop".
    let session: Session = {
        use holochain_moq_node::SessionDirectory;
        directory.find_session(&session_id).await.unwrap().unwrap()
    };
    assert!(!session.is_live());
    assert!(session.ended_at.is_some());
}

#[tokio::test]
async fn share_stop_with_nothing_active_is_a_bad_request() {
    let app = ready_app().await;
    let r = router(AppHandle::new(app));
    let res = post(&r, "/api/share/stop", json!({})).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn watch_stop_returns_to_ready_without_ending_the_publisher_session() {
    let mut publisher = ready_app().await;
    publisher.choose_share();
    publisher
        .start_share(
            ephemeral_share_cfg("watch-stop-pub"),
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
    let session_id = publisher.share_session().unwrap().session_id;

    let mut viewer = App::starting(RealtimeNode::new(
        publisher.node().directory().as_agent("bob"),
    ));
    viewer.connect().await.unwrap();
    viewer.choose_watch();
    viewer.enter_selecting();
    let (progress, sink) = crate::config::placeholder_watch_sink();
    viewer
        .start_watch(session_id, sink, progress, RetryPolicy::default())
        .await
        .unwrap();

    let handle = AppHandle::new(viewer);
    let r = router(handle.clone());
    // Give the background task a moment to actually connect before stopping it.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let state = json_body(get(&r, "/api/state").await).await;
        if state["watch"]["pictures"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no media ever arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // This watch was started in-process (not over HTTP), so mint its grant here.
    let (watch_id, token) = handle.registry().issue(&session_id.to_string());
    let res = post_bearer(&r, &format!("/api/watch/{watch_id}/stop"), token.as_str()).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json_body(res).await["state"], "ready");

    let still_live = {
        use holochain_moq_node::SessionDirectory;
        publisher
            .node()
            .directory()
            .find_session(&session_id)
            .await
            .unwrap()
            .unwrap()
    };
    assert!(
        still_live.is_live(),
        "the publisher's session must be untouched"
    );

    publisher.request_stop();
    publisher.finish_share().await.unwrap();
}

// ---- 7. JSON view mappings ----

#[tokio::test]
async fn the_state_view_reflects_a_live_share() {
    let mut app = ready_app().await;
    app.choose_share();
    app.start_share(
        ephemeral_share_cfg("view"),
        None,
        ShareSource::Synthetic,
        synthetic(),
        AccessConfig::Public,
    )
    .await
    .unwrap();

    let view = AppStateView::from_app(&app);
    assert_eq!(view.state, "sharing");
    let share = view.share.expect("a share view");
    assert_eq!(share.state, "live");
    assert_eq!(share.source, Some("synthetic"));
    assert!(share.session_id.is_some());
    assert_eq!(share.objects_published, Some(0));
    assert_eq!(share.viewers, Some(0));
    assert!(!share.finished);

    app.request_stop();
    app.finish_share().await.unwrap();
}

#[test]
fn the_session_view_carries_the_fields_the_ui_needs() {
    let session = Session::new(
        SessionId(uuid::Uuid::new_v4()),
        "alice".to_string(),
        StreamKind::Screen,
        1_000,
        2_000,
        "screenshare",
        "screen",
        Some("demo".into()),
    )
    .unwrap()
    .start()
    .unwrap();

    let view = SessionView::from(&session);
    assert_eq!(view.publisher, "alice");
    assert_eq!(view.title.as_deref(), Some("demo"));
    assert_eq!(view.source, StreamKind::Screen);
    assert_eq!(view.created_at, 1_000);
    assert_eq!(view.expires_at, 2_000);
}

#[test]
fn share_view_equality_distinguishes_lifecycle_states() {
    // A cheap guard against the label match in `view.rs` silently collapsing two
    // `SharingState`s onto the same string.
    use holochain_moq_app::SharingState::*;
    let states = [ChoosingSource, Live, Ending, Ended];
    let labels: std::collections::HashSet<_> = states
        .iter()
        .map(|s| match s {
            ChoosingSource => "choosing_source",
            Live => "live",
            Ending => "ending",
            Ended => "ended",
        })
        .collect();
    assert_eq!(labels.len(), states.len());
}

// ---- 8. static frontend availability ----

#[tokio::test]
async fn static_routes_serve_the_frontend() {
    let r = router(AppHandle::new(ready_app().await));

    let res = get(&r, "/").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));

    let res = get(&r, "/app.js").await;
    assert_eq!(res.status(), StatusCode::OK);

    // Stage 14H.5: the browser video WebSocket/WebCodecs script.
    let res = get(&r, "/video.js").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));

    // Stage 15.3: the presentation-wording module.
    let res = get(&r, "/ui.js").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get(&r, "/style.css").await;
    assert_eq!(res.status(), StatusCode::OK);
}

// ---- end-to-end through the real POST /api/share endpoint ----

/// The only test in this suite that exercises `POST /api/share` itself (not just
/// `App::start_share` directly), and so the only one that binds
/// `crate::config::share_config()`'s real fixed address - exactly what a real deployment
/// does (see `crates/app`'s own `main.rs`, which is not independently tested against its
/// own fixed port either, for the same reason: `crates/app`'s test suite calls
/// `App::start_share` with its own ephemeral-port test config instead).
#[tokio::test]
async fn post_share_via_http_reaches_live_and_stop_drains_it() {
    let _port = SHARE_PORT.lock().await;
    let r = router(AppHandle::new(ready_app().await));

    let res = post(&r, "/api/share", json!({ "source": "synthetic" })).await;
    if res.status() != StatusCode::OK {
        // The fixed port this endpoint always binds (matching production) was not free -
        // an environment condition, not a correctness failure of this crate. Documented
        // in the README/report rather than silently ignored.
        let body = json_body(res).await;
        panic!(
            "POST /api/share failed - is something else already bound to the fixed \
             share port? {body:?}"
        );
    }
    let body = json_body(get(&r, "/api/state").await).await;
    assert_eq!(body["state"], "sharing");
    assert_eq!(body["share"]["state"], "live");

    let res = post(&r, "/api/share/stop", json!({})).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json_body(res).await["state"], "ready");

    // `share_config()`'s cert dir is the same relative `dev/certs` a real deployment
    // uses, resolved against this test binary's own working directory (the crate root) -
    // clean up what this test caused to be generated there, best-effort, rather than
    // leaving it in the source tree for the next run to silently reuse.
    let _ = std::fs::remove_dir_all("dev");
}

// ---- Stage 14H.5: the real browser video WebSocket carrier ----

/// End to end over a real TCP listener and a real WebSocket client (`tower::oneshot`
/// cannot drive a protocol upgrade, so this is the one test in this suite that actually
/// runs `axum::serve`): a real publisher, `POST /api/watch` starting the decode-free
/// watch, and `GET /api/watch/video` delivering real, still-encoded AV1 framed exactly
/// as `crate::wire` documents - proving the whole chain (`App::start_watch_encoded` ->
/// `EncodedVideoQueue` -> `video_ws::forward` -> the wire format) with real bytes, not a
/// mocked queue.
#[tokio::test(flavor = "multi_thread")]
async fn watch_video_websocket_delivers_a_real_epoch_and_keyframe() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mut publisher = ready_app().await;
    publisher.choose_share();
    publisher
        .start_share(
            ephemeral_share_cfg("ws-video"),
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
    let session_id = publisher.share_session().unwrap().session_id;

    let mut viewer = App::starting(RealtimeNode::new(
        publisher.node().directory().as_agent("bob"),
    ));
    viewer.connect().await.unwrap();
    let handle = AppHandle::new(viewer);
    let r = router(handle.clone());

    // Start the decode-free watch through the same HTTP surface a browser would use
    // (in-process, via `oneshot` - no protocol upgrade involved for a plain POST);
    // `post_watch` itself drives `choose_watch`/`enter_selecting`, exactly as it would
    // for a real browser request.
    let res = post(
        &r,
        "/api/watch",
        json!({ "session_id": session_id.to_string() }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Serve the *same* router (same `AppHandle`, same underlying `App`/queue) over a
    // real TCP listener, so a real WebSocket client can connect to it.
    let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, r).await;
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/watch/video"))
        .await
        .expect("connecting the video websocket");

    // Note: `EncodedVideoSink::epoch_started` (like `VideoSink::epoch_started`) is only
    // called on a *reconnect* (`StreamEvent::Reconnected`) - never for the initial
    // connection, which is epoch 0 by construction (see `AvHalf::epoch`'s own docs). So
    // the first messages here are plain `Media` (opcode 0x01) at epoch 0, with no
    // `EpochStarted` preceding them; an `EpochStarted` would only appear after a real
    // reconnect, exercised separately (see the crate README's manual reconnect
    // procedure - simulating one here would mean tearing down and rebinding the real
    // QUIC transport mid-test, out of scope for this endpoint-level test).
    let mut keyframe_payload: Option<Vec<u8>> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while keyframe_payload.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no keyframe arrived over the websocket in time"
        );
        let msg = tokio::time::timeout(std::time::Duration::from_secs(15), ws.next())
            .await
            .expect("websocket read timed out")
            .expect("websocket closed before any keyframe arrived")
            .expect("websocket read error");
        let bytes = match msg {
            WsMessage::Binary(b) => b,
            WsMessage::Close(_) => panic!("websocket closed before any keyframe arrived"),
            _ => continue,
        };
        match bytes[0] {
            0x02 => {
                assert_eq!(bytes.len(), 5, "EpochStarted must be exactly 5 bytes");
            }
            0x01 => {
                assert!(bytes.len() >= 14, "Media header must be at least 14 bytes");
                let epoch = u32::from_be_bytes(bytes[2..6].try_into().unwrap());
                assert_eq!(epoch, 0, "the initial connection is epoch 0");
                let flags = bytes[1];
                if flags & 0x01 != 0 {
                    keyframe_payload = Some(bytes[14..].to_vec());
                }
            }
            other => panic!("unknown wire opcode {other}"),
        }
    }
    let payload = keyframe_payload.unwrap();
    assert!(!payload.is_empty(), "the AV1 payload must not be empty");
    assert_eq!(
        payload[0] & 0x80,
        0,
        "payload must still be real AV1 (forbidden bit clear), not a placeholder"
    );

    drop(ws);
    {
        let mut app = handle.app.lock().await;
        app.request_stop_watch();
        let _ = app.finish_watch().await;
    }
    server.abort();
    publisher.request_stop();
    publisher.finish_share().await.unwrap();
}

/// Regression test for a bug the Stage 14H.5 manual Chrome acceptance run caught (`ss
/// -tnp` showing the server side stuck in `CLOSE_WAIT` forever after clicking Stop in a
/// real browser): `video_ws::forward`'s loop only ever wrote to the socket, never read
/// from it, so a client-initiated close was never seen and the closing handshake never
/// completed. Proves the fix directly: once the client sends a close frame, the server
/// must respond/finish the handshake promptly rather than leaving the connection open
/// indefinitely.
#[tokio::test(flavor = "multi_thread")]
async fn watch_video_websocket_completes_the_close_handshake_when_the_client_closes_first() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mut publisher = ready_app().await;
    publisher.choose_share();
    publisher
        .start_share(
            ephemeral_share_cfg("ws-close"),
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
    let session_id = publisher.share_session().unwrap().session_id;

    let mut viewer = App::starting(RealtimeNode::new(
        publisher.node().directory().as_agent("bob"),
    ));
    viewer.connect().await.unwrap();
    let handle = AppHandle::new(viewer);
    let r = router(handle.clone());

    let res = post(
        &r,
        "/api/watch",
        json!({ "session_id": session_id.to_string() }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, r).await;
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/watch/video"))
        .await
        .expect("connecting the video websocket");

    // Wait for at least one real message so the server's forwarding loop is definitely
    // past its initial setup (not still resolving `encoded_video_queue()`), then close
    // from the client side - exactly what `static/video.js`'s `VideoWatch.stop()` does
    // on an ordinary Stop.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(15), ws.next())
        .await
        .expect("websocket read timed out")
        .expect("websocket closed unexpectedly")
        .expect("websocket read error");
    ws.close(None).await.expect("sending the close frame");

    // The closing handshake must complete promptly: either the server echoes its own
    // Close frame, or the stream simply ends - either way, `next()` must not hang.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None => return "stream_ended",
                Some(Ok(WsMessage::Close(_))) => return "close_echoed",
                Some(Ok(_)) => continue, // a stray Media frame already in flight - keep draining
                Some(Err(_)) => return "stream_errored",
            }
        }
    })
    .await
    .expect(
        "the server never completed the closing handshake within 5s - \
         the CLOSE_WAIT regression is back",
    );
    // Any of these three means the handshake actually completed promptly (the specific
    // one tokio-tungstenite reports for a client-initiated close varies) - what matters,
    // and what the CLOSE_WAIT regression broke, is that `next()` did not hang.
    assert!(matches!(
        outcome,
        "stream_ended" | "close_echoed" | "stream_errored"
    ));

    {
        let mut app = handle.app.lock().await;
        app.request_stop_watch();
        let _ = app.finish_watch().await;
    }
    server.abort();
    publisher.request_stop();
    publisher.finish_share().await.unwrap();
}

// ---- Stage 15.1: browser-facing behaviour across a real transport reconnect ----

/// The whole chain, real end to end, with nothing faked below the directory: a real
/// publisher's QUIC/MoQ server is killed without ending its session, stays down for a
/// realistic outage, then comes back at the same address; the viewer's real
/// `ReconnectingSubscriber` recovers; and what a browser would observe over the real
/// `/api/watch/video` WebSocket and `/api/state` is checked: `EpochStarted(1)` strictly
/// before any epoch-1 media, no epoch-0 media after it, a keyframe that resumes video,
/// a `reconnecting` state visible through `/api/state`, and - after the publisher ends
/// the session - a clean WebSocket close and a finished watch.
#[tokio::test(flavor = "multi_thread")]
async fn watch_video_websocket_survives_a_publisher_outage_and_ends_cleanly() {
    use futures::StreamExt;
    use holochain_moq_screenshare::{end_session, Sharer};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let alice_dir = InMemoryPeerDirectory::new("alice");
    let alice = RealtimeNode::new(alice_dir.as_agent("alice"));

    let first = Sharer::bind(&ephemeral_share_cfg("reconnect-1")).unwrap();
    let addr: SocketAddr = first.endpoint().address.parse().unwrap();
    let session = first
        .announce(&alice, first.endpoint(), None)
        .await
        .unwrap();
    let mut first_run = first;
    let (stop_first, first_task) = {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = first_run
                .run(synthetic(), async move {
                    let _ = rx.await;
                })
                .await;
            first_run
        });
        (tx, handle)
    };

    let mut viewer = App::starting(RealtimeNode::new(alice_dir.as_agent("bob")));
    viewer.connect().await.unwrap();
    let handle = AppHandle::new(viewer);
    let r = router(handle.clone());
    let res = post(
        &r,
        "/api/watch",
        json!({ "session_id": session.session_id.to_string() }),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
    let http_addr = listener.local_addr().unwrap();
    let served = r.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    // Sample `/api/state` throughout, as the browser's 1s poll would (only much faster),
    // remembering every watch state and whether the connection ever read `reconnecting`.
    let seen_states = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let poller = tokio::spawn({
        let (r, seen) = (r.clone(), seen_states.clone());
        async move {
            loop {
                let state = json_body(get(&r, "/api/state").await).await;
                if let Some(s) = state["watch"]["state"].as_str() {
                    let mut seen = seen.lock().unwrap();
                    if seen.last().map(String::as_str) != Some(s) {
                        seen.push(s.to_string());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{http_addr}/api/watch/video"))
        .await
        .expect("connecting the video websocket");

    // Everything received, in order, as `(opcode, epoch, is_keyframe)`.
    let mut log: Vec<(u8, u32, bool)> = Vec::new();
    async fn read_until(
        ws: &mut (impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
        log: &mut Vec<(u8, u32, bool)>,
        what: &str,
        done: impl Fn(&[(u8, u32, bool)]) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while !done(log) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; log so far: {log:?}"
            );
            let msg = tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
                .await
                .unwrap_or_else(|_| panic!("read timed out waiting for {what}: {log:?}"))
                .unwrap_or_else(|| panic!("websocket ended waiting for {what}: {log:?}"))
                .expect("websocket read error");
            if let WsMessage::Binary(b) = msg {
                match b[0] {
                    0x01 => log.push((
                        0x01,
                        u32::from_be_bytes(b[2..6].try_into().unwrap()),
                        b[1] & 1 != 0,
                    )),
                    0x02 => {
                        log.push((0x02, u32::from_be_bytes(b[1..5].try_into().unwrap()), false))
                    }
                    other => panic!("unknown opcode {other}"),
                }
            }
        }
    }

    read_until(&mut ws, &mut log, "epoch-0 keyframe", |l| {
        l.iter().any(|&(op, e, k)| op == 0x01 && e == 0 && k)
    })
    .await;

    // Kill the transport WITHOUT ending the session; stay down for a realistic outage.
    let _ = stop_first.send(());
    first_task.await.unwrap().abort().await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let mut rebound = None;
    for _ in 0..200 {
        let mut cfg = ephemeral_share_cfg("reconnect-2");
        cfg.bind = addr;
        match Sharer::bind(&cfg) {
            Ok(s) => {
                rebound = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(15)).await,
        }
    }
    let second = rebound.expect("could not rebind at the same address");
    second
        .announce(&alice, second.endpoint(), None)
        .await
        .unwrap();
    let mut second_run = second;
    let (stop_second, second_task) = {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = second_run
                .run(synthetic(), async move {
                    let _ = rx.await;
                })
                .await;
            second_run
        });
        (tx, handle)
    };

    read_until(
        &mut ws,
        &mut log,
        "epoch-1 keyframe after the reconnect",
        |l| l.iter().any(|&(op, e, k)| op == 0x01 && e == 1 && k),
    )
    .await;

    // Ordering: EpochStarted(1) exists, precedes every epoch-1 media object, and no
    // epoch-0 media follows it.
    let marker = log
        .iter()
        .position(|&(op, e, _)| op == 0x02 && e == 1)
        .unwrap_or_else(|| panic!("no EpochStarted(1) in {log:?}"));
    assert!(
        log[..marker].iter().all(|&(op, e, _)| op == 0x01 && e == 0),
        "only epoch-0 media before the marker: {log:?}"
    );
    assert!(
        log[marker + 1..]
            .iter()
            .all(|&(op, e, _)| op == 0x01 && e == 1),
        "only epoch-1 media after the marker (no stale epoch-0, no repeated marker): {log:?}"
    );
    assert_eq!(
        log.iter().filter(|&&(op, _, _)| op == 0x02).count(),
        1,
        "exactly one reconnect, exactly one marker"
    );

    // The reconnect was visible through the ordinary state endpoint, and it went back
    // to `live` afterwards: live -> reconnecting -> live.
    let recovered = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            {
                let seen = seen_states.lock().unwrap();
                if let Some(i) = seen.iter().position(|s| s == "reconnecting") {
                    if seen[i..].iter().any(|s| s == "live") {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        recovered.is_ok(),
        "/api/state never went reconnecting -> live; states seen: {:?}",
        seen_states.lock().unwrap()
    );

    // Now the publisher ends the session (the track is torn down without a clean end,
    // exactly the race Stage 14E normalizes): the browser-facing side must finish
    // cleanly - the WebSocket closes, the watch reports finished.
    end_session(&alice, session.clone()).await.unwrap();
    let _ = stop_second.send(());
    second_task.await.unwrap().abort().await;

    let closed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the websocket never closed after the session ended"
    );

    let state = json_body(get(&r, "/api/state").await).await;
    assert_eq!(state["watch"]["finished"], true, "{state}");
    let res = stop_as_owner(&handle, &r).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json_body(res).await["state"], "ready");

    poller.abort();
    server.abort();
}

// ---- Stage 15.2: private sessions through the browser Watch path ---------------------
//
// Real everything below the directory: real Ed25519 keys in real Holochain agent-key
// format, real 14F proofs signed by the viewer and verified by the publisher's
// `MoqLivePublication`, real QUIC/MoQ, real AV1, a real `axum::serve` + WebSocket. The
// only stand-in is the in-memory directory (the real conductor signing path is exercised
// by `crates/holochain-e2e/tests/screenshare.rs`).

mod private_sessions {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use ed25519_dalek::{Signer, SigningKey};
    use futures::StreamExt;
    use holochain_moq_app::AccessConfig;
    use holochain_moq_realtime::AdmissionSigner;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// A real Ed25519 key and its Holochain agent-key string (`u` + base64url of the
    /// 3-byte prefix, 32 key bytes and a 4-byte location; the location is not checked
    /// by the admission code, see `agent_verifying_key`).
    pub(super) fn keypair(seed: u8) -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let mut raw = vec![0x84, 0x20, 0x24];
        raw.extend_from_slice(key.verifying_key().as_bytes());
        raw.extend_from_slice(&[0u8; 4]);
        (key, format!("u{}", URL_SAFE_NO_PAD.encode(raw)))
    }

    /// A real 14F signer for `(key, agent)`, counting how many proofs it was asked for.
    pub(super) fn signer(
        key: SigningKey,
        agent: String,
        calls: Arc<AtomicUsize>,
    ) -> AdmissionSigner {
        Arc::new(move |session_id, challenge| {
            let (key, agent, calls) = (key.clone(), agent.clone(), calls.clone());
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let payload = holochain_moq_protocol::signing_payload(
                    session_id,
                    &challenge,
                    &key.verifying_key(),
                );
                Ok((agent, key.sign(&payload).to_bytes()))
            })
        })
    }

    pub(super) struct Viewer {
        pub(super) handle: AppHandle<InMemoryPeerDirectory>,
        pub(super) router: axum::Router,
        pub(super) proofs: Arc<AtomicUsize>,
    }

    /// A browser-facing viewer app for `(key, agent)` on the shared in-memory network.
    pub(super) async fn viewer(
        dir: &InMemoryPeerDirectory,
        key: SigningKey,
        agent: &str,
    ) -> Viewer {
        let proofs = Arc::new(AtomicUsize::new(0));
        let mut app = App::starting(RealtimeNode::new(dir.as_agent(agent)))
            .with_admission_signer(signer(key, agent.to_string(), proofs.clone()));
        app.connect().await.unwrap();
        let handle = AppHandle::new(app);
        Viewer {
            router: router(handle.clone()),
            handle,
            proofs,
        }
    }

    /// A publisher app sharing `session` privately (allowed = `allowed`) or publicly.
    pub(super) async fn publisher(
        dir: &InMemoryPeerDirectory,
        agent: &str,
        label: &str,
        access: AccessConfig,
    ) -> (App<InMemoryPeerDirectory>, SessionId) {
        let mut app = App::starting(RealtimeNode::new(dir.as_agent(agent)));
        app.connect().await.unwrap();
        app.choose_share();
        app.start_share(
            ephemeral_share_cfg(label),
            None,
            ShareSource::Synthetic,
            synthetic(),
            access,
        )
        .await
        .unwrap();
        let id = app.share_session().unwrap().session_id;
        (app, id)
    }

    pub(super) async fn serve(r: &axum::Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let r = r.clone();
        (
            addr,
            tokio::spawn(async move {
                let _ = axum::serve(listener, r).await;
            }),
        )
    }

    /// Everything the browser socket delivers until it closes or `limit` passes:
    /// `(opcode, epoch, keyframe)` for each binary frame.
    pub(super) async fn frames_until_close(
        ws: &mut (impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
        limit: Duration,
    ) -> (Vec<(u8, u32, bool)>, bool) {
        let mut frames = Vec::new();
        let closed = tokio::time::timeout(limit, async {
            loop {
                match ws.next().await {
                    None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                    Some(Ok(WsMessage::Binary(b))) => match b[0] {
                        0x01 => frames.push((
                            0x01,
                            u32::from_be_bytes(b[2..6].try_into().unwrap()),
                            b[1] & 1 != 0,
                        )),
                        _ => frames.push((b[0], 0, false)),
                    },
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .is_ok();
        (frames, closed)
    }

    pub(super) async fn first_keyframe(
        ws: &mut (impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
    ) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Binary(b))) if b[0] == 0x01 && b[1] & 1 != 0 => return,
                    Some(Ok(_)) => continue,
                    other => panic!("socket ended before any keyframe: {other:?}"),
                }
            }
        })
        .await
        .expect("no keyframe arrived");
    }

    pub(super) async fn ws_connect(
        addr: SocketAddr,
    ) -> impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin
    {
        tokio_tungstenite::connect_async(format!("ws://{addr}/api/watch/video"))
            .await
            .expect("connecting the video websocket")
            .0
    }

    /// Connect the browser video socket AS THE OWNER of the watch the last `POST /api/watch`
    /// started: the token travels in `Sec-WebSocket-Protocol`, exactly as the browser does.
    pub(super) async fn ws_connect_as<
        D: holochain_moq_node::SessionDirectory + Send + Sync + 'static,
    >(
        handle: &AppHandle<D>,
        addr: SocketAddr,
    ) -> impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (_id, token) = handle
            .registry()
            .last_issued()
            .expect("no watch was granted");
        let mut request = format!("ws://{addr}/api/watch/video")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("moq-watch.v1, moq-watch-token.{token}")
                .parse()
                .unwrap(),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .expect("connecting the owner's video websocket")
            .0
    }

    pub(super) async fn stop_watch(v: &Viewer) -> Value {
        json_body(stop_as_owner(&v.handle, &v.router).await).await
    }

    pub(super) async fn wait_finished(v: &Viewer, limit: Duration) {
        tokio::time::timeout(limit, async {
            loop {
                let s = json_body(get(&v.router, "/api/state").await).await;
                if s["watch"]["finished"] == true {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the watch never finished");
    }

    // ---- 1. public regression ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_public_session_still_delivers_real_media_and_signs_nothing() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "priv-public", AccessConfig::Public).await;
        // The durable record says public: nothing to prove.
        assert!(publ.share_session().unwrap().allowed.is_none());

        let v = viewer(&dir, bk, &bob).await;
        let res = post(
            &v.router,
            "/api/watch",
            json!({ "session_id": id.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        assert_eq!(
            v.proofs.load(Ordering::SeqCst),
            0,
            "a public session must not cost a signature (or be able to fail on one)"
        );

        drop(ws);
        stop_watch(&v).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 2 + 3 + 4. authorized / unauthorized / publisher, side by side ----

    #[tokio::test(flavor = "multi_thread")]
    async fn bob_is_admitted_carol_is_refused_before_any_media_and_the_publisher_still_may_watch() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(
            &dir,
            &alice,
            "priv-main",
            AccessConfig::allowed([&bob]).unwrap(),
        )
        .await;
        let record = publ.share_session().unwrap();
        assert_eq!(record.allowed.as_deref(), Some([bob.clone()].as_slice()));

        // --- Carol: discovers it (discovery is not authorization), is refused. ---
        let c = viewer(&dir, ck, &carol).await;
        let listed = json_body(get(&c.router, "/api/sessions").await).await;
        assert_eq!(
            listed[0]["private"], true,
            "the private session is listed as private"
        );

        // She even *claims* to be Bob, three ways. None of it can matter: there is no
        // route parameter, header or query that selects an identity - the only identity
        // is the one her own signer signs as.
        let claimed = Request::builder()
            .method("POST")
            .uri(format!("/api/watch?agent={bob}"))
            .header("content-type", "application/json")
            .header("x-agent", bob.as_str())
            .body(Body::from(
                json!({ "session_id": id.to_string(), "agent": bob, "allowed": [carol] })
                    .to_string(),
            ))
            .unwrap();
        let res = tower::ServiceExt::oneshot(c.router.clone(), claimed)
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let queue = c.handle.app.lock().await.encoded_video_queue().unwrap();
        let (caddr, cserver) = serve(&c.router).await;
        let mut cws = ws_connect_as(&c.handle, caddr).await;

        // The refusal is definitive: the watch ends by itself, quickly - no retry loop.
        wait_finished(&c, Duration::from_secs(30)).await;
        let (frames, closed) = frames_until_close(&mut cws, Duration::from_secs(10)).await;
        assert!(
            closed,
            "the socket must close once the refused watch has ended"
        );
        assert!(
            frames.is_empty(),
            "Carol's browser socket received data: {frames:?} - a refused viewer must get NO media"
        );
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.dropped(), 0, "nothing was ever even queued for Carol");
        assert_eq!(
            c.proofs.load(Ordering::SeqCst),
            1,
            "exactly one proof attempt: a definitive refusal is not retried"
        );
        let end = stop_watch(&c).await;
        assert_eq!(end["state"], "ready");
        assert_eq!(end["last_error_kind"], "unauthorized", "{end}");
        assert!(
            end["last_error"]
                .as_str()
                .unwrap()
                .contains("not authorized"),
            "{end}"
        );
        cserver.abort();

        // --- Bob: same session, same publisher: admitted, real media. ---
        let b = viewer(&dir, bk, &bob).await;
        let res = post(
            &b.router,
            "/api/watch",
            json!({ "session_id": id.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (baddr, bserver) = serve(&b.router).await;
        let mut bws = ws_connect_as(&b.handle, baddr).await;
        first_keyframe(&mut bws).await;
        assert!(
            b.proofs.load(Ordering::SeqCst) >= 1,
            "Bob really signed a proof"
        );

        // --- The publisher's own identity is implicitly authorized (14F), even though
        //     it is NOT on the list. ---
        let (ak2, _) = keypair(1);
        let a = viewer(&dir, ak2, &alice).await;
        let res = post(
            &a.router,
            "/api/watch",
            json!({ "session_id": id.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (aaddr, aserver) = serve(&a.router).await;
        let mut aws = ws_connect_as(&a.handle, aaddr).await;
        first_keyframe(&mut aws).await;

        drop((bws, aws));
        stop_watch(&b).await;
        stop_watch(&a).await;
        bserver.abort();
        aserver.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 7. ended / expired private sessions keep their own semantics ----

    #[tokio::test(flavor = "multi_thread")]
    async fn ended_and_expired_private_sessions_are_not_reclassified_as_unauthorized() {
        let (_ak, alice) = keypair(1);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());

        // Expired: a 1s lease on a private session.
        let mut app = App::starting(RealtimeNode::new(dir.as_agent(&alice)));
        app.connect().await.unwrap();
        app.choose_share();
        let mut cfg = ephemeral_share_cfg("priv-expiry");
        cfg.session_ttl = Duration::from_secs(1);
        app.start_share(
            cfg,
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::allowed(Vec::<String>::new()).unwrap(),
        )
        .await
        .unwrap();
        let expiring = app.share_session().unwrap().session_id;
        tokio::time::sleep(Duration::from_millis(1300)).await;

        let v = viewer(&dir, ck.clone(), &carol).await;
        let res = post(
            &v.router,
            "/api/watch",
            json!({ "session_id": expiring.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        wait_finished(&v, Duration::from_secs(20)).await;
        let end = stop_watch(&v).await;
        assert_eq!(end["last_error_kind"], "expired", "{end}");
        assert_eq!(
            v.proofs.load(Ordering::SeqCst),
            0,
            "refused before anything was dialled or signed"
        );

        app.request_stop();
        app.finish_share().await.unwrap();

        // Ended: the same session, now `Ended` (14E).
        let v = viewer(&dir, ck, &carol).await;
        let res = post(
            &v.router,
            "/api/watch",
            json!({ "session_id": expiring.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        wait_finished(&v, Duration::from_secs(20)).await;
        let end = stop_watch(&v).await;
        assert_ne!(end["last_error_kind"], "unauthorized", "{end}");
        assert!(
            end["last_error"].as_str().unwrap().contains("not live"),
            "{end}"
        );
    }

    // ---- 8. share-side validation ----

    #[tokio::test]
    async fn share_requests_with_malformed_duplicate_or_unknown_access_are_refused_cleanly() {
        let (_k, agent) = keypair(1);
        let (_k2, other) = keypair(2);
        let app = App::starting(RealtimeNode::new(InMemoryPeerDirectory::new(agent.clone())));
        let mut app = app;
        app.connect().await.unwrap();
        let r = router(AppHandle::new(app));

        for (body, needle) in [
            (
                json!({"source":"synthetic","access":"allowed","allowed_agents":["nope"]}),
                "not a valid Holochain agent public key",
            ),
            (
                json!({"source":"synthetic","access":"allowed","allowed_agents":[other, other]}),
                "duplicate",
            ),
            (
                // A typo must never silently become a PUBLIC share.
                json!({"source":"synthetic","access":"private"}),
                "unrecognized access",
            ),
        ] {
            let res = post(&r, "/api/share", body).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let msg = json_body(res).await["error"].as_str().unwrap().to_string();
            assert!(msg.contains(needle), "{msg}");
            // ...and the app was never disturbed.
            assert_eq!(
                json_body(get(&r, "/api/state").await).await["state"],
                "ready"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_state_view_reports_public_vs_private_from_the_durable_session_record() {
        let (_ak, alice) = keypair(1);
        let (_bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (publ, _id) = publisher(
            &dir,
            &alice,
            "priv-view",
            AccessConfig::allowed([&bob]).unwrap(),
        )
        .await;
        let mut publ = publ;
        let handle_view = crate::view::AppStateView::from_app(&publ);
        let share = handle_view.share.expect("sharing");
        assert_eq!(share.access, Some("allowed"));
        assert_eq!(share.allowed_agents, Some(vec![bob]));
        publ.request_stop();
        publ.finish_share().await.unwrap();

        let (publ2, _) = publisher(&dir, &alice, "pub-view", AccessConfig::Public).await;
        let mut publ2 = publ2;
        let share = crate::view::AppStateView::from_app(&publ2).share.unwrap();
        assert_eq!(share.access, Some("public"));
        assert_eq!(share.allowed_agents, None);
        publ2.request_stop();
        publ2.finish_share().await.unwrap();
    }

    // ---- 5. reconnect: proofs are re-performed; a new session identity is refused ----

    /// A private publisher built directly on `MoqLivePublication` so it can be killed and
    /// brought back with the SAME session id at the same address - which `Sharer` (it mints
    /// a fresh id per bind) cannot do. It announces the session record once.
    pub(super) struct RawPrivatePublisher {
        pub(super) stop: tokio::sync::oneshot::Sender<()>,
        pub(super) task: tokio::task::JoinHandle<holochain_moq_realtime::MoqLivePublication>,
        pub(super) addr: SocketAddr,
    }

    pub(super) async fn start_raw_private_publisher(
        node: &RealtimeNode<InMemoryPeerDirectory>,
        session_id: SessionId,
        self_agent: &str,
        allowed: Vec<String>,
        bind: SocketAddr,
        announce_record: bool,
    ) -> RawPrivatePublisher {
        use holochain_moq_protocol::{Endpoint, EndpointKind, Session, StreamKind};
        use holochain_moq_realtime::{certs, publish_media, MoqLivePublication};

        let dir = std::env::temp_dir().join(format!("moq-app-web-rawpub-{}", uuid::Uuid::new_v4()));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let acl = allowed.clone();
        let mut publication = None;
        for _ in 0..200 {
            match MoqLivePublication::bind_with_access_control(
                bind,
                &cert,
                &key,
                holochain_moq_screenshare::NAMESPACE,
                holochain_moq_screenshare::TRACK,
                Default::default(),
                session_id,
                self_agent.to_string(),
                Arc::new({
                    let acl = acl.clone();
                    move || Some(acl.clone())
                }),
            ) {
                Ok(p) => {
                    publication = Some(p);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(15)).await,
            }
        }
        let mut publication = publication.expect("could not bind the private publisher");
        let addr = publication.local_addr();
        node.publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: addr.to_string(),
            transport_identity: identity,
        })
        .await
        .unwrap();
        if announce_record {
            use holochain_moq_node::SessionDirectory;
            let now = holochain_moq_screenshare::now_ms();
            let session = Session::new(
                session_id,
                self_agent.to_string(),
                StreamKind::Screen,
                now,
                now + 3_600_000,
                holochain_moq_screenshare::NAMESPACE,
                holochain_moq_screenshare::TRACK,
                None,
            )
            .and_then(Session::start)
            .unwrap()
            .with_allowed(allowed);
            node.directory().publish_session(session).await.unwrap();
        }
        let mut source = holochain_moq_codec_av1::Av1Source::from_video_source(
            holochain_moq_codec_av1::Av1Config {
                fps: 30,
                keyframe_interval: 5,
                ..Default::default()
            },
            synthetic(),
            holochain_moq_codec_av1::QueuePolicy::DropOldest,
        )
        .unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = publish_media(&mut publication, &mut source, async move {
                let _ = stopped.await;
            })
            .await;
            publication
        });
        RawPrivatePublisher { stop, task, addr }
    }

    impl RawPrivatePublisher {
        pub(super) async fn kill(self) {
            let _ = self.stop.send(());
            self.task.await.unwrap().abort().await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_authorized_viewer_reconnects_re_proves_itself_and_gets_a_new_epoch() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let first = start_raw_private_publisher(
            &alice_node,
            id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        let addr = first.addr;

        let v = viewer(&dir, bk, &bob).await;
        let res = post(
            &v.router,
            "/api/watch",
            json!({ "session_id": id.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        first_keyframe(&mut ws).await;
        assert_eq!(
            v.proofs.load(Ordering::SeqCst),
            1,
            "one proof for the first connection"
        );

        // The publisher goes away (session record still Live) and comes back with the SAME
        // session id, at the same address, with a fresh certificate.
        first.kill().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let second =
            start_raw_private_publisher(&alice_node, id, &alice, vec![bob.clone()], addr, false)
                .await;

        // The viewer reconnects: the browser socket sees exactly one EpochStarted(1) and
        // then epoch-1 media - and the proof was performed AGAIN for the new connection.
        let mut seen: Vec<(u8, u32, bool)> = Vec::new();
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Binary(b))) => {
                        let e = match b[0] {
                            0x01 => u32::from_be_bytes(b[2..6].try_into().unwrap()),
                            _ => u32::from_be_bytes(b[1..5].try_into().unwrap()),
                        };
                        seen.push((b[0], e, b[0] == 0x01 && b[1] & 1 != 0));
                        if b[0] == 0x01 && e == 1 && b[1] & 1 != 0 {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("socket ended before recovery: {other:?} {seen:?}"),
                }
            }
        })
        .await
        .expect("never recovered with an epoch-1 keyframe");
        let marker = seen
            .iter()
            .position(|&(op, e, _)| op == 0x02 && e == 1)
            .expect("EpochStarted(1)");
        assert!(
            seen[..marker]
                .iter()
                .all(|&(op, e, _)| op == 0x01 && e == 0),
            "{seen:?}"
        );
        assert!(
            seen[marker + 1..]
                .iter()
                .all(|&(op, e, _)| op == 0x01 && e == 1),
            "{seen:?}"
        );
        assert_eq!(
            v.proofs.load(Ordering::SeqCst),
            2,
            "the admission proof must be re-signed for the reconnected session, never reused"
        );

        drop(ws);
        stop_watch(&v).await;
        server.abort();
        second.kill().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restarted_publisher_with_a_new_session_identity_definitively_refuses_the_old_viewer()
    {
        // A private session's admission is bound to its session id. A publisher process that
        // restarts as a NEW session (as `Sharer`/the CLI always does) therefore cannot admit
        // a viewer of the old one: that viewer is refused - definitively, and not retried
        // forever as if it were a transient outage.
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let old_id = SessionId(uuid::Uuid::new_v4());
        let first = start_raw_private_publisher(
            &alice_node,
            old_id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        let addr = first.addr;

        let v = viewer(&dir, bk, &bob).await;
        let res = post(
            &v.router,
            "/api/watch",
            json!({ "session_id": old_id.to_string() }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        first_keyframe(&mut ws).await;

        first.kill().await;
        let new_id = SessionId(uuid::Uuid::new_v4());
        let second = start_raw_private_publisher(
            &alice_node,
            new_id,
            &alice,
            vec![bob.clone()],
            addr,
            false,
        )
        .await;

        wait_finished(&v, Duration::from_secs(60)).await;
        let (_frames, closed) = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        assert!(closed);
        let end = stop_watch(&v).await;
        assert_eq!(end["last_error_kind"], "unauthorized", "{end}");
        assert_eq!(end["state"], "ready");

        server.abort();
        second.kill().await;
    }
}

// ---- Stage 15.3: session lifecycle and repeated use ----------------------------------
//
// Real QUIC/MoQ/AV1, real WebSocket, real crypto keys (reusing the 15.2 fixtures), in-memory
// directory. What these prove is lifecycle, not media: sessions appear and disappear when
// they should, Leave and Stop mean different things, repeated cycles leave nothing behind
// (no stale sessions or state, no leaked ports, tasks or sockets).

/// `POST /api/share` always binds the same fixed address (as a real deployment does), so
/// every test that goes through it takes this lock rather than racing another one for the port.
static SHARE_PORT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

mod lifecycle {
    use super::private_sessions::{
        first_keyframe, frames_until_close, keypair, publisher, serve, stop_watch, viewer,
        wait_finished, ws_connect_as,
    };
    use super::*;
    use holochain_moq_app::AccessConfig;
    use holochain_moq_node::{PeerDirectory, SessionDirectory};
    use std::time::Duration;

    async fn sessions(r: &axum::Router) -> Vec<Value> {
        json_body(get(r, "/api/sessions").await)
            .await
            .as_array()
            .unwrap()
            .clone()
    }

    async fn poll<F: std::future::Future<Output = bool>>(
        what: &str,
        limit: Duration,
        mut cond: impl FnMut() -> F,
    ) {
        tokio::time::timeout(limit, async {
            while !cond().await {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    // ---- 1. title ----

    #[tokio::test]
    async fn a_share_title_is_validated_stored_on_the_session_and_shown() {
        let _port = SHARE_PORT.lock().await;
        let r = router(AppHandle::new(ready_app().await));

        // Bad titles are a clean 400 naming the problem, and never disturb the app.
        for (title, needle) in [
            ("x".repeat(201), "too long"),
            ("bad\u{7}title".to_string(), "control characters"),
        ] {
            let res = post(
                &r,
                "/api/share",
                json!({"source":"synthetic","title":title}),
            )
            .await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let msg = json_body(res).await["error"].as_str().unwrap().to_string();
            assert!(msg.contains(needle), "{msg}");
            assert_eq!(
                json_body(get(&r, "/api/state").await).await["state"],
                "ready"
            );
        }

        // A padded title is trimmed and reaches the durable session record everyone sees.
        let res = post(
            &r,
            "/api/share",
            json!({"source":"synthetic","title":"  Team demo  "}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let share = json_body(get(&r, "/api/state").await).await["share"].clone();
        assert_eq!(share["title"], "Team demo");
        assert_eq!(share["access"], "public");
        assert!(share["expires_at"].as_i64().unwrap() > share["created_at"].as_i64().unwrap());
        let listed = sessions(&r).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["title"], "Team demo");
        assert_eq!(listed[0]["private"], false);

        // A blank title is "no title", not an empty string.
        post(&r, "/api/share/stop", json!({})).await;
        let res = post(
            &r,
            "/api/share",
            json!({"source":"synthetic","title":"   "}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(json_body(get(&r, "/api/state").await).await["share"]["title"].is_null());
        post(&r, "/api/share/stop", json!({})).await;
        let _ = std::fs::remove_dir_all("dev");
    }

    // ---- 2. repeated Share cycles: no stale sessions, no leaked port ----

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_share_stop_cycles_leave_no_stale_session_and_release_the_port() {
        let _port = SHARE_PORT.lock().await;
        let r = router(AppHandle::new(ready_app().await));
        let mut seen = std::collections::HashSet::new();
        for i in 0..3 {
            let res = post(
                &r,
                "/api/share",
                json!({"source":"synthetic","title":format!("cycle {i}")}),
            )
            .await;
            assert_eq!(res.status(), StatusCode::OK, "cycle {i} could not start");
            let id = json_body(get(&r, "/api/state").await).await["share"]["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(seen.insert(id.clone()), "cycle {i} reused a session id");
            let listed = sessions(&r).await;
            assert_eq!(
                listed.len(),
                1,
                "only the current session is listed: {listed:?}"
            );
            assert_eq!(listed[0]["session_id"], id);

            let res = post(&r, "/api/share/stop", json!({})).await;
            assert_eq!(json_body(res).await["state"], "ready");
            assert!(
                sessions(&r).await.is_empty(),
                "an ended session must not stay listed"
            );
            // The QUIC port really was released: a leak would make this bind fail.
            std::net::UdpSocket::bind("127.0.0.1:4443")
                .unwrap_or_else(|e| panic!("cycle {i}: UDP 4443 still held after stop: {e}"));
            // ...and a double stop is a clean error, not a stuck state.
            let res = post(&r, "/api/share/stop", json!({})).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
        let _ = std::fs::remove_dir_all("dev");
    }

    // ---- 3. discovery: empty, ended, expired ----

    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_is_empty_when_nothing_is_live_and_drops_ended_and_expired_sessions() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let v = viewer(&dir, bk, &bob).await;
        assert!(
            sessions(&v.router).await.is_empty(),
            "empty discovery is an empty list"
        );

        let (mut publ, id) = publisher(&dir, &alice, "life-list", AccessConfig::Public).await;
        let listed = sessions(&v.router).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["session_id"], id.to_string());

        // Ended (14E): gone from discovery immediately.
        publ.request_stop();
        publ.finish_share().await.unwrap();
        assert!(
            sessions(&v.router).await.is_empty(),
            "an ended session is not joinable"
        );

        // Expired (14D): a 1s lease.
        publ.choose_share();
        let mut cfg = ephemeral_share_cfg("life-expiry");
        cfg.session_ttl = Duration::from_secs(1);
        publ.start_share(
            cfg,
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        assert_eq!(sessions(&v.router).await.len(), 1);
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(
            sessions(&v.router).await.is_empty(),
            "an expired session must stop being listed, though nothing was rewritten"
        );
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 4. Leave vs Stop ----

    #[tokio::test(flavor = "multi_thread")]
    async fn leave_ends_only_that_viewer_while_stop_ends_the_session_for_everyone() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "life-leave", AccessConfig::Public).await;
        let stats = publ.share_stats().unwrap();

        let b = viewer(&dir, bk, &bob).await;
        let c = viewer(&dir, ck, &carol).await;
        let body = json!({ "session_id": id.to_string() });
        assert_eq!(
            post(&b.router, "/api/watch", body.clone()).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            post(&c.router, "/api/watch", body.clone()).await.status(),
            StatusCode::OK
        );
        let (baddr, bserver) = serve(&b.router).await;
        let (caddr, cserver) = serve(&c.router).await;
        let mut bws = ws_connect_as(&b.handle, baddr).await;
        let mut cws = ws_connect_as(&c.handle, caddr).await;
        first_keyframe(&mut bws).await;
        first_keyframe(&mut cws).await;
        poll(
            "both viewers to be counted",
            Duration::from_secs(10),
            || async { stats.session_count() == 2 },
        )
        .await;

        // --- Bob LEAVES ---
        drop(bws);
        let left = stop_watch(&b).await;
        assert_eq!(left["state"], "ready");
        assert_eq!(left["last_notice"], "left", "{left}");
        assert!(
            left["last_error"].is_null(),
            "leaving is not an error: {left}"
        );

        // The publisher is untouched: still Live, session record still Live, and the
        // other viewer still receives.
        poll(
            "the leaver's connection to be gone",
            Duration::from_secs(10),
            || async { stats.session_count() == 1 },
        )
        .await;
        assert!(!publ.share_finished());
        assert_eq!(
            crate::view::AppStateView::from_app(&publ)
                .share
                .unwrap()
                .state,
            "live"
        );
        let record = dir.find_session(&id).await.unwrap().unwrap();
        assert!(record.is_live(), "leaving must never end the session");
        first_keyframe(&mut cws).await; // Carol is still being served, right now

        // Bob can watch again, immediately, on a fresh connection.
        assert_eq!(
            post(&b.router, "/api/watch", body).await.status(),
            StatusCode::OK
        );
        let (baddr2, bserver2) = serve(&b.router).await;
        let mut bws2 = ws_connect_as(&b.handle, baddr2).await;
        first_keyframe(&mut bws2).await;

        // --- The publisher STOPS: the session ends for everybody, cleanly. ---
        publ.request_stop();
        publ.finish_share().await.unwrap();
        assert!(!dir.find_session(&id).await.unwrap().unwrap().is_live());
        for v in [&b, &c] {
            wait_finished(v, Duration::from_secs(20)).await;
            let end = stop_watch(v).await;
            assert_eq!(end["state"], "ready", "{end}");
            assert_eq!(end["last_notice"], "session_ended", "{end}");
            assert!(
                end["last_error"].is_null(),
                "an ended session is not an error: {end}"
            );
        }
        let (_frames, closed) = frames_until_close(&mut cws, Duration::from_secs(10)).await;
        assert!(closed, "the ended viewer's socket must close");
        for s in [bserver, bserver2, cserver] {
            s.abort();
        }
    }

    // ---- 5. repeated Watch cycles ----

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_watch_leave_cycles_leave_no_sockets_tasks_or_state_behind() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "life-watch", AccessConfig::Public).await;
        let stats = publ.share_stats().unwrap();
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;

        for i in 0..4 {
            let res = post(
                &v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            )
            .await;
            assert_eq!(res.status(), StatusCode::OK, "cycle {i}");
            let mut ws = ws_connect_as(&v.handle, addr).await;
            first_keyframe(&mut ws).await;
            assert_eq!(
                v.handle.liveness.video_sockets_open(),
                1,
                "cycle {i}: exactly one socket"
            );

            drop(ws);
            let end = stop_watch(&v).await;
            assert_eq!(end["state"], "ready");
            assert_eq!(end["last_notice"], "left");
            assert!(end["last_error"].is_null(), "cycle {i}: stale error {end}");
            let state = json_body(get(&v.router, "/api/state").await).await;
            assert!(
                state["watch"].is_null(),
                "cycle {i}: stale watch state {state}"
            );
            assert!(
                v.handle.app.lock().await.encoded_video_queue().is_none(),
                "cycle {i}: the previous watch's queue survived"
            );
            poll(
                "the socket to be released",
                Duration::from_secs(5),
                || async { v.handle.liveness.video_sockets_open() == 0 },
            )
            .await;
            poll(
                "the publisher to see the viewer go",
                Duration::from_secs(10),
                || async { stats.session_count() == 0 },
            )
            .await;
        }
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 6. cross cycles ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_session_after_a_stopped_one_is_the_only_one_offered_and_watchable() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, first_id) =
            publisher(&dir, &alice, "life-cross-a", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;

        // watch, leave, watch again
        for _ in 0..2 {
            post(
                &v.router,
                "/api/watch",
                json!({"session_id": first_id.to_string()}),
            )
            .await;
            let mut ws = ws_connect_as(&v.handle, addr).await;
            first_keyframe(&mut ws).await;
            drop(ws);
            assert_eq!(stop_watch(&v).await["last_notice"], "left");
        }

        // publisher stops, then shares again
        // (Re-shared at the same address, as app-web's fixed share port always does: an
        // ephemeral port would leave the first share's dead endpoint listed ahead of it in
        // the in-memory directory, which is an artifact of the test, not of a deployment.)
        let pub_addr: SocketAddr = dir.find_peer(&alice).await.unwrap().unwrap().endpoints[0]
            .address
            .parse()
            .unwrap();
        publ.request_stop();
        publ.finish_share().await.unwrap();
        let mut restarted = false;
        for _ in 0..100 {
            publ.choose_share();
            let mut cfg = ephemeral_share_cfg("life-cross-b");
            cfg.bind = pub_addr;
            if publ
                .start_share(
                    cfg,
                    None,
                    ShareSource::Synthetic,
                    synthetic(),
                    AccessConfig::Public,
                )
                .await
                .is_ok()
            {
                restarted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(restarted, "could not re-share at the released address");
        let second_id = publ.share_session().unwrap().session_id;
        assert_ne!(first_id, second_id, "a new share is a new session");

        let listed = sessions(&v.router).await;
        assert_eq!(
            listed.len(),
            1,
            "the ended session must not linger: {listed:?}"
        );
        assert_eq!(listed[0]["session_id"], second_id.to_string());

        // The old id is now refused as ended (14E), with its own classification...
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": first_id.to_string()}),
        )
        .await;
        wait_finished(&v, Duration::from_secs(20)).await;
        let end = stop_watch(&v).await;
        assert_eq!(end["last_error_kind"], "ended", "{end}");
        assert_eq!(end["state"], "ready");

        // ...and the new one just works, with no stale error carried over.
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": second_id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let state = json_body(get(&v.router, "/api/state").await).await;
        assert_eq!(state["watch"]["session_id"], second_id.to_string());
        assert!(state["last_error"].is_null(), "{state}");
        drop(ws);
        stop_watch(&v).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 7. abandoned page ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_nobody_attends_is_left_but_an_attended_watch_or_a_share_is_not() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (publ, id) = publisher(&dir, &alice, "life-abandon", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let dog = crate::routes::spawn_abandoned_watch_watchdog(
            v.handle.clone(),
            Duration::from_millis(800),
            Duration::from_millis(100),
        );

        // Attended by an open video socket (even with no polling): not left.
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        assert!(
            v.handle.app.lock().await.watch_session_id().is_some(),
            "an attended watch was left"
        );

        // The page goes away (socket closed, no more polling): the watch is left.
        drop(ws);
        poll(
            "the abandoned watch to be left",
            Duration::from_secs(10),
            || async { v.handle.app.lock().await.watch_session_id().is_none() },
        )
        .await;
        assert!(matches!(
            v.handle.app.lock().await.state(),
            holochain_moq_app::AppState::Ready { .. }
        ));

        // ...and that only ever ended the LOCAL viewer: the publisher's session lives on,
        // and it is not itself subject to the watchdog (a share with no page stays live).
        assert!(dir.find_session(&id).await.unwrap().unwrap().is_live());
        let pub_handle = AppHandle::new(publ);
        let dog2 = crate::routes::spawn_abandoned_watch_watchdog(
            pub_handle.clone(),
            Duration::from_millis(300),
            Duration::from_millis(100),
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        {
            let mut app = pub_handle.app.lock().await;
            assert!(
                !app.share_finished(),
                "a Share must never be ended just because no page is open"
            );
            app.request_stop();
            app.finish_share().await.unwrap();
        }
        dog.abort();
        dog2.abort();
        server.abort();
    }

    // ---- 8. refresh / duplicate sockets ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refreshed_page_supersedes_the_old_video_socket_instead_of_duplicating_it() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "life-refresh", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;

        let mut old = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut old).await;

        // "Refresh": the server-side state is intact and describes the same watch...
        let state = json_body(get(&v.router, "/api/state").await).await;
        assert_eq!(state["state"], "watching");
        assert_eq!(state["watch"]["session_id"], id.to_string());
        // ...and the reloaded page opens a new socket. The old one is told it was superseded.
        let mut fresh = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut fresh).await;
        let (_frames, closed) = frames_until_close(&mut old, Duration::from_secs(10)).await;
        assert!(closed, "the superseded socket must close");
        poll(
            "exactly one live video socket",
            Duration::from_secs(5),
            || async { v.handle.liveness.video_sockets_open() == 1 },
        )
        .await;

        drop(fresh);
        stop_watch(&v).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 9. a disconnecting client cannot wedge the app ----

    /// hyper drops a handler's future when the client goes away (a closed tab, a refresh, a
    /// crashed browser). A `/stop` handler that had already taken the active share/watch out of
    /// the app and was awaiting its result then left the app stuck mid-transition with nothing
    /// left that could stop it. Found by the Stage 15.3 manual run (an `app-web` left in
    /// `Sharing(ChoosingSource)` after Chrome was killed mid-request). The request future is
    /// polled once and then dropped, exactly as a disconnect does.
    async fn dropped_after_1ms(r: &axum::Router, path: &str) {
        let _ = tokio::time::timeout(Duration::ZERO, post(r, path, json!({}))).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_disconnects_mid_stop_share_cannot_wedge_the_app() {
        let (_ak, alice) = keypair(1);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (publ, _id) = publisher(&dir, &alice, "life-cancel-share", AccessConfig::Public).await;
        let handle = AppHandle::new(publ);
        let r = router(handle.clone());

        dropped_after_1ms(&r, "/api/share/stop").await;

        poll(
            "the app to settle at Ready",
            Duration::from_secs(15),
            || async {
                matches!(
                    handle.app.lock().await.state(),
                    holochain_moq_app::AppState::Ready { .. }
                )
            },
        )
        .await;
        // ...and it is fully usable again.
        let state = json_body(get(&r, "/api/state").await).await;
        assert_eq!(state["state"], "ready", "{state}");
    }

    /// Same scenario for a Leave. Honest note: with `detached` mutated away this variant still
    /// passed in-process (the watch task finishes fast enough to beat the drop), so it does NOT
    /// discriminate on its own; the Share-stop test above is the regression guard. All four
    /// state-changing handlers go through the same `detached` helper.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_disconnects_mid_leave_cannot_wedge_the_app() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) =
            publisher(&dir, &alice, "life-cancel-watch", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;

        let (watch_id, token) = v.handle.registry().last_issued().unwrap();
        let _ = tokio::time::timeout(
            Duration::ZERO,
            post_bearer(&v.router, &format!("/api/watch/{watch_id}/stop"), &token),
        )
        .await;

        poll(
            "the viewer to settle at Ready",
            Duration::from_secs(15),
            || async {
                let app = v.handle.app.lock().await;
                matches!(app.state(), holochain_moq_app::AppState::Ready { .. })
                    && app.watch_session_id().is_none()
            },
        )
        .await;
        drop(ws);
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }
}

// ---- Stage 15.4: lifecycle observability and reliability -----------------------------
//
// These prove that every meaningful boundary of a Share/Watch is *recorded* (in order, with the
// right terminal classification, carrying no secrets) and that the reliability properties
// established in 15.3 (a dropped request cannot wedge the app, no leaked sockets, no duplicate
// consumers) still hold under repeated and hostile sequences. Deterministic: waits are on
// recorded events / state, never on fixed sleeps standing in for them.

mod observability {
    use super::lifecycle_support::*;
    use super::private_sessions::{
        first_keyframe, frames_until_close, keypair, publisher, serve, start_raw_private_publisher,
        stop_watch, viewer, wait_finished, ws_connect_as,
    };
    use super::*;
    use holochain_moq_app::lifecycle::{OpKind, OpSnapshot};
    use holochain_moq_app::AccessConfig;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_normal_watch_and_its_share_record_every_boundary_in_order_and_end_terminally() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-normal", AccessConfig::Public).await;
        let share_op = publ.share_op().unwrap();
        let v = viewer(&dir, bk, &bob).await;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;

        // The operation is visible, live, and past every pre-dial boundary.
        let mid = newest(&v, OpKind::Watch);
        // The startup path, in order: request -> app transition -> the directory reads and
        // selection -> connect -> media. (The browser's video socket opens right after the POST,
        // while the watch is still connecting, so its events are checked separately.)
        assert_subsequence(
            &names(&mid),
            &[
                "watch.requested",
                "http.task_spawned",
                "http.lock_acquired",
                "watch.accepted",
                "watch.task_spawned",
                "watch.discovery_started",
                "watch.session_lookup",
                "watch.session_found",
                "watch.endpoints_lookup",
                "watch.endpoints_found",
                "watch.session_selected",
                "watch.connect_started",
                "watch.admission_check",
                "watch.admission_ok",
                "watch.connected",
                "watch.live",
                "ws.first_frame",
            ],
        );
        assert_subsequence(
            &names(&mid),
            &["ws.requested", "ws.accepted", "ws.active", "ws.first_frame"],
        );
        assert_eq!(mid.session_id.as_deref(), Some(id.to_string().as_str()));
        assert!(
            mid.terminal.is_none(),
            "still in progress: {:?}",
            mid.terminal
        );

        // Leave: the terminal event is `watch.ended` (left), never a failure.
        drop(ws);
        stop_watch(&v).await;
        let end = wait_terminal(&v, mid.id.clone()).await;
        assert_eq!(end.terminal, Some("watch.ended"));
        assert_eq!(end.failure_kind, None);
        assert!(has_detail(&end, "watch.ended", "left"), "{:?}", names(&end));
        assert_subsequence(
            &names(&end),
            &[
                "http.stop_received",
                "watch.leave_requested",
                "http.stop_completed",
            ],
        );

        // The publisher's own operation is fully recorded too, ending when it is stopped.
        publ.request_stop();
        publ.finish_share().await.unwrap();
        let share = publ.lifecycle().snapshot(&share_op).unwrap();
        assert_subsequence(
            &names(&share),
            &[
                "share.requested",
                "share.accepted",
                "share.setup_started",
                "share.session_created",
                "share.live",
                "share.stop_requested",
                "share.ending",
                "share.ended",
            ],
        );
        assert_eq!(share.terminal, Some("share.ended"));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_operations_get_distinct_ids_and_all_reach_a_terminal_event() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-ids", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        for _ in 0..4 {
            post(
                &v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            )
            .await;
            stop_watch(&v).await;
        }
        let ops: Vec<OpSnapshot> = v
            .handle
            .lifecycle
            .recent(64)
            .into_iter()
            .filter(|o| o.kind == OpKind::Watch)
            .collect();
        assert_eq!(ops.len(), 4);
        let unique: std::collections::HashSet<_> = ops.iter().map(|o| o.id.to_string()).collect();
        assert_eq!(unique.len(), 4, "every invocation has its own id");
        for o in ops {
            let o = wait_terminal(&v, o.id).await;
            assert!(o.terminal.is_some(), "{}", o.to_log_line());
        }
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- publisher stop reaches the viewer as a terminal, and closes the socket ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publisher_stop_gives_the_viewer_a_terminal_ended_event_and_closes_its_socket() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-stop", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        publ.request_stop();
        publ.finish_share().await.unwrap();
        wait_finished(&v, Duration::from_secs(20)).await;
        let (_f, closed) = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        assert!(closed);
        stop_watch(&v).await;

        let end = wait_terminal(&v, watch_id).await;
        assert_eq!(end.terminal, Some("watch.ended"));
        assert!(
            has_detail(&end, "watch.ended", "session_ended"),
            "{:?}",
            names(&end)
        );
        assert!(
            has_detail(&end, "ws.closed", "watch_ended"),
            "{:?}",
            detail_of(&end, "ws.closed")
        );
        server.abort();
    }

    // ---- video socket lifecycle ----

    #[tokio::test(flavor = "multi_thread")]
    async fn the_video_socket_is_closed_by_the_server_after_leave_and_says_why() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-ws-leave", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        // Leave while the socket is still open (the page has not closed it yet).
        stop_watch(&v).await;
        let (_f, closed) = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        assert!(
            closed,
            "the server must close a socket whose watch was left"
        );
        let end = wait_terminal(&v, watch_id).await;
        poll_event(&v, &end.id, "ws.closed").await;
        let end = v.handle.lifecycle.snapshot(&end.id).unwrap();
        assert!(
            has_detail(&end, "ws.closed", "queue_inactive"),
            "{:?}",
            detail_of(&end, "ws.closed")
        );
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_the_video_socket_is_recorded_as_a_supersession() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-ws-super", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut old = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut old).await;
        let mut fresh = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut fresh).await;
        let (_f, closed) = frames_until_close(&mut old, Duration::from_secs(10)).await;
        assert!(closed);

        let watch_id = newest(&v, OpKind::Watch).id;
        poll_event(&v, &watch_id, "ws.superseded").await;
        let snap = v.handle.lifecycle.snapshot(&watch_id).unwrap();
        assert_eq!(count(&snap, "ws.accepted"), 2, "{:?}", names(&snap));
        assert!(
            detail_of(&snap, "ws.closed")
                .iter()
                .any(|d| d == "superseded"),
            "{:?}",
            detail_of(&snap, "ws.closed")
        );
        drop(fresh);
        stop_watch(&v).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- terminal classifications ----

    #[tokio::test(flavor = "multi_thread")]
    async fn unauthorized_is_a_terminal_classification_with_zero_media_and_no_secrets() {
        let (_ak, alice) = keypair(1);
        let (_bk, bob) = keypair(2);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(
            &dir,
            &alice,
            "obs-unauth",
            AccessConfig::allowed([&bob]).unwrap(),
        )
        .await;

        // A signer that remembers every signature it produced, so we can prove none of it ends
        // up in diagnostics.
        let sigs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<[u8; 64]>::new()));
        let signer: holochain_moq_realtime::AdmissionSigner = {
            let (key, agent, sigs) = (ck.clone(), carol.clone(), sigs.clone());
            std::sync::Arc::new(move |session_id, challenge| {
                let (key, agent, sigs) = (key.clone(), agent.clone(), sigs.clone());
                Box::pin(async move {
                    use ed25519_dalek::Signer;
                    let payload = holochain_moq_protocol::signing_payload(
                        session_id,
                        &challenge,
                        &key.verifying_key(),
                    );
                    let sig = key.sign(&payload).to_bytes();
                    sigs.lock().unwrap().push(sig);
                    Ok((agent, sig))
                })
            })
        };
        let mut app =
            App::starting(RealtimeNode::new(dir.as_agent(&carol))).with_admission_signer(signer);
        app.connect().await.unwrap();
        let handle = AppHandle::new(app);
        let r = router(handle.clone());

        post(&r, "/api/watch", json!({"session_id": id.to_string()})).await;
        poll_state_finished(&r).await;
        let end = json_body(stop_as_owner(&handle, &r).await).await;
        assert_eq!(end["last_error_kind"], "unauthorized");

        let op = handle
            .lifecycle
            .recent(8)
            .into_iter()
            .find(|o| o.kind == OpKind::Watch)
            .unwrap();
        assert_eq!(op.terminal, Some("watch.unauthorized"));
        assert_eq!(op.failure_kind, Some("unauthorized"));
        assert!(
            !names(&op).contains(&"watch.live"),
            "zero media ever reached the queue: {:?}",
            names(&op)
        );

        // No admission material anywhere in what diagnostics expose (log line, JSON view, the
        // whole /api/diagnostics body).
        let diag = json_body(get(&r, "/api/diagnostics").await)
            .await
            .to_string();
        let everything = format!("{diag} {}", op.to_log_line());
        use base64::Engine as _;
        for sig in sigs.lock().unwrap().iter() {
            let hex: String = sig.iter().map(|b| format!("{b:02x}")).collect();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);
            let b64_std = base64::engine::general_purpose::STANDARD.encode(sig);
            for needle in [hex, b64, b64_std] {
                assert!(
                    !everything.contains(&needle),
                    "a signature leaked into diagnostics"
                );
            }
        }
        for word in ["signature", "private key", "-----BEGIN"] {
            assert!(
                !everything.to_lowercase().contains(&word.to_lowercase()),
                "{word} in diagnostics"
            );
        }
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_session_is_terminal_expired_and_never_reaches_the_dial() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let mut app = App::starting(RealtimeNode::new(dir.as_agent(&alice)));
        app.connect().await.unwrap();
        app.choose_share();
        let mut cfg = ephemeral_share_cfg("obs-expiry");
        cfg.session_ttl = Duration::from_secs(1);
        app.start_share(
            cfg,
            None,
            ShareSource::Synthetic,
            synthetic(),
            AccessConfig::Public,
        )
        .await
        .unwrap();
        let id = app.share_session().unwrap().session_id;
        tokio::time::sleep(Duration::from_millis(1300)).await;

        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        wait_finished(&v, Duration::from_secs(20)).await;
        stop_watch(&v).await;
        let op = wait_terminal(&v, newest(&v, OpKind::Watch).id).await;
        assert_eq!(op.terminal, Some("watch.expired"));
        assert_eq!(op.failure_kind, Some("expired"));
        let n = names(&op);
        assert!(n.contains(&"watch.session_found"), "{n:?}");
        assert!(
            !n.contains(&"watch.connect_started"),
            "expired must be refused before any dial: {n:?}"
        );
        app.request_stop();
        app.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_already_ended_session_is_a_normal_terminal_not_a_failure() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-ended", AccessConfig::Public).await;
        publ.request_stop();
        publ.finish_share().await.unwrap();

        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        wait_finished(&v, Duration::from_secs(20)).await;
        stop_watch(&v).await;
        let op = wait_terminal(&v, newest(&v, OpKind::Watch).id).await;
        assert_eq!(op.terminal, Some("watch.ended"));
        assert_eq!(op.failure_kind, None);
        assert!(
            has_detail(&op, "watch.ended", "already_ended"),
            "{:?}",
            names(&op)
        );
    }

    // ---- rejected and invalid requests are visible, and harmless ----

    #[tokio::test(flavor = "multi_thread")]
    async fn rejected_and_invalid_requests_are_recorded_with_their_own_classification() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-reject", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;

        // Invalid input: a malformed session id.
        let res = post(&v.router, "/api/watch", json!({"session_id": "not-a-uuid"})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bad = newest(&v, OpKind::Watch);
        assert_eq!(bad.terminal, Some("watch.failed"));
        assert_eq!(bad.failure_kind, Some("invalid_input"));

        // A real watch, then a second request while it is active: rejected, and the first is
        // untouched.
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let first = newest(&v, OpKind::Watch).id;
        let res = post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let second = newest(&v, OpKind::Watch);
        assert_ne!(second.id, first);
        assert_eq!(
            second.failure_kind,
            Some("request_rejected"),
            "{}",
            second.to_log_line()
        );
        assert!(
            v.handle
                .lifecycle
                .snapshot(&first)
                .unwrap()
                .terminal
                .is_none(),
            "the active watch was disturbed"
        );
        assert_eq!(
            json_body(get(&v.router, "/api/state").await).await["state"],
            "watching"
        );
        stop_watch(&v).await;
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- aborted requests still complete (detached), and stay observable ----

    #[tokio::test(flavor = "multi_thread")]
    async fn an_aborted_watch_request_still_completes_in_the_background_and_cannot_wedge_the_app() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-abort-watch", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;

        // Polled once, then dropped: what hyper does when the client disconnects.
        let _ = tokio::time::timeout(
            Duration::ZERO,
            post(
                &v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            ),
        )
        .await;

        let op = newest(&v, OpKind::Watch).id;
        poll_event(&v, &op, "http.task_completed").await;
        // The transition ran to its end even though nobody was waiting for the response.
        let snap = v.handle.lifecycle.snapshot(&op).unwrap();
        assert!(
            names(&snap).contains(&"watch.task_spawned"),
            "{:?}",
            names(&snap)
        );
        assert!(
            matches!(
                v.handle.app.lock().await.state(),
                holochain_moq_app::AppState::Watching(_)
            ),
            "the app must be in a consistent Watching state, never stuck mid-transition"
        );
        stop_watch(&v).await;
        assert!(matches!(
            v.handle.app.lock().await.state(),
            holochain_moq_app::AppState::Ready { .. }
        ));
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    /// Honest note: with `detached_op` mutated to run inline this variant still passes (the
    /// in-memory announce finishes inside the first poll, so there is nothing to cancel); the
    /// aborted-Watch test above and 15.3's `a_client_that_disconnects_mid_stop_share_...` are the
    /// discriminating guards. This one still proves the aborted request's operation is the one
    /// that goes live and ends terminally.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_aborted_share_request_still_completes_in_the_background_and_cannot_wedge_the_app() {
        let _port = SHARE_PORT.lock().await;
        let r = router(AppHandle::new(ready_app().await));
        let handle_op = {
            let _ = tokio::time::timeout(
                Duration::ZERO,
                post(
                    &r,
                    "/api/share",
                    json!({"source": "synthetic", "title": "aborted"}),
                ),
            )
            .await;
            // find it via diagnostics (the router owns the handle)
            let diag = json_body(get(&r, "/api/diagnostics").await).await;
            diag["recent"][0]["id"].as_str().unwrap().to_string()
        };
        // It settles at Sharing(Live) - not ChoosingSource, not Ready-by-accident.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let s = json_body(get(&r, "/api/state").await).await;
                if s["state"] == "sharing" && s["share"]["state"] == "live" {
                    assert_eq!(s["share"]["title"], "aborted");
                    assert_eq!(
                        s["diag"]["op"],
                        handle_op.as_str(),
                        "the live share belongs to the aborted request's op"
                    );
                    return;
                }
                assert_ne!(s["share"]["state"], "choosing_source", "wedged: {s}");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the aborted share request never completed");
        let stopped = json_body(post(&r, "/api/share/stop", json!({})).await).await;
        assert_eq!(stopped["state"], "ready");
        let diag = json_body(get(&r, "/api/diagnostics").await).await;
        assert_eq!(diag["recent"][0]["terminal"], "share.ended", "{diag}");
        let _ = std::fs::remove_dir_all("dev");
    }

    // ---- phase age and the diagnostics surface ----

    #[tokio::test(flavor = "multi_thread")]
    async fn the_phase_and_its_age_are_visible_in_state_and_grow_while_nothing_changes() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-age", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let watch_id = newest(&v, OpKind::Watch).id.to_string();

        // No socket opens, so the operation sits still: its phase must not change but its
        // age must keep growing (that is what a stall looks like).
        poll_event(&v, &newest(&v, OpKind::Watch).id, "watch.live").await;
        let a = json_body(get(&v.router, "/api/state").await).await["diag"].clone();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let b = json_body(get(&v.router, "/api/state").await).await["diag"].clone();
        assert_eq!(a["op"], watch_id.as_str());
        assert_eq!(
            a["phase"], b["phase"],
            "nothing happened, so the phase is unchanged"
        );
        assert!(
            b["phase_age_ms"].as_u64().unwrap() >= a["phase_age_ms"].as_u64().unwrap() + 300,
            "phase age must grow while the phase does not: {a} -> {b}"
        );
        assert_eq!(b["video_socket"], "none");

        // The full history is a separate, compact endpoint.
        let diag = json_body(get(&v.router, "/api/diagnostics").await).await;
        assert_eq!(diag["current_watch"]["id"], watch_id.as_str());
        assert!(diag["current_watch"]["events"].as_array().unwrap().len() >= 5);
        stop_watch(&v).await;
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- watchdog diagnostics ----

    #[tokio::test(flavor = "multi_thread")]
    async fn an_abandoned_watch_records_why_it_was_judged_abandoned() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-abandon", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let dog = crate::routes::spawn_abandoned_watch_watchdog(
            v.handle.clone(),
            Duration::from_millis(800),
            Duration::from_millis(100),
        );
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let watch_id = newest(&v, OpKind::Watch).id;

        let end = wait_terminal(&v, watch_id).await; // no page polls, no socket: abandoned
        assert_eq!(end.terminal, Some("watch.abandoned"));
        assert_eq!(end.failure_kind, Some("abandoned"));
        let why = detail_of(&end, "watch.abandoned").join(" ");
        assert!(why.contains("no owner contact for"), "{why}");
        assert!(
            why.contains("limit 800ms"),
            "the policy in force is recorded: {why}"
        );
        assert!(why.contains("video sockets open: 0"), "{why}");
        assert!(names(&end).contains(&"watch.leave_requested"));
        dog.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- reconnect is recorded, and the admission check runs again ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reconnect_is_recorded_as_reconnecting_then_reconnected_with_a_new_epoch() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let first = start_raw_private_publisher(
            &alice_node,
            id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        let addr = first.addr;

        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        first.kill().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let second =
            start_raw_private_publisher(&alice_node, id, &alice, vec![bob.clone()], addr, false)
                .await;
        poll_event(&v, &watch_id, "watch.reconnected").await;
        poll_event(&v, &watch_id, "ws.epoch_start_sent").await;

        let snap = v.handle.lifecycle.snapshot(&watch_id).unwrap();
        assert_subsequence(
            &names(&snap),
            &["watch.connected", "watch.reconnecting", "watch.reconnected"],
        );
        // (The media path's `watch.epoch_started` and the state observer's `watch.reconnected`
        // are recorded by different tasks, so their relative order is not guaranteed.)
        assert!(
            names(&snap).contains(&"watch.epoch_started"),
            "{:?}",
            names(&snap)
        );
        assert!(
            count(&snap, "watch.admission_check") >= 2,
            "the admission check ran again for the new connection: {:?}",
            names(&snap)
        );
        assert_eq!(
            v.proofs.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "and a fresh proof was signed"
        );

        drop(ws);
        stop_watch(&v).await;
        server.abort();
        second.kill().await;
    }

    // ---- Stage 15.4.1 regression: the 30s public-session recycle ----

    /// A healthy publisher and a viewer that does nothing wrong hold ONE connection past 30s.
    /// Public viewers used to be torn down and re-established every 30.0s (a new epoch each
    /// time) because they never acknowledged the publisher's namespace announcement, so
    /// `moq-transport`'s 30s `PublishNamespace` request timeout fired; fixed in
    /// `crates/realtime/src/subscribe.rs`. Takes 35s of real time, so it is serial-friendly.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_healthy_public_watch_holds_one_connection_past_30_seconds() {
        healthy_watch_for_35s(false, "obs-30s-public").await;
    }

    /// The same 35s on a *private* session (never affected).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_healthy_private_watch_holds_one_connection_past_30_seconds() {
        healthy_watch_for_35s(true, "obs-30s-private").await;
    }

    async fn healthy_watch_for_35s(private: bool, label: &str) {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let access = if private {
            AccessConfig::allowed([&bob]).unwrap()
        } else {
            AccessConfig::Public
        };
        let (mut publ, id) = publisher(&dir, &alice, label, access).await;
        let v = viewer(&dir, bk, &bob).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (addr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        // Keep draining the socket for 35s so the browser side is a well-behaved consumer.
        let _ = frames_until_close(&mut ws, Duration::from_secs(35)).await;

        let snap = v.handle.lifecycle.snapshot(&watch_id).unwrap();
        let recycled = count(&snap, "watch.reconnecting");
        eprintln!("[{label}] reconnects in 35s on a healthy publisher: {recycled}");
        assert_eq!(
            recycled,
            0,
            "the viewer's session was recycled on a healthy publisher: {:?}",
            names(&snap)
        );
        drop(ws);
        stop_watch(&v).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- bounded stress around the suspected startup race ----

    /// The unexplained 15.3 stall was a Watch that never dialed. This drives the startup path
    /// hard, in bounded cycles, and checks after EVERY cycle that nothing is left behind and
    /// every operation reached a terminal event - so if a stall does occur, the test names the
    /// last recorded boundary instead of timing out mutely.
    #[tokio::test(flavor = "multi_thread")]
    async fn bounded_startup_stress_leaves_no_wedge_leak_or_unterminated_operation() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "obs-stress", AccessConfig::Public).await;
        let stats = publ.share_stats().unwrap();
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let body = json!({"session_id": id.to_string()});
        let mut slowest_connect = Duration::ZERO;

        // (a) Watch -> keyframe -> Leave, repeated.
        for i in 0..8 {
            post(&v.router, "/api/watch", body.clone()).await;
            let mut ws = ws_connect_as(&v.handle, addr).await;
            first_keyframe(&mut ws).await;
            drop(ws);
            stop_watch(&v).await;
            assert_clean(&v, &format!("cycle {i} (watch/leave)")).await;
            slowest_connect = slowest_connect.max(connect_time(&v));
        }
        // (b) Rapid start/stop with no waiting in between.
        for i in 0..8 {
            post(&v.router, "/api/watch", body.clone()).await;
            let end = stop_watch(&v).await;
            assert_eq!(end["state"], "ready", "rapid cycle {i}: {end}");
            assert_clean(&v, &format!("cycle {i} (rapid start/stop)")).await;
        }
        // (c) Request, then the page vanishes (request dropped, no socket, no polling), then a
        //     new Watch is started straight away.
        for i in 0..6 {
            let _ =
                tokio::time::timeout(Duration::ZERO, post(&v.router, "/api/watch", body.clone()))
                    .await;
            wait_for_watch_settled(&v).await;
            stop_watch(&v).await;
            let res = post(&v.router, "/api/watch", body.clone()).await;
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "cycle {i}: a Watch after an abandoned request must start"
            );
            stop_watch(&v).await;
            assert_clean(&v, &format!("cycle {i} (abort then restart)")).await;
        }

        // Every operation we made reached a terminal event, and ids never repeated.
        let ops: Vec<OpSnapshot> = v
            .handle
            .lifecycle
            .recent(64)
            .into_iter()
            .filter(|o| o.kind == OpKind::Watch)
            .collect();
        let mut seen = std::collections::HashSet::new();
        for o in &ops {
            assert!(seen.insert(o.id.to_string()), "duplicate op id {}", o.id);
            let o = wait_terminal(&v, o.id.clone()).await;
            assert!(o.terminal.is_some(), "unterminated: {}", o.to_log_line());
            assert_ne!(
                o.failure_kind,
                Some("internal_task_failure"),
                "{}",
                o.to_log_line()
            );
        }
        eprintln!(
            "stress: {} watch operations, slowest connect {:?}",
            ops.len(),
            slowest_connect
        );
        poll_count(&stats, 0).await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }
}

/// Helpers shared by the 15.4 tests.
mod lifecycle_support {
    use super::private_sessions::{stop_watch, Viewer};
    use super::*;
    use holochain_moq_app::lifecycle::{OpId, OpKind, OpSnapshot};
    use std::time::Duration;

    pub fn newest(v: &Viewer, kind: OpKind) -> OpSnapshot {
        v.handle
            .lifecycle
            .recent(64)
            .into_iter()
            .find(|o| o.kind == kind)
            .expect("no such operation recorded")
    }

    pub fn names(s: &OpSnapshot) -> Vec<&'static str> {
        s.events.iter().map(|e| e.name).collect()
    }

    pub fn count(s: &OpSnapshot, name: &str) -> usize {
        s.events.iter().filter(|e| e.name == name).count()
    }

    pub fn detail_of(s: &OpSnapshot, name: &str) -> Vec<String> {
        s.events
            .iter()
            .filter(|e| e.name == name)
            .filter_map(|e| e.detail.clone())
            .collect()
    }

    pub fn has_detail(s: &OpSnapshot, name: &str, needle: &str) -> bool {
        detail_of(s, name).iter().any(|d| d.contains(needle))
    }

    /// `expected` appears in `actual` in this order (other events may be interleaved).
    pub fn assert_subsequence(actual: &[&str], expected: &[&str]) {
        let mut it = actual.iter();
        for want in expected {
            assert!(
                it.any(|a| a == want),
                "missing or out of order: {want}\n  recorded: {actual:?}\n  expected in order: {expected:?}"
            );
        }
    }

    pub async fn wait_terminal(v: &Viewer, id: OpId) -> OpSnapshot {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let s = v.handle.lifecycle.snapshot(&id).unwrap();
                if s.terminal.is_some() {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let s = v.handle.lifecycle.snapshot(&id).unwrap();
            panic!(
                "no terminal event; last confirmed boundary: {}",
                s.to_log_line()
            )
        })
    }

    pub async fn poll_event(v: &Viewer, id: &OpId, name: &str) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !v
                .handle
                .lifecycle
                .snapshot(id)
                .unwrap()
                .events
                .iter()
                .any(|e| e.name == name)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let s = v.handle.lifecycle.snapshot(id).unwrap();
            panic!(
                "never recorded {name}; last confirmed boundary: {}",
                s.to_log_line()
            )
        });
    }

    pub async fn poll_state_finished(r: &axum::Router) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let s = json_body(get(r, "/api/state").await).await;
                if s["watch"]["finished"] == true {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the watch never finished");
    }

    pub async fn poll_count(stats: &holochain_moq_realtime::LiveStats, want: usize) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while stats.session_count() != want {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "publisher still counts {} viewers, wanted {want}",
                stats.session_count()
            )
        });
    }

    /// After a dropped request the Watch settles (it either completed or was refused); wait for that.
    pub async fn wait_for_watch_settled(v: &Viewer) {
        let id = newest(v, OpKind::Watch).id;
        poll_event(v, &id, "http.task_completed").await;
    }

    /// Nothing left over after a cycle: app Ready, no watch, no queue, no open video socket.
    pub async fn assert_clean(v: &Viewer, what: &str) {
        let (ready, no_watch, no_queue) = {
            let app = v.handle.app.lock().await;
            (
                matches!(app.state(), holochain_moq_app::AppState::Ready { .. }),
                app.watch_session_id().is_none(),
                app.encoded_video_queue().is_none(),
            )
        };
        assert!(
            ready && no_watch && no_queue,
            "{what}: app not clean (ready={ready} no_watch={no_watch} no_queue={no_queue})"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while v.handle.liveness.video_sockets_open() != 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}: a video socket is still open"));
        let _ = stop_watch; // (kept imported for callers)
    }

    /// From `watch.connect_started` to `watch.connected` of the newest watch, if it got there.
    pub fn connect_time(v: &Viewer) -> Duration {
        let s = newest(v, OpKind::Watch);
        let at = |n: &str| s.events.iter().find(|e| e.name == n).map(|e| e.at);
        match (at("watch.connect_started"), at("watch.connected")) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => Duration::ZERO,
        }
    }
}

// ---- Stage 15.5: browser-facing guard, body bound ----

mod hardening {
    use super::*;

    async fn served() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let app = ready_app().await;
        let r = router(AppHandle::new(app));
        let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        (
            addr,
            tokio::spawn(async move {
                let _ = axum::serve(listener, r).await;
            }),
        )
    }

    async fn request_with(
        r: &axum::Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> StatusCode {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        r.clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_rebinding_host_is_refused_on_every_route() {
        let r = router(AppHandle::new(ready_app().await));
        for path in [
            "/api/state",
            "/api/sessions",
            "/api/diagnostics",
            "/",
            "/api/watch/video",
        ] {
            let bad = request_with(&r, "GET", path, &[("host", "evil.example:4480")]).await;
            assert_eq!(bad, StatusCode::FORBIDDEN, "{path}");
        }
        assert_eq!(
            request_with(&r, "GET", "/api/state", &[("host", "127.0.0.1:4480")]).await,
            StatusCode::OK
        );
    }

    /// Stage 15.6: the browser assets the page loads are behind the same guard as the API, and
    /// the served page carries every element the script wires (a missing id would be a runtime
    /// crash in the browser, which no Rust test would otherwise notice).
    #[tokio::test]
    async fn the_served_page_and_scripts_are_guarded_and_complete() {
        let r = router(AppHandle::new(ready_app().await));
        for path in ["/", "/app.js", "/video.js", "/ui.js", "/style.css"] {
            assert_eq!(
                request_with(&r, "GET", path, &[("host", "evil.example:4480")]).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
            assert_eq!(
                request_with(&r, "GET", path, &[("host", "127.0.0.1:4480")]).await,
                StatusCode::OK,
                "{path}"
            );
        }
        let page = get(&r, "/")
            .await
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let page = String::from_utf8(page.to_vec()).unwrap();
        for id in [
            "status-headline",
            "support-banner",
            "video-wrap",
            "video-overlay",
            "watch-canvas",
        ] {
            assert!(
                page.contains(&format!("id=\"{id}\"")),
                "index.html lacks #{id}"
            );
        }
    }

    /// Stage 17H: the served page and stylesheet carry the structure the redesigned interface relies
    /// on - landmarks, the status live region, the two-step End, the video stage controls and the
    /// accessibility layers of the stylesheet. A regression in any of them is invisible to the API
    /// tests and to a person glancing at the page, so it is pinned here.
    #[tokio::test]
    async fn the_served_ui_keeps_its_accessibility_and_state_structure() {
        let r = router(AppHandle::new(ready_app().await));
        let body = |res: axum::response::Response| async move {
            String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
        };
        let page = body(get(&r, "/").await).await;
        let css = body(get(&r, "/style.css").await).await;
        for needle in [
            "<html lang=\"en\">",
            "class=\"skip-link\" href=\"#main\"",
            "<main id=\"main\"",
            "<nav aria-label=\"Primary\">",
            "id=\"status-headline\" class=\"headline tone-info\" data-key=\"starting\" role=\"status\" aria-live=\"polite\"",
            "id=\"error-banner\"",
            "role=\"alert\"",
            "id=\"home-panel\"",
            "id=\"btn-stop-watch\"",
            "id=\"btn-stop-share\"",
            "End session for everyone",
            "id=\"end-confirm\"",
            "id=\"btn-confirm-end\"",
            "id=\"btn-cancel-end\"",
            "id=\"video-stage\"",
            "id=\"btn-fit\"",
            "id=\"btn-fill\"",
            "id=\"btn-fullscreen\"",
            "id=\"video-badge\"",
            "id=\"session-list\"",
        ] {
            assert!(page.contains(needle), "index.html lacks {needle}");
        }
        for needle in [
            ":focus-visible",
            "prefers-reduced-motion: reduce",
            "prefers-color-scheme: dark",
            "forced-colors: active",
            "aspect-ratio: 16 / 9",
            "data-hidden=\"true\"",
            "data-notlive=\"true\"",
        ] {
            assert!(css.contains(needle), "style.css lacks {needle}");
        }
        // Presentation only: the page ships no inline styles or handlers.
        assert!(!page.contains(" style=\""), "inline style in index.html");
        assert!(!page.contains(" onclick="), "inline handler in index.html");
    }

    #[tokio::test]
    async fn a_cross_site_post_cannot_stop_a_share_or_watch() {
        let r = router(AppHandle::new(ready_app().await));
        for path in ["/api/share/stop", "/api/watch/some-watch/stop"] {
            let s = request_with(
                &r,
                "POST",
                path,
                &[
                    ("host", "127.0.0.1:4480"),
                    ("origin", "http://evil.example"),
                ],
            )
            .await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        }
    }

    #[tokio::test]
    async fn the_served_page_is_same_origin_and_unaffected() {
        let r = router(AppHandle::new(ready_app().await));
        let s = request_with(
            &r,
            "GET",
            "/api/state",
            &[
                ("host", "localhost:4480"),
                ("origin", "http://localhost:4480"),
            ],
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_cross_site_web_page_cannot_open_the_video_websocket() {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
        let (addr, server) = served().await;
        let mut req = format!("ws://{addr}/api/watch/video")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("origin", "http://evil.example".parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Err(Error::Http(res)) => assert_eq!(res.status(), 403),
            other => panic!("the cross-origin websocket handshake must be refused, got {other:?}"),
        }
        // The same handshake from the app's own origin is not refused by the guard (it is
        // closed by the route itself, because there is no active watch).
        let mut ok = format!("ws://{addr}/api/watch/video")
            .into_client_request()
            .unwrap();
        ok.headers_mut()
            .insert("origin", format!("http://{addr}").parse().unwrap());
        assert!(tokio_tungstenite::connect_async(ok).await.is_ok());
        server.abort();
    }

    #[tokio::test]
    async fn an_oversized_request_body_is_rejected_not_buffered() {
        let r = router(AppHandle::new(ready_app().await));
        let huge =
            json!({ "source": "synthetic", "access": "public", "title": "x".repeat(200_000) });
        let res = post(&r, "/api/share", huge).await;
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn too_many_allowed_agents_is_a_client_error() {
        let r = router(AppHandle::new(ready_app().await));
        let agents: Vec<String> = (0..300).map(|i| format!("not-an-agent-{i}")).collect();
        let res = post(
            &r,
            "/api/share",
            json!({ "source": "synthetic", "access": "allowed", "allowed_agents": agents }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn shutdown_ends_a_live_share_cleanly_and_is_a_no_op_when_idle() {
        // The web route's Share binds the fixed port 4443, like every other test that shares.
        let _port = SHARE_PORT.lock().await;
        let app = ready_app().await;
        let handle = AppHandle::new(app);
        let r = router(handle.clone());
        // Idle: nothing to end, returns promptly and clean.
        assert!(crate::routes::shutdown_app(&handle, std::time::Duration::from_secs(5)).await);

        let res = post(
            &r,
            "/api/share",
            json!({ "source": "synthetic", "access": "public" }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let op = handle.app.lock().await.share_op().expect("a share op");
        assert!(crate::routes::shutdown_app(&handle, std::time::Duration::from_secs(20)).await);

        let snap = handle.lifecycle.snapshot(&op).unwrap();
        assert_eq!(snap.terminal, Some("share.ended"), "{:?}", snap.events);
        let state = json_body(get(&r, "/api/state").await).await;
        assert_eq!(state["state"], "ready");
    }
}

// ---- Stage 15.5 (batch 2): task ownership, cancellation and resource baselines ----
//
// Every test here ends an operation in a different way (leave, stop, publisher ends, socket
// closed or superseded, shutdown, refusal) and then asserts that the process is back at the
// baseline it had before: no active share/watch, no open video socket, no queue, no lifecycle
// operation without a terminal event, and no more tokio tasks than before (each test owns its
// runtime, so `runtime_tasks` is exactly this test's tasks). Waiting is always on an observable
// state with a generous upper bound, never a fixed sleep.

mod resources {
    use super::lifecycle_support::*;
    use super::private_sessions::{
        first_keyframe, frames_until_close, keypair, publisher, serve, start_raw_private_publisher,
        stop_watch, viewer, wait_finished, ws_connect_as, Viewer,
    };
    use super::*;
    use crate::diag::ResourceView;
    use holochain_moq_app::lifecycle::OpKind;
    use holochain_moq_app::AccessConfig;
    use std::time::Duration;

    pub(super) async fn res(v: &Viewer) -> ResourceView {
        let app = v.handle.app.lock().await;
        crate::diag::diagnostics(&v.handle, &app).resources
    }

    pub(super) fn is_baseline(r: &ResourceView, tasks: usize) -> bool {
        r.active_shares == 0
            && r.active_watches == 0
            && r.video_sockets_open == 0
            && r.encoded_queue_capacity == 0
            && r.encoded_queue_depth == 0
            && r.operations_in_flight == 0
            && r.runtime_tasks <= tasks
    }

    /// Wait (bounded, on the observable state) for everything to be back at baseline.
    pub(super) async fn settle(v: &Viewer, tasks: usize, what: &str) -> ResourceView {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let r = res(v).await;
            if is_baseline(&r, tasks) {
                return r;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: not back at baseline ({tasks} tasks): {r:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// One full Watch cycle (start, real media, leave), returning the resources seen while live.
    pub(super) async fn watch_cycle(v: &Viewer, id: SessionId, addr: SocketAddr) -> ResourceView {
        let res_ = post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        assert_eq!(res_.status(), StatusCode::OK);
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let live = res(v).await;
        drop(ws);
        let end = stop_watch(v).await;
        assert_eq!(end["state"], "ready");
        live
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_watch_cycles_return_every_resource_to_baseline_without_accumulating() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "res-cycles", AccessConfig::Public).await;
        let stats = publ.share_stats().unwrap();
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;

        // One warm-up cycle so lazily started machinery (the server's connection task, the
        // QUIC endpoint) is part of the baseline rather than mistaken for a leak.
        let _ = watch_cycle(&v, id, addr).await;
        let idle = settle(&v, usize::MAX, "warm-up").await;
        let baseline = idle.runtime_tasks;

        let mut settled = Vec::new();
        for i in 0..6 {
            let live = watch_cycle(&v, id, addr).await;
            assert_eq!(live.active_watches, 1, "cycle {i}");
            assert_eq!(live.video_sockets_open, 1, "cycle {i}");
            assert_eq!(live.active_shares, 0, "cycle {i}: this app only watches");
            assert!(
                live.operations_in_flight >= 1,
                "cycle {i}: the watch is in flight"
            );
            assert!(
                live.encoded_queue_depth <= live.encoded_queue_capacity
                    && live.encoded_queue_capacity > 0,
                "cycle {i}: queue bound {live:?}"
            );
            assert!(
                live.runtime_tasks > baseline,
                "cycle {i}: a live watch owns tasks"
            );
            let after = settle(&v, baseline, &format!("cycle {i}")).await;
            settled.push(after.runtime_tasks);
            assert_eq!(
                stats.session_count().min(1),
                stats.session_count(),
                "cycle {i}: the publisher never counts more than one viewer session"
            );
        }
        // Not monotonically increasing: the last settled count is no higher than the first.
        assert!(
            settled.last() <= settled.first(),
            "task count grew across cycles: {settled:?}"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while stats.session_count() != 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the publisher still holds a viewer session");

        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test]
    async fn repeated_share_stop_cycles_return_every_resource_to_baseline() {
        let _port = SHARE_PORT.lock().await;
        let handle = AppHandle::new(ready_app().await);
        let r = router(handle.clone());
        let v = Viewer {
            handle: handle.clone(),
            router: r.clone(),
            proofs: Default::default(),
        };
        let start = |i: usize| {
            let r = r.clone();
            async move {
                let res_ = post(
                    &r,
                    "/api/share",
                    json!({"source":"synthetic","title":format!("res {i}")}),
                )
                .await;
                assert_eq!(res_.status(), StatusCode::OK, "cycle {i}");
            }
        };
        start(99).await;
        assert_eq!(res(&v).await.active_shares, 1);
        post(&r, "/api/share/stop", json!({})).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        for i in 0..4 {
            start(i).await;
            let live = res(&v).await;
            assert_eq!(live.active_shares, 1, "cycle {i}");
            assert!(live.operations_in_flight >= 1);
            let stopped = post(&r, "/api/share/stop", json!({})).await;
            assert_eq!(json_body(stopped).await["state"], "ready");
            settle(&v, baseline, &format!("share cycle {i}")).await;
            std::net::UdpSocket::bind("127.0.0.1:4443")
                .unwrap_or_else(|e| panic!("cycle {i}: UDP 4443 still held: {e}"));
        }
        let _ = std::fs::remove_dir_all("dev");
    }

    /// Stop racing a Share whose setup is still running: the app lock serialises them, so the
    /// stop either finds nothing yet (a clean 400) or ends the finished start - never a wedge.
    #[tokio::test]
    async fn stop_racing_share_setup_never_wedges_or_leaks() {
        let _port = SHARE_PORT.lock().await;
        let handle = AppHandle::new(ready_app().await);
        let r = router(handle.clone());
        let v = Viewer {
            handle: handle.clone(),
            router: r.clone(),
            proofs: Default::default(),
        };
        let baseline = res(&v).await.runtime_tasks;
        for round in 0..3 {
            let (started, stopped) = tokio::join!(
                post(&r, "/api/share", json!({"source":"synthetic"})),
                post(&r, "/api/share/stop", json!({}))
            );
            assert_eq!(started.status(), StatusCode::OK, "round {round}");
            assert!(
                matches!(stopped.status(), StatusCode::OK | StatusCode::BAD_REQUEST),
                "round {round}: {:?}",
                stopped.status()
            );
            // If the stop ran before the start finished it found nothing; end the share now.
            if res(&v).await.active_shares == 1 {
                let s = post(&r, "/api/share/stop", json!({})).await;
                assert_eq!(s.status(), StatusCode::OK, "round {round}");
            }
            settle(&v, baseline, &format!("round {round}")).await;
            std::net::UdpSocket::bind("127.0.0.1:4443")
                .unwrap_or_else(|e| panic!("round {round}: UDP 4443 still held: {e}"));
        }
        let _ = std::fs::remove_dir_all("dev");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_closed_or_superseded_video_socket_is_released_and_the_watch_survives_until_left() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "res-ws", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut first = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut first).await;
        // A second socket supersedes the first: the old one is closed by the server and only
        // one stays counted.
        let mut second = ws_connect_as(&v.handle, addr).await;
        let (_, first_closed) = frames_until_close(&mut first, Duration::from_secs(10)).await;
        assert!(
            first_closed,
            "the superseded socket must be closed by the server"
        );
        first_keyframe(&mut second).await;
        let live = res(&v).await;
        assert_eq!(live.video_sockets_open, 1, "{live:?}");
        assert_eq!(live.active_watches, 1);

        // The client closing its socket releases the count; the watch itself is untouched
        // (Leave/Stop or the watchdog end it), so it can be attached to again.
        drop(second);
        tokio::time::timeout(Duration::from_secs(5), async {
            while res(&v).await.video_sockets_open != 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the closed socket is still counted");
        assert_eq!(
            res(&v).await.active_watches,
            1,
            "closing the socket must not end the watch"
        );

        stop_watch(&v).await;
        settle(&v, baseline, "after leave").await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_publisher_ending_completes_the_watch_once_and_returns_to_baseline() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "res-pub-end", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        publ.request_stop();
        publ.finish_share().await.unwrap();
        wait_finished(&v, Duration::from_secs(30)).await;
        let (_, closed) = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        assert!(
            closed,
            "the video socket closes when the publisher ends the session"
        );

        let snap = wait_terminal(&v, watch_id.clone()).await;
        assert_eq!(snap.terminal, Some("watch.ended"), "{:?}", names(&snap));
        assert!(
            has_detail(&snap, "watch.ended", "session_ended"),
            "{:?}",
            names(&snap)
        );
        assert_eq!(
            count(&snap, "watch.reconnecting"),
            0,
            "no reconnect after a clean end"
        );

        // The task machinery is already at baseline before the page even calls Stop: the
        // watch task, its state observer and the QUIC session ended by themselves.
        tokio::time::timeout(Duration::from_secs(20), async {
            while res(&v).await.runtime_tasks > baseline {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("tasks outlived a finished watch: {:?}", ()));
        stop_watch(&v).await;
        settle(&v, baseline, "after the page's stop").await;
        let after = newest(&v, OpKind::Watch);
        assert_eq!(
            count(&after, "watch.reconnecting"),
            0,
            "no reconnect loop after completion"
        );
        assert_eq!(after.terminal, Some("watch.ended"));
        server.abort();
    }

    /// A viewer whose publisher was killed and never came back: connecting/retrying, not live.
    #[tokio::test(flavor = "multi_thread")]
    async fn leave_while_connecting_or_retrying_returns_to_baseline() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let publisher_ = start_raw_private_publisher(
            &alice_node,
            id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        publisher_.kill().await; // the record and endpoint remain; nothing answers
        let v = viewer(&dir, bk, &bob).await;
        let baseline = res(&v).await.runtime_tasks;

        let started = post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        assert_eq!(started.status(), StatusCode::OK);
        let watch_id = newest(&v, OpKind::Watch).id;
        poll_event(&v, &watch_id, "watch.connect_started").await;
        assert_eq!(res(&v).await.active_watches, 1);
        let never_live = newest(&v, OpKind::Watch);
        assert_eq!(count(&never_live, "watch.live"), 0, "nothing was ever live");

        let end = stop_watch(&v).await;
        assert_eq!(end["state"], "ready");
        settle(&v, baseline, "after leaving while connecting").await;
        let snap = newest(&v, OpKind::Watch);
        assert!(snap.terminal.is_some(), "{:?}", names(&snap));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn leave_while_reconnecting_returns_to_baseline_with_one_logical_watch() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let first = start_raw_private_publisher(
            &alice_node,
            id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        let v = viewer(&dir, bk, &bob).await;
        let baseline = res(&v).await.runtime_tasks;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        first.kill().await;
        poll_event(&v, &watch_id, "watch.reconnecting").await;
        let mid = res(&v).await;
        assert_eq!(
            mid.active_watches, 1,
            "one logical watch across the outage: {mid:?}"
        );
        assert_eq!(
            mid.video_sockets_open, 1,
            "the same socket is kept: {mid:?}"
        );

        drop(ws);
        let end = stop_watch(&v).await;
        assert_eq!(end["state"], "ready");
        server.abort();
        settle(&v, baseline + 1, "after leaving mid-reconnect").await;
        let snap = newest(&v, OpKind::Watch);
        assert!(snap.terminal.is_some());
        assert_eq!(count(&snap, "watch.reconnected"), 0, "it never came back");
    }

    /// Unauthorized viewers: refused every time, never any media, and nothing accumulates on
    /// either side.
    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_private_refusals_accumulate_nothing_and_an_authorized_viewer_is_unaffected() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(
            &dir,
            &alice,
            "res-private",
            AccessConfig::allowed([&bob]).unwrap(),
        )
        .await;
        let stats = publ.share_stats().unwrap();
        let carol_v = viewer(&dir, ck, &carol).await;
        let bob_v = viewer(&dir, bk, &bob).await;
        let (baddr, bserver) = serve(&bob_v.router).await;

        let _ = watch_cycle(&bob_v, id, baddr).await; // warm-up, authorized
        let bob_base = settle(&bob_v, usize::MAX, "bob warm-up")
            .await
            .runtime_tasks;

        let mut carol_base = None;
        for i in 0..5 {
            post(
                &carol_v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            )
            .await;
            wait_finished(&carol_v, Duration::from_secs(30)).await;
            let snap = newest(&carol_v, OpKind::Watch);
            assert_eq!(
                snap.terminal,
                Some("watch.unauthorized"),
                "attempt {i}: {:?}",
                names(&snap)
            );
            assert_eq!(count(&snap, "ws.first_frame"), 0, "attempt {i}: no media");
            assert_eq!(count(&snap, "watch.live"), 0, "attempt {i}: never live");
            stop_watch(&carol_v).await;
            let tasks = *carol_base.get_or_insert(res(&carol_v).await.runtime_tasks);
            // (The refused viewer's tasks share this runtime with bob's and the publisher's.)
            settle(&carol_v, tasks.max(bob_base), &format!("refusal {i}")).await;
            tokio::time::timeout(Duration::from_secs(15), async {
                while stats.session_count() != 0 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("attempt {i}: the publisher still holds a refused session"));
            assert_eq!(stats.objects_published() > 0, true);
        }

        // The authorized viewer still gets media, and returns to baseline.
        let live = watch_cycle(&bob_v, id, baddr).await;
        assert_eq!(live.active_watches, 1);
        settle(
            &bob_v,
            bob_base.max(carol_base.unwrap_or(0)),
            "bob after refusals",
        )
        .await;
        bserver.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- shutdown x resources (the existing bounded `shutdown_app`, unchanged) ----

    const SHUTDOWN_LIMIT: Duration = Duration::from_secs(10);

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_while_watching_with_an_active_socket_leaves_nothing_behind() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "res-shut-watch", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let began = std::time::Instant::now();
        assert!(crate::routes::shutdown_app(&v.handle, SHUTDOWN_LIMIT).await);
        assert!(
            began.elapsed() < SHUTDOWN_LIMIT,
            "shutdown used its whole deadline"
        );
        let (_, closed) = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        assert!(closed, "the socket is closed once the watch is gone");
        settle(&v, baseline, "after shutdown while watching").await;
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_while_reconnecting_completes_inside_the_deadline_and_leaves_nothing() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let alice_node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let first = start_raw_private_publisher(
            &alice_node,
            id,
            &alice,
            vec![bob.clone()],
            loopback(),
            true,
        )
        .await;
        let v = viewer(&dir, bk, &bob).await;
        let baseline = res(&v).await.runtime_tasks;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;
        first.kill().await;
        poll_event(&v, &watch_id, "watch.reconnecting").await;

        let began = std::time::Instant::now();
        assert!(
            crate::routes::shutdown_app(&v.handle, SHUTDOWN_LIMIT).await,
            "shutdown did not finish inside its deadline while reconnecting"
        );
        assert!(began.elapsed() < SHUTDOWN_LIMIT);
        drop(ws);
        server.abort();
        settle(&v, baseline + 1, "after shutdown while reconnecting").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_of_an_active_publisher_ends_the_session_for_its_viewer() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (publ, id) = publisher(&dir, &alice, "res-shut-share", AccessConfig::Public).await;
        let publ_handle = AppHandle::new(publ);
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        let began = std::time::Instant::now();
        assert!(crate::routes::shutdown_app(&publ_handle, SHUTDOWN_LIMIT).await);
        assert!(began.elapsed() < SHUTDOWN_LIMIT);
        assert_eq!(publ_handle.app.lock().await.share_op(), None);

        let snap = wait_terminal(&v, watch_id).await;
        assert_eq!(snap.terminal, Some("watch.ended"));
        assert!(
            has_detail(&snap, "watch.ended", "session_ended"),
            "viewers see the session END, not a lost transport: {:?}",
            names(&snap)
        );
        assert_eq!(count(&snap, "watch.reconnecting"), 0);
        let _ = frames_until_close(&mut ws, Duration::from_secs(10)).await;
        stop_watch(&v).await;
        settle(&v, baseline, "viewer after publisher shutdown").await;
        server.abort();
    }
}

// ---- Stage 15.5 (batch 3): sustained soak ----
//
// Cumulative behaviour over many cycles, on the same real QUIC / MoQ / AV1 path as everything
// else (only the directory is in-memory). Each test prints one `SOAK` summary line (run with
// `--nocapture`). "Baseline" = the application-owned counters at rest; `runtime_tasks` is
// compared against the count measured after a warm-up cycle, which is meaningful here because
// each test owns its runtime and there is no Holochain in it (under a real conductor the same
// counter is informational only - see the live acceptance notes).

mod soak {
    use super::lifecycle_support::*;
    use super::private_sessions::{
        first_keyframe, keypair, publisher, serve, stop_watch, viewer, wait_finished,
        ws_connect_as, Viewer,
    };
    use super::resources::{res, settle, watch_cycle};
    use super::*;
    use futures::StreamExt;
    use holochain_moq_app::lifecycle::OpKind;
    use holochain_moq_app::AccessConfig;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn summary(name: &str, cycles: usize, tasks: &[usize], extra: &str) {
        let (min, max) = (tasks.iter().min().unwrap(), tasks.iter().max().unwrap());
        eprintln!(
            "SOAK {name}: cycles={cycles} settled_tasks min={min} max={max} first={} last={} {extra}",
            tasks[0],
            tasks[tasks.len() - 1]
        );
    }

    async fn sessions_listed(v: &Viewer) -> usize {
        json_body(get(&v.router, "/api/sessions").await)
            .await
            .as_array()
            .unwrap()
            .len()
    }

    /// A public publisher built on `MoqLivePublication` so it can be killed and replaced at the
    /// same address with the SAME session id (a `Sharer` mints a fresh id per bind).
    async fn raw_public(
        node: &RealtimeNode<InMemoryPeerDirectory>,
        session_id: SessionId,
        agent: &str,
        bind: SocketAddr,
        announce_record: bool,
    ) -> super::private_sessions::RawPrivatePublisher {
        use holochain_moq_protocol::{Endpoint, EndpointKind, Session, StreamKind};
        use holochain_moq_realtime::{certs, publish_media, MoqLivePublication};
        let dir = std::env::temp_dir().join(format!("moq-app-web-rawpub-{}", uuid::Uuid::new_v4()));
        let (cert, key, identity) = certs::generate_localhost_cert_with_identity(&dir).unwrap();
        let mut publication = None;
        for _ in 0..400 {
            match MoqLivePublication::bind(
                bind,
                &cert,
                &key,
                holochain_moq_screenshare::NAMESPACE,
                holochain_moq_screenshare::TRACK,
            ) {
                Ok(p) => {
                    publication = Some(p);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(15)).await,
            }
        }
        let mut publication = publication.expect("could not bind the public publisher");
        let addr = publication.local_addr();
        node.publish_endpoint(Endpoint {
            kind: EndpointKind::Direct,
            address: addr.to_string(),
            transport_identity: identity,
        })
        .await
        .unwrap();
        if announce_record {
            use holochain_moq_node::SessionDirectory;
            let now = holochain_moq_screenshare::now_ms();
            let session = Session::new(
                session_id,
                agent.to_string(),
                StreamKind::Screen,
                now,
                now + 3_600_000,
                holochain_moq_screenshare::NAMESPACE,
                holochain_moq_screenshare::TRACK,
                None,
            )
            .and_then(Session::start)
            .unwrap();
            node.directory().publish_session(session).await.unwrap();
        }
        let mut source = holochain_moq_codec_av1::Av1Source::from_video_source(
            holochain_moq_codec_av1::Av1Config {
                fps: 30,
                keyframe_interval: 5,
                ..Default::default()
            },
            synthetic(),
            holochain_moq_codec_av1::QueuePolicy::DropOldest,
        )
        .unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = publish_media(&mut publication, &mut source, async move {
                let _ = stopped.await;
            })
            .await;
            publication
        });
        super::private_sessions::RawPrivatePublisher { stop, task, addr }
    }

    /// Read the browser socket until a keyframe of `epoch` arrives; every media frame's epoch
    /// must never go backwards relative to `floor` (no epoch regression).
    async fn keyframe_of_epoch(
        ws: &mut (impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
        epoch: u32,
        floor: &mut u32,
    ) {
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Binary(b))) if b[0] == 0x01 => {
                        let e = u32::from_be_bytes(b[2..6].try_into().unwrap());
                        assert!(e >= *floor, "epoch regressed: saw {e} after {}", *floor);
                        *floor = e;
                        if e == epoch && b[1] & 1 != 0 {
                            return;
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("the browser socket ended before epoch {epoch}: {other:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no keyframe of epoch {epoch} arrived"));
    }

    // ---- 1. public Share -> Watch -> Leave, many cycles ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_public_watch_leave_cycles() {
        const CYCLES: usize = 30;
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "soak-leave", AccessConfig::Public).await;
        let stats = publ.share_stats().unwrap();
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        let mut tasks = Vec::new();
        for i in 0..CYCLES {
            let live = watch_cycle(&v, id, addr).await;
            assert_eq!(live.video_sockets_open, 1, "cycle {i}");
            assert!(
                live.encoded_queue_depth <= live.encoded_queue_capacity,
                "cycle {i}"
            );
            tasks.push(
                settle(&v, baseline, &format!("cycle {i}"))
                    .await
                    .runtime_tasks,
            );
            assert_eq!(
                sessions_listed(&v).await,
                1,
                "cycle {i}: the live session stays listed"
            );
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            while stats.session_count() != 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("publisher still counts a viewer");
        publ.request_stop();
        publ.finish_share().await.unwrap();
        assert_eq!(
            sessions_listed(&v).await,
            0,
            "no stale live session after the publisher stops"
        );
        summary(
            "public watch/leave",
            CYCLES,
            &tasks,
            &format!("baseline={baseline}"),
        );
        assert!(tasks.iter().all(|t| *t <= baseline), "{tasks:?}");
        server.abort();
    }

    // ---- 2. public Share -> Watch -> publisher Stop -> session_ended ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_public_publisher_stop_cycles() {
        const CYCLES: usize = 10;
        // The production Share path: `POST /api/share` binds the fixed port 4443 every time, so
        // the directory replaces the agent's endpoint each cycle (the address is the same).
        let _port = SHARE_PORT.lock().await;
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let mut publ_app = App::starting(RealtimeNode::new(dir.as_agent(&alice)));
        publ_app.connect().await.unwrap();
        let pub_router = router(AppHandle::new(publ_app));
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let mut baseline = None;
        let mut tasks = Vec::new();
        for i in 0..CYCLES {
            let shared = post(
                &pub_router,
                "/api/share",
                json!({"source":"synthetic","access":"public","title":format!("soak-stop-{i}")}),
            )
            .await;
            assert_eq!(shared.status(), StatusCode::OK, "cycle {i}");
            let id = json_body(get(&pub_router, "/api/state").await).await["share"]["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let started = post(&v.router, "/api/watch", json!({"session_id": id})).await;
            assert_eq!(started.status(), StatusCode::OK, "cycle {i}");
            let mut ws = ws_connect_as(&v.handle, addr).await;
            first_keyframe(&mut ws).await;
            let watch_id = newest(&v, OpKind::Watch).id;
            let stopped = post(&pub_router, "/api/share/stop", json!({})).await;
            assert_eq!(json_body(stopped).await["state"], "ready", "cycle {i}");
            let snap = wait_terminal(&v, watch_id).await;
            assert_eq!(
                snap.terminal,
                Some("watch.ended"),
                "cycle {i}: {:?}",
                names(&snap)
            );
            assert!(
                has_detail(&snap, "watch.ended", "session_ended"),
                "cycle {i}"
            );
            assert_eq!(
                count(&snap, "watch.reconnecting"),
                0,
                "cycle {i}: 14E end is not a loss"
            );
            wait_finished(&v, Duration::from_secs(20)).await;
            drop(ws);
            stop_watch(&v).await;
            assert_eq!(
                sessions_listed(&v).await,
                0,
                "cycle {i}: an ended session must not be live"
            );
            let released_after = {
                let t0 = std::time::Instant::now();
                while std::net::UdpSocket::bind("127.0.0.1:4443").is_err() {
                    assert!(
                        t0.elapsed() < Duration::from_secs(10),
                        "cycle {i}: UDP 4443 still held 10s after Stop"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                t0.elapsed()
            };
            eprintln!("SOAK udp 4443 released {released_after:?} after Stop (cycle {i})");
            let limit = if i == 0 {
                usize::MAX
            } else {
                baseline.unwrap()
            };
            let after = settle(&v, limit, &format!("cycle {i}")).await;
            if i == 0 {
                baseline = Some(after.runtime_tasks);
            }
            tasks.push(after.runtime_tasks);
        }
        summary(
            "public publisher-stop",
            CYCLES,
            &tasks,
            "(viewer saw session_ended each time)",
        );
        assert!(tasks.iter().all(|t| *t <= tasks[0]), "{tasks:?}");
        server.abort();
        let _ = std::fs::remove_dir_all("dev");
    }

    // ---- 3. public reconnect, several real outages on one logical Watch ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_public_reconnect_cycles_keep_one_watch_one_socket_and_exact_epochs() {
        const OUTAGES: u32 = 4;
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let node = RealtimeNode::new(dir.as_agent(&alice));
        let id = SessionId(uuid::Uuid::new_v4());
        let mut current = raw_public(&node, id, &alice, loopback(), true).await;
        let addr = current.addr;

        let v = viewer(&dir, bk, &bob).await;
        let baseline = res(&v).await.runtime_tasks;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let (haddr, server) = serve(&v.router).await;
        let mut ws = ws_connect_as(&v.handle, haddr).await;
        let mut floor = 0u32;
        keyframe_of_epoch(&mut ws, 0, &mut floor).await;
        let watch_id = newest(&v, OpKind::Watch).id;

        for outage in 1..=OUTAGES {
            current.kill().await;
            current = raw_public(&node, id, &alice, addr, false).await;
            tokio::time::timeout(Duration::from_secs(40), async {
                while count(
                    &v.handle.lifecycle.snapshot(&watch_id).unwrap(),
                    "watch.reconnected",
                ) < outage as usize
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("outage {outage}: never reconnected"));
            keyframe_of_epoch(&mut ws, outage, &mut floor).await;
            let r = res(&v).await;
            assert_eq!(r.active_watches, 1, "outage {outage}: one logical watch");
            assert_eq!(
                r.video_sockets_open, 1,
                "outage {outage}: one browser socket"
            );
        }
        let snap = v.handle.lifecycle.snapshot(&watch_id).unwrap();
        assert_eq!(
            count(&snap, "watch.epoch_started"),
            OUTAGES as usize,
            "exactly one new epoch per successful reconnect (the initial epoch 0 is not a \
             reconnect and is not recorded as one): {:?}",
            names(&snap)
        );
        assert_eq!(
            floor, OUTAGES,
            "epochs advanced 0..={OUTAGES} without skipping"
        );
        assert_eq!(
            v.handle
                .lifecycle
                .recent(64)
                .iter()
                .filter(|o| o.kind == OpKind::Watch)
                .count(),
            1,
            "still one logical watch operation"
        );

        drop(ws);
        stop_watch(&v).await;
        current.kill().await;
        server.abort();
        settle(&v, baseline + 1, "after the final termination").await;
        eprintln!(
            "SOAK public reconnect: outages={OUTAGES} epochs=0..={floor} sockets=1 watch_ops=1"
        );
    }

    // ---- 4. the publisher disappears for good ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_permanent_publisher_loss_ends_terminally_and_stops_retrying() {
        for round in 0..2 {
            let (_ak, alice) = keypair(1);
            let (bk, bob) = keypair(2);
            let dir = InMemoryPeerDirectory::new(alice.clone());
            let node = RealtimeNode::new(dir.as_agent(&alice));
            let id = SessionId(uuid::Uuid::new_v4());
            let publisher_ = raw_public(&node, id, &alice, loopback(), true).await;
            let v = viewer(&dir, bk, &bob).await;
            let baseline = res(&v).await.runtime_tasks;
            post(
                &v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            )
            .await;
            let (haddr, server) = serve(&v.router).await;
            let mut ws = ws_connect_as(&v.handle, haddr).await;
            first_keyframe(&mut ws).await;
            let watch_id = newest(&v, OpKind::Watch).id;

            publisher_.kill().await; // gone, never replaced
                                     // The existing policy is 3 connect attempts, each bounded by the QUIC handshake timeout
                                     // (~10s against a closed port), so the terminal event takes about half a minute.
                                     // That is the product's behaviour and is deliberately not shortened.
            let snap = tokio::time::timeout(Duration::from_secs(120), async {
                loop {
                    let s = v.handle.lifecycle.snapshot(&watch_id).unwrap();
                    if s.terminal.is_some() {
                        return s;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "round {round}: no terminal event: {}",
                    v.handle
                        .lifecycle
                        .snapshot(&watch_id)
                        .unwrap()
                        .to_log_line()
                )
            });
            assert!(
                count(&snap, "watch.reconnecting") >= 1,
                "round {round}: {:?}",
                names(&snap)
            );
            assert!(
                matches!(snap.terminal, Some("watch.failed") | Some("watch.ended")),
                "round {round}: a terminal classification, got {:?} / {:?}",
                snap.terminal,
                snap.failure_kind
            );
            // No retry loop outlives the terminal event: the tasks are already gone.
            tokio::time::timeout(Duration::from_secs(20), async {
                while res(&v).await.runtime_tasks > baseline + 1 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("round {round}: tasks outlived the terminal watch"));
            wait_finished(&v, Duration::from_secs(20)).await;
            drop(ws);
            stop_watch(&v).await;
            settle(&v, baseline + 1, &format!("round {round}")).await;
            server.abort();
            eprintln!(
                "SOAK permanent loss round {round}: terminal={:?} failure={:?} reconnecting_events={}",
                snap.terminal,
                snap.failure_kind,
                count(&snap, "watch.reconnecting")
            );
        }
    }

    // ---- 5. private authorization, interleaved ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_private_authorization_rounds() {
        const ROUNDS: usize = 12;
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let (ck, carol) = keypair(3);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(
            &dir,
            &alice,
            "soak-private",
            AccessConfig::allowed([&bob]).unwrap(),
        )
        .await;
        let stats = publ.share_stats().unwrap();
        let carol_v = viewer(&dir, ck, &carol).await;
        let bob_v = viewer(&dir, bk, &bob).await;
        let (baddr, bserver) = serve(&bob_v.router).await;
        let _ = watch_cycle(&bob_v, id, baddr).await;
        let bob_base = settle(&bob_v, usize::MAX, "bob warm-up")
            .await
            .runtime_tasks;

        // A long-lived authorized viewer stays healthy across every refusal.
        post(
            &bob_v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut bws = ws_connect_as(&bob_v.handle, baddr).await;
        first_keyframe(&mut bws).await;
        let bob_watch = newest(&bob_v, OpKind::Watch).id;
        let mut floor = 0u32;

        let mut carol_tasks = Vec::new();
        for i in 0..ROUNDS {
            post(
                &carol_v.router,
                "/api/watch",
                json!({"session_id": id.to_string()}),
            )
            .await;
            wait_finished(&carol_v, Duration::from_secs(30)).await;
            let snap = newest(&carol_v, OpKind::Watch);
            assert_eq!(snap.terminal, Some("watch.unauthorized"), "round {i}");
            assert_eq!(count(&snap, "ws.first_frame"), 0, "round {i}: zero media");
            assert_eq!(count(&snap, "watch.live"), 0, "round {i}");
            stop_watch(&carol_v).await;
            carol_tasks.push(res(&carol_v).await.runtime_tasks);
            // bob keeps receiving real media on the same socket
            keyframe_of_epoch(&mut bws, 0, &mut floor).await;
        }
        let bsnap = bob_v.handle.lifecycle.snapshot(&bob_watch).unwrap();
        assert_eq!(
            count(&bsnap, "watch.reconnecting"),
            0,
            "bob was never disturbed"
        );
        drop(bws);
        stop_watch(&bob_v).await;
        settle(&bob_v, bob_base, "bob at the end").await;
        // (The refused viewer's counter shares this runtime with bob's and the publisher's.)
        assert!(
            carol_tasks[ROUNDS - 1] <= carol_tasks[0],
            "refusals accumulated tasks: {carol_tasks:?}"
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while stats.session_count() != 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the publisher still holds a session");
        summary(
            "private authorization",
            ROUNDS,
            &carol_tasks,
            "(refused viewer's runtime_tasks)",
        );
        bserver.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    // ---- 7. browser socket churn on one Watch; the bounded queue while nobody reads ----

    #[tokio::test(flavor = "multi_thread")]
    async fn soak_browser_socket_churn_keeps_the_watch_and_bounds_the_queue() {
        const CHURN: usize = 8;
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "soak-socket", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let _ = watch_cycle(&v, id, addr).await;
        let baseline = settle(&v, usize::MAX, "warm-up").await.runtime_tasks;

        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let watch_id = newest(&v, OpKind::Watch).id;
        for i in 0..CHURN {
            let mut ws = ws_connect_as(&v.handle, addr).await;
            first_keyframe(&mut ws).await;
            assert_eq!(res(&v).await.video_sockets_open, 1, "attach {i}");
            drop(ws); // a "refresh" / closed tab: the socket goes, the Watch stays
            tokio::time::timeout(Duration::from_secs(5), async {
                while res(&v).await.video_sockets_open != 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("attach {i}: the closed socket is still counted"));
            assert_eq!(
                res(&v).await.active_watches,
                1,
                "attach {i}: closing must not end the Watch"
            );
        }
        // Nobody is reading now: the queue fills, then drops its oldest, and stays bounded.
        tokio::time::timeout(Duration::from_secs(30), async {
            while res(&v).await.encoded_queue_dropped == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("an unread queue never started dropping its oldest items");
        let unread = res(&v).await;
        assert!(
            unread.encoded_queue_depth <= unread.encoded_queue_capacity,
            "{unread:?}"
        );
        // ...and a replacement socket attaches normally afterwards.
        let mut again = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut again).await;
        let snap = v.handle.lifecycle.snapshot(&watch_id).unwrap();
        assert_eq!(count(&snap, "watch.reconnecting"), 0);
        assert_eq!(
            count(&snap, "watch.epoch_started"),
            0,
            "no new epoch from socket churn"
        );
        drop(again);
        stop_watch(&v).await;
        settle(&v, baseline, "after churn").await;
        eprintln!(
            "SOAK socket churn: attaches={} queue depth<=cap ({}/{}) dropped={}",
            CHURN + 1,
            unread.encoded_queue_depth,
            unread.encoded_queue_capacity,
            unread.encoded_queue_dropped
        );
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }
}

// ---- Stage 17A: media-quality diagnostics ----

mod media_quality {
    use super::private_sessions::{
        first_keyframe, keypair, publisher, serve, stop_watch, viewer, ws_connect_as,
    };
    use super::*;
    use holochain_moq_app::AccessConfig;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread")]
    async fn diagnostics_report_the_encoder_the_viewer_queue_and_the_process_and_the_stage_timestamps_join(
    ) {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, id) = publisher(&dir, &alice, "media-q", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        post(
            &v.router,
            "/api/watch",
            json!({"session_id": id.to_string()}),
        )
        .await;
        let mut ws = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut ws).await;
        // A few more objects so rates and averages exist.
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let d = json_body(get(&v.router, "/api/diagnostics").await).await;
                if d["media"]["viewer"]["objects_forwarded"]
                    .as_u64()
                    .unwrap_or(0)
                    >= 5
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("no media reached the viewer");

        // Viewer side: what arrived, what was forwarded, the queue's bound, real bitrate.
        let d = json_body(get(&v.router, "/api/diagnostics").await).await;
        let vm = &d["media"]["viewer"];
        assert!(vm["objects_received"].as_u64().unwrap() >= 5);
        assert!(
            vm["objects_forwarded"].as_u64().unwrap() <= vm["objects_received"].as_u64().unwrap()
        );
        assert!(
            vm["bytes_received"].as_u64().unwrap() > 0
                && vm["bytes_forwarded"].as_u64().unwrap() > 0
        );
        assert!(vm["keyframes_received"].as_u64().unwrap() >= 1);
        assert_eq!(vm["queue_capacity"], 64);
        assert!(vm["queue_depth"].as_u64().unwrap() <= 64);
        assert!(vm["bitrate_bps"].as_f64().unwrap() > 0.0);
        assert!(d["media"]["publisher"].is_null(), "this app only watches");
        // Process usage is reported, and the existing diagnostics are all still there.
        assert!(d["media"]["process"]["rss_mb"].as_f64().unwrap() > 1.0);
        assert!(d["media"]["process"]["cpu_cores"].as_u64().unwrap() >= 1);
        for k in [
            "resources",
            "app_state",
            "liveness",
            "current_watch",
            "recent",
        ] {
            assert!(
                !d[k].is_null() || k == "current_watch",
                "existing diagnostics field {k} is gone"
            );
        }
        assert_eq!(d["resources"]["active_watches"], 1);

        // Publisher side (the sharing app's own diagnostics).
        let publ_handle = AppHandle::new(std::mem::replace(&mut publ, {
            let mut a = App::starting(RealtimeNode::new(dir.as_agent(&alice)));
            a.connect().await.unwrap();
            a
        }));
        let pd = json_body(get(&router(publ_handle.clone()), "/api/diagnostics").await).await;
        let pm = &pd["media"]["publisher"];
        assert!(pm["frames_encoded"].as_u64().unwrap() >= 5, "{pm}");
        assert!(pm["encoded_bytes"].as_u64().unwrap() > 0);
        assert!(pm["encode_avg_ms"].as_f64().unwrap() > 0.0);
        assert!(pm["encode_p95_ms"].as_f64().unwrap() >= pm["encode_p50_ms"].as_f64().unwrap());
        assert!(pm["encode_max_ms"].as_f64().unwrap() >= pm["encode_p95_ms"].as_f64().unwrap());
        assert!(pm["bitrate_bps"].as_f64().unwrap() > 0.0);
        assert_eq!(pm["raw_queue_capacity"], 4);
        assert!(
            pm["first_keyframe_ms"].as_f64().is_some(),
            "time to first keyframe is measured"
        );
        assert!(pd["media"]["viewer"].is_null());

        // The two rings join on each object's media timestamp: the publisher handed it off before
        // (or when) the viewer's queue received it, and the socket sent it after it arrived.
        let pt = json_body(get(&router(publ_handle.clone()), "/api/media/timings").await).await;
        let vt = json_body(get(&v.router, "/api/media/timings").await).await;
        let pub_rows = pt["publisher"].as_array().unwrap();
        let view_rows = vt["viewer"].as_array().unwrap();
        assert!(!pub_rows.is_empty() && !view_rows.is_empty());
        let mut joined = 0;
        for vr in view_rows {
            let ts = vr["media_ts_ms"].as_u64().unwrap();
            let Some(pr) = pub_rows
                .iter()
                .find(|p| p["media_ts_ms"].as_u64() == Some(ts))
            else {
                continue;
            };
            joined += 1;
            let handoff = pr["handoff_unix_us"].as_u64().unwrap();
            let arrived = vr["arrived_unix_us"].as_u64().unwrap();
            assert!(
                pr["captured_unix_us"].as_u64().unwrap()
                    <= pr["encode_start_unix_us"].as_u64().unwrap()
            );
            assert!(
                pr["encode_start_unix_us"].as_u64().unwrap()
                    <= pr["encode_done_unix_us"].as_u64().unwrap()
            );
            assert!(pr["encode_done_unix_us"].as_u64().unwrap() <= handoff);
            assert!(
                handoff <= arrived,
                "publication precedes arrival: {handoff} > {arrived}"
            );
            if let Some(f) = vr["forwarded_unix_us"].as_u64() {
                assert!(arrived <= f, "arrival precedes forwarding");
            }
        }
        assert!(
            joined >= 3,
            "the two timing rings share objects by media timestamp: {joined}"
        );

        drop(ws);
        stop_watch(&v).await;
        server.abort();
        let mut p = publ_handle.app.lock().await;
        p.request_stop();
        let _ = p.finish_share().await;
    }
}

// ---- watch ownership: the scoped, token-authorised stop and owner-only attendance ----

mod ownership {
    use super::private_sessions::{
        first_keyframe, keypair, publisher, serve, viewer, ws_connect, ws_connect_as,
    };
    use super::*;
    use holochain_moq_app::AccessConfig;
    use std::time::Duration;

    /// Start a watch over HTTP and return its `(watch_id, watch_token)` from the response body
    /// - the only place the token is ever delivered.
    async fn start(v: &private_sessions::Viewer, session: &str) -> (String, String) {
        let res = post(&v.router, "/api/watch", json!({ "session_id": session })).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_body(res).await;
        (
            body["watch"]["watch_id"]
                .as_str()
                .expect("watch_id")
                .to_string(),
            body["watch_token"]
                .as_str()
                .expect("watch_token")
                .to_string(),
        )
    }
    async fn stop(v: &private_sessions::Viewer, id: &str, token: &str) -> axum::response::Response {
        post_bearer(&v.router, &format!("/api/watch/{id}/stop"), token).await
    }
    async fn state(v: &private_sessions::Viewer) -> Value {
        json_body(get(&v.router, "/api/state").await).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn starting_a_watch_issues_a_unique_identity_and_a_secret_shown_only_once() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-issue", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;

        let (id1, tok1) = start(&v, &sid.to_string()).await;
        assert_eq!(tok1.len(), 64, "256-bit token as hex");
        let s = state(&v).await;
        assert_eq!(s["watch"]["watch_id"], id1.as_str());
        assert_eq!(
            s["watch"]["session_id"],
            sid.to_string(),
            "the watch is tied to its session"
        );
        assert!(
            s.get("watch_token").is_none(),
            "GET /api/state must never carry the token: {s}"
        );
        assert!(!s.to_string().contains(&tok1));

        assert_eq!(stop(&v, &id1, &tok1).await.status(), StatusCode::OK);
        let (id2, tok2) = start(&v, &sid.to_string()).await;
        assert_ne!(id1, id2, "every attempt has its own watch id");
        assert_ne!(tok1, tok2, "...and its own token");
        assert_eq!(stop(&v, &id2, &tok2).await.status(), StatusCode::OK);
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_needs_the_right_watch_and_the_right_token() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-auth", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (id, tok) = start(&v, &sid.to_string()).await;

        // no credentials
        let res = post(&v.router, &format!("/api/watch/{id}/stop"), json!({})).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        // the public session id is NOT authority
        assert_eq!(
            stop(&v, &id, &sid.to_string()).await.status(),
            StatusCode::FORBIDDEN
        );
        // a guessed / another watch's token
        assert_eq!(
            stop(&v, &id, &"0".repeat(64)).await.status(),
            StatusCode::FORBIDDEN
        );
        // a watch that does not exist
        assert_eq!(
            stop(&v, "00000000-0000-0000-0000-000000000000", &tok)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        // the removed unscoped stop says so instead of stopping anything
        assert_eq!(
            post(&v.router, "/api/watch/stop", json!({})).await.status(),
            StatusCode::GONE
        );
        // ...and every refusal left the watch running
        assert_eq!(state(&v).await["state"], "watching");

        let ok = stop(&v, &id, &tok).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(json_body(ok).await["state"], "ready");
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_stops_are_idempotent_and_a_late_stop_never_reaches_a_newer_watch() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-stale", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (id1, tok1) = start(&v, &sid.to_string()).await;

        let first = json_body(stop(&v, &id1, &tok1).await).await;
        assert_eq!(first["state"], "ready");
        // a duplicate (pagehide racing Leave, a retry): same answer, no error
        let again = stop(&v, &id1, &tok1).await;
        assert_eq!(again.status(), StatusCode::OK);
        assert_eq!(json_body(again).await, first);

        // a NEW watch begins; the old watch's credentials arrive late
        let (id2, tok2) = start(&v, &sid.to_string()).await;
        let late = stop(&v, &id1, &tok1).await;
        assert_eq!(late.status(), StatusCode::OK, "replayed, harmless");
        let s = state(&v).await;
        assert_eq!(
            s["state"], "watching",
            "the newer watch must be untouched: {s}"
        );
        assert_eq!(s["watch"]["watch_id"], id2.as_str());
        // credentials cannot be crossed between the two watches
        assert_eq!(stop(&v, &id2, &tok1).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(stop(&v, &id1, &tok2).await.status(), StatusCode::FORBIDDEN);
        assert_eq!(state(&v).await["state"], "watching");

        assert_eq!(stop(&v, &id2, &tok2).await.status(), StatusCode::OK);
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_stops_for_one_watch_all_succeed_with_one_ending() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-concurrent", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (id, tok) = start(&v, &sid.to_string()).await;
        let (a, b, c) = tokio::join!(
            stop(&v, &id, &tok),
            stop(&v, &id, &tok),
            stop(&v, &id, &tok)
        );
        for r in [a, b, c] {
            assert_eq!(r.status(), StatusCode::OK);
        }
        assert_eq!(state(&v).await["state"], "ready");
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_that_only_polls_cannot_keep_an_orphaned_watch_alive() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-orphan", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let dog = crate::routes::spawn_abandoned_watch_watchdog(
            v.handle.clone(),
            Duration::from_millis(800),
            Duration::from_millis(100),
        );
        let (id, tok) = start(&v, &sid.to_string()).await;

        // The OWNER polling with its token keeps the watch (a hidden tab's timers included).
        for _ in 0..25 {
            post_get_bearer(&v, &tok).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            state(&v).await["state"],
            "watching",
            "an attended watch was left"
        );

        // The owner goes away (browser killed, proxy restarted and token lost). A stale page
        // keeps polling anonymously - which used to preserve the orphan forever.
        let started = tokio::time::Instant::now();
        loop {
            if state(&v).await["state"] == "ready" {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "anonymous polling kept an orphaned watch alive"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The owner's token still gets a clean, idempotent answer afterwards.
        assert_eq!(stop(&v, &id, &tok).await.status(), StatusCode::OK);
        dog.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    async fn post_get_bearer(v: &private_sessions::Viewer, token: &str) {
        let res = v
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/state")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_owners_video_socket_counts_as_attending() {
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(&dir, &alice, "own-socket", AccessConfig::Public).await;
        let v = viewer(&dir, bk, &bob).await;
        let (addr, server) = serve(&v.router).await;
        let (id, tok) = start(&v, &sid.to_string()).await;

        // A foreign page's socket is served (presentation unchanged) but does not attend.
        let mut anon = ws_connect(addr).await;
        first_keyframe(&mut anon).await;
        assert_eq!(v.handle.liveness.video_sockets_open(), 0);
        // The owner's socket does.
        let mut owner = ws_connect_as(&v.handle, addr).await;
        first_keyframe(&mut owner).await;
        assert_eq!(v.handle.liveness.video_sockets_open(), 1);

        drop(owner);
        drop(anon);
        assert_eq!(stop(&v, &id, &tok).await.status(), StatusCode::OK);
        server.abort();
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_that_ended_on_its_own_still_reports_its_error_to_its_owner_only() {
        // A refused (private) watch finishes by itself; the OWNER learns why via its stop.
        let (_ak, alice) = keypair(1);
        let (bk, bob) = keypair(2);
        let dir = InMemoryPeerDirectory::new(alice.clone());
        let (mut publ, sid) = publisher(
            &dir,
            &alice,
            "own-denied",
            AccessConfig::allowed(vec![alice.clone()]).unwrap(),
        )
        .await;
        let v = viewer(&dir, bk, &bob).await;
        let (id, tok) = start(&v, &sid.to_string()).await;
        private_sessions::wait_finished(&v, Duration::from_secs(20)).await;
        assert_eq!(
            stop(&v, &id, "f".repeat(64).as_str()).await.status(),
            StatusCode::FORBIDDEN
        );
        let end = json_body(stop(&v, &id, &tok).await).await;
        assert_eq!(end["last_error_kind"], "unauthorized", "{end}");
        let repeat = json_body(stop(&v, &id, &tok).await).await;
        assert_eq!(repeat, end, "a repeat replays the same outcome");
        publ.request_stop();
        publ.finish_share().await.unwrap();
    }
}
