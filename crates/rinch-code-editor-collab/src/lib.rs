//! Transport-agnostic collaborative editing for the rinch code editor.
//!
//! This crate wraps an automerge document whose `Text` object mirrors the
//! editor's [`Buffer`](rinch_code_editor_buffer::Buffer), and speaks only
//! *bytes*: sync-protocol messages out and in. It never opens a socket, holds
//! a connection, or knows a network exists — the embedder pumps the bytes
//! through whatever transport it already runs, exactly as automerge's own sync
//! module is designed to be used. A reference websocket transport ships as a
//! separate crate for the demo only.
//!
//! Ephemeral presence (remote carets and selections) is a separate byte
//! channel layered on in a later increment; this module is sync only.
//!
//! # Shape
//!
//! - [`CollabDoc`] — the automerge mirror of one editor buffer. Local edits
//!   go in as [`Change`](rinch_code_editor_buffer::Change)s; remote edits
//!   come out as [`Splice`]s for the model to replay.
//! - [`SyncSession`] — per-peer sync-protocol state; one per connection,
//!   fresh on every reconnect.
//! - The pump rule, framing requirements, and the create/join/load session
//!   rules live in [`doc`]'s module docs.

mod doc;

pub use doc::{CollabDoc, CollabError, Splice, SyncSession};

/// The whole crate's offset arithmetic assumes automerge indexes `Text` in
/// Unicode scalar values — the same units as ropey's char indexing — which is
/// automerge's behavior only while *neither* the `utf8-indexing` nor the
/// `utf16-indexing` cargo feature is enabled anywhere in the dependency
/// graph. A transitive dependency flipping one on would silently corrupt
/// every splice, patch index, and cursor position this crate exchanges.
/// This test turns that silent corruption into a loud build failure.
#[cfg(test)]
mod pins {
    #[test]
    fn automerge_text_indexing_is_unicode_code_points() {
        use automerge::{ObjType, ROOT, ReadDoc, transaction::Transactable};

        let mut doc = automerge::AutoCommit::new();
        let text = doc.put_object(ROOT, "content", ObjType::Text).unwrap();
        // "é🦀" is 2 code points, 6 UTF-8 bytes, 3 UTF-16 code units — the
        // three candidate encodings all disagree on its length.
        doc.splice_text(&text, 0, 0, "é🦀").unwrap();
        assert_eq!(
            doc.length(&text),
            2,
            "automerge Text must index in Unicode scalar values (= ropey chars); \
             a dependency enabled utf8-indexing or utf16-indexing"
        );
    }
}
