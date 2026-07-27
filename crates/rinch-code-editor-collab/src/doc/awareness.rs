//! Presence: who else is in the document, and where their carets are.
//!
//! This rides the y-protocols *awareness* protocol over the same sessions and
//! the same outbox as the document itself, which is what keeps a caret from
//! overtaking the edit it refers to. It never touches the text: nothing here
//! opens a write transaction, and nothing here produces a
//! [`Splice`](rinch_code_editor_buffer::Splice).
//!
//! What goes *into* a presence state — and why it is shaped exactly the way
//! Yjs shapes it, so y-codemirror can read ours — lives in
//! [`crate::presence`].

use std::sync::atomic::Ordering;

use rinch_code_editor_buffer::Selection;
use yrs::sync::time::Clock;
use yrs::{Assoc, IndexedSequence, ReadTxn, StickyIndex, TextRef, Transact};

use super::CollabDoc;
use crate::presence::{Peer, PortableClock, PresenceState, StickyCursor, User};

impl CollabDoc {
    /// This document's own client id — the key peers will see it under.
    pub fn local_id(&self) -> u64 {
        self.awareness.client_id().get()
    }

    /// A counter bumped whenever any peer's presence actually changes.
    ///
    /// Presence carries no text, so
    /// [`receive_message`](Self::receive_message) returns no splices for it;
    /// watch this instead and re-read [`peers`](Self::peers) when it moves. A
    /// bare liveness heartbeat does **not** bump it — it changes a clock and
    /// nothing anyone can see.
    pub fn presence_version(&self) -> u64 {
        self.presence_version.load(Ordering::Relaxed)
    }

    /// Announce who we are, replacing any identity already published.
    ///
    /// Publishing an identity is what makes this document visible to others at
    /// all: a peer that never calls this is present in the protocol but absent
    /// from every [`peers`](Self::peers) list, which is the right default for
    /// an embedder that has not opted into presence.
    pub fn set_local_user(&mut self, user: User) {
        let cursor = self.local_state().and_then(|s| s.cursor);
        self.publish(PresenceState { user, cursor });
    }

    /// Publish where our caret is, in **chars**.
    ///
    /// Offsets are resolved to [`StickyIndex`] anchors *now*, against the
    /// document as it currently stands, so they keep pointing at the same text
    /// through anyone's later edits. `None` means "present, but no caret here"
    /// — what to publish when the editor loses focus.
    ///
    /// A no-op until [`set_local_user`](Self::set_local_user) has been called;
    /// a caret with nobody attached to it cannot be drawn or labelled.
    pub fn set_local_selection(&mut self, selection: Option<Selection>) {
        let Some(state) = self.local_state() else {
            return;
        };
        let cursor = selection.and_then(|sel| self.sticky_cursor(sel));
        self.publish(PresenceState {
            user: state.user,
            cursor,
        });
    }

    /// Withdraw from the document — the explicit "I am leaving" every other
    /// peer needs to drop our caret at once rather than waiting out
    /// [`PEER_TIMEOUT_MS`](crate::PEER_TIMEOUT_MS).
    ///
    /// Pump afterwards, or nobody hears it.
    pub fn clear_local_presence(&mut self) {
        self.awareness.clean_local_state();
    }

    /// Re-announce our own state to keep it from ageing out.
    ///
    /// Call every [`HEARTBEAT_MS`](crate::HEARTBEAT_MS) while connected, then
    /// pump. It re-publishes an identical state on purpose: the awareness clock
    /// increments on every set, and that increment *is* the liveness signal.
    /// Doing nothing when idle is what makes a peer look departed.
    pub fn presence_heartbeat(&mut self) {
        if let Some(state) = self.local_state() {
            self.publish(state);
        }
    }

    /// Drop peers that have not been heard from within `timeout_ms`, returning
    /// the ones removed.
    ///
    /// Call every [`SWEEP_MS`](crate::SWEEP_MS) with
    /// [`PEER_TIMEOUT_MS`](crate::PEER_TIMEOUT_MS). This is the only thing that
    /// clears a peer whose connection died without a goodbye — a closed laptop
    /// sends no "leave" message.
    pub fn expire_stale_peers(&mut self, timeout_ms: u64) -> Vec<u64> {
        let now = PortableClock.now();
        let local = self.awareness.client_id();
        // yrs's ClientID is a newtype; it stays inside this crate and only its
        // yjs-compatible u64 value crosses the public boundary.
        let stale: Vec<_> = self
            .awareness
            .iter()
            .filter(|(id, _)| *id != local)
            .filter_map(|(id, _)| {
                let (_, last_seen) = self.awareness.meta(id)?;
                (now.saturating_sub(last_seen) > timeout_ms).then_some(id)
            })
            .collect();
        for id in &stale {
            self.awareness.remove_state(*id);
        }
        stale.into_iter().map(|id| id.get()).collect()
    }

