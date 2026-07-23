//! Multi-peer convergence: pairs and meshes of `CollabDoc`s syncing in
//! process, each peer with a model `Buffer` replaying the emitted splices —
//! the C2 integration contract in miniature. Every receive asserts the
//! replayed model matches the document exactly; convergence asserts all
//! peers agree.

use rinch_code_editor_buffer::{Buffer, Change, Edit};
use rinch_code_editor_collab::{CollabDoc, Splice, SyncSession};
use std::collections::HashMap;

struct Peer {
    doc: CollabDoc,
    /// The buffer this peer's editor would hold, kept in sync only through
    /// the crate's public contract: local `Change`s in, remote `Splice`s out.
    model: Buffer,
    /// One session per connected peer, keyed by that peer's index.
    sessions: HashMap<usize, SyncSession>,
    /// Every splice batch this peer has received, for tests that assert the
    /// shape of what came out (e.g. the joiner's single full-text splice).
    received: Vec<Vec<Splice>>,
}

struct Net {
    peers: Vec<Peer>,
    links: Vec<(usize, usize)>,
}

impl Net {
    fn new(docs: Vec<CollabDoc>, links: &[(usize, usize)]) -> Self {
        let mut net = Self {
            peers: Vec::new(),
            links: Vec::new(),
        };
        for doc in docs {
            net.add_peer(doc, &[]);
        }
        for &(a, b) in links {
            net.link(a, b);
        }
        net
    }

    fn add_peer(&mut self, doc: CollabDoc, connect_to: &[usize]) -> usize {
        let index = self.peers.len();
        self.peers.push(Peer {
            model: Buffer::from_text(&doc.text()),
            doc,
            sessions: HashMap::new(),
            received: Vec::new(),
        });
        for &other in connect_to {
            self.link(index, other);
        }
        index
    }

    fn link(&mut self, a: usize, b: usize) {
        self.peers[a].sessions.insert(b, SyncSession::new());
        self.peers[b].sessions.insert(a, SyncSession::new());
        self.links.push((a, b));
    }

    /// Apply local edits at peer `i` the way the editor will: buffer first,
    /// then the resulting `Change` into the document.
    fn edit(&mut self, i: usize, edits: Vec<Edit>) {
        let peer = &mut self.peers[i];
        let change = Change::apply(&mut peer.model, edits);
        peer.doc.apply_local(&change).unwrap();
    }

    fn take_message(&mut self, from: usize, to: usize) -> Option<Vec<u8>> {
        let peer = &mut self.peers[from];
        peer.doc
            .generate_message(peer.sessions.get_mut(&to).unwrap())
    }

    fn receive(&mut self, to: usize, from: usize, bytes: &[u8]) {
        let peer = &mut self.peers[to];
        let splices = peer
            .doc
            .receive_message(peer.sessions.get_mut(&from).unwrap(), bytes)
            .unwrap();
        for splice in &splices {
            apply_splice(&mut peer.model, splice);
        }
        assert_eq!(
            peer.model.text(),
            peer.doc.text(),
            "peer {to}: replaying the emitted splices diverged from the document"
        );
        peer.received.push(splices);
    }

    /// One synchronous exchange: every peer generates toward every linked
    /// peer *first*, then all messages are delivered — so messages cross on
    /// the wire, the adversarial case for a sync protocol.
    fn pass(&mut self) -> bool {
        let mut deliveries = Vec::new();
        for (a, b) in self.links.clone() {
            for (from, to) in [(a, b), (b, a)] {
                if let Some(bytes) = self.take_message(from, to) {
                    deliveries.push((to, from, bytes));
                }
            }
        }
        let any = !deliveries.is_empty();
        for (to, from, bytes) in deliveries {
            self.receive(to, from, &bytes);
        }
        any
    }

    /// Exchange until quiescent: a full pass over every link moves nothing.
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
            assert_eq!(
                peer.doc.text(),
                reference,
                "peer {i}'s doc did not converge"
            );
            assert_eq!(
                peer.model.text(),
                reference,
                "peer {i}'s model did not converge"
            );
        }
    }
}

/// Replay one remote splice onto the model the way C2 will: as an ordinary
/// atomic replacement through the buffer's own edit machinery.
fn apply_splice(model: &mut Buffer, splice: &Splice) {
    Change::apply(
        model,
        vec![Edit::replace(
            splice.pos..splice.pos + splice.del,
            splice.insert.clone(),
        )],
    );
}

/// A creator and a joiner, already bootstrapped.
fn pair(initial: &str) -> Net {
    let mut net = Net::new(
        vec![CollabDoc::create(initial), CollabDoc::join()],
        &[(0, 1)],
    );
    net.pump();
    net.assert_converged();
    net
}

