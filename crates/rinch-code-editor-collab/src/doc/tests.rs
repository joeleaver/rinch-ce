//! Unit tests for the document itself — the local edit path, the snapshot
//! round trip, and the mirror invariant behind both.
//!
//! These live inside the crate rather than in `tests/` because they call
//! [`CollabDoc::assert_mirror_intact`], which is private on purpose: the mirror
//! is the thing a careless edit corrupts silently, and this project tests
//! exclusively in release, where the edit path's `debug_assert_eq!`s are
//! compiled out. The protocol, presence, convergence and foreign-peer suites
//! drive only the public surface and live in `tests/`.

use super::*;
use rinch_code_editor_buffer::{Buffer, Edit};

/// The load-bearing equivalence: `apply_local`'s running-delta walk lands
/// every edit at the same place `Change::apply` does, including multi-
/// cursor changes and text where chars ≠ bytes ≠ UTF-16 units.
#[test]
fn apply_local_matches_change_apply() {
    for (initial, edits) in [
        ("hello world", vec![Edit::insert(5, ",")]),
        (
            "abc",
            vec![
                Edit::insert(0, "x"),
                Edit::insert(1, "yy"),
                Edit::insert(3, "z"),
            ],
        ),
        (
            "one two three",
            vec![
                Edit::replace(0..3, "1"),
                Edit::delete(4..7),
                Edit::replace(8..13, "33333"),
            ],
        ),
        (
            "héllo 🦀 wörld",
            vec![Edit::replace(1..2, "é🦀é"), Edit::delete(6..8)],
        ),
        // Touching edits (one ends exactly where the next begins) and two
        // insertions at the same offset — both legal (non-overlapping) and
        // the running-delta walk must keep them aligned.
        (
            "abcdef",
            vec![Edit::replace(0..3, "XYZ"), Edit::replace(3..6, "W")],
        ),
        ("abc", vec![Edit::insert(1, "1"), Edit::insert(1, "2")]),
    ] {
        let mut buffer = Buffer::from_text(initial);
        let mut doc = CollabDoc::create(initial);
        let change = Change::apply(&mut buffer, edits);
        doc.apply_local(&change).unwrap();
        assert_eq!(doc.text(), buffer.text(), "diverged from {initial:?}");
        doc.assert_mirror_intact();
    }
}

#[test]
fn undo_applied_as_a_change_round_trips() {
    let mut buffer = Buffer::from_text("hello world");
    let mut doc = CollabDoc::create("hello world");
    let change = Change::apply(&mut buffer, vec![Edit::replace(0..5, "goodbye")]);
    doc.apply_local(&change).unwrap();

    // The model captures an undo as a fresh Change built from the inverse
    // edits — reverting is just another edit to a shared document — so the
    // doc must track it like any other local change.
    let undo = Change::apply(&mut buffer, change.inverse().to_vec());
    doc.apply_local(&undo).unwrap();
    assert_eq!(doc.text(), "hello world");
    assert_eq!(buffer.text(), "hello world");
    doc.assert_mirror_intact();
}

#[test]
fn local_edit_before_first_sync_is_refused() {
    let mut buffer = Buffer::from_text("");
    let mut doc = CollabDoc::join();
    let change = Change::apply(&mut buffer, vec![Edit::insert(0, "too eager")]);
    assert!(matches!(
        doc.apply_local(&change),
        Err(CollabError::NotSynced)
    ));
}

#[test]
fn is_synced_reflects_content_presence() {
    assert!(
        CollabDoc::create("x").is_synced(),
        "creator is synced at once"
    );
    assert!(CollabDoc::create("").is_synced(), "even an empty creator");
    assert!(
        !CollabDoc::join().is_synced(),
        "a fresh joiner is not synced"
    );
    let mut seed = CollabDoc::create("x");
    assert!(
        CollabDoc::load(&seed.save()).unwrap().is_synced(),
        "a loaded document is synced"
    );
    let mut empty = CollabDoc::create("");
    assert!(
        CollabDoc::load(&empty.save()).unwrap().is_synced(),
        "including one loaded from an empty snapshot"
    );
}

#[test]
fn empty_change_is_a_harmless_readiness_probe_before_sync() {
    // NotSynced is returned *before* the doc is touched, so an empty Change
    // probes readiness without mutating anything — but is_synced() is the
    // intended, side-effect-free way to ask.
    let mut doc = CollabDoc::join();
    let empty = Change::apply(&mut Buffer::from_text(""), vec![]);
    assert!(matches!(
        doc.apply_local(&empty),
        Err(CollabError::NotSynced)
    ));
    assert_eq!(doc.text(), "");
}

#[test]
fn empty_change_on_a_synced_doc_is_a_noop() {
    let mut doc = CollabDoc::create("unchanged");
    let empty = Change::apply(&mut Buffer::from_text("unchanged"), vec![]);
    doc.apply_local(&empty).unwrap();
    assert_eq!(doc.text(), "unchanged");
    doc.assert_mirror_intact();
}

