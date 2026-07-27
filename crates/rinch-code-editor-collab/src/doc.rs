//! The collaborative document and its per-peer sync sessions.
//!
//! [`CollabDoc`] is a CRDT mirror of the editor's buffer: local edits flow in as
//! [`Change`]s ([`CollabDoc::apply_local`]), remote edits flow out as
//! [`Splice`]s ([`CollabDoc::receive_message`]), and everything between two
//! documents travels as opaque protocol bytes the embedder transports.
//!
//! The engine is [yrs](https://docs.rs/yrs), the Rust core of Yjs, and the wire
//! format is the **y-sync protocol**: a varint message type then its payload,
//! which is exactly what y-websocket puts in a binary frame. So these bytes go
//! onto such a connection unwrapped, and a session can talk to an unmodified
//! y-websocket or y-sweet relay, and to Yjs peers, not only to us.
//!
//! **Not Hocuspocus.** It prefixes every frame with a lib0 varString document
//! name *before* the message type, so it is not y-sync on the wire and our
//! decoder would read that length byte as a message type. Talking to it needs an
//! adapter, and the adapter belongs in the embedder's transport rather than
//! here: a document name is addressing, which y-sync deliberately leaves to the
//! layer above.
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rinch_code_editor_buffer::{Change, Selection, Splice};
use ropey::Rope;
use yrs::encoding::read::Cursor;
use yrs::sync::time::Clock;
use yrs::sync::{Awareness, Message, MessageReader, SyncMessage};
use yrs::types::Delta;
use yrs::types::text::TextEvent;
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::Encode;
use yrs::{
    Assoc, ClientID, Doc, GetString, IndexedSequence, Observable, OffsetKind, Options, Origin, Out,
    ReadTxn, StateVector, StickyIndex, Subscription, Text, TextRef, Transact, Update,
};

