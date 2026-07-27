//! A reference relay for the rinch code editor's collaborative layer.
//!
//! Run it, point two editors at it, and they share a document:
//!
//! ```text
//! cargo run --release -p rinch-code-editor-relay
//! cargo run --release -p rinch-code-editor-net-demo -- Ada
//! cargo run --release -p rinch-code-editor-net-demo -- Grace
//! ```
//!
//! # What this is, and what it is not
//!
//! It is a **worked example of the transport the editor deliberately does not
//! own**. The collaboration crate speaks bytes and knows nothing about sockets;
//! this is one way to carry them, in about two hundred lines, and swapping it
//! for something else should not require touching anything below it.
//!
//! It is **not** a product. There is no persistence (a room exists only while
//! someone is in it), no authentication, no TLS, and no back-pressure. What it
//! does have is the protocol behaviour, and that part is real: it speaks the
//! y-sync protocol over WebSocket binary frames with the room in the URL path,
//! which is what the Yjs ecosystem's own servers speak. Reach for
//! y-websocket or y-sweet when you want the product. (Hocuspocus speaks a
//! different envelope — see the collaboration crate's docs.)
//!
//! # Shape
//!
//! A thread per connection, and a [`Hub`] they share. The thread owns its
//! socket and does every read and write on it; the hub owns the documents and
//! is locked only for the moment it takes to apply a message and work out what
//! everyone is owed. Nothing blocks on a socket while holding the lock — that
//! mistake turns one idle client into a stalled server.

mod hub;

use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use hub::Hub;
use tungstenite::handshake::server::{Request, Response};
use tungstenite::{Message, WebSocket};

/// How long a connection thread waits for a frame before going round again to
/// see whether it has anything to send. It bounds the delay between one client
/// typing and another hearing about it, so it wants to be short — but it is a
/// poll, so it also wants not to be zero.
const READ_TIMEOUT: Duration = Duration::from_millis(20);

/// How often to drop clients that vanished without disconnecting cleanly.
/// y-protocols' own sweep interval.
const SWEEP: Duration = Duration::from_secs(3);

const DEFAULT_ADDR: &str = "127.0.0.1:4455";

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var("RINCH_CE_RELAY_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string())
    });

    let listener = match TcpListener::bind(&addr) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("relay: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("relay: listening on ws://{addr}/<room>");

    let hub = Arc::new(Hub::new());

    // The liveness sweep. It is the only thing that clears the caret of a client
    // whose machine went to sleep rather than closing its socket.
    {
        let hub = hub.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(SWEEP);
                hub.sweep();
            }
        });
    }

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("relay: accept failed: {e}");
                continue;
            }
        };
        let hub = hub.clone();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        thread::spawn(move || {
            if let Err(e) = serve(&hub, stream, id) {
                eprintln!("relay: client {id}: {e}");
            }
        });
    }
}

/// One connection, start to finish.
fn serve(hub: &Arc<Hub>, stream: TcpStream, id: u64) -> Result<(), String> {
    // A short read timeout is what lets one thread both read its socket and
    // write what the hub has queued for it, without a second thread and without
    // ever sharing the WebSocket.
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| format!("cannot set a read timeout: {e}"))?;

    // The room is the URL path, which is what the y-websocket client sends and
    // therefore what an unmodified Yjs client would ask for.
    let mut room = String::new();
    // The closure's error type is tungstenite's whole rejection response, which
    // is large and which we never build — we always accept and let the room
    // check below do the rejecting.
    #[allow(clippy::result_large_err)]
    let mut socket = tungstenite::accept_hdr(stream, |req: &Request, res: Response| {
        room = req.uri().path().trim_start_matches('/').to_string();
        Ok(res)
    })
    .map_err(|e| format!("handshake failed: {e}"))?;

    if room.is_empty() {
        return Err("no room in the URL path".into());
    }
    println!("relay: client {id} joined '{room}'");

    let queue = hub.join(&room, id);
    let result = pump(hub, &mut socket, &room, id, &queue);

    hub.leave(&room, id);
    println!("relay: client {id} left '{room}'");
    let _ = socket.close(None);
    result
}

/// Alternate between reading whatever the client sent and writing whatever the
/// hub has queued for it, until the connection ends.
fn pump(
    hub: &Arc<Hub>,
    socket: &mut WebSocket<TcpStream>,
    room: &str,
    id: u64,
    queue: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    loop {
        // Everything the hub owes this client, including anything queued while
        // we were blocked in the read below.
        while let Ok(bytes) = queue.try_recv() {
            socket
                .send(Message::Binary(bytes))
                .map_err(|e| format!("send failed: {e}"))?;
        }

        match socket.read() {
            Ok(Message::Binary(bytes)) => hub.receive(room, id, &bytes)?,
            // The protocol is binary. A text frame is a client that has
            // misunderstood us, not something to guess at.
            Ok(Message::Text(_)) => return Err("unexpected text frame".into()),
            Ok(Message::Close(_)) => return Ok(()),
            // Ping/pong are handled by tungstenite itself; nothing to do.
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // The read timeout, which is the normal case: go round and see
                // whether the hub has given us anything to send.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}