#[test]
fn a_change_reaching_past_the_shared_text_is_rejected_whole() {
    // The model and the document have disagreed about the contents — an
    // integration bug. What matters is that the document does not end up
    // half-edited with a *mirror* describing text that no longer exists:
    // the mirror is what every later offset conversion is computed against,
    // so a stale one silently corrupts every splice that follows it.
    let mut doc = CollabDoc::create("abc");
    let mut long = Buffer::from_text("abcdefgh");
    let change = Change::apply(&mut long, vec![Edit::insert(0, "!"), Edit::delete(5..8)]);

    let err = doc.apply_local(&change).unwrap_err();
    // `len` is the length as of the failing edit — the insertion before it
    // has already been accounted for by the running walk, exactly as it
    // would have been had the change been applied.
    assert!(
        matches!(
            err,
            CollabError::OutOfRange {
                pos: 5,
                del: 3,
                len: 4
            }
        ),
        "got {err:?}"
    );
    assert_eq!(doc.text(), "abc", "the first edit must not have landed");
    assert_eq!(doc.len_chars(), 3);
    doc.assert_mirror_intact();

    // And the document still works: the mirror was never spoiled.
    let mut buffer = Buffer::from_text("abc");
    let ok = Change::apply(&mut buffer, vec![Edit::insert(3, "d")]);
    doc.apply_local(&ok).unwrap();
    assert_eq!(doc.text(), "abcd");
    doc.assert_mirror_intact();
}

#[test]
fn len_chars_tracks_the_shared_text() {
    let mut doc = CollabDoc::create("héllo 🦀");
    assert_eq!(
        doc.len_chars(),
        7,
        "chars, not bytes (11) or UTF-16 units (8)"
    );
    assert_eq!(CollabDoc::join().len_chars(), 0);

    let mut buffer = Buffer::from_text("héllo 🦀");
    let change = Change::apply(&mut buffer, vec![Edit::delete(0..6)]);
    doc.apply_local(&change).unwrap();
    assert_eq!(doc.len_chars(), 1);
    doc.assert_mirror_intact();
}

#[test]
fn snapshot_round_trip_preserves_text() {
    let mut doc = CollabDoc::create("héllo 🦀");
    let loaded = CollabDoc::load(&doc.save()).unwrap();
    assert_eq!(loaded.text(), "héllo 🦀");
    loaded.assert_mirror_intact();
}

/// An empty document is a document. It round-trips, it is synced, and it is
/// editable afterwards.
///
/// This is a regression pin, not a formality: yrs encodes an empty document
/// as an update carrying **no root type at all** (two bytes), so `load`
/// used to look for the content key, not find it, and refuse the snapshot
/// with `SnapshotMissingContent` — meaning saving and reopening an empty
/// file was an error. See [`CollabDoc::load`] for why no check can tell an
/// empty document from a foreign one.
#[test]
fn an_empty_snapshot_round_trips_and_is_editable() {
    let mut doc = CollabDoc::create("");
    let bytes = doc.save();
    let mut loaded = CollabDoc::load(&bytes).expect("an empty document is still a document");
    assert_eq!(loaded.text(), "");
    assert_eq!(loaded.len_chars(), 0);
    assert!(loaded.is_synced());

    let mut buffer = Buffer::from_text("");
    let change = Change::apply(&mut buffer, vec![Edit::insert(0, "typed after reopening")]);
    loaded.apply_local(&change).unwrap();
    assert_eq!(loaded.text(), "typed after reopening");
    loaded.assert_mirror_intact();
}

#[test]
fn garbage_bytes_are_refused_by_load() {
    for bytes in [&b"not a yrs document"[..], &b""[..], &[0xffu8; 8][..]] {
        assert!(
            matches!(CollabDoc::load(bytes), Err(CollabError::BadSnapshot(_))),
            "expected a decode failure from {bytes:?}"
        );
    }
}

/// A document that is not ours loads as an empty one, deliberately.
///
/// yrs cannot distinguish it: root types are untyped on the wire (a root
/// arriving in an update reports `TypeRef::Undefined` whether it was a text
/// or a map at the source), and `get_text` coerces rather than
/// discriminates. This test exists so that the next person to notice the
/// missing check finds the reasoning attached to the behaviour instead of
/// reinstating a guard that rejects empty documents to catch nothing.
#[test]
fn a_foreign_document_loads_as_empty_because_yrs_cannot_tell() {
    use yrs::{Any, Map};

    let foreign = Doc::new();
    let map = foreign.get_or_insert_map(CONTENT);
    {
        let mut txn = foreign.transact_mut();
        map.insert(&mut txn, "k", Any::from("a map, not a text"));
    }
    let bytes = foreign
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let loaded = CollabDoc::load(&bytes).expect("it decodes, so it loads");
    assert_eq!(
        loaded.text(),
        "",
        "the map is invisible to a text projection"
    );
    loaded.assert_mirror_intact();
}
