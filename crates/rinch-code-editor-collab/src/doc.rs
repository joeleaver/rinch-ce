//! The collaborative document and its per-peer sync sessions.
//!
//! [`CollabDoc`] is a CRDT mirror of the editor's buffer: local edits flow in as
//! [`Change`]s ([`CollabDoc::apply_local`]), remote edits flow out as
//! [`Splice`]s ([`CollabDoc::receive_message`]), and everything between two
//! documents travels as opaque protocol bytes the embedder transports.
//!
//! The engine is [yrs](https://docs.rs/yrs), the Rust core of Yjs, and the wire
//! format is the **y-sync protocol** — so a session can talk to an unmodified
//! Yjs relay (y-websocket, Hocuspocus) and to Yjs peers, not only to us.
//!
//! # The pump rule
//!
//! The protocol is a per-peer conversation. After **any** local change and after
//! **any** received message, call [`CollabDoc::generate_message`] for **every**
//! peer, delivering what comes out, until a full round over all peers returns
//! `None` — that is quiescence. Updates are idempotent, so one reaching a peer
//! twice (say, relayed through two different peers) converges harmlessly.
//!
//! # Framing
//!
//! A message is a length-delimited unit, not a stream: the transport must
//! preserve message boundaries. WebSocket binary frames give this for free; a
//! raw TCP stream needs a length prefix. Each peer's messages must arrive
//! reliably and in order — after a transport reconnect, start a fresh
//! [`SyncSession`] (both sides), never resume the old one.
//!
//! # Offsets
//!
//! Everything this crate exposes is in **chars** (Unicode scalars), matching
//! [`Buffer`](rinch_code_editor_buffer::Buffer) and ropey. yrs indexes its text
//! in **UTF-16 code units**, so this module converts at its boundary — see
//! [`CollabDoc::mirror`]. That conversion is the one thing in here that a
//! careless edit can silently corrupt, so it happens in exactly two functions
//! and nowhere else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rinch_code_editor_buffer::{Change, Splice};
use ropey::Rope;
use yrs::sync::{Message, SyncMessage};
use yrs::types::Delta;
use yrs::types::text::TextEvent;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, Observable, OffsetKind, Options, Origin, Out, ReadTxn, StateVector,
    Subscription, Text, TextRef, Transact, Update,
};

/// The key the shared text lives under. Root types in yrs are implicit and
/// keyed by name, so two peers naming the same key get the *same* object with no
/// coordination — unlike a CRDT where the container itself is an operation,
/// there is no "both sides created it" hazard to repair.
const CONTENT: &str = "content";

/// An update this document produced, tagged with the session it arrived on so
/// it is never echoed back down that connection.
type Outgoing = (Option<u64>, Vec<u8>);
/// The shared outbox. `Arc<Mutex<_>>` rather than `Rc<RefCell<_>>` only because
/// yrs requires observer closures to be `Send + Sync`; it is never contended.
type Outbox = Arc<Mutex<Vec<Outgoing>>>;

/// One recognized step of a remote edit, in yrs's UTF-16 units, captured while
/// an update is being applied. Converted to [`Splice`]s (chars) immediately
/// afterwards; see [`CollabDoc::drain_steps`].
enum Step {
    Retain(u32),
    Insert(String),
    Delete(u32),
    /// A change to the text this plain-text projection cannot represent — an
    /// embedded value, or formatting attributes. Triggers a resync rather than
    /// a guess.
    Unrepresentable,
}

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
    id: u64,
    /// Whether our opening state vector has gone out.
    opened: bool,
    /// A reply owed to this peer, produced while handling its last message.
    /// Handed over by the next [`CollabDoc::generate_message`].
    pending: Option<Vec<u8>>,
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
            pending: None,
            cursor: 0,
        }
    }
}

