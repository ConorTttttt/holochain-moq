//! `GET /api/watch/video`: the Stage 14H.5 browser video carrier - a same-origin
//! WebSocket that forwards the still-encoded AV1 objects `App::start_watch_encoded`
//! queues onto the [`crate::wire`] binary format, for `static/app.js`'s `VideoDecoder`.
//!
//! This is presentation only: it neither starts nor stops a watch (`POST /api/watch` /
//! `POST /api/watch/stop`, unchanged, remain the one lifecycle authority - see the
//! crate README, "Browser video" section, for why) and it owns no reconnect, admission
//! or decode logic of its own - it only drains the bounded, already-backpressured
//! [`holochain_moq_app::EncodedVideoQueue`] `App::encoded_video_queue()` already
//! exposes, on a short interval, until the watch finishes or the socket closes.

use crate::wire;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use holochain_moq_node::SessionDirectory;
use holochain_moq_playback::SinkEvent;
use std::time::Duration;

use crate::routes::AppHandle;

/// How often the forwarding loop wakes to drain the queue. Short enough that the
/// browser sees new frames promptly; the queue itself (not this interval) is what
/// bounds memory if the browser falls behind - see `EncodedVideoQueue`'s own docs.
const DRAIN_INTERVAL: Duration = Duration::from_millis(20);

/// Longest a single send to the browser may take (Stage 15.5). A browser that stops reading (a
/// frozen or debugger-paused tab) fills the socket's buffers and makes `send` wait forever;
/// while waiting, the loop could not notice supersession or shutdown, and the socket stayed
/// counted as open - which also told the abandonment watchdog the watch was still attended.
/// A healthy browser drains a frame in milliseconds, so this only ever ends a stalled socket
/// (the watch itself is untouched: the queue is bounded and drops its oldest items meanwhile).
const SEND_STALL_LIMIT: Duration = Duration::from_secs(10);

/// Run one send, bounded by `limit`; `Err` names why the socket must be closed.
async fn send_within<F, E>(limit: Duration, send: F) -> Result<(), &'static str>
where
    F: std::future::Future<Output = Result<(), E>>,
{
    match tokio::time::timeout(limit, send).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err("send_failed"),
        Err(_) => Err("send_stalled"),
    }
}

/// Admission for the browser video socket. The socket carries decoded-free (private) video, so
/// it is authenticated BEFORE the upgrade: the client must offer the active watch's token as
/// `Sec-WebSocket-Protocol: moq-watch.v1, moq-watch-token.<token>` (a browser cannot set
/// `Authorization` on a WebSocket, and a URL token would be logged). Anything else - no token,
/// malformed, wrong, a previous watch's, or no active watch - is refused with 401/403 and
/// never reaches the shared video queue or the socket-supersession logic. Host/Origin checks
/// (`guard`) still run first but are not authentication.
pub async fn get_watch_video<D: SessionDirectory + Send + Sync + 'static>(
    State(handle): State<AppHandle<D>>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let offered = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok());
    let Some(token) =
        crate::ownership::ws_token(offered).filter(|t| crate::ownership::is_well_formed(t))
    else {
        return (StatusCode::UNAUTHORIZED, "watch token required").into_response();
    };
    let Some(watch_id) = handle.registry().admit(token) else {
        return (StatusCode::FORBIDDEN, "not the owner of the active watch").into_response();
    };
    ws.protocols([crate::ownership::WS_PROTOCOL])
        .on_upgrade(move |socket| forward(socket, handle, watch_id))
        .into_response()
}

/// Milliseconds from the operation's `watch.connected` event to now, if it has one: the number
/// that separates "the connection was slow" from "the connection was up but video was not".
fn since_connected<D: SessionDirectory + Send + Sync + 'static>(
    handle: &AppHandle<D>,
    op: &holochain_moq_app::lifecycle::OpId,
) -> String {
    let Some(snap) = handle.lifecycle.snapshot(op) else {
        return "unknown".into();
    };
    match snap.events.iter().find(|e| e.name == "watch.connected") {
        Some(e) => format!("{}ms", snap.elapsed.saturating_sub(e.at).as_millis()),
        None => "not yet connected".into(),
    }
}

