//! Robustness against a peer that is not us.
//!
//! The engine models far more than the plain text this crate projects: a peer
//! built on the same CRDT can format a range or embed a value, and a hostile one
//! can send rubbish. None of that may panic, silently desynchronize the model,
//! or be mistaken for corruption when it is merely richer than we render.
//!
//! These drive a raw yrs document as the foreign peer, so they exercise the real
//! wire format rather than a mock of it. They replace the automerge-era versions
//! (block markers, corrupt change chunks, garbage envelopes) test for test; the
//! hazard is the same, only the vocabulary of "richer than plain text" changed.

use rinch_code_editor_buffer::{Buffer, Change};
use rinch_code_editor_collab::{CollabDoc, CollabError, Splice, SyncSession};
use yrs::sync::{Message, SyncMessage};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Any, Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, Transact, Update};

/// A peer speaking the same protocol directly, with none of our restraint.
struct Foreign {
    doc: Doc,
    text: yrs::TextRef,
}

impl Foreign {
    fn new() -> Self {
        let options = Options {
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        };
        let doc = Doc::with_options(options);
        let text = doc.get_or_insert_text("content");
        Self { doc, text }
    }

    fn state(&self) -> StateVector {
        self.doc.transact().state_vector()
    }

    fn sync_step1(&self) -> Vec<u8> {
        Message::Sync(SyncMessage::SyncStep1(self.state())).encode_v1()
    }

    /// Everything we hold that a peer at `sv` does not, as an Update message.
    fn update_since(&self, sv: &StateVector) -> Vec<u8> {
        let update = self.doc.transact().encode_diff_v1(sv);
        Message::Sync(SyncMessage::Update(update)).encode_v1()
    }

    fn absorb(&mut self, bytes: &[u8]) {
        if let Ok(Message::Sync(msg)) = Message::decode_v1(bytes) {
            let payload = match msg {
                SyncMessage::SyncStep2(u) | SyncMessage::Update(u) => u,
                SyncMessage::SyncStep1(_) => return,
            };
            if let Ok(update) = Update::decode_v1(&payload) {
                let mut txn = self.doc.transact_mut();
                let _ = txn.apply_update(update);
            }
        }
    }

    fn text(&self) -> String {
        self.text.get_string(&self.doc.transact())
    }
}

/// Bring a foreign peer up to date with ours.
fn introduce(ours: &mut CollabDoc, session: &mut SyncSession, theirs: &mut Foreign) {
    let step1 = theirs.sync_step1();
    ours.receive_message(session, &step1).unwrap();
    while let Some(msg) = ours.generate_message(session) {
        theirs.absorb(&msg);
    }
}

/// Replaying the emitted splices onto the model must reproduce the document —
/// the contract every remote batch has to keep, however it was produced.
#[track_caller]
fn assert_replayable(start: &str, splices: &[Splice], doc: &CollabDoc) {
    let mut model = Buffer::from_text(start);
    for splice in splices {
        let len = model.len_chars();
        Change::apply(&mut model, vec![splice.to_edit(len)]);
    }
    assert_eq!(
        model.text(),
        doc.text(),
        "replaying the emitted splices diverged from the document"
    );
}

#[test]
fn a_peer_formatting_a_range_keeps_the_projection_honest() {
    // Formatting changes the text in a way a plain-text projection cannot
    // represent. Guessing would desynchronize the model silently; the crate
    // resynchronizes instead, and the model still ends up holding exactly what
    // the document holds.
    let mut ours = CollabDoc::create("hello world");
    let mut session = SyncSession::new();
    let mut theirs = Foreign::new();
    introduce(&mut ours, &mut session, &mut theirs);
    assert_eq!(theirs.text(), "hello world");

    let before = theirs.state();
    {
        let mut attrs = Attrs::new();
        attrs.insert("bold".into(), Any::Bool(true));
        let mut txn = theirs.doc.transact_mut();
        theirs.text.format(&mut txn, 0, 5, attrs);
    }
    let msg = theirs.update_since(&before);
    let splices = ours.receive_message(&mut session, &msg).unwrap();

    assert_eq!(ours.text(), theirs.text(), "text must not drift");
    assert_replayable("hello world", &splices, &ours);
}

#[test]
fn plain_edits_after_a_formatted_range_still_converge() {
    // The resync must not leave the session wedged: ordinary typing from the
    // same peer afterwards has to keep flowing.
    let mut ours = CollabDoc::create("hello world");
    let mut session = SyncSession::new();
    let mut theirs = Foreign::new();
    introduce(&mut ours, &mut session, &mut theirs);

    let before = theirs.state();
    {
        let mut attrs = Attrs::new();
        attrs.insert("italic".into(), Any::Bool(true));
        let mut txn = theirs.doc.transact_mut();
        theirs.text.format(&mut txn, 6, 5, attrs);
    }
    let msg = theirs.update_since(&before);
    ours.receive_message(&mut session, &msg).unwrap();

    let before = theirs.state();
    {
        let mut txn = theirs.doc.transact_mut();
        theirs.text.insert(&mut txn, 11, "!");
    }
    let msg = theirs.update_since(&before);
    let splices = ours.receive_message(&mut session, &msg).unwrap();

    assert_eq!(ours.text(), theirs.text());
    assert_eq!(ours.text(), "hello world!");
    assert!(
        !splices.is_empty(),
        "an ordinary insertion after a resync must still be reported"
    );
}