    /// Everyone else currently in the document, with their carets resolved to
    /// char offsets **as of now**.
    ///
    /// Excludes us. Peers that have not published a [`User`] are omitted: they
    /// are in the protocol but have nothing renderable. Order is unspecified —
    /// key off [`Peer::id`].
    pub fn peers(&self) -> Vec<Peer> {
        let local = self.awareness.client_id();
        let txn = self.doc.transact();
        self.awareness
            .iter()
            .filter(|(id, _)| *id != local)
            .filter_map(|(id, _)| {
                let state: PresenceState = self.awareness.state(id)?;
                let selection = state
                    .cursor
                    .and_then(|c| {
                        Some(Selection::new(
                            self.resolve(&txn, &c.anchor)?,
                            self.resolve(&txn, &c.head)?,
                        ))
                    })
                    .map(|mut sel| {
                        // An anchor can outlive the text it named, landing past the
                        // end after a concurrent delete. Clamping keeps a stale
                        // caret drawable instead of panicking the renderer.
                        let len = self.mirror.len_chars();
                        sel.anchor = sel.anchor.min(len);
                        sel.head = sel.head.min(len);
                        sel
                    });
                Some(Peer {
                    id: id.get(),
                    user: state.user,
                    selection,
                })
            })
            .collect()
    }

    /// Our own published state, if any.
    fn local_state(&self) -> Option<PresenceState> {
        self.awareness.local_state()
    }

    /// Publish a state, letting the failure to serialize it be unreachable
    /// rather than merely unlikely: every field is a `String` or a
    /// [`StickyIndex`], and neither can fail to become JSON.
    fn publish(&mut self, state: PresenceState) {
        let _ = self.awareness.set_local_state(state);
    }

    /// Turn a char-offset selection into op-identity anchors.
    ///
    /// The offsets cross the same chars → UTF-16 boundary as everything else
    /// here, through the mirror.
    /// Note the **read** transaction: naming a position does not modify the
    /// document, and taking a write transaction here would open the door to
    /// presence emitting document updates — the thing
    /// `publishing_presence_does_not_change_the_document` exists to forbid.
    fn sticky_cursor(&self, sel: Selection) -> Option<StickyCursor> {
        let len = self.mirror.len_chars();
        let anchor_cu = self.mirror.char_to_utf16_cu(sel.anchor.min(len)) as u32;
        let head_cu = self.mirror.char_to_utf16_cu(sel.head.min(len)) as u32;
        let txn = self.doc.transact();
        Some(StickyCursor {
            anchor: Self::anchor_at(&self.text, &txn, anchor_cu)?,
            head: Self::anchor_at(&self.text, &txn, head_cu)?,
        })
    }

    /// Anchor one offset, preferring the association that makes a caret behave.
    ///
    /// `Assoc::After` is the one we want nearly everywhere: it names the
    /// operation the caret sits *before*, so text a peer types at that exact
    /// spot appears after the caret rather than pushing it along — which is
    /// what a human expects to watch happen, and y-codemirror's choice.
    ///
    /// But `Assoc::After` **cannot be represented at the end of the document**:
    /// yrs walks to the requested index and, finding nothing after it to name,
    /// returns `None`. (yrs is stricter than Yjs here, which falls through to a
    /// position relative to the type itself.) Left unhandled that silently
    /// drops the single most common caret position there is — the end of the
    /// file — and only that one, which is exactly the kind of hole that reaches
    /// production. So the end of the document anchors `Before` the last
    /// character instead, which resolves back to the same offset and keeps the
    /// same behaviour: appended text still lands after the caret.
    fn anchor_at<T: ReadTxn>(text: &TextRef, txn: &T, offset_cu: u32) -> Option<StickyIndex> {
        if let Some(at) = text.sticky_index(txn, offset_cu, Assoc::After) {
            return Some(at);
        }
        text.sticky_index(txn, offset_cu, Assoc::Before)
    }

    /// Resolve an anchor back to a char offset against the document as it is.
    fn resolve<T: ReadTxn>(&self, txn: &T, at: &StickyIndex) -> Option<usize> {
        let offset = at.get_offset(txn)?;
        let cu = (offset.index as usize).min(self.mirror.len_utf16_cu());
        Some(self.mirror.utf16_cu_to_char(cu))
    }
}