async fn forward<D: SessionDirectory + Send + Sync + 'static>(
    mut socket: WebSocket,
    handle: AppHandle<D>,
    watch_id: String,
) {
    let lc = handle.lifecycle.clone();
    let waited = std::time::Instant::now();
    let (queue, op) = {
        let app = handle.app.lock().await;
        (app.encoded_video_queue(), app.watch_op())
    };
    let (Some(queue), Some(op)) = (queue, op) else {
        // No decode-free watch is active (the browser opened this before/without a
        // successful `POST /api/watch`, or a decoded (non-browser) watch is active
        // instead) - nothing to forward; close explicitly rather than leaving the
        // client hanging on an open socket that will never send anything.
        tracing::warn!(target: "lifecycle", "ws.rejected: no active browser (decode-free) watch");
        let _ = send_within(
            SEND_STALL_LIMIT,
            socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4004,
                reason: "no active browser (decode-free) watch".into(),
            }))),
        )
        .await;
        return;
    };
    // The token was checked before the upgrade; the watch may have ended or been replaced since.
    // Re-check while REGISTERING (same registry lock), so a socket admitted for watch A can never
    // become the consumer of watch B's queue, nor register after A ended.
    let registered = {
        let registry = handle.registry();
        registry
            .is_current(&watch_id)
            .then(|| handle.liveness.register_video_socket())
    };
    let Some((generation, _open)) = registered else {
        tracing::warn!(target: "lifecycle", "ws.rejected: the watch ended before the socket was admitted");
        let _ = send_within(
            SEND_STALL_LIMIT,
            socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4001,
                reason: "watch ended".into(),
            }))),
        )
        .await;
        return;
    };
    lc.event(
        &op,
        "ws.requested",
        Some(format!(
            "app lock waited {}ms",
            waited.elapsed().as_millis()
        )),
    );
    lc.event(&op, "ws.accepted", None);

    // Stage 15.3: this socket is now THE video consumer; an older one (a refreshed page's
    // previous socket) sees it was superseded and closes, so the queue never has two readers.
    let previous_open = handle.liveness.video_sockets_open();
    if previous_open > 0 {
        lc.event(
            &op,
            "ws.superseded",
            Some(format!("{previous_open} older socket(s) replaced")),
        );
    }
    lc.event(
        &op,
        "ws.active",
        Some(format!(
            "since watch.connected: {}",
            since_connected(&handle, &op)
        )),
    );

    let mut current_epoch: u32 = 0;
    let mut sent_media = false;
    let mut interval = tokio::time::interval(DRAIN_INTERVAL);
    // Every way out of the loop names its reason, recorded once as `ws.closed`.
    let reason: &'static str = 'serve: loop {
        if !handle.liveness.is_current_video_socket(generation) {
            let _ = send_within(
                SEND_STALL_LIMIT,
                socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 4009,
                    reason: "superseded by a newer video connection".into(),
                }))),
            )
            .await;
            break 'serve "superseded";
        }
        tokio::select! {
            // Never actually sends anything meaningful (this route is server-to-browser
            // only), but must still be polled: without reading, a client-initiated close
            // (`static/video.js`'s `VideoWatch.stop()`, on every ordinary Stop) is never
            // seen, the closing handshake never completes, and the TCP connection is
            // left in CLOSE_WAIT indefinitely - found by the Stage 14H.5 manual Chrome
            // acceptance run (`ss -tnp` showed exactly this) and fixed here rather than
            // relying on some other timeout to eventually clean it up.
            incoming = socket.recv() => {
                match incoming {
                    None | Some(Err(_)) => break 'serve "client_disconnected",
                    Some(Ok(Message::Close(_))) => break 'serve "client_closed",
                    Some(Ok(_)) => {} // nothing else is expected on this route; ignore it
                }
            }
            _ = interval.tick() => {
                // The socket is bound to the watch it was admitted for: once that watch is no
                // longer the active one (stopped, abandoned, replaced) no further frame leaves,
                // not even ones already queued.
                if !handle.registry().is_current(&watch_id) {
                    let _ = send_within(
                        SEND_STALL_LIMIT,
                        socket.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 4001,
                            reason: "watch ended".into(),
                        }))),
                    )
                    .await;
                    break 'serve "watch_not_current";
                }
                for event in queue.drain() {
                    let mut forwarded: Option<(u64, usize)> = None;
                    let (frame, is_media) = match event {
                        SinkEvent::EpochStarted(epoch) => {
                            current_epoch = epoch;
                            lc.event(&op, "ws.epoch_start_sent", Some(epoch.to_string()));
                            (wire::encode_epoch_started(epoch), false)
                        }
                        SinkEvent::Item(object) => {
                            forwarded = Some((object.timestamp_ms, object.payload.len()));
                            (wire::encode_media(current_epoch, &object), true)
                        }
                    };
                    if let Err(why) =
                        send_within(SEND_STALL_LIMIT, socket.send(Message::Binary(frame))).await
                    {
                        // The browser disconnected (tab closed, navigated away, network
                        // drop). The underlying watch is untouched - it keeps running
                        // until the existing `POST /api/watch/stop` lifecycle stops it (or
                        // the abandonment watchdog does), exactly as an HTTP-polling-only
                        // client leaving would (see the README).
                        break 'serve why;
                    }
                    if let Some((ts, len)) = forwarded {
                        // Measurement only (Stage 17A): the object reached the browser socket.
                        queue.note_forwarded(ts, len);
                    }
                    if is_media && !sent_media {
                        sent_media = true;
                        lc.event(
                            &op,
                            "ws.first_frame",
                            Some(format!("since watch.connected: {}", since_connected(&handle, &op))),
                        );
                    }
                }
                // Checked *after* draining, so anything queued before the watch ended is
                // still delivered rather than dropped on the finish race. The watch is over
                // for this socket when it finished on its own (the publisher ended it), OR
                // this queue is no longer the app's active one - the viewer left
                // (`finish_watch` clears it, after which `watch_finished()` is false
                // forever) or a different watch replaced it. Without the second condition a
                // socket left open after a Leave was never closed server-side (Stage 15.3).
                let (finished, mine) = {
                    let app = handle.app.lock().await;
                    let mine = app
                        .encoded_video_queue()
                        .is_some_and(|active| std::sync::Arc::ptr_eq(&active, &queue));
                    (app.watch_finished(), mine)
                };
                if (finished || !mine) && queue.is_empty() {
                    let _ = send_within(SEND_STALL_LIMIT, socket.send(Message::Close(None))).await;
                    break 'serve if finished { "watch_ended" } else { "queue_inactive" };
                }
            }
        }
    };
    lc.event(&op, "ws.closed", Some(reason.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paused time: the stall is decided by the clock auto-advancing, not by waiting.
    #[tokio::test(start_paused = true)]
    async fn a_send_that_never_completes_is_reported_stalled_at_the_limit() {
        let never = std::future::pending::<Result<(), ()>>();
        let started = tokio::time::Instant::now();
        assert_eq!(
            send_within(SEND_STALL_LIMIT, never).await,
            Err("send_stalled")
        );
        assert_eq!(started.elapsed(), SEND_STALL_LIMIT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_or_failed_send_is_not_reported_as_a_stall() {
        assert_eq!(
            send_within(SEND_STALL_LIMIT, async { Ok::<(), ()>(()) }).await,
            Ok(())
        );
        assert_eq!(
            send_within(SEND_STALL_LIMIT, async { Err::<(), _>("closed") }).await,
            Err("send_failed")
        );
    }
}
