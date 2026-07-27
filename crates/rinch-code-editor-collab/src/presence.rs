//! Ephemeral presence: who else is in this document, and where their carets are.
//!
//! The protocol is **y-protocols awareness**, which yrs implements
//! ([`yrs::sync::Awareness`]) — a map of client id → `{clock, state}` where the
//! state is arbitrary JSON, the clock is the liveness heartbeat, and peers that
//! go quiet are dropped on a timeout. None of that is written here; this module
//! only decides what goes *in* the state and converts it to and from the
//! editor's vocabulary.
//!
//! # Why the state looks like this
//!
//! The JSON shape is the one y-codemirror.next and y-prosemirror publish:
//!
//! ```json
//! { "user":   { "name": "Ada", "color": "#30bced" },
//!   "cursor": { "anchor": {"item": {"client": 1, "clock": 4}, "assoc": 0},
//!               "head":   {"item": {"client": 1, "clock": 9}, "assoc": 0} } }
//! ```
//!
//! Matching it is nearly free — [`StickyIndex`] already serializes to exactly
//! Yjs's `RelativePosition` JSON — and it buys real interoperability: a browser
//! peer running y-codemirror sees our carets and we see theirs, with no
//! translation layer. Deviating would have cost the same effort and bought
//! nothing. See the crate docs for the transport rule this rides on.
//!
//! # Why anchors and not offsets
//!
//! A caret published as "character 400" is wrong the instant anyone edits above
//! it, and *stays* wrong, because presence carries no history to rebase
//! against. [`StickyIndex`] instead names the *operation* the caret sits
//! against, so a concurrent insert 300 characters earlier moves the remote
//! caret with the text it was attached to, for free and exactly. This is the
//! same mechanism Yjs's `RelativePosition` provides and the reason
//! y-codemirror uses it rather than integers.

use serde::{Deserialize, Serialize};
use yrs::StickyIndex;
use yrs::sync::time::{Clock, Timestamp};

pub use rinch_code_editor_buffer::Selection;

/// How long a peer may stay silent before it is presumed gone, in milliseconds.
///
/// y-protocols' own constant. Pair it with [`HEARTBEAT_MS`]: a live peer
/// re-announces well inside this window, so only a peer that actually
/// disappeared — a closed tab, a dropped connection, a crash — ages out.
pub const PEER_TIMEOUT_MS: u64 = 30_000;

/// How often an idle peer should re-announce itself, in milliseconds.
///
/// y-protocols' own constant, and comfortably under [`PEER_TIMEOUT_MS`] so two
/// missed heartbeats are survivable. See
/// [`CollabDoc::presence_heartbeat`](crate::CollabDoc::presence_heartbeat).
pub const HEARTBEAT_MS: u64 = 15_000;

/// How often to sweep for peers that have aged out, in milliseconds.
///
/// y-protocols' own constant. The sweep is cheap and this only bounds how long
/// a departed peer's caret lingers. See
/// [`CollabDoc::expire_stale_peers`](crate::CollabDoc::expire_stale_peers).
pub const SWEEP_MS: u64 = 3_000;

/// Who a peer says they are.
///
/// Deliberately just a name and a colour: presence is a rendering concern, and
/// anything more is the embedder's identity system, which this crate has no
/// business modelling. The colour travels *with* the state (rather than being
/// derived from the client id at each end) so every peer draws the same person
/// the same colour — y-codemirror's convention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Display name, shown in the name chip above the caret.
    pub name: String,
    /// A CSS colour, e.g. `"#30bced"`. Used for the caret, the selection tint
    /// and the chip.
    pub color: String,
}

impl User {
    pub fn new(name: impl Into<String>, color: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: color.into(),
        }
    }
}

/// A selection as op-identity anchors — the wire form of a caret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StickyCursor {
    pub anchor: StickyIndex,
    pub head: StickyIndex,
}

