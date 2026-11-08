//! The entire serialization boundary between `holochain_moq_app::App` and the browser.
//!
//! Per the 14H.0/14H.1 brief: `crates/app` stays independent of any web representation
//! (no `Serialize` derives added there); every type here is instead built *from* `App`'s
//! existing read-only getters, in one place, so no HTTP handler has to know how to read
//! `App`'s state itself. If a handler needs a fact this module cannot already produce
//! from an existing `App` getter, that is a signal the fact does not exist at the
//! application-layer API yet - see this crate's README, "Discovered API gaps", rather
//! than reaching into `App` internals or guessing.
//!
//! Generic over `D: SessionDirectory` for the same reason `App` itself is (see its own
//! docs): the production binary always instantiates `App<HolochainPeerDirectory>`, and
//! this crate's tests instantiate `App<InMemoryPeerDirectory>` against the exact same
//! code, without a conductor - the pattern `crates/app`'s own tests already use.

use holochain_moq_app::{App, AppState, SharingState, WatchingState};
use holochain_moq_node::SessionDirectory;
use holochain_moq_protocol::{Session, SessionState, StreamKind};
use serde::Serialize;

/// The full application snapshot returned by `GET /api/state` and by every control
/// endpoint once it has acted (so the browser always has a fresh view without a second
/// round trip). Exactly one of `share`/`watch` is `Some`, matching `AppState::Sharing`/
/// `Watching` - never both (see [`App::choose_share`]/[`App::choose_watch`]'s own
/// `Ready`-only precondition, unchanged and enforced by `App`, not duplicated here).
#[derive(Debug, Serialize, PartialEq)]
pub struct AppStateView {
    /// `"starting" | "ready" | "sharing" | "watching"` - the outer `AppState` variant.
    pub state: &'static str,
    pub agent: Option<String>,
    pub share: Option<ShareView>,
    pub watch: Option<WatchView>,
    /// Set only by a `/stop` endpoint whose underlying operation did not end cleanly
    /// (`finish_share`/`finish_watch` returned `Err`) - the stop itself still completed
    /// (the app is back at `Ready`), this is purely informational. The exact same
    /// `anyhow`/`WatchError` display text `holochain-moq-app`'s own CLI already prints
    /// (`{:#}`), never a backtrace or panic payload.
    pub last_error: Option<String>,
    /// Stage 15.2: a machine-readable classification of `last_error` where the underlying
    /// error is one this UI distinguishes - `"unauthorized"` (the publisher's 14F admission
    /// check refused this viewer) or `"expired"` (14D) - so the browser can show the right
    /// message without parsing prose. Absent for every other error. It carries no
    /// authorization authority: it only reports what the Rust layers already decided.
    pub last_error_kind: Option<&'static str>,
    /// Stage 15.3: how a watch that ended *without error* ended: `"session_ended"` (the
    /// publisher ended it) or `"left"` (this viewer did). Set only by `POST /api/watch/stop`.
    pub last_notice: Option<&'static str>,
    /// Stage 15.4: where the active operation is (op id, phase, phase age). Compact on purpose;
    /// the history is `GET /api/diagnostics`.
    pub diag: Option<crate::diag::DiagView>,
    /// The bearer secret of the watch this response just started: present ONLY in the
    /// `POST /api/watch` response, never in `GET /api/state`, and redacted in `Debug`. Whoever
    /// holds it owns that watch (see `ownership`); `session_id` is not authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_token: Option<crate::ownership::WatchToken>,
}

/// Classify a stop/finish error for [`AppStateView::last_error_kind`]. Downcasts to the
/// typed `WatchError` the screenshare layer already produces; nothing is inferred from text,
/// and no new error category is invented - each kind is one existing variant:
///
/// | kind | `WatchError` |
/// |---|---|
/// | `unauthorized` | `Unauthorized` (publisher's 14F admission refusal) |
/// | `expired` | `SessionExpired` (14D) |
/// | `ended` | `SessionNotLive` (the session was `Ended`, 14E, before it could be joined) |
/// | `not_found` | `SessionNotFound` |
/// | `publisher_unavailable` | `Discovery` (no usable endpoint or peer for the publisher) |
/// | `connection_failed` | `Session` (the connection could not be (re-)established) |
pub fn error_kind(err: &anyhow::Error) -> Option<&'static str> {
    use holochain_moq_screenshare::WatchError as W;
    Some(match err.downcast_ref::<W>()? {
        W::Unauthorized => "unauthorized",
        W::SessionExpired { .. } => "expired",
        W::SessionNotLive { .. } => "ended",
        W::SessionNotFound(_) => "not_found",
        W::Discovery(_) => "publisher_unavailable",
        W::Session(_) => "connection_failed",
    })
}

