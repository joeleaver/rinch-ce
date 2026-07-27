//! Two real editors, one shared document.
//!
//! `convergence.rs` proves the document layer converges with a plain `Buffer`
//! standing in for the editor. This file removes the stand-in: two
//! [`EditorModel`]s, each with its own selections, undo history and
//! highlighter, exchanging nothing but sync bytes. It is the C2 contract
//! end to end — local changes out through `take_local_changes`, remote splices
//! in through `apply_remote` — and the shape the two-pane demo will take.

use rinch_code_editor_collab::{CollabDoc, CollabError, SyncSession};
use rinch_code_editor_model::{EditorModel, Highlighter, PlainHighlighter, Theme};
use std::collections::HashMap;

fn editor(text: &str) -> EditorModel {
    let theme = Theme::builder().build();
    let hl: Box<dyn Highlighter> = Box::new(PlainHighlighter::new(theme));
    let mut model = EditorModel::from_text(text, hl);
    model.record_local_changes(true);
    model
}

struct Peer {
    model: EditorModel,
    doc: CollabDoc,
    sessions: HashMap<usize, SyncSession>,
}

struct Net {
    peers: Vec<Peer>,
    links: Vec<(usize, usize)>,
}

impl Net {
    /// A session already in flight: peer 0 created it holding `initial`, peer 1
    /// joined with an empty editor and has been brought up to date.
    fn pair(initial: &str) -> Self {
        let mut net = Self {
            peers: Vec::new(),
            links: Vec::new(),
        };
        net.add(editor(initial), CollabDoc::create(initial));
        net.add(editor(""), CollabDoc::join());
        net.link(0, 1);
        net.pump();
        net
    }

    fn add(&mut self, model: EditorModel, doc: CollabDoc) -> usize {
        self.peers.push(Peer {
            model,
            doc,
            sessions: HashMap::new(),
        });
        self.peers.len() - 1
    }

    fn link(&mut self, a: usize, b: usize) {
        self.peers[a].sessions.insert(b, SyncSession::new());
        self.peers[b].sessions.insert(a, SyncSession::new());
        self.links.push((a, b));
    }

    fn model(&mut self, i: usize) -> &mut EditorModel {
        &mut self.peers[i].model
    }

    fn text(&self, i: usize) -> String {
        self.peers[i].model.text()
    }

    fn carets(&self, i: usize) -> Vec<usize> {
        self.peers[i]
            .model
            .selections()
            .iter()
            .map(|s| s.head)
            .collect()
    }

    /// Hand everything the editor has done since last time to its document.
    /// This is the step the pump rule and `apply_remote`'s ordering contract
    /// both hang on: it must happen before any message is generated *or*
    /// received.
    fn publish(&mut self, i: usize) {
        let peer = &mut self.peers[i];
        for change in peer.model.take_local_changes() {
            peer.doc.apply_local(&change).unwrap();
        }
    }

    /// One synchronous exchange: everybody publishes, then every peer generates
    /// toward every linked peer *before* anything is delivered — so messages
    /// cross on the wire, the adversarial case for a sync protocol.
    fn pass(&mut self) -> bool {
        for i in 0..self.peers.len() {
            self.publish(i);
        }
        let mut deliveries = Vec::new();
        for (a, b) in self.links.clone() {
            for (from, to) in [(a, b), (b, a)] {
                let peer = &mut self.peers[from];
                let session = peer.sessions.get_mut(&to).unwrap();
                if let Some(bytes) = peer.doc.generate_message(session) {
                    deliveries.push((to, from, bytes));
                }
            }
        }
        let any = !deliveries.is_empty();
        for (to, from, bytes) in deliveries {
            let peer = &mut self.peers[to];
            let session = peer.sessions.get_mut(&from).unwrap();
            let splices = peer.doc.receive_message(session, &bytes).unwrap();
            peer.model.apply_remote(&splices);
            assert_eq!(
                peer.model.text(),
                peer.doc.text(),
                "peer {to}: the editor and its document disagree after a receive"
            );
        }
        any
    }

    fn pump(&mut self) {
        for _ in 0..1000 {
            if !self.pass() {
                return;
            }
        }
        panic!("sync did not reach quiescence within 1000 passes");
    }

    #[track_caller]
    fn assert_converged(&self) {
        let reference = self.peers[0].doc.text();
        for (i, peer) in self.peers.iter().enumerate() {
            assert_eq!(peer.doc.text(), reference, "peer {i}'s document diverged");
            assert_eq!(peer.model.text(), reference, "peer {i}'s editor diverged");
        }
    }
}

#[test]
fn a_joiner_receives_the_whole_document() {
    let net = Net::pair("fn main() {\n    println!(\"hi\");\n}\n");
    net.assert_converged();
    assert_eq!(net.peers[1].model.len_lines(), 4);
    assert_eq!(
        net.carets(1),
        vec![0],
        "the bootstrap is an insertion into an empty editor: the caret stays"
    );
}

#[test]
fn a_joiner_of_an_empty_document_is_synced_without_any_splice() {
    // The readiness signal C1 added: an empty shared document delivers no
    // bootstrap splice at all, so a joiner watching the splice stream alone
    // would wait forever.
    let net = Net::pair("");
    assert!(net.peers[1].doc.is_synced());
    net.assert_converged();
}

#[test]
fn editing_before_the_first_sync_is_refused() {
    // What the demo's input gate is for: a joined editor is read-only until
    // its document has the content.
    let mut model = editor("");
    let mut doc = CollabDoc::join();
    assert!(!doc.is_synced());
    model.insert("too eager");
    let change = model.take_local_changes().remove(0);
    assert!(matches!(
        doc.apply_local(&change),
        Err(CollabError::NotSynced)
    ));
}

