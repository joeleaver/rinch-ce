//! The collaborative document and its per-peer sync sessions.
//!
//! [`CollabDoc`] is a CRDT mirror of the editor's buffer: local edits flow in as
//! [`Change`]s ([`CollabDoc::apply_local`]), remote edits flow out as
//! [`Splice`](rinch_code_editor_buffer::Splice)s
//! ([`CollabDoc::receive_message`]), and everything between two documents
//! travels as opaque protocol bytes the embedder transports.
//!
//! The engine is [yrs](https://docs.rs/yrs), the Rust core of Yjs, and the wire
//! format is the **y-sync protocol**: a varint message type then its payload,
//! which is exactly what y-websocket puts in a binary frame. So these bytes go
//! onto such a connection unwrapped, and a session can talk to an unmodified
//! y-websocket or y-sweet relay, and to Yjs peers, not only to us.
//!
//! # Where the rest of it lives
//!
//! This module owns the document itself: the CRDT handle, the ropey mirror
//! beside it, local edits, and snapshots. The conversation around it is split
//! by direction, because the two halves have almost nothing in common beyond
//! the document they share.
//!
//! - [`session`] — [`SyncSession`] and the *outgoing* half:
//!   [`CollabDoc::generate_message`], the outbox, and retiring a connection.
//!   **The pump rule and the framing requirements live there**, and an embedder
//!   that reads nothing else should read those.
//! - [`sync`] — the *incoming* half: decoding a frame, applying what a peer
//!   sent, and turning it back into splices for the model.
//! - [`awareness`] — presence. Who is here, where their carets are, and when to
//!   stop believing in them.
//! - [`error`] — [`CollabError`], organized by *who is at fault*.
//!
//! # Offsets
//!
//! Everything this crate exposes is in **chars** (Unicode scalars), matching
//! [`Buffer`](rinch_code_editor_buffer::Buffer) and ropey. yrs indexes its text
//! in **UTF-16 code units**, so this module converts at its boundary — see
//! [`CollabDoc::mirror`]. That conversion is the one thing in here that a
//! careless edit can silently corrupt, so it is confined to four functions and
//! happens nowhere else: [`apply_local`](CollabDoc::apply_local) and [`sync`]'s
//! `drain_steps` carry edits across it in each direction, and [`awareness`]'s
//! `sticky_cursor` and `resolve` carry caret positions across it.

mod awareness;
mod error;
mod session;
mod sync;

#[cfg(test)]
mod tests;

pub use error::CollabError;
pub use session::SyncSession;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rinch_code_editor_buffer::Change;
use ropey::Rope;
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::types::Delta;
use yrs::types::text::TextEvent;
use yrs::updates::decoder::Decode;
use yrs::{
    ClientID, Doc, GetString, Observable, OffsetKind, Options, Out, ReadTxn, StateVector,
    Subscription, Text, TextRef, Transact, Update,
};

use crate::presence::PortableClock;
use session::{encode, origin_session};

/// The key the shared text lives under. Root types in yrs are implicit and
/// keyed by name, so two peers naming the same key get the *same* object with no
/// coordination — unlike a CRDT where the container itself is an operation,
/// there is no "both sides created it" hazard to repair.
const CONTENT: &str = "content";

/// A fully-encoded protocol message this document produced, tagged with the
/// session it arrived on so it is never echoed back down that connection.
///
/// Encoded at production rather than at send because two different kinds of
/// message share this queue — document updates and awareness — and their
/// relative order matters: a caret that refers to an operation must not
/// overtake the operation it refers to.
type Outgoing = (Option<u64>, Vec<u8>);
/// The shared outbox. `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` only because
/// yrs requires observer closures to be `Send + Sync`; it is never contended.
type Outbox = Arc<Mutex<Vec<Outgoing>>>;

/// One recognized step of a remote edit, in yrs's UTF-16 units, captured while
/// an update is being applied. Converted to
/// [`Splice`](rinch_code_editor_buffer::Splice)s (chars) immediately
/// afterwards; see [`sync`]'s `drain_steps`.
enum Step {
    Retain(u32),
    Insert(String),
    Delete(u32),
    /// A change to the text this plain-text projection cannot represent — an
    /// embedded value, or formatting attributes. Triggers a resync rather than
    /// a guess.
    Unrepresentable,
}