/// Why a [`CollabDoc`] operation could not proceed.
///
/// The variants split cleanly by *who is at fault*, which is also the recovery
/// each one calls for:
///
/// - [`BadMessage`](Self::BadMessage) and [`BadPeerData`](Self::BadPeerData)
///   blame the **remote peer** — the bytes on the wire were malformed or
///   unapplicable. Network input is untrusted; the recovery is to drop that
///   connection, never to crash.
/// - [`BadSnapshot`](Self::BadSnapshot) blames the **bytes handed to
///   [`CollabDoc::load`]** — wrong file, wrong document.
/// - [`NotSynced`](Self::NotSynced) blames **call ordering** — an edit before
///   the content exists; hold editing until [`CollabDoc::is_synced`].
/// - [`OutOfRange`](Self::OutOfRange) and [`Internal`](Self::Internal) blame
///   **this integration** — the model and the document disagree about the text.
///   Never reachable from network input, and a bug: fail fast.
#[derive(Debug)]
#[non_exhaustive]
pub enum CollabError {
    /// The bytes handed to [`CollabDoc::receive_message`] do not decode as a
    /// protocol message. Untrusted input; drop the connection, do not crash.
    BadMessage(yrs::encoding::read::Error),
    /// A message decoded, but the update it carried could not be applied — a
    /// corrupt payload from a hostile or buggy peer. Same fault as
    /// [`BadMessage`](Self::BadMessage), same recovery. The document is left
    /// untouched, so the peer may reconnect with a fresh [`SyncSession`].
    BadPeerData(yrs::error::UpdateError),
    /// The bytes handed to [`CollabDoc::load`] do not decode as a document.
    BadSnapshot(yrs::encoding::read::Error),
    /// A local edit arrived before the shared text exists. A document created
    /// with [`CollabDoc::join`] is read-only until the first sync delivers the
    /// content; gate editing on [`CollabDoc::is_synced`].
    NotSynced,
    /// A local [`Change`] addressed text outside the shared document — the
    /// model and the document have disagreed about the contents. Caught
    /// *before* anything is applied, so the document is untouched and still
    /// describes itself honestly; resynchronize the model from
    /// [`CollabDoc::text`] rather than carrying on.
    OutOfRange {
        /// The offending edit's start, in pre-change coordinates.
        pos: usize,
        /// How many chars it tried to remove.
        del: usize,
        /// The length of the shared text the change was validated against.
        len: usize,
    },
    /// An operation the engine rejected for a reason that is not the peer's
    /// fault and not a range fault. An integration bug.
    Internal(String),
}

impl std::fmt::Display for CollabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMessage(e) => write!(f, "protocol message failed to decode: {e}"),
            Self::BadPeerData(e) => write!(f, "peer sent an update that could not be applied: {e}"),
            Self::BadSnapshot(e) => write!(f, "snapshot failed to load: {e}"),
            Self::NotSynced => write!(f, "local edit before the first sync delivered the content"),
            Self::OutOfRange { pos, del, len } => write!(
                f,
                "local change deletes {del} chars at {pos} of a {len}-char document"
            ),
            Self::Internal(e) => write!(f, "internal engine operation failed: {e}"),
        }
    }
}