use crate::presence::{Peer, PortableClock, PresenceState, StickyCursor, User};

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
    /// Replies owed to this peer, oldest first, produced while handling its
    /// last messages. Drained one per [`CollabDoc::generate_message`].
    ///
    /// A queue rather than a single slot because more than one exchange can be
    /// outstanding at once — a sync step 2 and an awareness reply are owed
    /// independently, and a single slot silently dropped whichever was
    /// answered second.
    pending: VecDeque<Vec<u8>>,
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
    /// A frame that carried no protocol message at all — bytes that were never
    /// this protocol, or a message truncated before its first field. Same fault
    /// and same recovery as [`BadMessage`](Self::BadMessage); it is separate
    /// only because the decoder reports the two conditions identically and
    /// saying so is more honest than picking one.
    EmptyFrame,
    /// The other end refused us, with its reason. Blames **neither side's
    /// code**: it is a policy answer from a server, and the only recovery is to
    /// stop pumping this connection and tell the user. Never produced by this
    /// crate — only received.
    Denied(String),
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
            Self::EmptyFrame => write!(f, "frame carried no protocol message"),
            Self::Denied(reason) => write!(f, "the other end refused this session: {reason}"),
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
            // The reason a server gave is its own words, not a nested error.
            Self::EmptyFrame
            | Self::Denied(_)
            | Self::NotSynced
            | Self::OutOfRange { .. }
            | Self::Internal(_) => None,
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

    // ── Presence ────────────────────────────────────────────────────────────

    /// This document's own client id — the key peers will see it under.
    pub fn local_id(&self) -> u64 {
        self.awareness.client_id().get()
    }

    /// A counter bumped whenever any peer's presence actually changes.
    ///
    /// Presence carries no text, so [`receive_message`](Self::receive_message)
    /// returns no splices for it; watch this instead and re-read
    /// [`peers`](Self::peers) when it moves. A bare liveness heartbeat does
    /// **not** bump it — it changes a clock and nothing anyone can see.
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

    /// The load-bearing equivalence: `apply_local`'s running-delta walk lands
    /// every edit at the same place `Change::apply` does, including multi-
    /// cursor changes and text where chars ≠ bytes ≠ UTF-16 units.
    #[test]
    fn apply_local_matches_change_apply() {
        for (initial, edits) in [
            ("hello world", vec![Edit::insert(5, ",")]),
            (
                "abc",
                vec![
                    Edit::insert(0, "x"),
                    Edit::insert(1, "yy"),
                    Edit::insert(3, "z"),
                ],
            ),
            (
                "one two three",
                vec![
                    Edit::replace(0..3, "1"),
                    Edit::delete(4..7),
                    Edit::replace(8..13, "33333"),
                ],
            ),
            (
                "héllo 🦀 wörld",
                vec![Edit::replace(1..2, "é🦀é"), Edit::delete(6..8)],
            ),
            // Touching edits (one ends exactly where the next begins) and two
            // insertions at the same offset — both legal (non-overlapping) and
            // the running-delta walk must keep them aligned.
            (
                "abcdef",
                vec![Edit::replace(0..3, "XYZ"), Edit::replace(3..6, "W")],
            ),
            ("abc", vec![Edit::insert(1, "1"), Edit::insert(1, "2")]),
        ] {
            let mut buffer = Buffer::from_text(initial);
            let mut doc = CollabDoc::create(initial);
            let change = Change::apply(&mut buffer, edits);
            doc.apply_local(&change).unwrap();
            assert_eq!(doc.text(), buffer.text(), "diverged from {initial:?}");
            doc.assert_mirror_intact();
        }
    }

    #[test]
    fn undo_applied_as_a_change_round_trips() {
        let mut buffer = Buffer::from_text("hello world");
        let mut doc = CollabDoc::create("hello world");
        let change = Change::apply(&mut buffer, vec![Edit::replace(0..5, "goodbye")]);
        doc.apply_local(&change).unwrap();

        // The model captures an undo as a fresh Change built from the inverse
        // edits — reverting is just another edit to a shared document — so the
        // doc must track it like any other local change.
        let undo = Change::apply(&mut buffer, change.inverse().to_vec());
        doc.apply_local(&undo).unwrap();
        assert_eq!(doc.text(), "hello world");
        assert_eq!(buffer.text(), "hello world");
        doc.assert_mirror_intact();
    }

    #[test]
    fn local_edit_before_first_sync_is_refused() {
        let mut buffer = Buffer::from_text("");
        let mut doc = CollabDoc::join();
        let change = Change::apply(&mut buffer, vec![Edit::insert(0, "too eager")]);
        assert!(matches!(
            doc.apply_local(&change),
            Err(CollabError::NotSynced)
        ));
    }

    #[test]
    fn is_synced_reflects_content_presence() {
        assert!(
            CollabDoc::create("x").is_synced(),
            "creator is synced at once"
        );
        assert!(CollabDoc::create("").is_synced(), "even an empty creator");
        assert!(
            !CollabDoc::join().is_synced(),
            "a fresh joiner is not synced"
        );
        let mut seed = CollabDoc::create("x");
        assert!(
            CollabDoc::load(&seed.save()).unwrap().is_synced(),
            "a loaded document is synced"
        );
        let mut empty = CollabDoc::create("");
        assert!(
            CollabDoc::load(&empty.save()).unwrap().is_synced(),
            "including one loaded from an empty snapshot"
        );
    }

    #[test]
    fn empty_change_is_a_harmless_readiness_probe_before_sync() {
        // NotSynced is returned *before* the doc is touched, so an empty Change
        // probes readiness without mutating anything — but is_synced() is the
        // intended, side-effect-free way to ask.
        let mut doc = CollabDoc::join();
        let empty = Change::apply(&mut Buffer::from_text(""), vec![]);
        assert!(matches!(
            doc.apply_local(&empty),
            Err(CollabError::NotSynced)
        ));
        assert_eq!(doc.text(), "");
    }

    #[test]
    fn empty_change_on_a_synced_doc_is_a_noop() {
        let mut doc = CollabDoc::create("unchanged");
        let empty = Change::apply(&mut Buffer::from_text("unchanged"), vec![]);
        doc.apply_local(&empty).unwrap();
        assert_eq!(doc.text(), "unchanged");
        doc.assert_mirror_intact();
    }

    #[test]
    fn a_change_reaching_past_the_shared_text_is_rejected_whole() {
        // The model and the document have disagreed about the contents — an
        // integration bug. What matters is that the document does not end up
        // half-edited with a *mirror* describing text that no longer exists:
        // the mirror is what every later offset conversion is computed against,
        // so a stale one silently corrupts every splice that follows it.
        let mut doc = CollabDoc::create("abc");
        let mut long = Buffer::from_text("abcdefgh");
        let change = Change::apply(&mut long, vec![Edit::insert(0, "!"), Edit::delete(5..8)]);

        let err = doc.apply_local(&change).unwrap_err();
        // `len` is the length as of the failing edit — the insertion before it
        // has already been accounted for by the running walk, exactly as it
        // would have been had the change been applied.
        assert!(
            matches!(
                err,
                CollabError::OutOfRange {
                    pos: 5,
                    del: 3,
                    len: 4
                }
            ),
            "got {err:?}"
        );
        assert_eq!(doc.text(), "abc", "the first edit must not have landed");
        assert_eq!(doc.len_chars(), 3);
        doc.assert_mirror_intact();

        // And the document still works: the mirror was never spoiled.
        let mut buffer = Buffer::from_text("abc");
        let ok = Change::apply(&mut buffer, vec![Edit::insert(3, "d")]);
        doc.apply_local(&ok).unwrap();
        assert_eq!(doc.text(), "abcd");
        doc.assert_mirror_intact();
    }

    #[test]
    fn len_chars_tracks_the_shared_text() {
        let mut doc = CollabDoc::create("héllo 🦀");
        assert_eq!(
            doc.len_chars(),
            7,
            "chars, not bytes (11) or UTF-16 units (8)"
        );
        assert_eq!(CollabDoc::join().len_chars(), 0);

        let mut buffer = Buffer::from_text("héllo 🦀");
        let change = Change::apply(&mut buffer, vec![Edit::delete(0..6)]);
        doc.apply_local(&change).unwrap();
        assert_eq!(doc.len_chars(), 1);
        doc.assert_mirror_intact();
    }

    #[test]
    fn snapshot_round_trip_preserves_text() {
        let mut doc = CollabDoc::create("héllo 🦀");
        let loaded = CollabDoc::load(&doc.save()).unwrap();
        assert_eq!(loaded.text(), "héllo 🦀");
        loaded.assert_mirror_intact();
    }

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
