//! Two editors, one document. Type in either pane and watch the other.
//!
//! This is the whole of Phase 3 standing up at once: two independent
//! `CodeEditor`s, two `CollabDoc`s, and a byte channel between them that here
//! happens to be a `Vec<u8>` handed across a function call. Nothing in the
//! editor crate knows a CRDT exists and nothing in the collaboration crate
//! knows a renderer does; this file is the only place that names both, which is
//! the point of the split.
//!
//! Run with `cargo run --release --bin collab-demo`.
//!
//! # What "the network" is here
//!
//! [`Session::pump`] is the transport. It generates each document's outgoing
//! y-sync message, delivers it to the other, and applies whatever splices come
//! back — the same exchange a websocket relay would carry, minus the sockets.
//! Swapping in a real transport means replacing that one function; every other
//! line here is what a networked application would write anyway.
//!
//! # Ada starts with the document, Grace joins empty
//!
//! Both panes are seeded from a *shared* document rather than from the same
//! text twice, and that distinction is load-bearing. Two peers who each created
//! their own document with the same text would have two independent insertions
//! that the CRDT would faithfully merge into the text twice over. So Ada's
//! document is `create`d with the sample and Grace's `join`s it empty: her pane
//! is blank for one pump tick and then fills in, which is exactly what opening
//! somebody else's document looks like.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rinch::core::set_timeout;
use rinch::prelude::*;
use rinch_code_editor::{CodeEditor, EditorHandle, RemotePresence, Rgba};
use rinch_code_editor_collab::{CollabDoc, PEER_TIMEOUT_MS, SWEEP_MS, SyncSession, User};

/// How often the housekeeping tick runs: presence heartbeats, expiry of peers
/// who vanished without saying goodbye, and a pump in case anything is still
/// owed. Edits do not wait for it — they pump the moment they happen.
const TICK_MS: u32 = SWEEP_MS as u32;

/// A bound on one pump's exchange, so a protocol bug becomes a visible stall
/// rather than a frozen window. Sync between two peers settles in a handful of
/// passes; the convergence tests use the same guard with the same reasoning.
const MAX_PASSES: usize = 100;

const SAMPLE: &str = r#"//! Two people, one document.
//!
//! Click into either pane and type. Watch the other one.

use std::collections::HashMap;

/// Tally how often each word appears.
fn word_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word.to_lowercase()).or_insert(0) += 1;
    }
    counts
}

