//! Transport-agnostic collaborative editing for the rinch code editor.
//!
//! This crate wraps a CRDT document whose text mirrors the editor's
//! [`Buffer`](rinch_code_editor_buffer::Buffer), and speaks only *bytes*:
//! protocol messages out and in. It never opens a socket, holds a connection,
//! or knows a network exists — the embedder pumps the bytes through whatever
//! transport it already runs. A reference websocket transport ships as a
//! separate crate for the demo only.
//!
//! The engine is [yrs](https://docs.rs/yrs), the Rust core of Yjs, and the wire
//! format is the y-sync protocol, so a session can talk to the existing Yjs
//! server ecosystem rather than only to itself.
//!
//! Ephemeral presence — remote carets, selections, names and colours — rides
//! the same channel as y-sync's awareness messages, through the same pump. It
//! never touches the document: presence is published and read separately, and
//! carries no text.
//!
//! # Shape
//!
//! - [`CollabDoc`] — the CRDT mirror of one editor buffer. Local edits go in as
//!   [`Change`](rinch_code_editor_buffer::Change)s; remote edits come out as
//!   [`Splice`]s for the model to replay.
//! - [`SyncSession`] — per-peer protocol state; one per connection, fresh on
//!   every reconnect.
//! - [`User`] and [`Peer`] — presence in and out. Publish with
//!   `set_local_user`/`set_local_selection`, read with `peers()`, and re-render
//!   when `presence_version()` moves.
//! - The pump rule, framing requirements, offset handling, and the
//!   create/join/load session rules live in [`doc`]'s module docs; what goes
//!   into a presence state, and why it is shaped the way Yjs shapes it, lives
//!   in [`presence`]'s.
//!
//! # Driving presence
//!
//! Two things the embedder must run on timers, because this crate owns no
//! clock of its own beyond reading one: [`CollabDoc::presence_heartbeat`] every
//! [`HEARTBEAT_MS`], and [`CollabDoc::expire_stale_peers`] every [`SWEEP_MS`]
//! with [`PEER_TIMEOUT_MS`]. Skip the first and other peers will decide you
//! left; skip the second and the carets of people who crashed never go away.

mod doc;
mod presence;

pub use doc::{CollabDoc, CollabError, SyncSession};
pub use presence::{HEARTBEAT_MS, PEER_TIMEOUT_MS, Peer, SWEEP_MS, Selection, User};

/// The unit of remote edit this crate emits, owned by the buffer crate because
/// the editor model applies them and neither layer should have to learn the
/// other's vocabulary. Note the *sequential* coordinate contract in its docs —
/// it is what makes a batch of them replayable.
pub use rinch_code_editor_buffer::Splice;

/// The offset invariant, pinned.
///
/// Everything this crate exposes is in **chars** (Unicode scalars), matching
/// ropey and the buffer crate. yrs indexes text in **UTF-16 code units**, so
/// unlike the automerge engine this replaced — where the two happened to
/// coincide and offsets crossed the boundary untouched — there is a real
/// conversion at the boundary now, and a real chance of getting it wrong.
///
/// These tests fail loudly if the engine's indexing changes under us, or if the
/// conversion stops being exact for text where chars, bytes and UTF-16 units
/// all disagree.
#[cfg(test)]
mod pins {
    use super::*;
    use rinch_code_editor_buffer::{Buffer, Change, Edit};

    /// "héllo 🦀" is 7 chars, 11 UTF-8 bytes and 8 UTF-16 code units — all three
    /// candidate encodings disagree, so nothing here can pass by coincidence.
    const AWKWARD: &str = "héllo 🦀";

    #[test]
    fn yrs_indexes_text_in_utf16_code_units() {
        use yrs::{GetString, OffsetKind, Options, Text, Transact};

        let options = Options {
            offset_kind: OffsetKind::Utf16,
            ..Options::default()
        };
        let doc = yrs::Doc::with_options(options);
        let text = doc.get_or_insert_text("content");
        {
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, AWKWARD);
        }
        let len = {
            let txn = doc.transact();
            text.len(&txn)
        };
        assert_eq!(
            len, 8,
            "yrs must index in UTF-16 code units; the boundary conversion assumes it"
        );
        assert_eq!(
            AWKWARD.chars().count(),
            7,
            "…and chars are not the same unit"
        );
        assert_eq!(text.get_string(&doc.transact()), AWKWARD);
    }

    /// The conversion the crate actually performs, end to end: an edit
    /// expressed in chars must land where the model meant it to, in text where
    /// a byte- or UTF-16-shaped mistake would be visible.
    #[test]
    fn char_offsets_survive_the_boundary() {
        let mut buffer = Buffer::from_text(AWKWARD);
        let mut doc = CollabDoc::create(AWKWARD);
        assert_eq!(doc.len_chars(), 7);

        // The crab is the 7th char, at index 6, so char offset 7 is the end of
        // the document: byte 11, UTF-16 unit 8. All three disagree, so any
        // encoding confusion puts this insertion somewhere visible.
        let change = Change::apply(&mut buffer, vec![Edit::insert(7, "!")]);
        doc.apply_local(&change).unwrap();
        assert_eq!(doc.text(), buffer.text());
        assert_eq!(doc.text(), "héllo 🦀!");

        // And one before it, where chars and UTF-16 units still agree, so a
        // failure here would mean something other than the encoding is wrong.
        let change = Change::apply(&mut buffer, vec![Edit::insert(1, "?")]);
        doc.apply_local(&change).unwrap();
        assert_eq!(doc.text(), buffer.text());
        assert_eq!(doc.text(), "h?éllo 🦀!");
    }

    /// A remote edit adjacent to an astral character converts back to chars
    /// exactly — the direction the model replays, where an off-by-one lands
    /// inside a surrogate pair.
    #[test]
    fn remote_splices_come_back_in_chars() {
        let mut a = CollabDoc::create(AWKWARD);
        let mut b = CollabDoc::join();
        let (mut sa, mut sb) = (SyncSession::new(), SyncSession::new());
        for _ in 0..8 {
            let mut moved = false;
            if let Some(m) = a.generate_message(&mut sa) {
                b.receive_message(&mut sb, &m).unwrap();
                moved = true;
            }
            if let Some(m) = b.generate_message(&mut sb) {
                a.receive_message(&mut sa, &m).unwrap();
                moved = true;
            }
            if !moved {
                break;
            }
        }
        assert_eq!(b.text(), AWKWARD);

        let mut buffer = Buffer::from_text(AWKWARD);
        let change = Change::apply(&mut buffer, vec![Edit::insert(7, "X")]);
        a.apply_local(&change).unwrap();
        let msg = a.generate_message(&mut sa).expect("an update to send");
        let splices = b.receive_message(&mut sb, &msg).unwrap();

        assert_eq!(splices.len(), 1);
        assert_eq!(
            splices[0].pos, 7,
            "the splice must be in chars, not bytes (11) or UTF-16 units (8)"
        );
        assert_eq!(b.text(), "héllo 🦀X");
    }
}
