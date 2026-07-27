//! The socket, and the thread it lives on.
//!
//! rinch is single-threaded and strict about it: writing a [`Signal`] flushes
//! its subscribers synchronously, so it is only safe from an event handler or a
//! timer callback, and never from another thread at all. A websocket, meanwhile,
//! wants to block on a read. Those two facts decide the whole shape of this
//! module.
//!
//! So: one background thread owns the socket and does every read and write on
//! it, and it talks to the editor through a pair of channels. The UI never
//! touches the socket and the socket thread never touches a signal. The editor
//! side drains the inbound queue from a timer tick, which is one of the two
//! contexts where it is allowed to write signals.
//!
//! The thread alternates rather than blocking indefinitely: a short read timeout
//! means it wakes often enough to notice bytes the editor has queued for
//! sending. That is why there is no second thread and no shared socket.

use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// How long the socket thread waits for a frame before checking whether the
/// editor has given it anything to send. Bounds outbound latency.
const READ_TIMEOUT: Duration = Duration::from_millis(10);

/// A live connection to the relay, as the editor sees it: two queues and a flag.
pub struct Link {
    inbound: Receiver<Vec<u8>>,
    outbound: Sender<Vec<u8>>,
    connected: Arc<AtomicBool>,
}

impl Link {
    /// Connect to `url` and start serving it on a background thread.
    ///
    /// Fails only if the initial handshake fails. A connection that drops
    /// *later* is not an error here — it clears [`is_connected`](Self::is_connected)
    /// and the queues go quiet.
    pub fn connect(url: &str) -> Result<Self, String> {
        let (socket, _response) =
            tungstenite::connect(url).map_err(|e| format!("cannot reach {url}: {e}"))?;

        // The read timeout is what lets one thread do both directions. Without
        // it the thread would sit in a blocking read and never notice the
        // editor's outgoing bytes.
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(READ_TIMEOUT))
                .map_err(|e| format!("cannot set a read timeout: {e}"))?;
        }

        let (in_tx, inbound) = channel();
        let (outbound, out_rx) = channel();
        let connected = Arc::new(AtomicBool::new(true));

        let flag = connected.clone();
        thread::spawn(move || {
            let mut socket = socket;
            let reason = serve(&mut socket, &in_tx, &out_rx);
            flag.store(false, Ordering::Relaxed);
            let _ = socket.close(None);
            if let Err(e) = reason {
                eprintln!("link: disconnected: {e}");
            }
        });

        Ok(Self {
            inbound,
            outbound,
            connected,
        })
    }

    /// Queue a protocol message for the relay. Silently dropped once the
    /// connection is gone, which is the same thing the network would have done.
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.outbound.send(bytes);
    }

    /// Everything that has arrived since the last call.
    ///
    /// Non-blocking on purpose: this is called from a timer tick on the UI
    /// thread, and that thread must never wait for a network.
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        while let Ok(bytes) = self.inbound.try_recv() {
            messages.push(bytes);
        }
        messages
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Alternate reading and writing until the connection ends.
fn serve(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    inbound: &Sender<Vec<u8>>,
    outbound: &Receiver<Vec<u8>>,
) -> Result<(), String> {
    loop {
        loop {
            match outbound.try_recv() {
                Ok(bytes) => socket
                    .send(Message::Binary(bytes))
                    .map_err(|e| format!("send failed: {e}"))?,
                Err(TryRecvError::Empty) => break,
                // The editor has gone: nothing left to serve.
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                // A closed receiver means the editor is gone.
                if inbound.send(bytes.to_vec()).is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Text(_)) => return Err("the relay sent a text frame".into()),
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // The read timeout: the normal idle path.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}