/// A CRDT document mirroring one editor buffer, as a text at the content key.
pub struct CollabDoc {
    doc: Doc,
    text: TextRef,
    /// A rope holding exactly what the shared text holds.
    ///
    /// It earns its keep three times over. It is the only way to convert
    /// between the char offsets this crate exposes and the UTF-16 units yrs
    /// indexes by — exactly, and in O(log n). It makes [`text`](Self::text) and
    /// [`len_chars`](Self::len_chars) cheap. And comparing it against the
    /// document is the divergence detector, which matters because the engine's
    /// own consistency checks are `debug_assert`s and this project ships
    /// release builds.
    mirror: Rope,
    /// Where the text observer deposits steps, and the flag that gates it.
    /// A mutex rather than a `RefCell` only because yrs requires observer
    /// closures to be `Send + Sync`; it is never contended, since the observer
    /// runs inline on the thread applying the update.
    ///
    /// `Some` only while a *remote* update is being applied, because the
    /// observer fires for local edits too and echoing those back to the model
    /// would apply every keystroke twice.
    steps: Arc<Mutex<Option<Vec<Step>>>>,
    /// Every update this document has produced, oldest first, each tagged with
    /// the session it arrived on (so it is not echoed back there).
    ///
    /// yrs is *push*-based and this is why: a state vector cannot tell you
    /// whether a peer is missing anything, because a **deletion advances no
    /// clock** — deletes live in a separate delete-set. Comparing state vectors
    /// to decide what to send therefore propagates inserts and silently drops
    /// every deletion, which is exactly the bug this replaced.
    outbox: Outbox,
    /// How many entries have been dropped off the front of `outbox`, so a
    /// session's absolute cursor still indexes correctly.
    trimmed: usize,
    /// The last cursor seen for each session, so consumed entries can be
    /// dropped. A session that stops being pumped holds the outbox open, so a
    /// session the embedder has finished with must be handed to
    /// [`CollabDoc::close_session`] rather than merely dropped.
    cursors: HashMap<u64, usize>,
    /// Which peers' presence arrived on which session, so closing a session can
    /// take their carets down with it.
    ///
    /// Only tracked for received awareness: our own state is not on anyone
    /// else's connection. An entry is added when a session first carries news of
    /// a client and removed with the session.
    session_clients: HashMap<u64, HashSet<ClientID>>,
    /// Whether the content has arrived. A joiner is read-only until it has.
    synced: bool,
    /// Ephemeral presence — carets, names, colours — over the y-protocols
    /// awareness protocol. It shares this document (yrs `Doc` is an `Arc`
    /// handle, so this is the *same* store, not a copy) because awareness is
    /// keyed by the document's own client id.
    awareness: Awareness,
    /// Bumped whenever the set of peers or any peer's state actually changes,
    /// so a reactive embedder can re-render presence without diffing.
    /// Deliberately not bumped by a bare heartbeat, which changes a clock and
    /// nothing a renderer can see.
    presence_version: Arc<AtomicU64>,
    _sub: Subscription,
    _updates: Subscription,
    _presence_out: Subscription,
    _presence_change: Subscription,
}

impl std::fmt::Debug for CollabDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollabDoc")
            .field("len_chars", &self.mirror.len_chars())
            .field("synced", &self.synced)
            .finish_non_exhaustive()
    }
}