fn fibonacci(n: u32) -> u64 {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

fn main() {
    let counts = word_counts("the cat sat on the mat");
    for (word, n) in &counts {
        println!("{word}: {n}");
    }
}
"#;

/// One participant: their shared document, their editor, and the presence list
/// their editor draws.
struct Peer {
    doc: CollabDoc,
    /// Protocol state for the one link this peer has. A real application keeps
    /// one of these per connection, and a fresh one on every reconnect.
    session: SyncSession,
    editor: EditorHandle,
    /// What the *other* people look like from this peer's seat.
    others: Signal<Vec<RemotePresence>>,
}

/// Both peers and the channel between them.
struct Session {
    peers: Vec<Peer>,
}

impl Session {
    /// Carry everything owed in both directions, then refresh what each pane
    /// draws of the other.
    ///
    /// The order of the first two steps is the contract, not a preference: a
    /// peer's splices are computed against a document that has already absorbed
    /// your keystrokes, so every local change must be published *before* any
    /// remote edit is replayed. `EditorModel::apply_remote` asserts on it in
    /// release builds rather than diverging quietly.
    fn pump(&mut self) {
        for peer in &mut self.peers {
            for change in peer.editor.take_local_changes() {
                peer.doc
                    .apply_local(&change)
                    .expect("a local change the editor made must apply to its own document");
            }
            // Republished every tick, including when it is `None` — an unfocused
            // editor has no caret to show, and saying so is what takes it off
            // the other screen.
            peer.doc.set_local_selection(peer.editor.selection());
        }

        for _ in 0..MAX_PASSES {
            if !self.exchange() {
                self.refresh_presence();
                return;
            }
        }
        panic!("sync did not settle in {MAX_PASSES} passes");
    }

    /// One pass: every peer generates toward its neighbour, *then* everything is
    /// delivered — so messages cross on the wire, which is the case a sync
    /// protocol is most likely to get wrong. Returns whether anything moved.
    fn exchange(&mut self) -> bool {
        let mut deliveries = Vec::new();
        for (i, peer) in self.peers.iter_mut().enumerate() {
            if let Some(bytes) = peer.doc.generate_message(&mut peer.session) {
                deliveries.push((1 - i, bytes));
            }
        }
        if deliveries.is_empty() {
            return false;
        }
        for (to, bytes) in deliveries {
            let peer = &mut self.peers[to];
            let splices = peer
                .doc
                .receive_message(&mut peer.session, &bytes)
                .expect("a message this session generated must be readable");
            // Replaying into the editor repaints that pane. It deliberately does
            // not scroll: somebody else typing is not a reason to move your
            // viewport.
            peer.editor.apply_remote(&splices);
        }
        true
    }

    /// Re-read who is where, and hand it to the pane that draws them.
    ///
    /// Only written when it actually differs: a rinch signal notifies its
    /// subscribers even when set to an equal value, and this runs on every
    /// keystroke.
    fn refresh_presence(&mut self) {
        for peer in &self.peers {
            let others: Vec<RemotePresence> = peer
                .doc
                .peers()
                .into_iter()
                .filter_map(|other| {
                    // A peer who is present but has no caret in this document
                    // has nothing to draw.
                    let selection = other.selection?;
                    Some(RemotePresence {
                        id: other.id,
                        name: other.user.name,
                        color: Rgba::from_hex(&other.user.color)
                            .unwrap_or(Rgba::rgb(0x80, 0x80, 0x80)),
                        selection,
                    })
                })
                .collect();
            if peer.others.get() != others {
                peer.others.set(others);
            }
        }
    }

    /// Presence's two standing obligations, which no library can do for us
    /// because this crate owns no clock: re-announce so nobody decides we left,
    /// and drop anyone who left without saying so.
    fn housekeeping(&mut self) {
        for peer in &mut self.peers {
            peer.doc.presence_heartbeat();
            peer.doc.expire_stale_peers(PEER_TIMEOUT_MS);
        }
    }

    /// Say goodbye for `index` and report the document text as it stands, so the
    /// pane can be rebuilt from it if they come back.
    ///
    /// The explicit goodbye matters: without it the others keep drawing a caret
    /// for someone who has gone until it ages out, half a minute later.
    fn disconnect(&mut self, index: usize) -> String {
        self.peers[index].doc.clear_local_presence();
        self.peers[index].doc.text()
    }
}

/// The session, plus the guard that keeps a pump from re-entering itself.
///
/// Re-entry is not currently reachable — nothing a pump does routes back into
/// an editor's input path — but the pump is called from event handlers, and an
/// event handler is exactly where a future change would make it reachable. A
/// skipped tick is recoverable; a panicking `RefCell` in a paint handler is
/// not.
#[derive(Clone)]
struct Pump {
    session: Rc<RefCell<Session>>,
    running: Rc<Cell<bool>>,
}

impl Pump {
    fn run(&self, housekeeping: bool) {
        if self.running.get() {
            return;
        }
        self.running.set(true);
        {
            let mut session = self.session.borrow_mut();
            if housekeeping {
                session.housekeeping();
            }
            session.pump();
        }
        self.running.set(false);
    }

    fn disconnect(&self, index: usize) -> String {
        self.session.borrow_mut().disconnect(index)
    }

    /// The document as `index`'s peer currently holds it — what to seed their
    /// pane with when they come back.
    fn text_of(&self, index: usize) -> String {
        self.session.borrow().peers[index].doc.text()
    }

    /// Keep the housekeeping tick going, re-arming from its own callback. A
    /// timer callback is one of the two contexts where writing a rinch signal is
    /// safe — the other being an event handler — because a write flushes
    /// subscribers synchronously and must not happen inside a running effect.
    fn schedule(&self) {
        let me = self.clone();
        set_timeout(TICK_MS, move || {
            me.run(true);
            me.schedule();
        });
    }
}

fn peer(doc: CollabDoc, name: &str, color: &str) -> Peer {
    let mut doc = doc;
    // Publishing an identity is what makes a document visible to the others at
    // all: a peer that never does this is in the protocol but absent from every
    // `peers()` list.
    doc.set_local_user(User::new(name, color));
    Peer {
        doc,
        session: SyncSession::new(),
        editor: EditorHandle::new(),
        others: Signal::new(Vec::new()),
    }
}

#[component]
fn app() -> NodeHandle {
    // Ada owns the document; Grace joins it empty and receives it on the first
    // pump. See the module docs for why they cannot both `create`.
    let ada = peer(CollabDoc::create(SAMPLE), "Ada", "#30bced");
    let grace = peer(CollabDoc::join(), "Grace", "#6f42c1");

    let (ada_editor, ada_others) = (ada.editor.clone(), ada.others);
    let (grace_editor, grace_others) = (grace.editor.clone(), grace.others);

    let pump = Pump {
        session: Rc::new(RefCell::new(Session {
            peers: vec![ada, grace],
        })),
        running: Rc::new(Cell::new(false)),
    };

    // The first exchange, which is what carries the document into Grace's pane.
    // Deferred to a timer rather than run here: this runs during mount, and a
    // signal write during mount would flush render effects into a half-built
    // tree.
    {
        let pump = pump.clone();
        set_timeout(0, move || pump.run(false));
    }
    pump.schedule();

    let on_ada_change = pump.clone();
    let on_grace_change = pump.clone();

    // Whether Grace's pane is mounted at all. Unmounting a live editor is worth
    // being able to do here rather than only in principle: it is what exercises
    // the teardown every one of the editor's timers and registries depends on
    // (the blink and chip clocks, the drag auto-scroller, the keyboard registry,
    // the collaborative handle), and rinch only gives a component its own scope
    // — and therefore its own cleanup — when it sits inside a control-flow
    // boundary like this `if`. Mounted statically, all of that teardown is code
    // nothing ever runs.
    let grace_here = Signal::new(true);
    // Grace's pane is empty the first time (the document arrives over the wire)
    // and seeded from the shared document if she comes back. Deliberately not a
    // signal: it is read while the pane is being built, and a signal read there
    // would make the pane's own remount one of its dependencies.
    let grace_text = Rc::new(RefCell::new(String::new()));

    let toggle = {
        let pump = pump.clone();
        let grace_text = grace_text.clone();
        move || {
            if grace_here.get() {
                // Publish anything outstanding, then say goodbye *before*
                // unmounting, so Ada hears it while Grace's editor still exists.
                pump.run(false);
                *grace_text.borrow_mut() = pump.disconnect(1);
                grace_here.set(false);
            } else {
                *grace_text.borrow_mut() = pump.text_of(1);
                grace_here.set(true);
            }
            // Carry the goodbye — or Grace's returning caret — to Ada.
            pump.run(false);
        }
    };

    rsx! {
        div {
            style: "width:100%; height:100%; display:flex; flex-direction:column; \
                    background:#131313; box-sizing:border-box; \
                    font-family:'DejaVu Sans Mono',monospace;",
            div {
                style: "flex:0 0 auto; padding:8px 12px;",
                button {
                    style: "padding:4px 10px; font-size:12px; border-radius:4px; \
                            background:#2a2d2e; color:#d4d4d4; border:1px solid #444;",
                    onclick: toggle,
                    { move || if grace_here.get() { "Disconnect Grace" } else { "Reconnect Grace" } }
                }
            }
            div {
                style: "flex:1 1 auto; display:flex; flex-direction:row; gap:12px; \
                        padding:0 12px 12px; box-sizing:border-box; min-height:0;",
                Pane {
                    name: "Ada",
                    color: "#30bced",
                    text: SAMPLE.to_string(),
                    handle: ada_editor,
                    peers: ada_others,
                    on_change: move || on_ada_change.run(false),
                }
                if grace_here.get() {
                    Pane {
                        name: "Grace",
                        color: "#6f42c1",
                        text: grace_text.borrow().clone(),
                        handle: grace_editor.clone(),
                        peers: grace_others,
                        // Cloned per mount: the branch body is an `Fn` that runs
                        // again every time the pane comes back, so it may only
                        // borrow what it captures.
                        on_change: {
                            let pump = on_grace_change.clone();
                            move || pump.run(false)
                        },
                    }
                }
            }
        }
    }
}

/// One titled pane wrapping one editor.
#[component]
fn Pane(
    name: String,
    color: String,
    text: String,
    handle: EditorHandle,
    peers: Signal<Vec<RemotePresence>>,
    on_change: Callback,
) -> NodeHandle {
    rsx! {
        div {
            style: "flex:1 1 0; display:flex; flex-direction:column; min-width:0; \
                    border:1px solid #000; border-radius:6px; overflow:hidden;",
            div {
                style: format!(
                    "flex:0 0 auto; padding:6px 10px; font-size:12px; font-weight:600; \
                     background:{color}; color:#fff;"
                ),
                { name }
            }
            div {
                style: "flex:1 1 auto; min-height:0;",
                CodeEditor {
                    text: text,
                    language: "rust",
                    peers: peers,
                    handle: Some(handle),
                    on_change: on_change,
                }
            }
        }
    }
}

fn main() {
    run("rinch-ce collaborative demo", 1400, 760, app);
}
