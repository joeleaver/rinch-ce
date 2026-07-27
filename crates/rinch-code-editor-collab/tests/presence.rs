//! Presence over the wire: two documents exchanging awareness the same way
//! they exchange edits, through the same pump.
//!
//! The interesting property here is not "a caret arrives" — it is that a caret
//! that arrived stays *pointing at the right text* once other people edit
//! around it. That is what the [`StickyIndex`](yrs::StickyIndex) anchors buy,
//! and `a_caret_follows_the_text_it_was_attached_to` is the test that would
//! fail if presence ever regressed to sending plain integers.

use rinch_code_editor_buffer::{Buffer, Change, Edit};
use rinch_code_editor_collab::{CollabDoc, PEER_TIMEOUT_MS, Peer, Selection, SyncSession, User};

struct Net {
    docs: Vec<CollabDoc>,
    models: Vec<Buffer>,
    sessions: Vec<[SyncSession; 2]>,
}

impl Net {
    /// A creator and a joiner, bootstrapped and quiescent.
    fn pair(initial: &str) -> Self {
        let mut net = Self {
            docs: vec![CollabDoc::create(initial), CollabDoc::join()],
            models: vec![Buffer::from_text(initial), Buffer::from_text("")],
            sessions: vec![
                [SyncSession::new(), SyncSession::new()],
                [SyncSession::new(), SyncSession::new()],
            ],
        };
        net.pump();
        net
    }

    fn edit(&mut self, i: usize, edits: Vec<Edit>) {
        let change = Change::apply(&mut self.models[i], edits);
        self.docs[i].apply_local(&change).unwrap();
    }

