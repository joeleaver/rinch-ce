//! The collaborative document and its per-peer sync sessions.
//!
//! [`CollabDoc`] is an automerge mirror of the editor's buffer: local edits
//! flow in as [`Change`]s ([`CollabDoc::apply_local`]), remote edits flow out
//! as [`Splice`]s ([`CollabDoc::receive_message`]), and everything between two
//! documents travels as opaque sync-protocol bytes the embedder transports.
//!
//! # The pump rule
//!
//! The sync protocol is a per-peer conversation. After **any** local change and
//! after **any** received message, call [`CollabDoc::generate_message`] for
//! **every** peer, delivering what comes out, until a full round over all peers
//! returns `None` — that is quiescence. Changes are content-addressed, so a
//! change reaching a peer twice (say, relayed through two different peers)
//! converges harmlessly.
//!
//! # Framing
//!
//! A sync message is a length-delimited unit, not a stream: the transport must
//! preserve message boundaries. WebSocket binary frames give this for free; a
//! raw TCP stream needs a length prefix. Each peer's messages must arrive
//! reliably and in order — after a transport reconnect, start a fresh
//! [`SyncSession`] (both sides), never resume the old one.

use automerge::sync::{self, SyncDoc};
use automerge::transaction::Transactable;
use automerge::{
    AutoCommit, AutomergeError, ObjId, ObjType, PatchAction, PatchLog, ROOT, ReadDoc, Value,
};
use rinch_code_editor_buffer::Change;

/// A single contiguous text replacement in char (Unicode scalar) offsets:
/// delete `del` chars at `pos`, then insert `insert` there.
///
/// A batch of splices from [`CollabDoc::receive_message`] is **sequential**:
/// each splice's `pos` is in the coordinates produced by the splices before
/// it — unlike the [`Edit`](rinch_code_editor_buffer::Edit)s inside a
/// [`Change`], which all share pre-change coordinates. Apply them in order,
/// one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub pos: usize,
    pub del: usize,
    pub insert: String,
}

/// The sync-protocol state for one remote peer.
///
/// Hold one per connection, next to that connection's transport handle. A
/// session assumes a reliable in-order byte stream underneath; when the
/// transport drops, throw the session away and start a new one on reconnect —
/// the protocol re-negotiates what the peer is missing from scratch.
#[derive(Debug, Default)]
pub struct SyncSession {
    state: sync::State,
}

impl SyncSession {
    pub fn new() -> Self {
        Self::default()
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
///   connection, never to crash. A sync message's envelope is validated up
///   front (`BadMessage`), but its change payload is opaque until applied, so a
///   corrupt or conflicting change surfaces later as `BadPeerData` — same
///   fault, same recovery, just a different point of detection.
/// - [`BadSnapshot`](Self::BadSnapshot) and
///   [`SnapshotMissingContent`](Self::SnapshotMissingContent) blame the
///   **bytes handed to [`CollabDoc::load`]** — wrong file, wrong document.
/// - [`NotSynced`](Self::NotSynced) blames **call ordering** — an edit before
///   the content exists; hold editing until [`CollabDoc::is_synced`].
/// - [`Internal`](Self::Internal) blames **this integration** — the model and
///   the document disagree about the text (an out-of-range local splice). It is
///   reachable only from [`CollabDoc::apply_local`], never from network input,
///   and it *is* a bug: fail fast.
#[derive(Debug)]
#[non_exhaustive]
pub enum CollabError {
    /// The bytes handed to [`CollabDoc::receive_message`] do not decode as a
    /// sync message — the envelope itself is malformed. Untrusted input; drop
    /// the connection, do not crash.
    BadMessage(sync::ReadMessageError),
    /// A sync message decoded, but applying the change payload it carried
    /// failed — a corrupt change chunk, or a `(actor, seq)` a hostile or buggy
    /// peer already used. `sync::Message::decode` validates only the envelope;
    /// the changes are opaque byte blobs until applied, so this is where bad
    /// *payload* (as opposed to a bad envelope) is caught. Same fault as
    /// [`BadMessage`](Self::BadMessage) — untrusted input — so the recovery is
    /// the same: drop the connection, do not crash. The document is left
    /// untouched (the failing change is rejected before it lands), so the peer
    /// may reconnect with a fresh [`SyncSession`].
    BadPeerData(AutomergeError),
    /// The bytes handed to [`CollabDoc::load`] do not decode as an automerge
    /// document.
    BadSnapshot(AutomergeError),
    /// A snapshot decoded fine but holds no `Text` at `ROOT["content"]` — it
    /// is some other automerge document, not one of ours.
    SnapshotMissingContent,
    /// A local edit arrived before the shared text exists. A document created
    /// with [`CollabDoc::join`] is read-only until the first sync delivers the
    /// content; the embedder should hold editing until [`CollabDoc::is_synced`]
    /// returns `true`.
    NotSynced,
    /// A local splice from [`CollabDoc::apply_local`] fell outside the shared
    /// text — the model and the document have disagreed about its contents.
    /// This is an integration bug, not a recoverable condition, and (unlike the
    /// peer-fault variants) is never reachable from network input.
    Internal(AutomergeError),
}

impl std::fmt::Display for CollabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMessage(e) => write!(f, "sync message envelope failed to decode: {e}"),
            Self::BadPeerData(e) => write!(f, "peer sent a change that could not be applied: {e}"),
            Self::BadSnapshot(e) => write!(f, "snapshot failed to load: {e}"),
            Self::SnapshotMissingContent => {
                write!(f, "snapshot has no Text at ROOT[\"content\"]")
            }
            Self::NotSynced => write!(f, "local edit before the first sync delivered the content"),
            Self::Internal(e) => write!(f, "internal automerge operation failed: {e}"),
        }
    }
}

