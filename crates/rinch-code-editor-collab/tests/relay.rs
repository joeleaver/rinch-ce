//! A document acting as a *relay*: one hub, several clients, nobody talking to
//! anybody directly.
//!
//! This is the topology the reference websocket server uses, and it is worth
//! testing without a socket in sight, because everything that can actually go
//! wrong here is protocol, not networking. A relay is a `CollabDoc` that never
//! edits: it holds one [`SyncSession`] per client, and its whole job is that an
//! update arriving on one session leaves by the others.
//!
//! That property is not obvious from the outside — it is a consequence of the
//! outbox tagging each entry with the session it came from and skipping only
//! that one — so it is pinned here. If it ever stopped holding, every relayed
//! session would go silently one-way.

use rinch_code_editor_buffer::{Buffer, Change, Edit};
use rinch_code_editor_collab::{CollabDoc, Selection, SyncSession, User};

/// A hub and its clients, wired star-fashion.
struct Relay {
    hub: CollabDoc,
    /// The hub's end of each connection.
    hub_sessions: Vec<SyncSession>,
    clients: Vec<Client>,
}

struct Client {
    doc: CollabDoc,
    /// This client's end of its one connection, to the hub.
    session: SyncSession,
    /// The buffer this client's editor would hold, kept up to date only through
    /// the splices the crate emits.
    model: Buffer,
}

impl Relay {
    /// A hub with `n` clients, the first of which brings the document.
    fn new(initial: &str, n: usize) -> Self {
        let mut relay = Self {
            // The hub joins like anyone else: it starts empty and receives the
            // document from whoever has it.
            hub: CollabDoc::join(),
            hub_sessions: Vec::new(),
            clients: Vec::new(),
        };
        for i in 0..n {
            let doc = if i == 0 {
                CollabDoc::create(initial)
            } else {
                CollabDoc::join()
            };
            let model = Buffer::from_text(&doc.text());
            relay.hub_sessions.push(SyncSession::new());
            relay.clients.push(Client {
                doc,
                session: SyncSession::new(),
                model,
            });
        }
        relay.pump();
        relay
    }

    fn edit(&mut self, i: usize, edits: Vec<Edit>) {
        let client = &mut self.clients[i];
        let change = Change::apply(&mut client.model, edits);
        client.doc.apply_local(&change).unwrap();
    }

