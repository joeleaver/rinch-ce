//! The clipboard bridge: the system clipboard on one side, the model's
//! Monaco-ported semantics on the other.
//!
//! All the *decisions* — what a copy takes, how a cut deletes, where a paste
//! lands — live in `rinch-code-editor-model`'s clipboard module, rinch-free
//! and unit-tested. This module only moves strings through
//! `rinch::clipboard` (sync on every backend) and keeps the one piece of
//! state Monaco keeps outside its editor: the metadata of the last copy.
//!
//! # The metadata store
//!
//! Monaco's `InMemoryClipboardMetadataManager` remembers, beside the system
//! clipboard, whether the last copy came from an empty selection (so pasting
//! it inserts the line *above* the caret's) and the per-cursor pieces of a
//! multi-cursor copy (so pasting onto the same cursor count distributes
//! them). The store is honoured only while the clipboard still holds exactly
//! the copied text — strict string equality, cleared on mismatch — so
//! anything copied outside the editor pastes plainly. One `thread_local`
//! mirrors Monaco's process-wide singleton: rinch is single-threaded, and
//! sharing it across editor instances is exactly the Monaco behavior.
//!
//! # Web caveat (upstream gap)
//!
//! On wasm, `rinch::clipboard::paste_text` reads an in-page buffer that only
//! `copy_text` fills — rinch-web wires no browser `paste` event into it — so
//! pasting *external* content cannot work there today; copy/cut out of the
//! editor and paste of the editor's own copies still do. Desktop is fully
//! functional through arboard.

use std::cell::RefCell;

use rinch_code_editor_model::{ClipboardPayload, EditSummary, EditorModel};

thread_local! {
    /// The last copy's payload — Monaco's in-memory clipboard metadata.
    static LAST_COPY: RefCell<Option<ClipboardPayload>> = const { RefCell::new(None) };
}

/// Copy: put the payload's text on the system clipboard and remember its
/// metadata. A failed system write (a clipboard service hiccup) is ignored,
/// as in Monaco: the stale-text equality check on paste already handles a
/// clipboard that does not hold what we think it does.
///
/// The payload uses `\r\n` on Windows — Monaco passes `isWindows` as
/// `getPlainTextToCopy`'s `forceCRLF`, matching the platform's clipboard
/// convention so multi-line copies survive CRLF-only consumers. The stored
/// metadata holds the same text, so the paste-side equality check still
/// matches, and the model normalizes line endings back on insert.
pub(crate) fn copy(m: &EditorModel) {
    let payload = m.clipboard_payload(cfg!(target_os = "windows"));
    let _ = rinch::clipboard::copy_text(&payload.text);
    LAST_COPY.set(Some(payload));
}

/// Cut: the copy above, then the model's line-wise delete. `None` when
/// nothing got deleted (the clipboard is still written — cutting in an empty
/// document copies its newline, as Monaco does).
pub(crate) fn cut(m: &mut EditorModel) -> Option<EditSummary> {
    copy(m);
    m.cut()
}

/// Paste: read the system clipboard and hand the text to the model, with the
/// stored metadata attached only if the clipboard still holds exactly the
/// text of our last copy (mismatch clears the store — Monaco's rule). An
/// unreadable or empty clipboard pastes nothing.
///
/// The read is synchronous on the UI thread — rinch's clipboard API has no
/// async path — so a hung X11 selection owner can stall the app for arboard's
/// ~4s timeout before the paste no-ops. Bounded, rare, and inherent to the
/// upstream API; noted so nobody rediscovers it the hard way.
pub(crate) fn paste(m: &mut EditorModel) -> Option<EditSummary> {
    let Ok(text) = rinch::clipboard::paste_text() else {
        return None;
    };
    if text.is_empty() {
        return None;
    }
    let meta = LAST_COPY.with_borrow_mut(|slot| match slot {
        Some(payload) if payload.text == text => Some(payload.clone()),
        _ => {
            *slot = None;
            None
        }
    });
    match meta {
        Some(p) => m.paste(&text, p.is_from_empty_selection, p.multicursor_text.as_deref()),
        None => m.paste(&text, false, None),
    }
}
