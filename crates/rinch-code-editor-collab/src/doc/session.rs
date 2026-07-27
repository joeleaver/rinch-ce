//! Per-peer protocol state, and the outgoing half of the conversation.
//!
//! A [`SyncSession`] is one connection's worth of protocol state; the document
//! holds none of it, which is what lets a single [`CollabDoc`](super::CollabDoc)
//! serve many peers at once — a relay is just a document with several sessions
//! and no editor attached.
//!
//! # The pump rule
//!
//! The protocol is a per-peer conversation. After **any** local change and after
//! **any** received message, call [`CollabDoc::generate_message`](super::CollabDoc::generate_message)
//! for **every** peer, delivering what comes out, until a full round over all
//! peers returns `None` — that is quiescence. Updates are idempotent, so one
//! reaching a peer twice (say, relayed through two different peers) converges
//! harmlessly.
//!
//! # Framing
//!
//! A message is a length-delimited unit, not a stream: the transport must
//! preserve message boundaries. WebSocket binary frames give this for free; a
//! raw TCP stream needs a length prefix. Each peer's messages must arrive
//! reliably and in order — after a transport reconnect, start a fresh
//! [`SyncSession`] (both sides), never resume the old one.
//!
//! **Not Hocuspocus.** It prefixes every frame with a lib0 varString document
//! name *before* the message type, so it is not y-sync on the wire and our
//! decoder would read that length byte as a message type. Talking to it needs an
//! adapter, and the adapter belongs in the embedder's transport rather than
//! here: a document name is addressing, which y-sync deliberately leaves to the
//! layer above.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use yrs::sync::{Message, SyncMessage};
use yrs::updates::encoder::Encode;
use yrs::{Origin, ReadTxn, Transact};

use super::CollabDoc;

/// The sync-protocol state for one remote peer.
///
/// Hold one per connection, next to that connection's transport handle. A
/// session assumes a reliable in-order byte stream underneath; when the
/// transport drops, throw the session away and start a new one on reconnect —
/// the protocol re-negotiates what the peer is missing from scratch.
#[derive(Debug)]
pub struct SyncSession {
    /// Identifies this connection, so an update that arrived on it is never
    /// sent straight back out of it.
    pub(super) id: u64,
    /// Whether our opening state vector has gone out.
    opened: bool,
    /// Replies owed to this peer, oldest first, produced while handling its
    /// last messages. Drained one per
    /// [`CollabDoc::generate_message`](super::CollabDoc::generate_message).
    ///
    /// A queue rather than a single slot because more than one exchange can be
    /// outstanding at once — a sync step 2 and an awareness reply are owed
    /// independently, and a single slot silently dropped whichever was
    /// answered second.
    pub(super) pending: VecDeque<Vec<u8>>,
    /// How far this peer has been caught up through the document's outbox.
    cursor: usize,
}

impl Default for SyncSession {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncSession {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            opened: false,
            pending: VecDeque::new(),
            cursor: 0,
        }
    }
}

impl CollabDoc {
    /// The next message for `peer`, if the conversation is not quiescent.
    ///
    /// See this module's docs for the pump rule: call this for every peer after
    /// any local change or receive, until all peers return `None`.
    pub fn generate_message(&mut self, peer: &mut SyncSession) -> Option<Vec<u8>> {
        // A reply owed from a message we already handled.
        if let Some(bytes) = peer.pending.pop_front() {
            return Some(bytes);
        }

        // The opening move: tell the peer what we have, so it can send the rest.
        if !peer.opened {
            peer.opened = true;
            // Everything already in the outbox is part of the state this
            // handshake will reconcile, so it must not be replayed afterwards.
            peer.cursor = self.trimmed + self.outbox.lock().map_or(0, |o| o.len());
            self.cursors.insert(peer.id, peer.cursor);
            // Ask who is here, straight after the document handshake. Without
            // it a peer that has been idle since before we connected stays
            // invisible until it happens to move — awareness is broadcast as
            // deltas, so there is nothing in the outbox to replay.
            peer.pending.push_back(encode(&Message::AwarenessQuery));
            // …and *tell* them who we know about, without waiting to be asked.
            // Asking alone is enough between two of these documents, because
            // both ends ask. It is not enough against the wider ecosystem: the
            // Yjs client announces itself and then waits, so a peer that only
            // ever asked would stay invisible to it until somebody moved. The
            // reference y-websocket server pushes here for the same reason.
            if let Ok(update) = self.awareness.update() {
                peer.pending.push_back(encode(&Message::Awareness(update)));
            }
            let sv = self.doc.transact().state_vector();
            return Some(encode(&Message::Sync(SyncMessage::SyncStep1(sv))));
        }

        // Steady state: the next message this peer has not seen, skipping the
        // ones it gave us in the first place.
        let next = {
            let outbox = self.outbox.lock().ok()?;
            let mut index = peer.cursor.saturating_sub(self.trimmed);
            let mut found = None;
            while index < outbox.len() {
                let (from, bytes) = &outbox[index];
                index += 1;
                if *from != Some(peer.id) {
                    found = Some(bytes.clone());
                    break;
                }
            }
            peer.cursor = self.trimmed + index;
            found
        };
        self.cursors.insert(peer.id, peer.cursor);
        self.trim_outbox();
        next
    }