impl std::error::Error for CollabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadMessage(e) => Some(e),
            Self::BadPeerData(e) | Self::BadSnapshot(e) | Self::Internal(e) => Some(e),
            Self::SnapshotMissingContent | Self::NotSynced => None,
        }
    }
}

/// An automerge document mirroring one editor buffer, as a `Text` object at
/// `ROOT["content"]`.
///
/// # Create, join, or load — never create twice
///
/// Exactly **one** peer per session calls [`CollabDoc::create`]; everyone else
/// calls [`CollabDoc::join`] (content arrives via sync) or [`CollabDoc::load`]
/// (content arrives as a snapshot). If two peers each `create`, they make two
/// *different* `Text` objects for the same key; the merge deterministically
/// keeps one and shadows the other, silently discarding the loser's edits.
/// `receive_message` detects the shadowed side and repairs it with a full
/// replace, but the discarded text is gone — this is a misuse to avoid, not a
/// mode to rely on.
#[derive(Debug)]
pub struct CollabDoc {
    doc: AutoCommit,
    /// The `Text` at `ROOT["content"]`, or `None` on a joined document that
    /// has not yet received it. Re-resolved after every receive: a change in
    /// the winning object's identity (the join bootstrap, or the misuse-only
    /// create/create conflict) is one of the triggers for a full-replace
    /// resync.
    text: Option<ObjId>,
    /// Chars the embedder's model currently mirrors — the length of the text
    /// as of the last splice batch we emitted (or local change we applied). A
    /// full-replace resync must delete exactly this many chars, which the
    /// post-receive document length can no longer tell us.
    projected_len: usize,
}

impl CollabDoc {
    /// Start a session as its originator, seeding the shared text.
    pub fn create(initial: &str) -> Self {
        let mut doc = AutoCommit::new();
        let text = doc
            .put_object(ROOT, "content", ObjType::Text)
            .expect("put_object at ROOT cannot fail");
        doc.splice_text(&text, 0, 0, initial)
            .expect("splice into a fresh Text cannot fail");
        Self {
            doc,
            text: Some(text),
            projected_len: initial.chars().count(),
        }
    }

    /// Join a session someone else created. The document is empty — and
    /// read-only, see [`CollabError::NotSynced`] — until the first sync
    /// delivers the content, at which point [`is_synced`](Self::is_synced)
    /// flips to `true` and [`receive_message`](Self::receive_message) emits one
    /// full-length [`Splice`] (an *empty* shared document emits none, so gate
    /// editing on `is_synced`, not on seeing that splice).
    pub fn join() -> Self {
        Self {
            doc: AutoCommit::new(),
            text: None,
            projected_len: 0,
        }
    }

    /// Join from a snapshot previously produced by [`CollabDoc::save`].
    pub fn load(bytes: &[u8]) -> Result<Self, CollabError> {
        let doc = AutoCommit::load(bytes).map_err(CollabError::BadSnapshot)?;
        let text = content_text(&doc).ok_or(CollabError::SnapshotMissingContent)?;
        let projected_len = doc.length(&text);
        Ok(Self {
            doc,
            text: Some(text),
            projected_len,
        })
    }

    /// A compacted snapshot of the whole document, for [`CollabDoc::load`].
    pub fn save(&mut self) -> Vec<u8> {
        self.doc.save()
    }

    /// The current shared text. Empty on a joined document before first sync.
    pub fn text(&self) -> String {
        match &self.text {
            Some(text) => self
                .doc
                .text(text)
                .expect("content Text is a live object in this doc"),
            None => String::new(),
        }
    }