impl std::error::Error for CollabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadMessage(e) | Self::BadSnapshot(e) => Some(e),
            Self::BadPeerData(e) => Some(e),
            Self::NotSynced | Self::OutOfRange { .. } | Self::Internal(_) => None,
        }
    }
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
    /// dropped. A session that stops being pumped holds the outbox open — the
    /// embedder should drop sessions it has finished with.
    cursors: HashMap<u64, usize>,
    /// Whether the content has arrived. A joiner is read-only until it has.
    synced: bool,
    _sub: Subscription,
    _updates: Subscription,
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
                    out.push((from, e.update.clone()));
                }
            })
            .expect("a fresh document accepts an update observer");
        let mirror = Rope::from_str(&text.get_string(&doc.transact()));
        Self {
            doc,
            text,
            mirror,
            steps,
            outbox,
            trimmed: 0,
            cursors: HashMap::new(),
            synced,
            _sub: sub,
            _updates: updates,
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
    /// the pump rule (see module docs) to get the change to the peers.
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

    /// The next message for `peer`, if the conversation is not quiescent.
    ///
    /// See the module docs for the pump rule: call this for every peer after
    /// any local change or receive, until all peers return `None`.
    pub fn generate_message(&mut self, peer: &mut SyncSession) -> Option<Vec<u8>> {
        // A reply owed from the last message we handled.
        if let Some(bytes) = peer.pending.take() {
            return Some(bytes);
        }

        // The opening move: tell the peer what we have, so it can send the rest.
        if !peer.opened {
            peer.opened = true;
            // Everything already in the outbox is part of the state this
            // handshake will reconcile, so it must not be replayed afterwards.
            peer.cursor = self.trimmed + self.outbox.lock().map_or(0, |o| o.len());
            self.cursors.insert(peer.id, peer.cursor);
            let sv = self.doc.transact().state_vector();
            return Some(encode(&Message::Sync(SyncMessage::SyncStep1(sv))));
        }

        // Steady state: the next update this peer has not seen, skipping the
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
        next.map(|update| encode(&Message::Sync(SyncMessage::Update(update))))
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

    /// Ingest one message from `peer`; the returned splices replay every remote
    /// edit it carried onto the model's buffer (sequentially — see [`Splice`]).
    ///
    /// Remember the pump rule: receiving usually obligates responses, to this
    /// peer and every other.
    ///
    /// Malformed or unapplicable input from `peer` returns
    /// [`CollabError::BadMessage`] or [`CollabError::BadPeerData`] and leaves
    /// this document untouched — the recovery is to drop the connection; it is
    /// never a panic and never a silent divergence.
    pub fn receive_message(
        &mut self,
        peer: &mut SyncSession,
        bytes: &[u8],
    ) -> Result<Vec<Splice>, CollabError> {
        let message = Message::decode_v1(bytes).map_err(CollabError::BadMessage)?;
        match message {
            Message::Sync(SyncMessage::SyncStep1(their_sv)) => {
                // They told us what they have; owe them the difference.
                let update = self.doc.transact().encode_diff_v1(&their_sv);
                peer.pending = Some(encode(&Message::Sync(SyncMessage::SyncStep2(update))));
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
            // Awareness rides the same channel in the y-sync protocol. Presence
            // is a later increment; ignoring these keeps us interoperable with
            // peers that send them rather than treating them as corrupt.
            Message::Awareness(_) | Message::AwarenessQuery | Message::Auth(_) => Ok(Vec::new()),
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

    /// Assert the mirror still holds exactly what the document holds.
    ///
    /// The same invariant the `debug_assert_eq!`s above state — but those are
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

fn encode(message: &Message) -> Vec<u8> {
    message.encode_v1()
}

/// Tag a transaction with the session that caused it.
///
/// Only the round trip matters, so the cheapest encoding yrs offers a
/// non-`ClientID` value is fine. An origin we cannot parse simply reads as
/// "not from any session", which is the safe answer: the update gets offered to
/// every peer rather than withheld from one.
fn session_origin(id: u64) -> Origin {
    Origin::from(id.to_string())
}

/// Recover the session id a transaction was tagged with, if any.
fn origin_session(origin: &Origin) -> Option<u64> {
    std::str::from_utf8(origin.as_ref()).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinch_code_editor_buffer::{Buffer, Edit};

    /// An empty document is a document. It round-trips, it is synced, and it is
    /// editable afterwards.
    ///
    /// This is a regression pin, not a formality: yrs encodes an empty document
    /// as an update carrying **no root type at all** (two bytes), so `load`
    /// used to look for the content key, not find it, and refuse the snapshot
    /// with `SnapshotMissingContent` — meaning saving and reopening an empty
    /// file was an error. See [`CollabDoc::load`] for why no check can tell an
    /// empty document from a foreign one.
    #[test]
    fn an_empty_snapshot_round_trips_and_is_editable() {
        let mut doc = CollabDoc::create("");
        let bytes = doc.save();
        let mut loaded = CollabDoc::load(&bytes).expect("an empty document is still a document");
        assert_eq!(loaded.text(), "");
        assert_eq!(loaded.len_chars(), 0);
        assert!(loaded.is_synced());

        let mut buffer = Buffer::from_text("");
        let change = Change::apply(&mut buffer, vec![Edit::insert(0, "typed after reopening")]);
        loaded.apply_local(&change).unwrap();
        assert_eq!(loaded.text(), "typed after reopening");
        loaded.assert_mirror_intact();
    }

    #[test]
    fn garbage_bytes_are_refused_by_load() {
        for bytes in [&b"not a yrs document"[..], &b""[..], &[0xffu8; 8][..]] {
            assert!(
                matches!(CollabDoc::load(bytes), Err(CollabError::BadSnapshot(_))),
                "expected a decode failure from {bytes:?}"
            );
        }
    }

    /// A document that is not ours loads as an empty one, deliberately.
    ///
    /// yrs cannot distinguish it: root types are untyped on the wire (a root
    /// arriving in an update reports `TypeRef::Undefined` whether it was a text
    /// or a map at the source), and `get_text` coerces rather than
    /// discriminates. This test exists so that the next person to notice the
    /// missing check finds the reasoning attached to the behaviour instead of
    /// reinstating a guard that rejects empty documents to catch nothing.
    #[test]
    fn a_foreign_document_loads_as_empty_because_yrs_cannot_tell() {
        use yrs::{Any, Map};

        let foreign = Doc::new();
        let map = foreign.get_or_insert_map(CONTENT);
        {
            let mut txn = foreign.transact_mut();
            map.insert(&mut txn, "k", Any::from("a map, not a text"));
        }
        let bytes = foreign
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        let loaded = CollabDoc::load(&bytes).expect("it decodes, so it loads");
        assert_eq!(
            loaded.text(),
            "",
            "the map is invisible to a text projection"
        );
        loaded.assert_mirror_intact();
    }
}