/// What a *successful* stop of a watch means, for [`AppStateView::last_notice`]. Ending is
/// not an error: `Completed` is the publisher ending the session (14E) - worth telling the
/// viewer - while `Stopped`/`ShutDown` is the viewer having chosen to leave.
pub fn watch_end_notice(end: holochain_moq_screenshare::PlayEnd) -> &'static str {
    match end {
        holochain_moq_screenshare::PlayEnd::Completed => "session_ended",
        holochain_moq_screenshare::PlayEnd::Stopped
        | holochain_moq_screenshare::PlayEnd::ShutDown => "left",
    }
}

impl AppStateView {
    pub fn from_app<D: SessionDirectory + Send + Sync + 'static>(app: &App<D>) -> Self {
        let (state, agent, share, watch) = match app.state() {
            AppState::Starting => ("starting", None, None, None),
            AppState::Ready { agent } => ("ready", Some(agent.clone()), None, None),
            AppState::Sharing(s) => ("sharing", None, Some(ShareView::from_app(app, *s)), None),
            AppState::Watching(w) => ("watching", None, None, Some(WatchView::from_app(app, *w))),
        };
        Self {
            state,
            agent,
            share,
            watch,
            last_error: None,
            last_error_kind: None,
            last_notice: None,
            diag: None,
            watch_token: None,
        }
    }

    /// Stamp the active watch's identity (non-secret) onto the view.
    pub fn with_watch_id(mut self, watch_id: Option<&str>) -> Self {
        if let (Some(w), Some(id)) = (self.watch.as_mut(), watch_id) {
            w.watch_id = Some(id.to_string());
        }
        self
    }

    pub fn with_watch_token(mut self, token: crate::ownership::WatchToken) -> Self {
        self.watch_token = Some(token);
        self
    }

    pub fn with_diag(mut self, diag: Option<crate::diag::DiagView>) -> Self {
        self.diag = diag;
        self
    }

    pub fn with_notice(mut self, notice: &'static str) -> Self {
        self.last_notice = Some(notice);
        self
    }

    pub fn with_error_kind(mut self, kind: Option<&'static str>) -> Self {
        self.last_error_kind = kind;
        self
    }

    pub fn with_last_error(mut self, message: impl Into<String>) -> Self {
        self.last_error = Some(message.into());
        self
    }
}

fn sharing_state_label(s: SharingState) -> &'static str {
    match s {
        SharingState::ChoosingSource => "choosing_source",
        SharingState::Live => "live",
        SharingState::Ending => "ending",
        SharingState::Ended => "ended",
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ShareView {
    pub state: &'static str,
    pub source: Option<&'static str>,
    pub session_id: Option<String>,
    /// Stage 15.3: from the durable session record, like `access` - what viewers see.
    pub title: Option<String>,
    pub created_at: Option<i64>,
    pub expires_at: Option<i64>,
    /// Stage 15.2: `"public"` or `"allowed"` (private), read from the durable session
    /// record the publisher announced (`Session.allowed`) - the same record viewers see -
    /// rather than remembered separately (which closes the "no access mode on a live share"
    /// gap this crate's README used to record). `None` until a session exists.
    pub access: Option<&'static str>,
    /// The allowlist recorded on the session; `Some` only for a private session.
    pub allowed_agents: Option<Vec<String>>,
    pub objects_published: Option<u64>,
    pub viewers: Option<usize>,
    pub finished: bool,
}

impl ShareView {
    fn from_app<D: SessionDirectory + Send + Sync + 'static>(
        app: &App<D>,
        state: SharingState,
    ) -> Self {
        let stats = app.share_stats();
        Self {
            state: sharing_state_label(state),
            source: app.share_source().map(|s| match s {
                holochain_moq_app::ShareSource::Screen => "screen",
                holochain_moq_app::ShareSource::Synthetic => "synthetic",
            }),
            session_id: app.share_session().map(|s| s.session_id.to_string()),
            title: app.share_session().and_then(|s| s.title.clone()),
            created_at: app.share_session().map(|s| s.created_at),
            expires_at: app.share_session().map(|s| s.expires_at),
            access: app.share_session().map(|s| {
                if s.allowed.is_some() {
                    "allowed"
                } else {
                    "public"
                }
            }),
            allowed_agents: app.share_session().and_then(|s| s.allowed.clone()),
            objects_published: stats.as_ref().map(|s| s.objects_published()),
            viewers: stats.as_ref().map(|s| s.session_count()),
            finished: app.share_finished(),
        }
    }
}

