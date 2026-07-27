//! What can go wrong, and whose fault each one is.

/// Why a [`CollabDoc`](crate::CollabDoc) operation could not proceed.
///
/// The variants split cleanly by *who is at fault*, which is also the recovery
/// each one calls for:
///
/// - [`BadMessage`](Self::BadMessage) and [`BadPeerData`](Self::BadPeerData)
///   blame the **remote peer** — the bytes on the wire were malformed or
///   unapplicable. Network input is untrusted; the recovery is to drop that
///   connection, never to crash.
/// - [`BadSnapshot`](Self::BadSnapshot) blames the **bytes handed to
///   [`CollabDoc::load`](crate::CollabDoc::load)** — wrong file, wrong
///   document.
/// - [`NotSynced`](Self::NotSynced) blames **call ordering** — an edit before
///   the content exists; hold editing until
///   [`CollabDoc::is_synced`](crate::CollabDoc::is_synced).
/// - [`OutOfRange`](Self::OutOfRange) and [`Internal`](Self::Internal) blame
///   **this integration** — the model and the document disagree about the text.
///   Never reachable from network input, and a bug: fail fast.
#[derive(Debug)]
#[non_exhaustive]
pub enum CollabError {
    /// The bytes handed to
    /// [`CollabDoc::receive_message`](crate::CollabDoc::receive_message) do not
    /// decode as a protocol message. Untrusted input; drop the connection, do
    /// not crash.
    BadMessage(yrs::encoding::read::Error),
    /// A message decoded, but the update it carried could not be applied — a
    /// corrupt payload from a hostile or buggy peer. Same fault as
    /// [`BadMessage`](Self::BadMessage), same recovery. The document is left
    /// untouched, so the peer may reconnect with a fresh
    /// [`SyncSession`](crate::SyncSession).
    BadPeerData(yrs::error::UpdateError),
    /// The bytes handed to [`CollabDoc::load`](crate::CollabDoc::load) do not
    /// decode as a document.
    BadSnapshot(yrs::encoding::read::Error),
    /// A local edit arrived before the shared text exists. A document created
    /// with [`CollabDoc::join`](crate::CollabDoc::join) is read-only until the
    /// first sync delivers the content; gate editing on
    /// [`CollabDoc::is_synced`](crate::CollabDoc::is_synced).
    NotSynced,
    /// A frame that carried no protocol message at all — bytes that were never
    /// this protocol, or a message truncated before its first field. Same fault
    /// and same recovery as [`BadMessage`](Self::BadMessage); it is separate
    /// only because the decoder reports the two conditions identically and
    /// saying so is more honest than picking one.
    EmptyFrame,
    /// The other end refused us, with its reason. Blames **neither side's
    /// code**: it is a policy answer from a server, and the only recovery is to
    /// stop pumping this connection and tell the user. Never produced by this
    /// crate — only received.
    Denied(String),
    /// A local [`Change`](rinch_code_editor_buffer::Change) addressed text
    /// outside the shared document — the model and the document have disagreed
    /// about the contents. Caught *before* anything is applied, so the document
    /// is untouched and still describes itself honestly; resynchronize the
    /// model from [`CollabDoc::text`](crate::CollabDoc::text) rather than
    /// carrying on.
    OutOfRange {
        /// The offending edit's start, in pre-change coordinates.
        pos: usize,
        /// How many chars it tried to remove.
        del: usize,
        /// The length of the shared text the change was validated against.
        len: usize,
    },
    /// An operation the engine rejected for a reason that is not the peer's
    /// fault and not a range fault. An integration bug.
    Internal(String),
}

impl std::fmt::Display for CollabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMessage(e) => write!(f, "protocol message failed to decode: {e}"),
            Self::BadPeerData(e) => write!(f, "peer sent an update that could not be applied: {e}"),
            Self::BadSnapshot(e) => write!(f, "snapshot failed to load: {e}"),
            Self::NotSynced => write!(f, "local edit before the first sync delivered the content"),
            Self::EmptyFrame => write!(f, "frame carried no protocol message"),
            Self::Denied(reason) => write!(f, "the other end refused this session: {reason}"),
            Self::OutOfRange { pos, del, len } => write!(
                f,
                "local change deletes {del} chars at {pos} of a {len}-char document"
            ),
            Self::Internal(e) => write!(f, "internal engine operation failed: {e}"),
        }
    }
}

impl std::error::Error for CollabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadMessage(e) | Self::BadSnapshot(e) => Some(e),
            Self::BadPeerData(e) => Some(e),
            // The reason a server gave is its own words, not a nested error.
            Self::EmptyFrame
            | Self::Denied(_)
            | Self::NotSynced
            | Self::OutOfRange { .. }
            | Self::Internal(_) => None,
        }
    }
}
