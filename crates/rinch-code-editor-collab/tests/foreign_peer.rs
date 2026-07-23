//! Robustness against automerge peers that are *not* this crate.
//!
//! The collab core is a transport-agnostic mirror speaking the standard
//! automerge sync protocol, so a peer can be any automerge client — including
//! one that uses Text features this plain-text projection does not model, or
//! one sending corrupt or hostile bytes. None of that may panic the editor or
//! silently diverge the mirror: the contract is a faithful reflection of
//! `doc.text()`, a typed error, or nothing — never a crash, never corruption.
//!
//! These tests drive a raw [`automerge::AutoCommit`] against a [`CollabDoc`]
//! through real sync bytes, so they exercise exactly what a foreign client on
//! the wire would produce.

use automerge::sync::SyncDoc;
use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjType, ROOT, ReadDoc};
use rinch_code_editor_buffer::{Buffer, Change, Edit};
use rinch_code_editor_collab::{CollabDoc, CollabError, SyncSession};

/// The content Text object of a document, by the crate's own convention.
fn content(doc: &AutoCommit) -> automerge::ObjId {
    let (val, id) = doc.get(ROOT, "content").unwrap().unwrap();
    assert_eq!(val, automerge::Value::Object(ObjType::Text));
    id
}

/// A foreign automerge peer that shares one `CollabDoc`'s document, driven
/// through the real sync protocol so its edits reach the `CollabDoc` as wire
/// bytes — the way any non-crate automerge client would appear.
struct Foreign {
    doc: AutoCommit,
    state: automerge::sync::State,
}

impl Foreign {
    /// Fork a foreign peer from a collab doc's current snapshot; both then
    /// share the same content Text object identity.
    fn fork(collab: &mut CollabDoc) -> Self {
        Self {
            doc: AutoCommit::load(&collab.save()).unwrap(),
            state: automerge::sync::State::new(),
        }
    }

    fn text_id(&self) -> automerge::ObjId {
        content(&self.doc)
    }

