//! A handle onto a mounted editor — what a collaborative binding drives.
//!
//! The editor owns its document. That is the right default: nearly every
//! embedder wants a text box, not a document to administer. But a collaborative
//! session needs to reach in — to replay a peer's edits, to collect this
//! editor's own for publication, and to read where the caret is — and rinch
//! gives a component no way to hand anything back to its parent (there is no
//! `ref:` prop, and a component body's return value is a DOM node).
//!
//! So the parent brings the container and the editor fills it in: construct an
//! [`EditorHandle`], pass it as the `handle` prop, and it is attached by the
//! time the first event fires. This is the same shape as the reference
//! bindings, which take an already-constructed editor object —
//! `new MonacoBinding(ytext, model, editors, awareness)` in y-monaco — rather
//! than constructing one.
//!
//! # The order that is not optional
//!
//! Local changes must reach the shared document **before** any remote edit is
//! replayed. A peer's splices were computed against a document that has already
//! absorbed your keystrokes; applying them to a buffer holding edits the
//! document has never seen puts every offset in the batch somewhere else. So a
//! pump tick is always: [`take_local_changes`](EditorHandle::take_local_changes)
//! → publish → receive → [`apply_remote`](EditorHandle::apply_remote). Getting
//! it backwards is not a subtle degradation, and the model asserts on it in
//! release builds rather than diverging quietly.
//!
//! # Why nothing here echoes
//!
//! An edit replayed through [`apply_remote`](EditorHandle::apply_remote) is
//! never recorded as a local change (`EditorModel::apply_edit` does not record
//! — recording happens only on the local edit path), so it cannot come back
//! round and be republished. y-monaco and y-codemirror.next both need an
//! explicit origin tag on the transaction to get this right; here the two paths
//! into the buffer are simply different functions, and only one of them records.

use std::cell::RefCell;
use std::rc::Rc;

use rinch::core::Signal;
use rinch_code_editor_model::{Change, EditorModel, Selection, Splice};

/// A live connection to one mounted [`CodeEditor`](crate::CodeEditor).
///
/// Construct one, pass it as the `handle` prop, keep your copy. Cloning is
/// cheap and every clone refers to the same editor. Until the component has
/// mounted, and after it unmounts, every method is a safe no-op — see
/// [`is_attached`](Self::is_attached).
///
/// Passing a handle is also what puts the editor into collaborative mode: it
/// starts recording local changes for [`take_local_changes`](Self::take_local_changes).
/// An editor with no handle records nothing and costs nothing.
#[derive(Clone, Default)]
pub struct EditorHandle(Rc<RefCell<Option<Attached>>>);

struct Attached {
    model: Rc<RefCell<EditorModel>>,
    version: Signal<u64>,
    focused: Signal<bool>,
}

impl std::fmt::Debug for EditorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("EditorHandle")
            .field(&if self.is_attached() {
                "attached"
            } else {
                "detached"
            })
            .finish()
    }
}

impl EditorHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the editor this belongs to is currently mounted.
    ///
    /// Worth checking before a pump tick does real work: a handle whose editor
    /// has gone away answers every question with "nothing", which is
    /// indistinguishable from an idle editor.
    pub fn is_attached(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// The document's full text.
    pub fn text(&self) -> String {
        self.with(|model| model.borrow().text()).unwrap_or_default()
    }

    /// Take every local change recorded since the last call, oldest first, and
    /// hand them to the shared document in that order.
    ///
    /// Includes undo and redo, deliberately: to a shared document, reverting is
    /// not a rewind but another edit, and one that goes unpublished silently
    /// splits the session in two.
    pub fn take_local_changes(&self) -> Vec<Change> {
        self.with(|model| model.borrow_mut().take_local_changes())
            .unwrap_or_default()
    }

    /// Replay a collaborator's edits and repaint. Returns whether anything
    /// actually changed.
    ///
    /// Deliberately does **not** scroll: a remote edit is not a reason to move
    /// this reader's viewport, and the caret-reveal that follows local input is
    /// skipped here for exactly that reason.
    ///
    /// Call only from an event handler or a timer callback. This writes a
    /// signal, and a rinch signal write flushes its subscribers synchronously —
    /// from inside a running effect that re-enters the effect and panics.
    pub fn apply_remote(&self, splices: &[Splice]) -> bool {
        let Some(attached) = self.snapshot() else {
            return false;
        };
        // Confine the borrow: the version bump flushes render effects, and they
        // read the model.
        let changed = {
            let mut model = attached.model.borrow_mut();
            model.apply_remote(splices).is_some()
        };
        if changed {
            attached.version.update(|v| *v = v.wrapping_add(1));
        }
        changed
    }

    /// Where this editor's caret is, for publishing as presence — or `None`
    /// when it does not have one worth publishing.
    ///
    /// `None` means the editor is unmounted or unfocused, and unfocused is the
    /// interesting case: it is how "I am still here but I am not editing this"
    /// is expressed, and republishing it is what removes your caret from
    /// everyone else's screen. The editor reports a focus change through
    /// `on_change` precisely so this gets re-read.
    ///
    /// The *primary* selection only. Presence carries one caret per person —
    /// Yjs's awareness shape, which the whole ecosystem reads — so a
    /// multi-cursor local selection publishes its primary, as y-codemirror.next
    /// publishes CodeMirror's main range.
    pub fn selection(&self) -> Option<Selection> {
        let attached = self.snapshot()?;
        if !attached.focused.get() {
            return None;
        }
        let model = attached.model.borrow();
        Some(*model.selections().primary())
    }

    /// Called once by the component, at mount.
    pub(crate) fn attach(
        &self,
        model: Rc<RefCell<EditorModel>>,
        version: Signal<u64>,
        focused: Signal<bool>,
    ) {
        // Holding a handle is the opt-in: from here on every local edit is
        // queued for publication.
        model.borrow_mut().record_local_changes(true);
        *self.0.borrow_mut() = Some(Attached {
            model,
            version,
            focused,
        });
    }

    /// Called by the component's cleanup. Drops the editor's model, so a handle
    /// an application forgot to release cannot keep a whole document alive.
    pub(crate) fn detach(&self) {
        *self.0.borrow_mut() = None;
    }

    fn with<T>(&self, f: impl FnOnce(&Rc<RefCell<EditorModel>>) -> T) -> Option<T> {
        self.0.borrow().as_ref().map(|a| f(&a.model))
    }

    /// The attached parts, copied out so the outer `RefCell` is not still
    /// borrowed while signals are written and effects run.
    fn snapshot(&self) -> Option<Attached> {
        self.0.borrow().as_ref().map(|a| Attached {
            model: a.model.clone(),
            version: a.version,
            focused: a.focused,
        })
    }
}