    /// Whether the shared text exists yet — equivalently, whether
    /// [`apply_local`](Self::apply_local) will be accepted rather than
    /// returning [`CollabError::NotSynced`].
    ///
    /// A [`create`](Self::create)d or [`load`](Self::load)ed document is synced
    /// from the start; a [`join`](Self::join)ed one becomes synced when the
    /// first sync delivers the content. **Poll this** after draining each
    /// [`receive_message`](Self::receive_message) to learn when a joiner may
    /// start editing: an empty shared document delivers no bootstrap
    /// [`Splice`] to announce itself, so watching the splice stream alone would
    /// leave a joiner of an empty document read-only forever.
    pub fn is_synced(&self) -> bool {
        self.text.is_some()
    }

    /// Record a local edit the model has already applied to its buffer.
    ///
    /// Every local mutation must come through here — typing, paste, undo,
    /// redo — or the document and the buffer silently diverge. Follow up with
    /// the pump rule (see module docs) to get the change to the peers.
    pub fn apply_local(&mut self, change: &Change) -> Result<(), CollabError> {
        let text = self.text.clone().ok_or(CollabError::NotSynced)?;
        // The same running-delta walk as `Change::apply`: the edits are
        // sorted, non-overlapping, and in pre-change coordinates, while
        // `splice_text` sees the document mutate under it as we go.
        let mut delta: isize = 0;
        for edit in change.edits() {
            let start = (edit.range.start as isize + delta) as usize;
            let removed = edit.range.end - edit.range.start;
            let inserted = edit.text.chars().count();
            self.doc
                .splice_text(&text, start, removed as isize, &edit.text)
                .map_err(CollabError::Internal)?;
            delta += inserted as isize - removed as isize;
        }
        self.projected_len = (self.projected_len as isize + delta) as usize;
        debug_assert_eq!(
            self.projected_len,
            self.doc.length(&text),
            "model and document disagree about the text length"
        );
        Ok(())
    }

    /// The next sync message for `peer`, if the conversation is not quiescent.
    ///
    /// See the module docs for the pump rule: call this for every peer after
    /// any local change or receive, until all peers return `None`.
    pub fn generate_message(&mut self, peer: &mut SyncSession) -> Option<Vec<u8>> {
        self.doc
            .sync()
            .generate_sync_message(&mut peer.state)
            .map(sync::Message::encode)
    }

    /// Ingest one sync message from `peer`; the returned splices replay every
    /// remote edit it carried onto the model's buffer (sequentially — see
    /// [`Splice`]).
    ///
    /// A joiner's first content-bearing batch is a single splice inserting the
    /// whole text (unless that text is empty — see
    /// [`is_synced`](Self::is_synced)). Remember the pump rule: receiving
    /// usually obligates responses, to this peer and every other.
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
        let message = sync::Message::decode(bytes).map_err(CollabError::BadMessage)?;
        let mut log = PatchLog::active();
        // Only the envelope was validated at decode; the change payload is
        // opaque until applied here, so a corrupt or conflicting change from an
        // untrusted peer surfaces now — as a broken connection, not a bug.
        self.doc
            .sync()
            .receive_sync_message_log_patches(&mut peer.state, message, &mut log)
            .map_err(CollabError::BadPeerData)?;
        let patches = self.doc.make_patches(&mut log);

        let old_len = self.projected_len;
        let winner = content_text(&self.doc);
        let splices = if winner != self.text {
            // The content object appeared or changed identity — the normal join
            // bootstrap (None → Some) or the create/create misuse. The text the
            // model mirrors is no longer the document's, so per-patch replay is
            // meaningless: resync the whole thing.
            self.text = winner;
            self.full_replace(old_len)
        } else if let Some(text) = self.text.clone() {
            // Replay the recognized text edits in order. A patch on the text
            // object we do NOT recognize — a block marker from a peer speaking
            // richer automerge than this plain-text mirror, say — makes the
            // incremental replay untrustworthy. Rather than panic (as a
            // debug_assert would) or silently drop it and diverge (as a release
            // build would), fall back to a full replace so the model still
            // faithfully reflects `doc.text()`. Normal collaborative text never
            // reaches this fallback: scalar and object inserts into a Text
            // arrive from automerge as `SpliceText` of U+FFFC, so only genuine
            // rich-text structure (which we don't model) trips it.
            let mut out = Vec::new();
            let mut surprised = false;
            for patch in patches.iter().filter(|patch| patch.obj == text) {
                match classify(&patch.action) {
                    TextPatch::Edit(splice) => out.push(splice),
                    TextPatch::NoOp => {}
                    TextPatch::Unknown => surprised = true,
                }
            }
            // The recognized edits must reproduce the document's new length; if
            // a surprise appeared or the arithmetic disagrees, they don't — so
            // resync instead of emitting splices that would desync the model.
            if surprised || fold_len(old_len, &out) != self.doc.length(&text) {
                self.full_replace(old_len)
            } else {
                out
            }
        } else {
            // No content object anywhere yet (two joiners with no creator).
            Vec::new()
        };

