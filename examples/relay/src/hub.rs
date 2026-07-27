//! Rooms of clients, each room one shared document.
//!
//! The whole relay is this: a [`CollabDoc`] per room that never edits anything,
//! holding one [`SyncSession`] per connected client. An update arriving on one
//! session leaves by all the others, because the document's outbox tags every
//! entry with the session it came from and skips only that one. Presence rides
//! the same path. So a relay does not implement the protocol — it *is* a peer
//! that happens to have no editor attached, which is exactly what a y-websocket
//! server is.
//!
//! `crates/rinch-code-editor-collab/tests/relay.rs` tests this topology with no
//! sockets in it, which is where the protocol behaviour is actually pinned. What
//! is left here is connection lifecycle.
//!
//! # Locking
//!
//! One mutex guards all the rooms, and it is held only for the CPU work of
//! applying a message and deciding what everyone is owed. Socket writes happen
//! outside it: each client owns an outbound channel, and its own connection
//! thread does the writing. Holding a lock across a blocking socket operation is
//! the classic way to build a relay where one idle client stalls every other,
//! and it is worth naming because it is easy to do by accident.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, MutexGuard};

use rinch_code_editor_collab::{CollabDoc, PEER_TIMEOUT_MS, SyncSession};

/// A bound on one exchange, so a protocol bug surfaces as a loud stall rather
/// than a spin. Sync between a client and a hub settles in a handful of passes.
const MAX_PASSES: usize = 100;

/// Every room this relay is serving.
#[derive(Default)]
pub struct Hub {
    rooms: Mutex<HashMap<String, Room>>,
}

struct Room {
    doc: CollabDoc,
    clients: Vec<Client>,
}

struct Client {
    id: u64,
    /// The hub's end of this connection.
    session: SyncSession,
    /// Bytes waiting to go out. The connection thread drains it; nothing here
    /// ever touches a socket.
    out: Sender<Vec<u8>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit a client to `room`, creating the room if this is the first arrival.
    ///
    /// Returns the queue its connection thread should drain. The opening
    /// handshake is already in it: joining pumps immediately, so the client has
    /// our state vector and an awareness query waiting before it says anything.
    pub fn join(&self, room: &str, id: u64) -> Receiver<Vec<u8>> {
        let (out, queue) = channel();
        let mut rooms = self.lock();
        let room = rooms.entry(room.to_string()).or_insert_with(|| Room {
            // The relay joins like any other peer: empty, and filled in by
            // whoever already has the document. It never calls `create`, because
            // it has no opinion about what the document should start as.
            doc: CollabDoc::join(),
            clients: Vec::new(),
        });
        room.clients.push(Client {
            id,
            session: SyncSession::new(),
            out,
        });
        room.pump();
        queue
    }

    /// Feed one frame from `id` into `room`'s document and hand out whatever it
    /// obliges us to say to everyone.
    ///
    /// An error here is the client's fault — malformed bytes — and the recovery
    /// is to drop that connection, never to take the room down with it.
    pub fn receive(&self, room: &str, id: u64, bytes: &[u8]) -> Result<(), String> {
        let mut rooms = self.lock();
        let Some(room) = rooms.get_mut(room) else {
            return Err("room is gone".into());
        };
        // Split the borrow: the document and the client list are separate fields,
        // and the call below needs both at once.
        let Room { doc, clients } = room;
        let Some(client) = clients.iter_mut().find(|c| c.id == id) else {
            return Err("client is gone".into());
        };
        // The splices are the *rendering* of a change, and a relay renders
        // nothing; the document has already absorbed the update, which is all we
        // need in order to pass it on.
        doc.receive_message(&mut client.session, bytes)
            .map_err(|e| format!("{e}"))?;
        room.pump();
        Ok(())
    }

    /// Retire a client. The room goes with the last one out.
    ///
    /// [`CollabDoc::close_session`] rather than merely dropping the session: it
    /// takes the departing client's caret off the other screens at once, and it
    /// releases the outbox, which the abandoned session's read cursor would
    /// otherwise pin for as long as the relay ran.
    pub fn leave(&self, room_name: &str, id: u64) {
        let mut rooms = self.lock();
        let Some(room) = rooms.get_mut(room_name) else {
            return;
        };
        if let Some(index) = room.clients.iter().position(|c| c.id == id) {
            let client = room.clients.remove(index);
            room.doc.close_session(client.session);
        }
        if room.clients.is_empty() {
            // Nobody left to hold the document up. A relay that kept every room
            // it had ever seen would be a memory leak with extra steps; a real
            // deployment persists here instead.
            rooms.remove(room_name);
            return;
        }
        room.pump();
    }

    /// Drop clients that stopped talking without disconnecting cleanly, and
    /// re-announce nothing — a relay publishes no presence of its own because it
    /// is not a participant.
    ///
    /// Call periodically. This is the only thing that clears the caret of
    /// someone whose machine went to sleep mid-session.
    pub fn sweep(&self) {
        let mut rooms = self.lock();
        for room in rooms.values_mut() {
            room.doc.expire_stale_peers(PEER_TIMEOUT_MS);
            room.pump();
        }
    }

    /// A poisoned mutex means another connection thread panicked while holding
    /// it. The rooms are still structurally intact — every mutation under the
    /// lock is a whole operation — so carrying on serves the remaining clients
    /// rather than killing the process for all of them.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Room>> {
        self.rooms.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Room {
    /// Ask the document what each client is owed until it says nothing, queueing
    /// it all for the connection threads to write.
    fn pump(&mut self) {
        for _ in 0..MAX_PASSES {
            let mut moved = false;
            for client in &mut self.clients {
                while let Some(bytes) = self.doc.generate_message(&mut client.session) {
                    // A send failure means that client's connection thread has
                    // already gone. Its `leave` is on the way; dropping the
                    // bytes is right.
                    let _ = client.out.send(bytes);
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
        eprintln!("warning: a room did not settle in {MAX_PASSES} passes");
    }
}