/// The whole awareness state one peer publishes. Field names are the
/// y-codemirror ones; see the module docs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PresenceState {
    pub user: User,
    /// Absent while a peer is present but has no caret in the document — which
    /// is a real state (joined, not yet focused), not a missing field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<StickyCursor>,
}

/// One remote participant, resolved against the document as it stands *now*.
///
/// The selection is in **chars**, like everything else this crate exposes, so
/// it drops straight into the view layer's existing rect maths. Resolving is
/// done on read rather than on receipt because the answer changes with every
/// local edit — an anchor is a stable name for a position, not a position.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    /// The peer's client id — stable for the life of its document, and the key
    /// to use when diffing one `peers()` snapshot against the next.
    pub id: u64,
    pub user: User,
    /// Where their caret is, if they have one in this document. `head` is the
    /// caret; `anchor` is the other end. Unordered on purpose — the direction
    /// is what tells the renderer which end to draw the caret bar at.
    pub selection: Option<Selection>,
}

/// A clock that works on every target this crate supports.
///
/// yrs's own `SystemClock` — and `Awareness::new` with it — are compiled out on
/// `wasm32-unknown-unknown`, exactly like `UndoManager::new`. So awareness is
/// always constructed through `with_clock`, and this is what it is given.
/// `web-time` is `std::time` off wasm and `Date.now()` on it.
pub(crate) struct PortableClock;

impl Clock for PortableClock {
    fn now(&self) -> Timestamp {
        web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map(|d| d.as_millis() as Timestamp)
            // Only reachable if the host clock is set before 1970. Presence
            // degrades to "everyone looks equally stale" rather than panicking
            // in a renderer.
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire format is a promise to the Yjs ecosystem, not an internal
    /// detail — it is the whole reason the state is shaped this way rather
    /// than however was convenient. Pin it: a field rename here silently stops
    /// y-codemirror from seeing our carets, and nothing else would notice.
    ///
    /// The expected shape is Yjs's own: `user` as `{name, color}`, and each
    /// cursor end as a `RelativePosition` — `{item: {client, clock}, assoc}`,
    /// with the fields Yjs leaves null simply absent.
    #[test]
    fn presence_state_serializes_in_the_yjs_shape() {
        use yrs::{Assoc, GetString, IndexedSequence, Text, Transact};

        let doc = yrs::Doc::new();
        let text = doc.get_or_insert_text("content");
        let cursor = {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, "hello world");
            StickyCursor {
                anchor: text.sticky_index(&txn, 0, Assoc::After).unwrap(),
                head: text.sticky_index(&txn, 5, Assoc::After).unwrap(),
            }
        };
        assert_eq!(text.get_string(&doc.transact()), "hello world");

        let state = PresenceState {
            user: User::new("Ada", "#30bced"),
            cursor: Some(cursor),
        };
        let json: serde_json::Value = serde_json::to_value(&state).unwrap();

        assert_eq!(json["user"]["name"], "Ada");
        assert_eq!(json["user"]["color"], "#30bced");

        for end in ["anchor", "head"] {
            let at = &json["cursor"][end];
            assert!(
                at["item"]["client"].is_number(),
                "{end}: a RelativePosition names an operation by client id, got {at}"
            );
            assert!(at["item"]["clock"].is_number(), "{end}: ...and clock");
            assert_eq!(at["assoc"], 0, "{end}: Assoc::After encodes as 0, like Yjs");
        }

        // A peer with no caret omits the field rather than sending null, which
        // is what `cursor == null` means to a Yjs reader anyway.
        let idle = PresenceState {
            user: User::new("Grace", "#6eeb83"),
            cursor: None,
        };
        let json = serde_json::to_string(&idle).unwrap();
        assert!(!json.contains("cursor"), "got {json}");

        // And it round-trips, so we can read what we write.
        let back: PresenceState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user, idle.user);
        assert!(back.cursor.is_none());
    }
}