#[test]
fn typing_in_one_editor_appears_in_the_other() {
    let mut net = Net::pair("hello");
    net.model(0).set_cursor(5);
    net.model(0).insert(" world");
    net.pump();
    assert_eq!(net.text(1), "hello world");
    net.assert_converged();
}

#[test]
fn a_remote_edit_carries_the_other_editors_caret() {
    let mut net = Net::pair("hello");
    net.model(1).set_cursor(5);
    net.model(0).set_cursor(0);
    net.model(0).insert("oh ");
    net.pump();
    assert_eq!(net.text(1), "oh hello");
    assert_eq!(net.carets(1), vec![8], "peer 1's caret rode the insertion");
    assert_eq!(net.carets(0), vec![3], "peer 0's caret is where it typed");
}

#[test]
fn typing_at_the_same_spot_from_both_sides_converges() {
    let mut net = Net::pair("hello");
    net.model(0).set_cursor(5);
    net.model(1).set_cursor(5);
    // Neither pump runs until both have typed: the edits are genuinely
    // concurrent, each made against a document that has not seen the other.
    net.model(0).insert(" there");
    net.model(1).insert(" world");
    net.pump();
    net.assert_converged();
    let text = net.text(0);
    assert!(
        text.contains(" there") && text.contains(" world"),
        "both insertions must survive, got {text:?}"
    );
}

#[test]
fn concurrent_edits_in_different_places_both_land() {
    let mut net = Net::pair("alpha beta gamma");
    net.model(0)
        .set_selections(rinch_code_editor_model::Selections::single(
            rinch_code_editor_model::Selection::new(0, 5),
        ));
    net.model(0).insert("ALPHA");
    net.model(1).set_cursor(16);
    net.model(1).insert("!");
    net.pump();
    net.assert_converged();
    assert_eq!(net.text(0), "ALPHA beta gamma!");
}

#[test]
fn a_multi_cursor_edit_replays_faithfully() {
    let mut net = Net::pair("aa\nbb\ncc");
    net.model(0)
        .set_selections(rinch_code_editor_model::Selections::new(
            vec![
                rinch_code_editor_model::Selection::cursor(0),
                rinch_code_editor_model::Selection::cursor(3),
                rinch_code_editor_model::Selection::cursor(6),
            ],
            0,
        ));
    net.model(0).insert(">");
    net.pump();
    assert_eq!(net.text(1), ">aa\n>bb\n>cc");
    net.assert_converged();
}

#[test]
fn an_undo_reaches_the_other_editor() {
    // The reason undo is recorded for publication at all: to a shared document
    // reverting is another edit, and one that goes unpublished splits the
    // session in two.
    let mut net = Net::pair("hello");
    net.model(0).set_cursor(5);
    net.model(0).insert(" world");
    net.pump();
    assert_eq!(net.text(1), "hello world");

    net.model(0).undo();
    net.pump();
    assert_eq!(net.text(1), "hello", "the undo must travel");
    net.assert_converged();

    net.model(0).redo();
    net.pump();
    assert_eq!(net.text(1), "hello world");
    net.assert_converged();
}

#[test]
fn a_coalesced_typing_run_survives_the_round_trip() {
    let mut net = Net::pair("");
    for ch in ["h", "e", "l", "l", "o"] {
        net.model(0).insert(ch);
    }
    net.pump();
    assert_eq!(net.text(1), "hello");
    // One undo step locally, five changes on the wire, and the far side ends
    // up with the same text either way.
    net.model(0).undo();
    net.pump();
    assert_eq!(net.text(0), "");
    net.assert_converged();
}

#[test]
fn unicode_offsets_cross_the_boundary_unconverted() {
    // The crate's load-bearing invariant: automerge indexes Text in Unicode
    // scalar values, exactly like ropey's chars, so nothing converts.
    let mut net = Net::pair("héllo 🦀 wörld");
    net.model(0).set_cursor(6);
    net.model(0).insert("🎉");
    net.pump();
    assert_eq!(net.text(1), "héllo 🎉🦀 wörld");
    net.assert_converged();
}

#[test]
fn a_late_joiner_catches_up_on_everything() {
    let mut net = Net::pair("start");
    net.model(0).set_cursor(5);
    net.model(0).insert(" middle");
    net.pump();

    let late = net.add(editor(""), CollabDoc::join());
    net.link(0, late);
    net.pump();
    net.assert_converged();
    assert_eq!(net.text(late), "start middle");

    // And is a full participant afterwards.
    net.model(late).set_cursor(12);
    net.model(late).insert(" end");
    net.pump();
    net.assert_converged();
    assert_eq!(net.text(1), "start middle end");
}

#[test]
fn a_long_interleaved_session_stays_converged() {
    let mut net = Net::pair("0\n");
    for round in 1..=25 {
        let writer = round % 2;
        let at = net.peers[writer].model.len_chars();
        net.model(writer).set_cursor(at);
        net.model(writer).insert(&format!("{round}\n"));
        // Pump only every other round, so half the edits are made against a
        // document already holding an unseen concurrent edit.
        if round % 2 == 0 {
            net.pump();
        }
    }
    net.pump();
    net.assert_converged();
    let text = net.text(0);
    for round in 1..=25 {
        assert!(text.contains(&format!("{round}\n")), "lost round {round}");
    }
}
