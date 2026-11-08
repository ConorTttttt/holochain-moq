//! Watch ownership: who may stop (and who keeps alive) the one active Watch.
//!
//! The control surface is a loopback server, but "loopback" is not "one person": any page in
//! the user's browser, a second tab, or a restarted dev proxy can reach it. Before this module
//! `POST /api/watch/stop` stopped *whatever* watch was active, and *any* page polling
//! `GET /api/state` counted as attending it - so a stale or foreign page could end a newer
//! watch, or keep an orphaned one alive indefinitely.
//!
//! Every successful `POST /api/watch` now mints a [`Grant`]:
//!
//! * a unique `watch_id` (identity; not secret, shown in `GET /api/state`);
//! * a 256-bit random bearer token (authority; returned **once**, in the `POST /api/watch`
//!   response, and held only by the page that started the watch).
//!
//! `session_id` is deliberately *not* authority: it is public (anyone can list sessions).
//! Only the token proves "I started this watch". Only a SHA-256 of it is stored, it has a
//! redacted `Debug`, and nothing here logs it.
//!
//! This is web-layer bookkeeping about the *browser*, like [`crate::routes::Liveness`]: the
//! session lifecycle itself stays entirely in `holochain_moq_app::App`.

use serde::{Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many recently ended watches are remembered, so a late or repeated stop for one of them
/// is answered idempotently instead of being mistaken for a request against a newer watch.
const ENDED_MEMORY: usize = 16;

/// The bearer secret. Serializes as its string (the one place it leaves this process: the
/// response to `POST /api/watch`) but never prints in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct WatchToken(String);

impl WatchToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WatchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchToken([redacted])")
    }
}

impl Serialize for WatchToken {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Constant-time comparison of two token hashes.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_token() -> WatchToken {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random source is unavailable");
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    WatchToken(s)
}

/// What a stop request for `(watch_id, token)` means right now.
#[derive(Debug, PartialEq)]
pub enum StopCheck {
    /// The active watch, correct token: stop it.
    Current,
    /// A watch that already ended, correct token: answer with what its stop returned.
    AlreadyEnded(Value),
    /// Known watch id, wrong token.
    BadToken,
    /// No such watch (never existed here, or too old to remember).
    Unknown,
}

struct Grant {
    watch_id: String,
    session_id: String,
    token_hash: [u8; 32],
    /// Last time the OWNER (valid token) talked to us: state poll, video socket, stop.
    last_owner_contact: Instant,
}

struct Ended {
    watch_id: String,
    token_hash: [u8; 32],
    /// The state view the ending stop (or the watchdog) produced; replayed to repeat stops.
    final_view: Value,
}

#[derive(Default)]
pub struct WatchRegistry {
    current: Option<Grant>,
    ended: VecDeque<Ended>,
    /// Test-only: the credentials of the most recent grant, so tests (which never see the
    /// `POST /api/watch` body they discard) can act as the owner.
    #[cfg(test)]
    last_issued: Option<(String, String)>,
}

impl WatchRegistry {
    /// A watch was just started for `session_id`: mint its identity and token. Any previous
    /// grant cannot still be current (the app is at `Ready` to start one); if it somehow is,
    /// it is retired without a replayable result and can never act on the new watch.
    pub fn issue(&mut self, session_id: &str) -> (String, WatchToken) {
        if self.current.is_some() {
            self.end_current(Value::Null);
        }
        let token = random_token();
        let watch_id = uuid::Uuid::new_v4().to_string();
        self.current = Some(Grant {
            watch_id: watch_id.clone(),
            session_id: session_id.to_string(),
            token_hash: hash(token.as_str()),
            last_owner_contact: Instant::now(),
        });
        #[cfg(test)]
        {
            self.last_issued = Some((watch_id.clone(), token.as_str().to_string()));
        }
        (watch_id, token)
    }

    #[cfg(test)]
    pub fn last_issued(&self) -> Option<(String, String)> {
        self.last_issued.clone()
    }

    pub fn current_watch_id(&self) -> Option<&str> {
        self.current.as_ref().map(|g| g.watch_id.as_str())
    }

