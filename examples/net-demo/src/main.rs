//! One editor, one process, one socket — the same document as everybody else in
//! the room.
//!
//! ```text
//! cargo run --release -p rinch-code-editor-relay
//! cargo run --release -p rinch-code-editor-net-demo -- Ada   --seed
//! cargo run --release -p rinch-code-editor-net-demo -- Grace
//! ```
//!
//! This is [`collab-demo`](../../desktop-demo/src/bin/collab-demo.rs) with the
//! in-process channel replaced by a real network, and the diff between them is
//! the point: the editor, the model and the collaboration crate are untouched.
//! Only the pump changed, because carrying bytes was always the application's
//! job.
//!
//! # Exactly one client may `--seed`
//!
//! The flag decides whether this process *creates* the document or *joins* it.
//! Two clients that each created their own copy of the same text would have two
//! independent insertions, and the CRDT would faithfully merge them into the
//! text twice over. So the first one in seeds, everybody else arrives to an
//! empty editor that fills in a moment later — which is what opening somebody
//! else's document looks like. Seeding into a room that already has content is
//! harmless in a different way: you would simply be inserting the sample again.
//!
//! # The pump
//!
//! Bytes arrive on a background thread ([`mod@link`]) and are drained from a
//! timer tick, because a timer callback is one of the two contexts where writing
//! a rinch signal is safe. The tick also carries anything the editor owes. It
//! runs whether or not the window is busy, which is what keeps a peer's typing
//! flowing in while you are doing nothing.

mod link;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use link::Link;
use rinch::core::set_timeout;
use rinch::prelude::*;
use rinch_code_editor::{CodeEditor, EditorHandle, RemotePresence, Rgba};
use rinch_code_editor_collab::{CollabDoc, HEARTBEAT_MS, SyncSession, User};

/// How often to carry whatever is owed in either direction. Fast enough that a
/// collaborator's typing looks live rather than stepped.
const TICK_MS: u32 = 16;

const DEFAULT_RELAY: &str = "ws://127.0.0.1:4455";
const DEFAULT_ROOM: &str = "demo";

/// A palette to hand out by name, so two processes that have never met agree on
/// what colour each person is. A real application takes this from its identity
/// system; presence carries the colour precisely so that everyone draws the same
/// person the same way.
fn colour_for(name: &str) -> Rgba {
    const PALETTE: [Rgba; 6] = [
        Rgba::rgb(0x30, 0xbc, 0xed),
        Rgba::rgb(0x6f, 0x42, 0xc1),
        Rgba::rgb(0xee, 0x63, 0x52),
        Rgba::rgb(0x6e, 0xeb, 0x83),
        Rgba::rgb(0xec, 0xd4, 0x44),
        Rgba::rgb(0x1b, 0xe7, 0xff),
    ];
    let hash = name.bytes().fold(0usize, |acc, b| acc * 31 + b as usize);
    PALETTE[hash % PALETTE.len()]
}

fn css(colour: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", colour.r, colour.g, colour.b)
}

const SAMPLE: &str = r#"//! Everyone in this room is editing this file.