    /// Exchange until a full round over every connection moves nothing.
    fn pump(&mut self) {
        for _ in 0..1000 {
            let mut moved = false;
            for i in 0..self.clients.len() {
                // Client -> hub.
                loop {
                    let client = &mut self.clients[i];
                    let Some(bytes) = client.doc.generate_message(&mut client.session) else {
                        break;
                    };
                    self.hub
                        .receive_message(&mut self.hub_sessions[i], &bytes)
                        .unwrap();
                    moved = true;
                }
                // Hub -> client.
                while let Some(bytes) = self.hub.generate_message(&mut self.hub_sessions[i]) {
                    let client = &mut self.clients[i];
                    let splices = client
                        .doc
                        .receive_message(&mut client.session, &bytes)
                        .unwrap();
                    for s in &splices {
                        Change::apply(
                            &mut client.model,
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
        panic!("the relay never went quiet");
    }

    fn assert_converged(&self) {
        let reference = self.hub.text();
        for (i, client) in self.clients.iter().enumerate() {
            assert_eq!(
                client.doc.text(),
                reference,
                "client {i}'s document diverged"
            );
            assert_eq!(
                client.model.text(),
                reference,
                "client {i}'s replayed model diverged from its document"
            );
        }
    }

    /// The names client `i` can currently see.
    fn names_seen_by(&self, i: usize) -> Vec<String> {
        let mut names: Vec<String> = self.clients[i]
            .doc
            .peers()
            .into_iter()
            .map(|p| p.user.name)
            .collect();
        names.sort();
        names
    }
}

/// A relay serves its connections from threads, so its document has to cross
/// one. Nothing in the crate's API says so, and nothing would fail until
/// somebody tried to write a server — so pin it here, where the server-shaped
/// tests live.
#[test]
fn a_document_and_its_sessions_can_cross_a_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<CollabDoc>();
    assert_send::<SyncSession>();
}

#[test]
fn an_edit_reaches_the_other_clients_through_the_hub() {
    let mut relay = Relay::new("fn main() {}\n", 3);
    relay.assert_converged();

    // Offset 12 is just past the closing brace: "fn main() {}" is 12 chars, so
    // 11 would land *inside* it, before the `}`.
    relay.edit(0, vec![Edit::insert(12, " /* hello */")]);
    relay.pump();

    relay.assert_converged();
    assert_eq!(relay.clients[2].doc.text(), "fn main() {} /* hello */\n");
}

#[test]
fn concurrent_edits_at_the_same_place_converge_through_the_hub() {
    let mut relay = Relay::new("abc", 3);

    // Two clients edit the same offset before either has heard from the other,
    // which is the case a relay makes easy to hit and a CRDT exists for.
    relay.edit(0, vec![Edit::insert(1, "X")]);
    relay.edit(1, vec![Edit::insert(1, "Y")]);
    relay.pump();

    relay.assert_converged();
    // Whichever order the CRDT picked, everybody has to have picked it.
    let text = relay.hub.text();
    assert!(text == "aXYbc" || text == "aYXbc", "got {text}");
}

#[test]
fn presence_reaches_the_other_clients_through_the_hub() {
    let mut relay = Relay::new("hello world", 3);

    relay.clients[0]
        .doc
        .set_local_user(User::new("Ada", "#30bced"));
    relay.clients[0]
        .doc
        .set_local_selection(Some(Selection::new(0, 5)));
    relay.clients[1]
        .doc
        .set_local_user(User::new("Grace", "#6f42c1"));
    relay.pump();

    // Each client sees the other, and never itself.
    assert_eq!(relay.names_seen_by(1), vec!["Ada".to_string()]);
    assert_eq!(
        relay.names_seen_by(2),
        vec!["Ada".to_string(), "Grace".to_string()]
    );

    // …and Ada's caret survived the extra hop with its offsets intact.
    let ada = relay.clients[1]
        .doc
        .peers()
        .into_iter()
        .find(|p| p.user.name == "Ada")
        .expect("Ada must be visible to client 1");
    assert_eq!(ada.selection, Some(Selection::new(0, 5)));
}

/// The reason [`CollabDoc::close_session`] exists. A relay outlives its clients,
/// so a departed one must leave nothing behind — not a caret on the other
/// screens, and not an outbox entry that can never be trimmed.
#[test]
fn closing_a_session_takes_that_clients_caret_down_everywhere() {
    let mut relay = Relay::new("hello world", 3);

    relay.clients[0]
        .doc
        .set_local_user(User::new("Ada", "#30bced"));
    relay.clients[0]
        .doc
        .set_local_selection(Some(Selection::cursor(3)));
    relay.clients[1]
        .doc
        .set_local_user(User::new("Grace", "#6f42c1"));
    relay.pump();
    assert_eq!(
        relay.names_seen_by(2),
        vec!["Ada".to_string(), "Grace".to_string()]
    );

    // Ada's connection dies. The hub retires her session…
    let ada_session = relay.hub_sessions.remove(0);
    relay.hub.close_session(ada_session);
    let ada = relay.clients.remove(0);
    drop(ada);
    relay.pump();

    // …and the news reaches everyone still connected, without waiting out the
    // liveness timeout.
    assert_eq!(
        relay.names_seen_by(1),
        vec!["Grace".to_string()],
        "the remaining clients must stop drawing a caret for a peer that left"
    );
}

/// Closing a session must not disturb the peers still on the other ones — the
/// mistake this guards against is removing awareness by connection rather than
/// by the clients that connection actually spoke for.
#[test]
fn closing_one_session_leaves_the_others_alone() {
    let mut relay = Relay::new("hello", 3);
    for (i, name) in ["Ada", "Grace", "Katherine"].iter().enumerate() {
        relay.clients[i]
            .doc
            .set_local_user(User::new(*name, "#30bced"));
        relay.clients[i]
            .doc
            .set_local_selection(Some(Selection::cursor(i)));
    }
    relay.pump();

    let katherine_session = relay.hub_sessions.remove(2);
    relay.hub.close_session(katherine_session);
    relay.clients.remove(2);
    relay.pump();

    assert_eq!(relay.names_seen_by(0), vec!["Grace".to_string()]);
    assert_eq!(relay.names_seen_by(1), vec!["Ada".to_string()]);
}

/// The mistake `close_session` is written to avoid, reproduced.
///
/// Answering an awareness query means sending your *whole* view, not just
/// yourself. A client that reconnects therefore tells the hub about everyone it
/// still remembers, and all of them are attributed to that one new session.
/// Tearing the session down must not take those people with it — they never went
/// anywhere.
#[test]
fn closing_a_session_that_spoke_for_others_does_not_evict_them() {
    let mut relay = Relay::new("hello", 2);
    relay.clients[0]
        .doc
        .set_local_user(User::new("Ada", "#30bced"));
    relay.clients[0]
        .doc
        .set_local_selection(Some(Selection::cursor(1)));
    relay.clients[1]
        .doc
        .set_local_user(User::new("Grace", "#6f42c1"));
    relay.clients[1]
        .doc
        .set_local_selection(Some(Selection::cursor(2)));
    relay.pump();
    // Grace now knows about Ada, which is the precondition that makes this
    // interesting.
    assert_eq!(relay.names_seen_by(1), vec!["Ada".to_string()]);

    // Grace's connection drops and comes back. Both ends start fresh sessions,
    // and the hub's opening query draws Grace's whole view out of her — Ada
    // included.
    let old = relay.hub_sessions.remove(1);
    relay.hub.close_session(old);
    relay.clients[1].session = SyncSession::new();
    relay.hub_sessions.push(SyncSession::new());
    relay.pump();

    // Now retire that second session. Ada is still connected on hers.
    let second = relay.hub_sessions.remove(1);
    relay.hub.close_session(second);
    relay.clients.remove(1);
    relay.pump();

    assert_eq!(
        relay.names_seen_by(0),
        Vec::<String>::new(),
        "Grace really did leave"
    );
    assert!(
        relay.hub.peers().iter().any(|p| p.user.name == "Ada"),
        "Ada never disconnected and must still be present on the hub"
    );
}

/// A client that leaves and comes back is a fresh session, and must be brought
/// fully up to date — including edits made while it was away.
#[test]
fn a_client_that_reconnects_is_caught_up() {
    let mut relay = Relay::new("hello", 2);

    let gone = relay.hub_sessions.remove(1);
    relay.hub.close_session(gone);
    relay.clients.remove(1);

    relay.edit(0, vec![Edit::insert(5, " world")]);
    relay.pump();

    // Back, with a brand new session at both ends — the protocol has no way to
    // resume an old one.
    relay.hub_sessions.push(SyncSession::new());
    relay.clients.push(Client {
        doc: CollabDoc::join(),
        session: SyncSession::new(),
        model: Buffer::from_text(""),
    });
    relay.pump();

    relay.assert_converged();
    assert_eq!(relay.clients[1].doc.text(), "hello world");
}