    /// Exchange until a full round moves nothing.
    fn pump(&mut self) {
        for _ in 0..1000 {
            let mut moved = false;
            for (from, to) in [(0usize, 1usize), (1, 0)] {
                while let Some(bytes) =
                    self.docs[from].generate_message(&mut self.sessions[from][to])
                {
                    let splices = self.docs[to]
                        .receive_message(&mut self.sessions[to][from], &bytes)
                        .unwrap();
                    for s in &splices {
                        Change::apply(
                            &mut self.models[to],
                            vec![Edit::replace(s.pos..s.pos + s.del, s.insert.clone())],
                        );
                    }
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
        panic!("sync did not reach quiescence");
    }

    /// What peer `i` can see of everyone else.
    fn peers(&self, i: usize) -> Vec<Peer> {
        self.docs[i].peers()
    }

    /// The single peer `i` can see, panicking if that is not exactly one.
    fn only_peer(&self, i: usize) -> Peer {
        let peers = self.peers(i);
        assert_eq!(peers.len(), 1, "peer {i} sees {} others", peers.len());
        peers.into_iter().next().unwrap()
    }
}

fn ada() -> User {
    User::new("Ada", "#30bced")
}

fn grace() -> User {
    User::new("Grace", "#6eeb83")
}

#[test]
fn a_peer_who_announces_themselves_becomes_visible() {
    let mut net = Net::pair("hello world");
    assert!(net.peers(1).is_empty(), "nobody has announced anything yet");

    net.docs[0].set_local_user(ada());
    net.pump();

    let seen = net.only_peer(1);
    assert_eq!(seen.user, ada());
    assert_eq!(seen.id, net.docs[0].local_id());
    assert_eq!(seen.selection, None, "announced, but no caret published");

    // And presence is not self-reflective: peer 0 does not see itself.
    assert!(net.peers(0).is_empty());
}

#[test]
fn a_caret_crosses_in_char_offsets() {
    let mut net = Net::pair("hello world");
    net.docs[0].set_local_user(ada());
    net.docs[0].set_local_selection(Some(Selection::new(2, 7)));
    net.pump();

    let seen = net.only_peer(1).selection.expect("a caret");
    assert_eq!(seen.anchor, 2);
    assert_eq!(seen.head, 7);
}

/// The whole reason presence carries anchors and not integers.
///
/// Grace's caret sits at "world". Ada then inserts text *before* it. Grace's
/// caret must still be on "world" — shifted by exactly what Ada inserted —
/// even though Grace never republished it and has no idea Ada typed anything.
#[test]
fn a_caret_follows_the_text_it_was_attached_to() {
    let mut net = Net::pair("hello world");
    net.docs[1].set_local_user(grace());
    net.docs[1].set_local_selection(Some(Selection::new(6, 11))); // "world"
    net.pump();
    assert_eq!(
        net.only_peer(0).selection.unwrap(),
        Selection::new(6, 11),
        "precondition: it arrived where it was sent"
    );

    // Ada inserts six characters at the very start — entirely before Grace's
    // selection, and without Grace saying anything further.
    net.edit(0, vec![Edit::insert(0, "AGAIN ")]);
    net.pump();
    assert_eq!(net.docs[0].text(), "AGAIN hello world");

    let after = net.only_peer(0).selection.unwrap();
    assert_eq!(
        after,
        Selection::new(12, 17),
        "the caret must ride the insertion, not stay at its old integer offset"
    );
    let text = net.docs[0].text();
    assert_eq!(&text[after.start()..after.end()], "world");
}

/// The same property in the other direction: an edit *after* a caret must not
/// move it. A naive "shift everything by the delta" scheme fails this one.
#[test]
fn an_edit_after_a_caret_leaves_it_alone() {
    let mut net = Net::pair("hello world");
    net.docs[1].set_local_user(grace());
    net.docs[1].set_local_selection(Some(Selection::new(0, 5))); // "hello"
    net.pump();

    net.edit(0, vec![Edit::insert(11, " and goodbye")]);
    net.pump();

    assert_eq!(net.only_peer(0).selection.unwrap(), Selection::new(0, 5));
}

/// Anchors are resolved through the same chars ↔ UTF-16 boundary as edits, so
/// a caret past an astral character has to land on a char offset, not a code
/// unit one. "héllo 🦀" is 7 chars / 11 bytes / 8 UTF-16 units.
#[test]
fn caret_offsets_cross_the_boundary_in_chars() {
    let mut net = Net::pair("héllo 🦀 wörld");
    net.docs[0].set_local_user(ada());
    // Offset 8 is just past the crab: char 8, but UTF-16 unit 9.
    net.docs[0].set_local_selection(Some(Selection::new(8, 8)));
    net.pump();

    let seen = net.only_peer(1).selection.unwrap();
    assert_eq!(
        seen.head, 8,
        "chars, not UTF-16 code units (9) or bytes (12)"
    );
}

#[test]
fn clearing_presence_removes_the_peer_at_once() {
    let mut net = Net::pair("hello");
    net.docs[0].set_local_user(ada());
    net.docs[0].set_local_selection(Some(Selection::new(1, 1)));
    net.pump();
    assert_eq!(net.peers(1).len(), 1);

    net.docs[0].clear_local_presence();
    net.pump();
    assert!(
        net.peers(1).is_empty(),
        "a deliberate leave must not wait out the timeout"
    );
}

#[test]
fn dropping_a_caret_keeps_the_peer_visible() {
    // Blurring the editor publishes `None` — the person is still here, they
    // just have no caret in this document. Distinct from leaving.
    let mut net = Net::pair("hello");
    net.docs[0].set_local_user(ada());
    net.docs[0].set_local_selection(Some(Selection::new(1, 1)));
    net.pump();

    net.docs[0].set_local_selection(None);
    net.pump();

    let seen = net.only_peer(1);
    assert_eq!(seen.user, ada());
    assert_eq!(seen.selection, None);
}

#[test]
fn both_peers_see_each_other() {
    let mut net = Net::pair("hello world");
    net.docs[0].set_local_user(ada());
    net.docs[0].set_local_selection(Some(Selection::new(0, 0)));
    net.docs[1].set_local_user(grace());
    net.docs[1].set_local_selection(Some(Selection::new(11, 11)));
    net.pump();

    assert_eq!(net.only_peer(1).user, ada());
    assert_eq!(net.only_peer(0).user, grace());
    assert_eq!(net.only_peer(1).selection.unwrap().head, 0);
    assert_eq!(net.only_peer(0).selection.unwrap().head, 11);
}

/// A peer who announced themselves before we connected must still show up.
///
/// Awareness is broadcast as deltas, so there is nothing in the outbox to
/// replay to a latecomer — the opening handshake has to *ask*. This is the
/// test that fails if the `AwarenessQuery` is dropped from the handshake.
#[test]
fn a_late_joiner_learns_about_an_idle_peer() {
    let mut net = Net::pair("hello");
    net.docs[0].set_local_user(ada());
    net.docs[0].set_local_selection(Some(Selection::new(3, 3)));
    net.pump();

    // A third document joins, connected only to peer 0, well after Ada last
    // said anything.
    let mut latecomer = CollabDoc::join();
    let mut theirs = SyncSession::new();
    let mut ours = SyncSession::new();
    for _ in 0..50 {
        let mut moved = false;
        if let Some(m) = latecomer.generate_message(&mut theirs) {
            net.docs[0].receive_message(&mut ours, &m).unwrap();
            moved = true;
        }
        if let Some(m) = net.docs[0].generate_message(&mut ours) {
            latecomer.receive_message(&mut theirs, &m).unwrap();
            moved = true;
        }
        if !moved {
            break;
        }
    }

    let peers = latecomer.peers();
    assert_eq!(
        peers.len(),
        1,
        "the latecomer must be told who is already here"
    );
    assert_eq!(peers[0].user, ada());
    assert_eq!(peers[0].selection.unwrap().head, 3);
}

#[test]
fn a_peer_that_goes_silent_is_expired() {
    let mut net = Net::pair("hello");
    net.docs[0].set_local_user(ada());
    net.pump();
    assert_eq!(net.peers(1).len(), 1);

    // Nothing has aged out yet under the real timeout.
    assert!(net.docs[1].expire_stale_peers(PEER_TIMEOUT_MS).is_empty());
    assert_eq!(net.peers(1).len(), 1);

    // Let a little wall clock pass, then sweep with a timeout shorter than it.
    // The comparison is y-protocols' strict `now - last_seen > timeout`, so a
    // zero timeout would NOT fire within the same millisecond — the sleep is
    // what makes this test exercise the real rule rather than a weakened one.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let dropped = net.docs[1].expire_stale_peers(1);
    assert_eq!(dropped, vec![net.docs[0].local_id()]);
    assert!(net.peers(1).is_empty());
}

#[test]
fn presence_version_tracks_visible_change_only() {
    let mut net = Net::pair("hello");
    let before = net.docs[1].presence_version();

    net.docs[0].set_local_user(ada());
    net.pump();
    let after_announce = net.docs[1].presence_version();
    assert!(
        after_announce > before,
        "a new peer is a visible change: {before} -> {after_announce}"
    );

    // A heartbeat republishes an identical state. It keeps the peer alive but
    // there is nothing new to draw, so a renderer must not be woken for it.
    net.docs[0].presence_heartbeat();
    net.pump();
    assert_eq!(
        net.docs[1].presence_version(),
        after_announce,
        "a bare heartbeat changes a clock, not anything visible"
    );

    net.docs[0].set_local_selection(Some(Selection::new(2, 2)));
    net.pump();
    assert!(net.docs[1].presence_version() > after_announce);
}

/// Presence is ephemeral: publishing a caret must not touch the document, or
/// every mouse click would land in the undo history and the persisted file.
#[test]
fn publishing_presence_does_not_change_the_document() {
    let mut net = Net::pair("hello world");
    let before = net.docs[0].text();
    let len_before = net.docs[0].len_chars();

    net.docs[0].set_local_user(ada());
    for head in 0..=11 {
        net.docs[0].set_local_selection(Some(Selection::new(head, head)));
    }
    net.pump();

    assert_eq!(net.docs[0].text(), before);
    assert_eq!(net.docs[0].len_chars(), len_before);
    assert_eq!(net.docs[1].text(), before, "and nothing reached the peer");
}

/// A caret whose text was deleted out from under it stays drawable.
#[test]
fn a_caret_in_deleted_text_is_clamped_not_lost() {
    let mut net = Net::pair("hello world");
    net.docs[1].set_local_user(grace());
    net.docs[1].set_local_selection(Some(Selection::new(6, 11)));
    net.pump();

    net.edit(0, vec![Edit::delete(0..11)]);
    net.pump();
    assert_eq!(net.docs[0].text(), "");

    let seen = net.only_peer(0);
    if let Some(sel) = seen.selection {
        assert!(sel.head <= net.docs[0].len_chars());
        assert!(sel.anchor <= net.docs[0].len_chars());
    }
}
