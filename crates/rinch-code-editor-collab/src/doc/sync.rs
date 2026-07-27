//! The incoming half: a frame off the wire, turned into edits for the model.
//!
//! One entry point, [`CollabDoc::receive_message`], and everything under it is
//! the road from bytes to [`Splice`]s — decode the frame, dispatch each message
//! it carried, apply what a peer sent, and convert the engine's UTF-16 delta
//! walk back into the char offsets the rest of the editor speaks.

use rinch_code_editor_buffer::Splice;
use ropey::Rope;
use yrs::encoding::read::Cursor;
use yrs::sync::{Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::{GetString, ReadTxn, Transact, Update};

use super::session::{encode, session_origin};
use super::{CollabDoc, CollabError, Step, SyncSession};

impl CollabDoc {
    /// Ingest one frame from `peer`; the returned splices replay every remote
    /// edit it carried onto the model's buffer (sequentially — see [`Splice`]).
    ///
    /// Remember the pump rule: receiving usually obligates responses, to this
    /// peer and every other.
    ///
    /// Malformed or unapplicable input from `peer` returns
    /// [`CollabError::BadMessage`] or [`CollabError::BadPeerData`] and leaves
    /// this document untouched — the recovery is to drop the connection; it is
    /// never a panic and never a silent divergence.
    ///
    /// # One frame, possibly several messages
    ///
    /// A frame may carry more than one protocol message, back to back, and every
    /// one of them is handled. This costs nothing between two of *our* documents
    /// — we emit one message per frame — and it is not optional against the
    /// wider ecosystem: yrs's own `DefaultProtocol::start` packs a sync step 1
    /// and an awareness update into a single frame, and the Yjs client does the
    /// same. Reading only the first message would drop the second silently, so
    /// the failure would look like presence occasionally not arriving.
    ///
    /// The reader stops at the end of the buffer, and it cannot distinguish
    /// "that was the last message" from "this message ran off the end" — both
    /// are the same condition to it. So a frame that decodes to **no** messages
    /// at all is rejected, which is what catches bytes that were never a
    /// protocol message. Trailing rubbish *after* a valid message is the one
    /// case that still passes quietly; the connection is already broken by then,
    /// and the alternative is arithmetic on re-encoded lengths that would fail
    /// working sessions whenever an encoder was not exactly its own inverse.
    pub fn receive_message(
        &mut self,
        peer: &mut SyncSession,
        bytes: &[u8],
    ) -> Result<Vec<Splice>, CollabError> {
        let mut decoder = DecoderV1::new(Cursor::new(bytes));
        let mut messages = Vec::new();
        for message in MessageReader::new(&mut decoder) {
            messages.push(message.map_err(CollabError::BadMessage)?);
        }
        if messages.is_empty() {
            return Err(CollabError::EmptyFrame);
        }

        let mut splices = Vec::new();
        for message in messages {
            // Splices from successive messages simply continue the sequence:
            // each addresses the text the ones before it produced, which is the
            // contract `Splice` already documents.
            splices.extend(self.handle_message(peer, message)?);
        }
        Ok(splices)
    }

    /// One decoded message.
    fn handle_message(
        &mut self,
        peer: &mut SyncSession,
        message: Message,
    ) -> Result<Vec<Splice>, CollabError> {
        match message {
            Message::Sync(SyncMessage::SyncStep1(their_sv)) => {
                // They told us what they have; owe them the difference.
                let update = self.doc.transact().encode_diff_v1(&their_sv);
                peer.pending
                    .push_back(encode(&Message::Sync(SyncMessage::SyncStep2(update))));
                Ok(Vec::new())
            }
            Message::Sync(SyncMessage::SyncStep2(update))
            | Message::Sync(SyncMessage::Update(update)) => {
                let update = Update::decode_v1(&update).map_err(CollabError::BadMessage)?;
                let splices = self.apply_remote_update(update, peer.id)?;
                // A step 2 is the answer to our opening move: the content, such
                // as it is, has arrived. An empty document produces no splices
                // at all, which is exactly why readiness is a flag and not an
                // observation of the splice stream.
                self.synced = true;
                Ok(splices)
            }
            // Presence. Carries no text, so no splices — watch
            // [`presence_version`](Self::presence_version) instead. A malformed
            // awareness payload is the peer's fault like any other bad input,
            // but it is *only* presence: dropping it costs a stale caret, not
            // document integrity, so it is swallowed rather than escalated into
            // a dropped connection.
            Message::Awareness(update) => {
                // Remember who this connection speaks for, so closing it can
                // take their carets down instead of leaving them until the
                // liveness timeout notices.
                self.session_clients
                    .entry(peer.id)
                    .or_default()
                    .extend(update.clients.keys().copied());
                let _ = self
                    .awareness
                    .apply_update_with(update, session_origin(peer.id));
                Ok(Vec::new())
            }
            // "Tell me who is here." Answer with everything we know, which is
            // how a joiner learns about peers that have been idle since before
            // it arrived and would otherwise stay invisible until they moved.
            Message::AwarenessQuery => {
                if let Ok(update) = self.awareness.update() {
                    peer.pending.push_back(encode(&Message::Awareness(update)));
                }
                Ok(Vec::new())
            }
            // A server refusing us. Swallowing it would leave the embedder
            // pumping a connection that will never carry anything, with nothing
            // to show the user — the one failure mode a collaborative session
            // cannot recover from on its own.
            Message::Auth(Some(reason)) => Err(CollabError::Denied(reason.to_string())),
            // Permission *granted* is the absence of news.
            Message::Auth(None) => Ok(Vec::new()),
            Message::Custom(_, _) => Ok(Vec::new()),
        }
    }

    /// Apply a remote update, capturing the text changes it produced and
    /// converting them to char-offset splices.
    fn apply_remote_update(
        &mut self,
        update: Update,
        from: u64,
    ) -> Result<Vec<Splice>, CollabError> {
        *self.steps.lock().expect("collector mutex") = Some(Vec::new());
        let result = {
            // Tagged with the session it came from, so the update observer can
            // keep it from being sent straight back down the same connection.
            let mut txn = self.doc.transact_mut_with(session_origin(from));
            txn.apply_update(update)
        };
        let steps = self
            .steps
            .lock()
            .expect("collector mutex")
            .take()
            .unwrap_or_default();

        if let Err(e) = result {
            // The document rejected it; the mirror was never touched.
            return Err(CollabError::BadPeerData(e));
        }

        let splices = self.drain_steps(steps);
        debug_assert_eq!(
            self.mirror.to_string(),
            self.text.get_string(&self.doc.transact()),
            "the mirror and the document disagree after a remote update"
        );
        Ok(splices)
    }

    /// Turn a captured delta walk into sequential [`Splice`]s, updating the
    /// mirror as it goes.
    ///
    /// yrs reports a *relative* walk — retain, insert, delete, with each step
    /// addressed to the text the previous steps produced — which is precisely
    /// the sequential contract [`Splice`] documents. The only work is the
    /// units: yrs counts UTF-16 code units, we speak chars, and the mirror is
    /// what converts between them. Because the mirror is mutated step by step,
    /// each conversion is computed against exactly the text that step sees.
    fn drain_steps(&mut self, steps: Vec<Step>) -> Vec<Splice> {
        if steps.iter().any(|s| matches!(s, Step::Unrepresentable)) {
            return self.full_replace();
        }

        let mut splices = Vec::new();
        let mut at = 0usize; // char cursor
        for step in steps {
            match step {
                Step::Retain(units) => {
                    let cu = self.mirror.char_to_utf16_cu(at) + units as usize;
                    at = self
                        .mirror
                        .utf16_cu_to_char(cu.min(self.mirror.len_utf16_cu()));
                }
                Step::Insert(s) => {
                    self.mirror.insert(at, &s);
                    let inserted = s.chars().count();
                    splices.push(Splice::new(at, 0, s));
                    at += inserted;
                }
                Step::Delete(units) => {
                    let cu = self.mirror.char_to_utf16_cu(at) + units as usize;
                    let end = self
                        .mirror
                        .utf16_cu_to_char(cu.min(self.mirror.len_utf16_cu()));
                    if end > at {
                        self.mirror.remove(at..end);
                        splices.push(Splice::new(at, end - at, ""));
                    }
                }
                Step::Unrepresentable => unreachable!("checked above"),
            }
        }
        splices
    }

    /// The single splice that rewrites the model's whole text to the document's
    /// — the resync for anything this plain-text projection cannot replay.
    ///
    /// A peer speaking richer Yjs than we model (formatting attributes, an
    /// embedded value) can produce a delta we would otherwise have to guess at.
    /// Guessing would desynchronize the model silently; resyncing is loud,
    /// correct, and rare.
    fn full_replace(&mut self) -> Vec<Splice> {
        let truth = self.text.get_string(&self.doc.transact());
        let old_len = self.mirror.len_chars();
        self.mirror = Rope::from_str(&truth);
        if old_len == 0 && truth.is_empty() {
            Vec::new()
        } else {
            vec![Splice::new(0, old_len, truth)]
        }
    }
}