#[test]
fn join_bootstraps_via_one_full_splice() {
    let mut net = Net::new(
        vec![CollabDoc::create("fn main() {}\n"), CollabDoc::join()],
        &[(0, 1)],
    );
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[1].doc.text(), "fn main() {}\n");

    let content_batches: Vec<&Vec<Splice>> = net.peers[1]
        .received
        .iter()
        .filter(|batch| !batch.is_empty())
        .collect();
    assert_eq!(
        content_batches,
        vec![&vec![Splice {
            pos: 0,
            del: 0,
            insert: "fn main() {}\n".into(),
        }]],
        "the joiner's bootstrap should be a single full-text splice"
    );
}

#[test]
fn joiner_of_an_empty_document_becomes_synced_without_a_splice() {
    // The empty-create case: the creator seeds "", so there is no content to
    // deliver and NO bootstrap splice is ever emitted. is_synced() is the only
    // signal that the joiner may now edit — watching the splice stream would
    // leave it read-only forever.
    let mut net = Net::new(vec![CollabDoc::create(""), CollabDoc::join()], &[(0, 1)]);
    assert!(!net.peers[1].doc.is_synced(), "joiner starts unsynced");
    net.pump();
    net.assert_converged();

    assert!(
        net.peers[1].doc.is_synced(),
        "joiner is synced after the handshake even with empty content"
    );
    assert!(
        net.peers[1].received.iter().all(|batch| batch.is_empty()),
        "no splice should be emitted for empty content"
    );

    // And editing is now accepted on both sides and still converges.
    net.edit(1, vec![Edit::insert(0, "joiner types first")]);
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[0].doc.text(), "joiner types first");
}

#[test]
fn concurrent_inserts_at_the_same_position_converge() {
    let mut net = pair("abc");
    net.edit(0, vec![Edit::insert(1, "XX")]);
    net.edit(1, vec![Edit::insert(1, "yy")]);
    net.pump();
    net.assert_converged();
    let text = net.peers[0].doc.text();
    assert!(
        text == "aXXyybc" || text == "ayyXXbc",
        "both concurrent insertions must survive at their position: {text:?}"
    );
}

#[test]
fn concurrent_overlapping_deletes_converge() {
    let mut net = pair("abcdefgh");
    net.edit(0, vec![Edit::delete(1..5)]); // bcde
    net.edit(1, vec![Edit::delete(3..7)]); // defg
    net.pump();
    net.assert_converged();
    // The union of the two deletions goes; deleting a char twice is one delete.
    assert_eq!(net.peers[0].doc.text(), "ah");
}

#[test]
fn concurrent_edit_and_delete_of_its_context_converge() {
    let mut net = pair("hello world");
    net.edit(0, vec![Edit::replace(0..5, "goodbye")]);
    net.edit(1, vec![Edit::delete(5..11)]); // " world"
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[0].doc.text(), "goodbye");
}

#[test]
fn interleaved_editing_rounds_converge() {
    let mut net = pair("");
    for round in 0..10 {
        let end = net.peers[0].model.len_chars();
        net.edit(0, vec![Edit::insert(end, format!("a{round} "))]);
        net.edit(1, vec![Edit::insert(0, format!("b{round} "))]);
        net.pump();
        net.assert_converged();
    }
}

#[test]
fn multi_cursor_changes_cross_as_one_atomic_batch() {
    let mut net = pair("aa\nbb\ncc");
    // Three cursors typing at once on peer 0, a concurrent edit on peer 1.
    net.edit(
        0,
        vec![
            Edit::insert(0, ">"),
            Edit::insert(3, ">"),
            Edit::insert(6, ">"),
        ],
    );
    net.edit(1, vec![Edit::replace(3..5, "BB")]);
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[0].doc.text(), ">aa\n>BB\n>cc");
}

#[test]
fn unicode_edits_converge_in_char_units() {
    let mut net = pair("é🦀é🦀");
    net.edit(0, vec![Edit::replace(1..2, "🚀🚀")]);
    net.edit(1, vec![Edit::insert(4, "ß")]);
    net.pump();
    net.assert_converged();
    // 🦀 at char 1 became 🚀🚀 and ß landed at the old end — all in char
    // units; byte or UTF-16 indexing anywhere would have shredded this.
    assert_eq!(net.peers[0].doc.text(), "é🚀🚀é🦀ß");
}

#[test]
fn relay_topology_reaches_the_far_peer() {
    // A ↔ B ↔ C: no direct A–C link; everything crosses through B.
    let mut net = Net::new(
        vec![
            CollabDoc::create("hub\n"),
            CollabDoc::join(),
            CollabDoc::join(),
        ],
        &[(0, 1), (1, 2)],
    );
    net.pump();
    net.assert_converged();

    net.edit(0, vec![Edit::insert(0, "from a\n")]);
    net.edit(2, vec![Edit::insert(4, "from c\n")]);
    net.pump();
    net.assert_converged();
    let text = net.peers[1].doc.text();
    assert!(
        text.contains("from a") && text.contains("from c"),
        "{text:?}"
    );
}

