//! Two collaborators moving around a document, so the presence overlays can be
//! seen working.
//!
//! The peers here are *scripted*, not networked, and that is the point rather
//! than a shortcut: `CodeEditor` takes presence as a plain
//! `Signal<Vec<RemotePresence>>` and has no idea where it came from. This is the
//! whole integration surface an application has to implement — map whatever your
//! collaboration layer calls a participant into these, write the signal, done.
//! A real embedder swaps the timer below for
//! `rinch_code_editor_collab::CollabDoc::peers()` and changes nothing else.
//!
//! Run with `cargo run --release --bin presence-demo`.
//!
//! The script is chosen to exercise the cases that are easy to get wrong:
//! a bare caret, a forward selection, a *reversed* selection (its caret has to
//! sit clear of its own tint), a selection spanning several lines, and a caret
//! on line 0 (its name chip has nowhere above to go and must flip below).

use std::cell::Cell;
use std::rc::Rc;

use rinch::core::set_timeout;
use rinch::prelude::*;
use rinch_code_editor::{CodeEditor, RemotePresence, Rgba, Selection};

/// How long each step of the script holds. Comfortably longer than the 1.5s
/// name-chip dwell, so every move shows the chip appearing and then giving way
/// to the dot again.
const STEP_MS: u32 = 2500;

const SAMPLE: &str = r#"//! A shared document. Two people are editing it.

use std::collections::HashMap;

/// Tally how often each word appears.
fn word_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word.to_lowercase()).or_insert(0) += 1;
    }
    counts
}

fn fibonacci(n: u32) -> u64 {
    match n {
        0 => 0,
        1 => 1,
        _ => fibonacci(n - 1) + fibonacci(n - 2),
    }
}

fn main() {
    let counts = word_counts("the cat sat on the mat");
    for (word, n) in &counts {
        println!("{word}: {n}");
    }
}
"#;

/// Char offset of the first occurrence of `needle`. Panics if it is missing —
/// the script is written against this file, so a miss is a typo, not a runtime
/// condition worth handling.
fn at(needle: &str) -> usize {
    let byte = SAMPLE.find(needle).expect("sample must contain the anchor");
    SAMPLE[..byte].chars().count()
}

/// A selection over `needle`, pointing forwards.
fn over(needle: &str) -> Selection {
    let start = at(needle);
    Selection::new(start, start + needle.chars().count())
}

/// The same span, pointing backwards — the caret lands at its *start*.
fn back_over(needle: &str) -> Selection {
    let sel = over(needle);
    Selection::new(sel.head, sel.anchor)
}

fn ada(selection: Selection) -> RemotePresence {
    RemotePresence {
        id: 1,
        name: "Ada".to_string(),
        // A light colour: its label has to come out black.
        color: Rgba::rgb(0x30, 0xbc, 0xed),
        selection,
    }
}

fn grace(selection: Selection) -> RemotePresence {
    RemotePresence {
        id: 2,
        name: "Grace".to_string(),
        // A dark one: its label has to come out white.
        color: Rgba::rgb(0x6f, 0x42, 0xc1),
        selection,
    }
}

/// The loop both peers walk. Each entry is one frame of presence.
fn script() -> Vec<Vec<RemotePresence>> {
    vec![
        // A bare caret each — no tint, just the bar and the dot.
        vec![
            ada(Selection::cursor(at("HashMap::new()"))),
            grace(Selection::cursor(at("fibonacci(n - 1)"))),
        ],
        // A forward selection, and a caret on line 0 whose chip must flip below.
        vec![
            ada(over("split_whitespace")),
            grace(Selection::cursor(at("A shared document"))),
        ],
        // A reversed selection: Grace's caret is at the *left* edge of her own
        // tint and has to be shifted clear of it to stay visible.
        vec![
            ada(over("to_lowercase")),
            grace(back_over("word_counts(\"the cat sat on the mat\")")),
        ],
        // A selection spanning several lines, including its line breaks.
        vec![
            ada(over("match n {\n        0 => 0,\n        1 => 1,")),
            grace(over("println!(\"{word}: {n}\")")),
        ],
        // Ada leaves: her caret, dot and chip must all disappear together.
        vec![grace(over("counts"))],
    ]
}

/// Advance the script forever. A timer callback is one of the two contexts where
/// writing a rinch signal is safe (the other being an event handler) — a write
/// flushes subscribers synchronously, so doing it from inside a running effect
/// would re-enter that effect.
fn advance(
    peers: Signal<Vec<RemotePresence>>,
    script: Rc<Vec<Vec<RemotePresence>>>,
    step: Rc<Cell<usize>>,
) {
    set_timeout(STEP_MS, move || {
        let next = (step.get() + 1) % script.len();
        step.set(next);
        peers.set(script[next].clone());
        advance(peers, script, step);
    });
}

#[component]
fn app() -> NodeHandle {
    let script = Rc::new(script());
    let peers = Signal::new(script[0].clone());
    advance(peers, script, Rc::new(Cell::new(0)));

    rsx! {
        div {
            style: "width:100%; height:100%; padding:24px; background:#131313; box-sizing:border-box;",
            div {
                style: "width:100%; height:100%; border:1px solid #000; border-radius:6px; overflow:hidden;",
                CodeEditor {
                    text: SAMPLE.to_string(),
                    language: "rust",
                    peers: peers,
                }
            }
        }
    }
}

fn main() {
    run("rinch-ce presence demo", 900, 600, app);
}