fn watching_state_label(s: WatchingState) -> &'static str {
    match s {
        WatchingState::Discovering => "discovering",
        WatchingState::Selecting => "selecting",
        WatchingState::Connecting => "connecting",
        WatchingState::Live => "live",
        WatchingState::Reconnecting { .. } => "reconnecting",
        WatchingState::Ended => "ended",
        WatchingState::Expired => "expired",
        WatchingState::Unauthorized => "unauthorized",
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct WatchView {
    /// Unique per watch attempt (non-secret identity, shared by `POST /api/watch`, `GET
    /// /api/state` and the scoped stop URL). `None` for a watch not started over HTTP.
    pub watch_id: Option<String>,
    pub state: &'static str,
    pub reconnect_attempt: Option<u32>,
    pub session_id: Option<String>,
    pub connection_state: Option<&'static str>,
    pub pictures: u64,
    pub epoch: u32,
    pub width: usize,
    pub height: usize,
    pub finished: bool,
}

impl WatchView {
    fn from_app<D: SessionDirectory + Send + Sync + 'static>(
        app: &App<D>,
        state: WatchingState,
    ) -> Self {
        let progress = app.watch_progress();
        Self {
            watch_id: None,
            state: watching_state_label(state),
            reconnect_attempt: match state {
                WatchingState::Reconnecting { attempt } => Some(attempt),
                _ => None,
            },
            session_id: app.watch_session_id().map(|id| id.to_string()),
            connection_state: app.watch_connection_state().map(|cs| match cs {
                holochain_moq_node::ConnectionState::Connected => "connected",
                holochain_moq_node::ConnectionState::Reconnecting { .. } => "reconnecting",
                holochain_moq_node::ConnectionState::Completed => "completed",
                holochain_moq_node::ConnectionState::ShutDown => "shut_down",
                holochain_moq_node::ConnectionState::Failed => "failed",
            }),
            pictures: progress.map(|p| p.pictures).unwrap_or(0),
            epoch: progress.map(|p| p.epoch).unwrap_or(0),
            width: progress.map(|p| p.width).unwrap_or(0),
            height: progress.map(|p| p.height).unwrap_or(0),
            finished: app.watch_finished(),
        }
    }
}

/// One discoverable, joinable session - `GET /api/sessions` maps
/// `App::discover_sessions()`'s result through this and nothing else. Every session this
/// type can describe is already joinable by construction (that is what
/// `discover_sessions`/`screenshare::joinable_sessions` already guarantees - Stage 14D's
/// live/unexpired/endpoint-fresh rule, unchanged and not re-checked here).
#[derive(Debug, Serialize, PartialEq)]
pub struct SessionView {
    pub session_id: String,
    pub publisher: String,
    pub title: Option<String>,
    /// `holochain_moq_protocol::StreamKind` - already a wire/durable type with its own
    /// `Serialize`, reused as-is (this is not an `App`-layer state type, so it is not
    /// covered by the "no `Serialize` on `App`'s state enums" rule).
    pub source: StreamKind,
    /// Always `Live` here (`discover_sessions` only ever returns joinable sessions), but
    /// carried through rather than hard-coded, so the UI is never lying about it.
    pub state: SessionState,
    pub created_at: i64,
    pub expires_at: i64,
    /// Stage 15.2: the session is private (`Session.allowed` is set). Display only: a
    /// viewer not on the allowlist can still *discover* it - discovery is not
    /// authorization; the publisher's admission check is.
    pub private: bool,
}

impl From<&Session> for SessionView {
    fn from(s: &Session) -> Self {
        Self {
            session_id: s.session_id.to_string(),
            publisher: s.publisher.clone(),
            title: s.title.clone(),
            source: s.source,
            state: s.state,
            created_at: s.created_at,
            expires_at: s.expires_at,
            private: s.allowed.is_some(),
        }
    }
}

pub fn sessions_view(sessions: &[Session]) -> Vec<SessionView> {
    sessions.iter().map(SessionView::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_moq_node::NodeError;
    use holochain_moq_protocol::{SessionId, SessionState};
    use holochain_moq_screenshare::{PlayEnd, WatchError};

    fn kind(e: WatchError) -> Option<&'static str> {
        error_kind(&anyhow::Error::new(e))
    }

    #[test]
    fn every_typed_watch_error_has_its_own_kind_and_untyped_errors_have_none() {
        let id = SessionId(uuid::Uuid::new_v4());
        assert_eq!(kind(WatchError::Unauthorized), Some("unauthorized"));
        assert_eq!(
            kind(WatchError::SessionExpired {
                session_id: id,
                expires_at: 0
            }),
            Some("expired")
        );
        assert_eq!(
            kind(WatchError::SessionNotLive {
                session_id: id,
                state: SessionState::Ended
            }),
            Some("ended")
        );
        assert_eq!(kind(WatchError::SessionNotFound(id)), Some("not_found"));
        assert_eq!(
            kind(WatchError::Discovery(NodeError::PeerNotFound("x".into()))),
            Some("publisher_unavailable")
        );
        // A plain error with no typed cause is never guessed at from its text.
        assert_eq!(error_kind(&anyhow::anyhow!("not authorized")), None);
    }

    #[test]
    fn a_publisher_ending_the_session_and_a_viewer_leaving_are_told_apart() {
        assert_eq!(watch_end_notice(PlayEnd::Completed), "session_ended");
        assert_eq!(watch_end_notice(PlayEnd::Stopped), "left");
        assert_eq!(watch_end_notice(PlayEnd::ShutDown), "left");
    }
}