#[test]
fn duplicate_message_delivery_converges() {
    let mut net = pair("abc");
    net.edit(0, vec![Edit::insert(3, "!")]);
    let bytes = net.take_message(0, 1).unwrap();
    net.receive(1, 0, &bytes);
    net.receive(1, 0, &bytes); // the transport delivered it twice
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[1].doc.text(), "abc!");
}

#[test]
fn snapshot_join_then_live_sync() {
    let mut scratch = Buffer::from_text("");
    let mut a = CollabDoc::create("");
    for text in ["one\n", "two\n", "three\n"] {
        let at = scratch.len_chars();
        let change = Change::apply(&mut scratch, vec![Edit::insert(at, text)]);
        a.apply_local(&change).unwrap();
    }
    let b = CollabDoc::load(&a.save()).unwrap();
    assert_eq!(b.text(), "one\ntwo\nthree\n");

    let mut net = Net::new(vec![a, b], &[(0, 1)]);
    net.edit(0, vec![Edit::insert(0, "A")]);
    net.edit(1, vec![Edit::insert(4, "B")]);
    net.pump();
    net.assert_converged();
}

#[test]
fn reconnect_with_fresh_sessions_converges() {
    let mut net = pair("shared");
    net.edit(0, vec![Edit::insert(0, "x")]);
    net.pump();
    // The transport drops mid-session; peer 1 keeps editing offline. On
    // reconnect BOTH sides start fresh sessions — the old ones are dead.
    net.edit(1, vec![Edit::insert(0, "y")]);
    net.peers[0].sessions.insert(1, SyncSession::new());
    net.peers[1].sessions.insert(0, SyncSession::new());
    net.pump();
    net.assert_converged();
    let text = net.peers[0].doc.text();
    assert!(text.contains('x') && text.contains('y'), "{text:?}");
}

#[test]
fn late_joiner_catches_up_and_participates() {
    let mut net = pair("v0\n");
    for round in 0..5 {
        let end = net.peers[0].model.len_chars();
        net.edit(0, vec![Edit::insert(end, format!("line {round}\n"))]);
        net.edit(1, vec![Edit::insert(0, "> ")]);
        net.pump();
    }
    let expected = net.peers[0].doc.text();

    let c = net.add_peer(CollabDoc::join(), &[0]);
    net.pump();
    net.assert_converged();
    assert_eq!(net.peers[c].doc.text(), expected);

    net.edit(c, vec![Edit::insert(0, "c was here\n")]);
    net.pump();
    net.assert_converged();
}

#[test]
fn dual_create_misuse_repairs_to_one_winner() {
    // The documented misuse: both peers `create`. The merge keeps exactly one
    // text; the loser must be repaired onto the winner, not left diverged.
    let mut net = Net::new(
        vec![CollabDoc::create("alpha"), CollabDoc::create("beta")],
        &[(0, 1)],
    );
    net.pump();
    net.assert_converged();
    let text = net.peers[0].doc.text();
    assert!(text == "alpha" || text == "beta", "{text:?}");

    // And the surviving session still edits normally afterwards.
    net.edit(0, vec![Edit::insert(0, "0")]);
    net.edit(1, vec![Edit::insert(0, "1")]);
    net.pump();
    net.assert_converged();
}

/// A minimal deterministic PRNG so the fuzz needs no dependencies and every
/// failure reproduces from its seed.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if n == 0 {
            0
        } else {
            ((self.0 >> 33) % n as u64) as usize
        }
    }
}

#[test]
fn fuzz_three_peer_mesh_with_messages_in_flight() {
    const INSERTS: [&str; 6] = ["a", "b", "é", "🦀", "\n", "xyz"];
    for seed in [1, 7, 42, 1337] {
        let mut rng = Lcg(seed);
        let mut net = Net::new(
            vec![
                CollabDoc::create("seed\n"),
                CollabDoc::join(),
                CollabDoc::join(),
            ],
            &[(0, 1), (0, 2), (1, 2)],
        );
        net.pump();

        for _round in 0..40 {
            for i in 0..3 {
                if rng.below(4) == 0 {
                    continue; // this peer sits the round out
                }
                let len = net.peers[i].model.len_chars();
                let pos = rng.below(len + 1);
                let del = rng.below((len - pos).min(4) + 1);
                let insert = INSERTS[rng.below(INSERTS.len())];
                net.edit(i, vec![Edit::replace(pos..pos + del, insert)]);
            }
            // One exchange only — edits keep landing while messages from the
            // previous rounds are still crossing the mesh.
            net.pass();
        }
        net.pump();
        net.assert_converged();
    }
}