#[test]
fn a_peer_embedding_a_value_does_not_desynchronize_the_model() {
    let mut ours = CollabDoc::create("abc");
    let mut session = SyncSession::new();
    let mut theirs = Foreign::new();
    introduce(&mut ours, &mut session, &mut theirs);

    let before = theirs.state();
    {
        let mut txn = theirs.doc.transact_mut();
        theirs
            .text
            .insert_embed(&mut txn, 1, Any::Map(Default::default()));
    }
    let msg = theirs.update_since(&before);
    let splices = ours.receive_message(&mut session, &msg).unwrap();

    assert_eq!(ours.text(), theirs.text());
    assert_replayable("abc", &splices, &ours);
}

#[test]
fn garbage_bytes_are_refused_without_touching_the_document() {
    let mut doc = CollabDoc::create("intact");
    let mut session = SyncSession::new();
    let err = doc
        .receive_message(&mut session, b"not a protocol message at all")
        .unwrap_err();
    // Either fault is the peer's, and either recovery is "drop the connection".
    // Which one it is depends on where the rubbish stops looking like a varint:
    // these bytes claim a payload longer than the frame, so the decoder runs out
    // before it produces anything and the frame comes back empty rather than
    // malformed.
    assert!(
        matches!(err, CollabError::BadMessage(_) | CollabError::EmptyFrame),
        "untrusted bytes must be a peer fault, not a crash: {err:?}"
    );
    assert_eq!(doc.text(), "intact");
}

#[test]
fn a_corrupt_update_payload_is_a_peer_fault() {
    // A well-formed envelope wrapped around rubbish: the envelope check passes
    // and the payload is opaque until applied, which is exactly where the
    // automerge-era version of this test found a mislabelled error.
    let mut doc = CollabDoc::create("hello");
    let mut session = SyncSession::new();
    let corrupt = Message::Sync(SyncMessage::Update(vec![0xff, 0xfe, 0xfd, 0x00])).encode_v1();
    let err = doc.receive_message(&mut session, &corrupt).unwrap_err();
    assert!(
        matches!(
            err,
            CollabError::BadMessage(_) | CollabError::BadPeerData(_)
        ),
        "a corrupt payload must blame the peer: {err:?}"
    );
    assert_eq!(doc.text(), "hello", "and leave the document untouched");
}

/// A frame may carry several messages back to back — yrs's own
/// `DefaultProtocol::start` packs two, and so does the Yjs client. Handling only
/// the first would drop the rest silently, which shows up as presence
/// occasionally not arriving rather than as anything that looks like a bug.
#[test]
fn several_messages_in_one_frame_are_all_handled() {
    use rinch_code_editor_collab::{Selection, User};

    let mut a = CollabDoc::create("hello world");
    a.set_local_user(User::new("Ada", "#30bced"));
    a.set_local_selection(Some(Selection::cursor(3)));
    let mut b = CollabDoc::join();
    let (mut sa, mut sb) = (SyncSession::new(), SyncSession::new());

    // Everything A wants to say, concatenated into a single frame.
    let mut packed = Vec::new();
    while let Some(message) = a.generate_message(&mut sa) {
        packed.extend_from_slice(&message);
    }
    assert!(
        packed.len() > 2,
        "the handshake should be more than one message"
    );

    b.receive_message(&mut sb, &packed).unwrap();

    // The sync step was handled: it obliges a step 2 in reply, and a reply owed
    // is drained ahead of B's own opening move.
    let reply = b.generate_message(&mut sb).expect("a reply is owed");
    assert!(
        matches!(
            Message::decode_v1(&reply),
            Ok(Message::Sync(SyncMessage::SyncStep2(_)))
        ),
        "the sync step inside the packed frame must have been answered"
    );

    // And the awareness that came *after* it in the same frame was not dropped —
    // the whole point of the loop.
    assert_eq!(
        b.peers()
            .into_iter()
            .map(|p| p.user.name)
            .collect::<Vec<_>>(),
        vec!["Ada".to_string()],
        "the awareness message after the sync step must not be dropped"
    );
}

/// A server refusing us must not look like silence. Swallowing the denial leaves
/// an embedder pumping a connection that will never carry anything, with nothing
/// to tell the user.
#[test]
fn an_auth_denial_is_surfaced() {
    let mut doc = CollabDoc::create("intact");
    let mut session = SyncSession::new();
    let denial = Message::Auth(Some("read-only token".to_string())).encode_v1();

    let err = doc.receive_message(&mut session, &denial).unwrap_err();
    assert!(
        matches!(&err, CollabError::Denied(reason) if reason.contains("read-only token")),
        "a refusal must carry its reason: {err:?}"
    );
    assert_eq!(doc.text(), "intact");

    // Permission *granted* is the absence of news, not an error.
    let granted = Message::Auth(None).encode_v1();
    assert!(doc.receive_message(&mut session, &granted).is_ok());
}