    /// Sync this peer with a collab doc until quiescent, applying every emitted
    /// splice to `model` and asserting the model never diverges from the doc.
    fn sync(&mut self, collab: &mut CollabDoc, peer: &mut SyncSession, model: &mut Buffer) {
        for _ in 0..100 {
            let mut moved = false;
            if let Some(msg) = self.doc.sync().generate_sync_message(&mut self.state) {
                let splices = collab.receive_message(peer, &msg.encode()).unwrap();
                for splice in &splices {
                    Change::apply(
                        model,
                        vec![Edit::replace(
                            splice.pos..splice.pos + splice.del,
                            splice.insert.clone(),
                        )],
                    );
                }
                assert_eq!(
                    model.text(),
                    collab.text(),
                    "mirror diverged from the collab document after a foreign sync"
                );
                moved = true;
            }
            if let Some(bytes) = collab.generate_message(peer) {
                let msg = automerge::sync::Message::decode(&bytes).unwrap();
                self.doc
                    .sync()
                    .receive_sync_message(&mut self.state, msg)
                    .unwrap();
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("foreign sync did not reach quiescence");
    }
}

#[test]
fn foreign_block_marker_does_not_panic_and_mirror_stays_faithful() {
    // The defect this pins: a foreign peer calling the public `split_block`
    // inserts a Map block marker into the Text. That arrives as
    // PatchAction::Insert on the text object — which a naive matcher would
    // debug_assert-panic on (debug) or silently drop (release, desyncing the
    // mirror by the marker's one U+FFFC position). The mirror must instead
    // resync and keep reflecting doc.text() exactly.
    let mut collab = CollabDoc::create("hello world");
    let mut model = Buffer::from_text(&collab.text());
    let mut peer = SyncSession::new();

    let mut foreign = Foreign::fork(&mut collab);
    let text = foreign.text_id();
    foreign.doc.split_block(&text, 5).unwrap();
    // ...and a plain edit after the marker, to prove subsequent offsets are not
    // skewed by one against the model.
    foreign.doc.splice_text(&text, 7, 0, "X").unwrap();

    foreign.sync(&mut collab, &mut peer, &mut model);

    // The plain-text projection of a block marker is U+FFFC; the mirror holds
    // exactly what the document holds, no panic, no off-by-one.
    assert!(
        collab.text().contains('\u{fffc}'),
        "marker should be present"
    );
    assert!(collab.text().contains('X'));
    assert_eq!(model.text(), collab.text());
}

#[test]
fn foreign_edits_after_a_marker_keep_converging() {
    // After the resync, ordinary collaboration must continue to work: a foreign
    // insert past the marker and a local edit both land correctly.
    let mut collab = CollabDoc::create("abcdef");
    let mut model = Buffer::from_text(&collab.text());
    let mut peer = SyncSession::new();

    let mut foreign = Foreign::fork(&mut collab);
    let text = foreign.text_id();
    foreign.doc.split_block(&text, 3).unwrap();
    foreign.sync(&mut collab, &mut peer, &mut model);
    assert_eq!(model.text(), collab.text());

    // A local edit while the marker is present still round-trips to the peer.
    let change = Change::apply(&mut model, vec![Edit::insert(0, ">")]);
    collab.apply_local(&change).unwrap();
    foreign.sync(&mut collab, &mut peer, &mut model);

    // And a further foreign edit still converges.
    let text = foreign.text_id();
    let end = foreign.doc.length(&text);
    foreign.doc.splice_text(&text, end, 0, "!").unwrap();
    foreign.sync(&mut collab, &mut peer, &mut model);
    assert_eq!(model.text(), collab.text());
    assert!(collab.text().starts_with('>') && collab.text().ends_with('!'));
}

#[test]
fn corrupt_change_payload_is_bad_peer_data_not_a_crash() {
    // A structurally valid sync envelope whose change chunk is garbage decodes
    // fine (so it is NOT BadMessage) but fails when applied. A joiner's empty
    // document takes automerge's strict first-chunk-load path, so this surfaces
    // as an error rather than being silently tolerated — and it must be the
    // peer-fault BadPeerData, the crate's must-disconnect signal, never the
    // must-crash Internal variant.
    let mut creator = CollabDoc::create("shared");
    let mut joiner = CollabDoc::join();
    let mut c_to_j = SyncSession::new();
    let mut j_to_c = SyncSession::new();

    // Let the creator produce a real, content-bearing sync message for the
    // joiner (drive the handshake far enough that changes are sent).
    let mut real = None;
    for _ in 0..10 {
        if let Some(bytes) = creator.generate_message(&mut c_to_j) {
            let msg = automerge::sync::Message::decode(&bytes).unwrap();
            if !msg.changes.is_empty() {
                real = Some(msg);
                break;
            }
            // Feed the joiner's reply back so the handshake advances.
            let splices = joiner.receive_message(&mut j_to_c, &bytes).unwrap();
            assert!(splices.is_empty());
            if let Some(reply) = joiner.generate_message(&mut j_to_c) {
                creator
                    .receive_message(&mut c_to_j, &reply)
                    .expect("creator handles the joiner's reply");
            }
        } else {
            break;
        }
    }
    let mut msg = real.expect("creator should eventually send a change-bearing message");

    // Corrupt the change payload while keeping the envelope well-formed.
    msg.changes = vec![vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01, 0x02, 0x03]].into();
    let corrupt = msg.encode();
    // It still decodes as a sync message — the envelope is valid.
    assert!(automerge::sync::Message::decode(&corrupt).is_ok());

    let err = joiner
        .receive_message(&mut j_to_c, &corrupt)
        .expect_err("a garbage change payload must be rejected");
    assert!(
        matches!(err, CollabError::BadPeerData(_)),
        "untrusted payload must be BadPeerData (drop the connection), got {err:?}"
    );
}

#[test]
fn garbage_envelope_is_bad_message() {
    // Bytes that are not even a valid sync envelope are the other untrusted-
    // input variant. Both are recoverable-by-disconnect, never a crash.
    let mut joiner = CollabDoc::join();
    let mut peer = SyncSession::new();
    let err = joiner
        .receive_message(&mut peer, b"not a sync message at all")
        .expect_err("garbage bytes must be rejected");
    assert!(matches!(err, CollabError::BadMessage(_)), "got {err:?}");
}
