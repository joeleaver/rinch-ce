//! The shared key interceptor must outlive whichever editor installed it.
//!
//! rinch ties an interceptor installed during a render to that render's scope
//! and clears it when the scope is disposed. The interceptor is installed once,
//! by the first editor to mount, so tying it to that editor meant that once it
//! unmounted no editor heard another key — an app that remounts the editor per
//! file, as Playweft's Code workspace does, went deaf on its second file
//! (rinch-ce#10). Each test mounts editors the way rinch does, inside their own
//! scope, and checks that a key still reaches whichever one has focus.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use rinch::core::{
    KeyEventData, Memo, NodeHandle, NodeId, Scope, Signal, dispatch_keyboard_event,
    push_dispatching_doc,
};
use rinch_code_editor_model::{EditorModel, Grid, PlainHighlighter};
use rinch_core::dom::mock::MockDomDocument;

use super::{EditorId, deregister, focus, register};
use crate::highlight::dark_theme;
use crate::input::InputCtx;
use crate::mouse::ClickCounter;
use crate::styles::LINE_HEIGHT;

/// One mounted editor: its scope (disposing it is the unmount) and its model.
struct Mounted {
    id: EditorId,
    scope: Scope,
    model: Rc<RefCell<EditorModel>>,
}

impl Mounted {
    /// Build an editor's input wiring and register it from inside its own
    /// scope, as the component does from its render.
    fn mount() -> Self {
        let scope = Scope::new();
        let model = Rc::new(RefCell::new(EditorModel::from_text(
            "",
            Box::new(PlainHighlighter::new(dark_theme())),
        )));
        let id = EditorId::next();
        scope.run(|| {
            let detached: Weak<RefCell<MockDomDocument>> = Weak::new();
            let ctx = InputCtx {
                id,
                model: model.clone(),
                grid: Grid::new(1.0, LINE_HEIGHT, 4),
                version: Signal::new(0),
                focused: Signal::new(false),
                cell_w: Memo::new(|| 1.0),
                content_w: Memo::new(|| 0.0),
                scroll_top: Signal::new(0.0),
                stop_after: Signal::new(usize::MAX),
                view: NodeHandle::new(NodeId(0), detached),
                after_op: Rc::new(|_| {}),
                blink_restart: Rc::new(|| {}),
                clicks: Rc::new(RefCell::new(ClickCounter::new())),
                gesture: Rc::new(RefCell::new(None)),
                scroller: Rc::new(RefCell::new(None)),
                on_change: Rc::new(|| {}),
            };
            register(id, ctx);
            rinch::core::reactive::on_cleanup(move || deregister(id));
            focus(id);
        });
        Self { id, scope, model }
    }

    fn unmount(&self) {
        self.scope.dispose();
    }

    fn text(&self) -> String {
        self.model.borrow().buffer().text()
    }
}

fn type_key(key: &str) -> bool {
    dispatch_keyboard_event(&KeyEventData::new(key, key))
}

#[test]
fn an_editor_mounted_after_the_first_unmounts_still_hears_keys() {
    let a = Mounted::mount();
    assert!(type_key("a"));
    assert_eq!(a.text(), "a");
    a.unmount();

    let b = Mounted::mount();
    assert!(type_key("b"), "the key reached no editor");
    assert_eq!(b.text(), "b");
    b.unmount();
}

#[test]
fn a_later_editor_unmounting_leaves_the_first_one_hearing_keys() {
    let a = Mounted::mount();
    let b = Mounted::mount();
    b.unmount();

    // Every mount takes focus, so `b` holds it until something gives it back.
    focus(a.id);
    assert!(type_key("a"), "the key reached no editor");
    assert_eq!(a.text(), "a");
    a.unmount();
}

#[test]
fn an_editor_in_a_second_document_hears_keys() {
    let a = {
        let _doc = push_dispatching_doc(1);
        Mounted::mount()
    };
    let b = {
        let _doc = push_dispatching_doc(2);
        Mounted::mount()
    };
    {
        let _doc = push_dispatching_doc(2);
        assert!(type_key("b"), "the second document's key reached no editor");
    }
    assert_eq!(b.text(), "b");
    b.unmount();
    a.unmount();
}