    /// Retire a session whose transport has gone, taking its peers' carets with
    /// it.
    ///
    /// Dropping a [`SyncSession`] is not enough, and the difference only shows
    /// up in the shape of deployment this exists for. The outbox can only be
    /// trimmed as far as the *least* advanced session has read, so a session
    /// that stops being pumped pins it forever: a long-lived document with
    /// clients coming and going — a relay — grows without bound. An editor's
    /// document lives as long as its editor and never notices.
    ///
    /// The presence half is the visible one. Every peer whose state arrived over
    /// this connection is removed, which broadcasts the removal onwards, so a
    /// collaborator whose connection died takes their caret off the other
    /// screens at once instead of lingering until
    /// [`expire_stale_peers`](Self::expire_stale_peers) times them out half a
    /// minute later. That matches what y-websocket does when a socket closes.
    ///
    /// Pump the *remaining* sessions afterwards, or nobody hears about it.
    ///
    /// Takes the session by value: a closed session must not be used again, and
    /// the protocol has no way to resume one — a reconnect starts a fresh
    /// [`SyncSession`] at both ends.
    pub fn close_session(&mut self, peer: SyncSession) {
        self.cursors.remove(&peer.id);
        let local = self.awareness.client_id();
        for client in self.session_clients.remove(&peer.id).unwrap_or_default() {
            // Removing ourselves would announce our own departure while we are
            // still here — reachable if a peer ever echoes our state back.
            if client == local {
                continue;
            }
            // A connection can carry news of people who are not on it. Answering
            // an awareness query means sending your *whole* view, so a peer that
            // reconnects tells us about everyone it knows, and all of them get
            // attributed to that one session. Dropping them all when it closes
            // would take down carets belonging to people who never went
            // anywhere. So a peer only goes when nobody else still vouches for
            // them.
            if self
                .session_clients
                .values()
                .any(|seen| seen.contains(&client))
            {
                continue;
            }
            self.awareness.remove_state(client);
        }
        self.trim_outbox();
    }

    /// Drop outbox entries every known session has consumed.
    fn trim_outbox(&mut self) {
        let Some(min) = self.cursors.values().copied().min() else {
            return;
        };
        let drop_to = min.saturating_sub(self.trimmed);
        if drop_to == 0 {
            return;
        }
        if let Ok(mut outbox) = self.outbox.lock() {
            let drop_to = drop_to.min(outbox.len());
            outbox.drain(..drop_to);
            self.trimmed += drop_to;
        }
    }
}

pub(super) fn encode(message: &Message) -> Vec<u8> {
    message.encode_v1()
}

/// Tag a transaction with the session that caused it.
///
/// Only the round trip matters, so the cheapest encoding yrs offers a
/// non-`ClientID` value is fine. An origin we cannot parse simply reads as
/// "not from any session", which is the safe answer: the update gets offered to
/// every peer rather than withheld from one.
pub(super) fn session_origin(id: u64) -> Origin {
    Origin::from(id.to_string())
}

/// Recover the session id a transaction was tagged with, if any.
pub(super) fn origin_session(origin: &Origin) -> Option<u64> {
    std::str::from_utf8(origin.as_ref()).ok()?.parse().ok()
}