fn fibonacci(n: u32) -> u64 {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

fn main() {
    for i in 0..10 {
        println!("fib({i}) = {}", fibonacci(i));
    }
}
"#;

/// Everything one participant needs, on the UI thread.
struct Session {
    doc: CollabDoc,
    /// Protocol state for the one connection this process has. A reconnect would
    /// need a fresh one at both ends — the protocol cannot resume an old one.
    sync: SyncSession,
    link: Link,
    editor: EditorHandle,
    peers: Signal<Vec<RemotePresence>>,
    connected: Signal<bool>,
    /// When the last liveness heartbeat went out.
    beats: u32,
}

impl Session {
    /// Carry everything owed in both directions, then refresh what is drawn.
    ///
    /// The order of the first two steps is the contract: a peer's splices were
    /// computed against a document that has already absorbed your keystrokes, so
    /// local changes must reach the document before any remote edit is replayed.
    fn pump(&mut self) {
        for change in self.editor.take_local_changes() {
            if let Err(e) = self.doc.apply_local(&change) {
                eprintln!("net-demo: a local change did not apply: {e}");
            }
        }
        // Republished every tick, including as `None` — an unfocused editor has
        // no caret worth showing, and saying so is what takes it off the other
        // screens.
        self.doc.set_local_selection(self.editor.selection());

        for bytes in self.link.drain() {
            match self.doc.receive_message(&mut self.sync, &bytes) {
                // Replaying into the editor repaints it, and deliberately does
                // not scroll: somebody else typing is not a reason to move your
                // viewport.
                Ok(splices) => {
                    self.editor.apply_remote(&splices);
                }
                // Malformed input is the other end's fault, and the recovery is
                // to drop the connection — not to take this editor down.
                Err(e) => eprintln!("net-demo: bad message from the relay: {e}"),
            }
        }

        while let Some(bytes) = self.doc.generate_message(&mut self.sync) {
            self.link.send(bytes);
        }

        self.refresh_presence();
        if self.connected.get() != self.link.is_connected() {
            self.connected.set(self.link.is_connected());
        }
    }

    /// Re-read who is where and hand it to the view.
    ///
    /// Only written when it differs: a rinch signal notifies its subscribers
    /// even when set to an equal value, and this runs sixty times a second.
    fn refresh_presence(&mut self) {
        let peers: Vec<RemotePresence> = self
            .doc
            .peers()
            .into_iter()
            .filter_map(|peer| {
                // Present but with no caret in this document: nothing to draw.
                let selection = peer.selection?;
                Some(RemotePresence {
                    id: peer.id,
                    name: peer.user.name,
                    color: Rgba::from_hex(&peer.user.color).unwrap_or(Rgba::rgb(0x80, 0x80, 0x80)),
                    selection,
                })
            })
            .collect();
        if self.peers.get() != peers {
            self.peers.set(peers);
        }
    }

    /// Re-announce ourselves so nobody decides we left. The relay expires a peer
    /// that goes quiet, and this is what stops that being us.
    fn heartbeat(&mut self) {
        self.beats += TICK_MS;
        if self.beats >= HEARTBEAT_MS as u32 {
            self.beats = 0;
            self.doc.presence_heartbeat();
        }
    }
}

/// The session plus the guard that stops a pump re-entering itself. A pump runs
/// both from a timer and from the editor's `on_change`, and an event handler is
/// exactly where a future change would make re-entry reachable.
#[derive(Clone)]
struct Pump {
    session: Rc<RefCell<Session>>,
    running: Rc<Cell<bool>>,
}

impl Pump {
    fn run(&self, heartbeat: bool) {
        if self.running.get() {
            return;
        }
        self.running.set(true);
        {
            let mut session = self.session.borrow_mut();
            if heartbeat {
                session.heartbeat();
            }
            session.pump();
        }
        self.running.set(false);
    }

    /// Keep ticking, re-armed from the callback. A timer callback is one of the
    /// two safe contexts for a signal write, the other being an event handler.
    fn schedule(&self) {
        let me = self.clone();
        set_timeout(TICK_MS, move || {
            me.run(true);
            me.schedule();
        });
    }
}

struct Args {
    name: String,
    room: String,
    relay: String,
    seed: bool,
}

fn args() -> Args {
    let mut positional = Vec::new();
    let mut seed = false;
    for arg in std::env::args().skip(1) {
        if arg == "--seed" {
            seed = true;
        } else {
            positional.push(arg);
        }
    }
    Args {
        name: positional.first().cloned().unwrap_or_else(|| "Anon".into()),
        room: positional
            .get(1)
            .cloned()
            .unwrap_or_else(|| DEFAULT_ROOM.into()),
        relay: std::env::var("RINCH_CE_RELAY").unwrap_or_else(|_| DEFAULT_RELAY.into()),
        seed,
    }
}

fn main() {
    let args = args();
    let url = format!("{}/{}", args.relay.trim_end_matches('/'), args.room);

    let link = match Link::connect(&url) {
        Ok(link) => link,
        Err(e) => {
            eprintln!("net-demo: {e}");
            eprintln!(
                "net-demo: start the relay first: cargo run --release -p rinch-code-editor-relay"
            );
            std::process::exit(1);
        }
    };
    println!("net-demo: {} connected to {url}", args.name);

    let colour = colour_for(&args.name);
    let mut doc = if args.seed {
        CollabDoc::create(SAMPLE)
    } else {
        CollabDoc::join()
    };
    // Publishing an identity is what makes this document visible to the others
    // at all: a peer that never does it is in the protocol but absent from every
    // `peers()` list.
    doc.set_local_user(User::new(args.name.clone(), css(colour)));

    let initial = if args.seed {
        SAMPLE.to_string()
    } else {
        String::new()
    };
    let title = format!("rinch-ce — {} in '{}'", args.name, args.room);

    // `run` takes a plain fn, so the pieces the component needs travel through a
    // thread-local rather than a capture. Single-threaded by construction, and
    // consumed exactly once at mount.
    STARTUP.with(|cell| {
        *cell.borrow_mut() = Some(Startup {
            doc,
            link,
            initial,
            name: args.name,
            colour,
        });
    });

    run(&title, 1000, 720, app);
}

struct Startup {
    doc: CollabDoc,
    link: Link,
    initial: String,
    name: String,
    colour: Rgba,
}

thread_local! {
    static STARTUP: RefCell<Option<Startup>> = const { RefCell::new(None) };
}

#[component]
fn app() -> NodeHandle {
    let Startup {
        doc,
        link,
        initial,
        name,
        colour,
    } = STARTUP
        .with(|cell| cell.borrow_mut().take())
        .expect("startup state is placed before `run` and taken exactly once");

    let editor = EditorHandle::new();
    let peers = Signal::new(Vec::new());
    let connected = Signal::new(true);

    let pump = Pump {
        session: Rc::new(RefCell::new(Session {
            doc,
            sync: SyncSession::new(),
            link,
            editor: editor.clone(),
            peers,
            connected,
            beats: 0,
        })),
        running: Rc::new(Cell::new(false)),
    };
    pump.schedule();
    let on_change = pump.clone();

    let header = format!(
        "flex:0 0 auto; padding:6px 10px; font-size:12px; font-weight:600; \
         background:{}; color:#fff;",
        css(colour)
    );

    rsx! {
        div {
            style: "width:100%; height:100%; display:flex; flex-direction:column; \
                    background:#131313; box-sizing:border-box; \
                    font-family:'DejaVu Sans Mono',monospace;",
            div {
                style: header,
                { name }
                // The disconnect notice is a separate node whose *style* is the
                // reactive part. A reactive text child would have to capture the
                // name, and the rsx macro reconstructs these closures on every
                // run — so they may only capture `Copy` values.
                span {
                    style: {
                        move || format!(
                            "margin-left:10px; font-weight:400; opacity:0.9;{}",
                            if connected.get() { " display:none;" } else { "" },
                        )
                    },
                    { "— relay disconnected" }
                }
            }
            div {
                style: "flex:1 1 auto; min-height:0;",
                CodeEditor {
                    text: initial,
                    language: "rust",
                    peers: peers,
                    handle: Some(editor),
                    on_change: move || on_change.run(false),
                }
            }
        }
    }
}