        // Having applied `splices`, the model now holds exactly `doc.text()`.
        self.projected_len = self.text.as_ref().map_or(0, |text| self.doc.length(text));
        debug_assert_eq!(
            fold_len(old_len, &splices),
            self.projected_len,
            "emitted splices do not reproduce the document length"
        );
        Ok(splices)
    }

    /// The single splice that rewrites the model's whole text to the document's
    /// current text — the resync used for bootstrap and for any surprise that
    /// makes incremental replay untrustworthy. `old_len` is the model's char
    /// count before this splice, i.e. exactly what must be deleted.
    fn full_replace(&self, old_len: usize) -> Vec<Splice> {
        let insert = self.text();
        if old_len == 0 && insert.is_empty() {
            Vec::new()
        } else {
            vec![Splice {
                pos: 0,
                del: old_len,
                insert,
            }]
        }
    }
}

/// The winning `Text` at `ROOT["content"]`, if one exists.
fn content_text(doc: &AutoCommit) -> Option<ObjId> {
    match doc.get(ROOT, "content") {
        Ok(Some((Value::Object(ObjType::Text), id))) => Some(id),
        _ => None,
    }
}

/// A text-object patch, sorted into what a plain-text mirror can do with it.
enum TextPatch {
    /// A recognized text edit to replay on the model.
    Edit(Splice),
    /// A patch that targets the text but moves no characters (a mark) — ignore.
    NoOp,
    /// A patch beyond a plain-text projection (e.g. a block marker inserted by
    /// a peer speaking richer automerge). The caller resyncs rather than guess.
    Unknown,
}

/// Sort a text-object patch into a [`TextPatch`]. Character offsets and lengths
/// are already in Unicode scalar values (the crate's invariant), so no
/// conversion is needed.
fn classify(action: &PatchAction) -> TextPatch {
    match action {
        PatchAction::SpliceText { index, value, .. } => TextPatch::Edit(Splice {
            pos: *index,
            del: 0,
            insert: value.make_string(),
        }),
        PatchAction::DeleteSeq { index, length } => TextPatch::Edit(Splice {
            pos: *index,
            del: *length,
            insert: String::new(),
        }),
        // Marks annotate a span without moving text; nothing for a plain-text
        // mirror to do.
        PatchAction::Mark { .. } => TextPatch::NoOp,
        // Anything else on a Text object is beyond this projection. Signal a
        // resync rather than corrupt the model by dropping it.
        _ => TextPatch::Unknown,
    }
}

/// Fold a batch of sequential splices into the char count they produce from a
/// text of length `start`.
fn fold_len(start: usize, splices: &[Splice]) -> usize {
    let mut len = start as isize;
    for splice in splices {
        len += splice.insert.chars().count() as isize - splice.del as isize;
    }
    len.max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinch_code_editor_buffer::{Buffer, Edit};

    /// The load-bearing equivalence: `apply_local`'s running-delta walk lands
    /// every edit at the same place `Change::apply` does, including multi-
    /// cursor changes and text where chars ≠ bytes.
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
        }
    }

    #[test]
    fn undo_applied_as_a_change_round_trips() {
        let mut buffer = Buffer::from_text("hello world");
        let mut doc = CollabDoc::create("hello world");
        let change = Change::apply(&mut buffer, vec![Edit::replace(0..5, "goodbye")]);
        doc.apply_local(&change).unwrap();

        // C2 will capture an undo as a fresh Change built from the inverse
        // edits; the doc must track it like any other local change.
        let undo = Change::apply(&mut buffer, change.inverse().to_vec());
        doc.apply_local(&undo).unwrap();
        assert_eq!(doc.text(), "hello world");
        assert_eq!(buffer.text(), "hello world");
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
    }

    #[test]
    fn snapshot_round_trip_preserves_text() {
        let mut doc = CollabDoc::create("héllo 🦀");
        let loaded = CollabDoc::load(&doc.save()).unwrap();
        assert_eq!(loaded.text(), "héllo 🦀");
    }

    #[test]
    fn loading_a_foreign_document_is_refused() {
        use automerge::transaction::Transactable;
        let mut foreign = automerge::AutoCommit::new();
        foreign
            .put(automerge::ROOT, "content", "a string, not a Text")
            .unwrap();
        let bytes = foreign.save();
        assert!(matches!(
            CollabDoc::load(&bytes),
            Err(CollabError::SnapshotMissingContent)
        ));
        assert!(matches!(
            CollabDoc::load(b"not an automerge document"),
            Err(CollabError::BadSnapshot(_))
        ));
    }
}