    pub fn current_session_id(&self) -> Option<&str> {
        self.current.as_ref().map(|g| g.session_id.as_str())
    }

    /// Does `token` belong to the active watch?
    #[cfg(test)]
    pub fn owns_current(&self, token: &str) -> bool {
        self.current
            .as_ref()
            .is_some_and(|g| same(&g.token_hash, &hash(token)))
    }

    /// The owner showed itself (valid token): refresh the attendance clock. `false` and no
    /// effect for any other caller - a stale or foreign page cannot keep a watch alive.
    pub fn owner_contact(&mut self, token: &str) -> bool {
        match self.current.as_mut() {
            Some(g) if same(&g.token_hash, &hash(token)) => {
                g.last_owner_contact = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Time since the owner last made contact; `None` when no grant is active (a watch that
    /// was not started through `POST /api/watch`, e.g. in-process tests).
    pub fn owner_idle_for(&self) -> Option<Duration> {
        self.current
            .as_ref()
            .map(|g| g.last_owner_contact.elapsed())
    }

    /// Video-socket admission: does `token` belong to the ACTIVE watch? On success returns that
    /// watch's id (the identity the socket is bound to for its whole life) and counts as owner
    /// contact. Fails closed: no active grant, a different watch's token, or any other string
    /// all yield `None`. Comparison is on SHA-256 hashes, constant time.
    pub fn admit(&mut self, token: &str) -> Option<String> {
        let g = self.current.as_mut()?;
        if !same(&g.token_hash, &hash(token)) {
            return None;
        }
        g.last_owner_contact = Instant::now();
        Some(g.watch_id.clone())
    }

    /// Is `watch_id` still the active watch? A socket admitted for it must stop receiving
    /// frames the moment this turns false (stop, watchdog, replacement).
    pub fn is_current(&self, watch_id: &str) -> bool {
        self.current
            .as_ref()
            .is_some_and(|g| g.watch_id == watch_id)
    }

    pub fn check_stop(&self, watch_id: &str, token: &str) -> StopCheck {
        let presented = hash(token);
        if let Some(g) = &self.current {
            if g.watch_id == watch_id {
                return if same(&g.token_hash, &presented) {
                    StopCheck::Current
                } else {
                    StopCheck::BadToken
                };
            }
        }
        match self.ended.iter().find(|e| e.watch_id == watch_id) {
            Some(e) if same(&e.token_hash, &presented) => {
                StopCheck::AlreadyEnded(e.final_view.clone())
            }
            Some(_) => StopCheck::BadToken,
            None => StopCheck::Unknown,
        }
    }

    /// The active watch ended (stop, watchdog, shutdown): remember its result for repeats.
    pub fn end_current(&mut self, final_view: Value) {
        if let Some(g) = self.current.take() {
            self.ended.push_back(Ended {
                watch_id: g.watch_id,
                token_hash: g.token_hash,
                final_view,
            });
            while self.ended.len() > ENDED_MEMORY {
                self.ended.pop_front();
            }
        }
    }
}

/// Extract `Bearer <token>` from an `Authorization` header value.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

/// The WebSocket subprotocol prefix a browser uses to present the watch token (browsers cannot
/// set an `Authorization` header on a WebSocket, and a token in the URL would be logged).
pub const WS_PROTOCOL: &str = "moq-watch.v1";
pub const WS_TOKEN_PREFIX: &str = "moq-watch-token.";

/// A token this service could have issued: exactly 64 lowercase hex digits. Anything else is
/// rejected as malformed without being compared.
pub fn is_well_formed(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The token a client offered in `Sec-WebSocket-Protocol`, if any.
pub fn ws_token(header: Option<&str>) -> Option<&str> {
    header?
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix(WS_TOKEN_PREFIX))
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_long_and_never_debug_printed() {
        let mut r = WatchRegistry::default();
        let (id1, t1) = r.issue("s1");
        r.end_current(Value::Null);
        let (id2, t2) = r.issue("s1");
        assert_ne!(id1, id2);
        assert_ne!(t1, t2);
        assert_eq!(t1.as_str().len(), 64);
        assert!(!format!("{t1:?}").contains(t1.as_str()));
        assert!(format!("{t1:?}").contains("redacted"));
    }

    #[test]
    fn session_id_alone_is_never_authority() {
        let mut r = WatchRegistry::default();
        let (id, _t) = r.issue("public-session-id");
        assert_eq!(r.check_stop(&id, "public-session-id"), StopCheck::BadToken);
        assert!(!r.owns_current("public-session-id"));
    }

    #[test]
    fn only_the_current_watch_with_its_token_can_stop() {
        let mut r = WatchRegistry::default();
        let (id, t) = r.issue("s1");
        assert_eq!(r.check_stop(&id, t.as_str()), StopCheck::Current);
        assert_eq!(r.check_stop(&id, "wrong"), StopCheck::BadToken);
        assert_eq!(r.check_stop("other-id", t.as_str()), StopCheck::Unknown);
    }

    #[test]
    fn a_stale_watch_cannot_touch_a_newer_one_and_repeats_replay_the_result() {
        let mut r = WatchRegistry::default();
        let (id1, t1) = r.issue("s1");
        r.end_current(serde_json::json!({"state": "ready", "n": 1}));
        let (id2, t2) = r.issue("s1");
        // old credentials: replayed result, and the new watch is still current & untouched
        assert_eq!(
            r.check_stop(&id1, t1.as_str()),
            StopCheck::AlreadyEnded(serde_json::json!({"state": "ready", "n": 1}))
        );
        assert_eq!(r.check_stop(&id1, t2.as_str()), StopCheck::BadToken);
        assert_eq!(r.current_watch_id(), Some(id2.as_str()));
        assert!(r.owns_current(t2.as_str()));
        assert!(!r.owns_current(t1.as_str()));
    }

    #[test]
    fn only_the_owner_refreshes_attendance() {
        let mut r = WatchRegistry::default();
        let (_id, t) = r.issue("s1");
        std::thread::sleep(Duration::from_millis(30));
        assert!(!r.owner_contact("not-the-token"));
        assert!(r.owner_idle_for().unwrap() >= Duration::from_millis(30));
        assert!(r.owner_contact(t.as_str()));
        assert!(r.owner_idle_for().unwrap() < Duration::from_millis(30));
    }

    #[test]
    fn ended_memory_is_bounded() {
        let mut r = WatchRegistry::default();
        let (first, t) = r.issue("s");
        r.end_current(Value::Null);
        for _ in 0..ENDED_MEMORY + 2 {
            r.issue("s");
            r.end_current(Value::Null);
        }
        assert_eq!(r.check_stop(&first, t.as_str()), StopCheck::Unknown);
    }

    #[test]
    fn admission_is_bound_to_the_active_watch_only() {
        let mut r = WatchRegistry::default();
        assert_eq!(r.admit(&"a".repeat(64)), None, "no watch: fail closed");
        let (id1, t1) = r.issue("s1");
        assert_eq!(r.admit(t1.as_str()), Some(id1.clone()));
        assert_eq!(r.admit(&"0".repeat(64)), None);
        assert_eq!(r.admit("s1"), None, "session_id is not a credential");
        assert!(r.is_current(&id1));
        r.end_current(Value::Null);
        assert!(!r.is_current(&id1));
        assert_eq!(r.admit(t1.as_str()), None, "ended watch's token is dead");
        let (id2, t2) = r.issue("s1");
        assert_eq!(r.admit(t1.as_str()), None, "previous watch's token");
        assert_eq!(r.admit(t2.as_str()), Some(id2.clone()));
        assert!(!r.is_current(&id1));
    }

    #[test]
    fn token_format_check() {
        assert!(is_well_formed(&"ab12".repeat(16)));
        assert!(!is_well_formed("short"));
        assert!(!is_well_formed(&"AB12".repeat(16)));
        assert!(!is_well_formed(&"zz".repeat(32)));
    }

    #[test]
    fn header_parsing() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(None), None);
        assert_eq!(
            ws_token(Some("moq-watch.v1, moq-watch-token.deadbeef")),
            Some("deadbeef")
        );
        assert_eq!(ws_token(Some("moq-watch.v1")), None);
    }
}