impl CollabDoc {
    fn build(doc: Doc, synced: bool) -> Self {
        let text = doc.get_or_insert_text(CONTENT);
        let steps: Arc<Mutex<Option<Vec<Step>>>> = Arc::new(Mutex::new(None));
        let sink = steps.clone();
        let sub = text.observe(move |txn, e: &TextEvent| {
            let Ok(mut slot) = sink.lock() else { return };
            let Some(out) = slot.as_mut() else {
                return; // a local edit; the model already has it
            };
            for delta in e.delta(txn) {
                out.push(match delta {
                    Delta::Retain(n, attrs) if attrs.is_none() => Step::Retain(*n),
                    Delta::Deleted(n) => Step::Delete(*n),
                    Delta::Inserted(Out::Any(yrs::Any::String(s)), attrs) if attrs.is_none() => {
                        Step::Insert(s.to_string())
                    }
                    // Formatting attributes on a range we render as plain text.
                    _ => Step::Unrepresentable,
                });
            }
        });
        let outbox: Outbox = Arc::new(Mutex::new(Vec::new()));
        let sink = outbox.clone();
        let updates = doc
            .observe_update_v1(move |txn, e| {
                let from = txn.origin().and_then(origin_session);
                if let Ok(mut out) = sink.lock() {
                    out.push((
                        from,
                        encode(&Message::Sync(SyncMessage::Update(e.update.clone()))),
                    ));
                }
            })
            .expect("a fresh document accepts an update observer");
        let mirror = Rope::from_str(&text.get_string(&doc.transact()));

        // Awareness rides the same connection and the same outbox as the
        // document, so ordering between "I inserted this" and "my caret is
        // against that insert" is preserved by construction.
        let mut awareness = Awareness::with_clock(doc.clone(), PortableClock);
        let sink = outbox.clone();
        let presence_out = awareness.on_update(move |aware, e, origin| {
            let from = origin.and_then(origin_session);
            // Only the clients this event actually touched — awareness updates
            // are deltas, never a full re-broadcast of everyone.
            let Ok(update) = aware.update_with_clients(e.all_changes()) else {
                return;
            };
            if let Ok(mut out) = sink.lock() {
                out.push((from, encode(&Message::Awareness(update))));
            }
        });
        let presence_version = Arc::new(AtomicU64::new(0));
        let counter = presence_version.clone();
        let presence_change = awareness.on_change(move |_, _, _| {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        Self {
            doc,
            text,
            mirror,
            steps,
            outbox,
            trimmed: 0,
            cursors: HashMap::new(),
            session_clients: HashMap::new(),
            synced,
            awareness,
            presence_version,
            _sub: sub,
            _updates: updates,
            _presence_out: presence_out,
            _presence_change: presence_change,
        }
    }

    fn new_doc() -> Doc {
        // The invariant this crate is built on. yrs offers UTF-8 bytes or
        // UTF-16 code units; UTF-16 is what Yjs itself uses, so it is the
        // choice that keeps us wire-compatible with the Yjs ecosystem. Two
        // peers configured differently still *converge* — the CRDT is
        // identity-based — but they would disagree about what an index means,
        // so this is a contract, not a local preference.
        let options = Options {
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        };
        Doc::with_options(options)
    }

    /// Start a session as its originator, seeding the shared text.
    pub fn create(initial: &str) -> Self {
        let mut this = Self::build(Self::new_doc(), true);
        if !initial.is_empty() {
            {
                let mut txn = this.doc.transact_mut();
                this.text.insert(&mut txn, 0, initial);
            }
            this.mirror = Rope::from_str(initial);
        }
        this
    }

    /// Join a session someone else created. The document is empty — and
    /// read-only, see [`CollabError::NotSynced`] — until the first sync
    /// delivers the content, at which point [`is_synced`](Self::is_synced)
    /// flips to `true`.
    pub fn join() -> Self {
        Self::build(Self::new_doc(), false)
    }

    /// Join from a snapshot previously produced by [`CollabDoc::save`].
    ///
    /// Bytes that do not decode are refused ([`CollabError::BadSnapshot`]).
    /// Bytes that *do* decode are accepted, whatever they contain — this
    /// deliberately does not try to verify that the snapshot is "one of ours",
    /// because in yrs that question has no answer:
    ///
    /// - Root types are implicit and untyped on the wire. A root that arrives
    ///   in an update reports `TypeRef::Undefined` whether it was a text or a
    ///   map at the source, and `get_text` **coerces** rather than discriminates
    ///   — its `None` means only "no root by that name", never "wrong type".
    /// - An *empty* document carries no root type at all. So a legitimate empty
    ///   snapshot from [`save`](Self::save) and a foreign document with no
    ///   content key are byte-for-byte indistinguishable by that test.
    ///
    /// A guard here would therefore reject every empty document to catch
    /// nothing — which is exactly what the previous one did. Yjs itself has no
    /// such concept (`Y.applyUpdate` accepts any update, root types spring into
    /// existence on demand), and identifying a file as yours belongs to the
    /// container format around the snapshot, not to the snapshot.
    pub fn load(bytes: &[u8]) -> Result<Self, CollabError> {
        let update = Update::decode_v1(bytes).map_err(CollabError::BadSnapshot)?;
        let doc = Self::new_doc();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(update)
                .map_err(|e| CollabError::Internal(e.to_string()))?;
        }
        Ok(Self::build(doc, true))
    }

    /// A snapshot of the whole document, for [`CollabDoc::load`].
    pub fn save(&mut self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// The current shared text. Empty on a joined document before first sync.
    pub fn text(&self) -> String {
        self.mirror.to_string()
    }

    /// The length of the shared text in chars — what the embedder's buffer must
    /// hold once every splice has been applied. Checking this against the
    /// model's own length after each exchange is the cheapest divergence
    /// detector there is, and worth doing.
    pub fn len_chars(&self) -> usize {
        self.mirror.len_chars()
    }

    /// Whether the shared text has arrived yet — equivalently, whether
    /// [`apply_local`](Self::apply_local) will be accepted rather than
    /// returning [`CollabError::NotSynced`].
    ///
    /// A [`create`](Self::create)d or [`load`](Self::load)ed document is synced
    /// from the start; a [`join`](Self::join)ed one becomes synced when a peer
    /// answers its opening message. **Poll this** rather than watching the
    /// splice stream: an *empty* shared document delivers no splices to
    /// announce itself, so a joiner watching for one would stay read-only
    /// forever.
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// Record a local edit the model has already applied to its buffer.
    ///
    /// Every local mutation must come through here — typing, paste, undo,
    /// redo — or the document and the buffer silently diverge. Follow up with
    /// the pump rule (the `doc::session` module docs) to get the change to the
    /// peers.
    pub fn apply_local(&mut self, change: &Change) -> Result<(), CollabError> {
        if !self.synced {
            return Err(CollabError::NotSynced);
        }

        // Validate the whole change before touching anything. A change that
        // fails partway would leave the document edited and the mirror stale,
        // and the mirror is what every later conversion is computed against —
        // so a half-applied change poisons every offset that follows it.
        let mut len = self.mirror.len_chars();
        let mut delta: isize = 0;
        for edit in change.edits() {
            let start = edit.range.start as isize + delta;
            let removed = edit.range.end.saturating_sub(edit.range.start);
            if start < 0 || (start as usize).saturating_add(removed) > len {
                return Err(CollabError::OutOfRange {
                    pos: edit.range.start,
                    del: removed,
                    len,
                });
            }
            let inserted = edit.text.chars().count();
            delta += inserted as isize - removed as isize;
            len = len - removed + inserted;
        }

        // The same running-delta walk as `Change::apply`: the edits are sorted,
        // non-overlapping, and in pre-change coordinates, while the document
        // and the mirror mutate under them as we go.
        let mut txn = self.doc.transact_mut();
        let mut delta: isize = 0;
        for edit in change.edits() {
            let start = (edit.range.start as isize + delta) as usize;
            let removed = edit.range.end.saturating_sub(edit.range.start);
            let end = start + removed;

            let start_cu = self.mirror.char_to_utf16_cu(start) as u32;
            let end_cu = self.mirror.char_to_utf16_cu(end) as u32;
            if end_cu > start_cu {
                self.text
                    .remove_range(&mut txn, start_cu, end_cu - start_cu);
            }
            if !edit.text.is_empty() {
                self.text.insert(&mut txn, start_cu, &edit.text);
            }

            if end > start {
                self.mirror.remove(start..end);
            }
            if !edit.text.is_empty() {
                self.mirror.insert(start, &edit.text);
            }
            delta += edit.text.chars().count() as isize - removed as isize;
        }
        drop(txn);

        debug_assert_eq!(
            self.mirror.to_string(),
            self.text.get_string(&self.doc.transact()),
            "the mirror and the document disagree after a local change"
        );
        Ok(())
    }

    /// Assert the mirror still holds exactly what the document holds.
    ///
    /// The same invariant the `debug_assert_eq!`s state — but those are
    /// compiled out of release builds, and this project builds and *tests*
    /// exclusively in release, so on its own that check never runs anywhere.
    /// This is the version the unit tests call explicitly, so the crate's
    /// load-bearing invariant is actually verified in the configuration we ship
    /// and test. It is O(document); that is why it is not on the edit path.
    #[cfg(test)]
    fn assert_mirror_intact(&self) {
        assert_eq!(
            self.mirror.to_string(),
            self.text.get_string(&self.doc.transact()),
            "the mirror and the document disagree"
        );
    }
}
